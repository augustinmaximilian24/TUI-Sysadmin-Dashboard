//! `eframe::App`-Implementierung: Kopfbereich, Live-Graph, Anomalie-Liste,
//! Detail-Panel und Systemzustands-Panel (Phase 7).
//!
//! Läuft im reaktiven Repaint-Modus (Regel 20): `update()` selbst startet
//! keinen Timer, sondern wird vom Hintergrund-Task in `client.rs` über
//! `ctx.request_repaint()` aufgeweckt, sobald neue Daten da sind. Die
//! einzige Ausnahme ist die Sekunden-Anzeige der Reconnect-Wartezeit, die
//! sich ohne neue Serverdaten ändert -- dafür wird gezielt
//! `request_repaint_after` verwendet, nicht Continuous-Repaint.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use eframe::egui;
use egui_plot::{Line, Plot, PlotPoints};

use logsentry_proto::{AnomalyEvent, AnomalyLevel, ClientMessage, ConnectionState, Snapshot};

use crate::client::{connection_label, is_connected, GuiState, HistoryPoint};

/// Wie viele Rohzeilen vor/nach dem Anomalie-Zeitpunkt beim Laden des
/// Kontexts angefragt werden. Der Daemon klemmt bei Bedarf serverseitig
/// weiter (`context_max_lines`); `truncated` in der Antwort zeigt das an.
const CONTEXT_LINES_EACH_SIDE: u16 = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LevelFilter {
    All,
    Info,
    Warn,
    Critical,
}

impl LevelFilter {
    fn matches(self, level: AnomalyLevel) -> bool {
        match self {
            LevelFilter::All => true,
            LevelFilter::Info => level == AnomalyLevel::Info,
            LevelFilter::Warn => level == AnomalyLevel::Warn,
            LevelFilter::Critical => level == AnomalyLevel::Critical,
        }
    }

    fn label(self) -> &'static str {
        match self {
            LevelFilter::All => "Alle",
            LevelFilter::Info => "Info",
            LevelFilter::Warn => "Warn",
            LevelFilter::Critical => "Critical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Time,
    Score,
}

/// Momentaufnahme des geteilten Zustands für einen Frame, damit
/// `Pause` das Mutex nicht bei jedem Redraw erneut sperren muss und die
/// UI-Logik auf Klondaten statt einer gehaltenen Sperre arbeitet.
#[derive(Default)]
struct RenderSnapshot {
    connection: Option<ConnectionState>,
    hostname: Option<String>,
    snapshot: Option<Snapshot>,
    history: Vec<HistoryPoint>,
    anomalies: Vec<AnomalyEvent>,
    context: Option<logsentry_proto::ContextReply>,
    log: Vec<String>,
}

pub struct LogsentryApp {
    state: Arc<Mutex<GuiState>>,
    outbound: tokio::sync::mpsc::Sender<ClientMessage>,
    paused: bool,
    dark_mode: bool,
    search: String,
    unit_filter: String,
    level_filter: LevelFilter,
    sort_key: SortKey,
    selected_anomaly: Option<u64>,
    next_request_id: u64,
    pending_context_request: Option<u64>,
    render: RenderSnapshot,
}

impl LogsentryApp {
    pub fn new(
        state: Arc<Mutex<GuiState>>,
        outbound: tokio::sync::mpsc::Sender<ClientMessage>,
    ) -> Self {
        Self {
            state,
            outbound,
            paused: false,
            dark_mode: true,
            search: String::new(),
            unit_filter: String::new(),
            level_filter: LevelFilter::All,
            sort_key: SortKey::Time,
            selected_anomaly: None,
            next_request_id: 1,
            pending_context_request: None,
            render: RenderSnapshot::default(),
        }
    }

    /// Übernimmt einen Klon des geteilten Zustands für diesen Frame. Wird
    /// bei `paused` übersprungen, damit die angezeigte Liste stehen
    /// bleibt, während der Hintergrund-Task weiter empfängt.
    fn refresh(&mut self) {
        if self.paused {
            return;
        }
        let guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.render.connection = guard.connection.clone();
        self.render.hostname = guard.hostname.clone();
        self.render.snapshot = guard.snapshot.clone();
        self.render.history = guard.entropy_history.iter().copied().collect();
        self.render.anomalies = guard.anomalies.iter().cloned().collect();
        self.render.context = guard.context_reply.clone();
        self.render.log = guard.log.iter().cloned().collect();
    }

    fn request_context(&mut self, anomaly: &AnomalyEvent) {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        self.pending_context_request = Some(request_id);
        let message = ClientMessage::GetContext {
            request_id,
            timestamp_us: anomaly.timestamp_us,
            before: CONTEXT_LINES_EACH_SIDE,
            after: CONTEXT_LINES_EACH_SIDE,
            unit: anomaly.unit.clone(),
        };
        // `try_send`: die Warteschlange ist bounded (Regel 17); läuft sie
        // voll, ist das kein Grund für die GUI zu blockieren -- die
        // Anfrage wird beim nächsten Klick einfach wiederholt.
        let _ = self.outbound.try_send(message);
    }

    /// Geklonte statt referenzierte Treffer: `self.selected_anomaly` wird
    /// beim Rendern der Liste durch einen Button-Klick verändert, eine
    /// über `self.render.anomalies` geliehene Liste stünde dem im Weg.
    fn filtered_anomalies(&self) -> Vec<AnomalyEvent> {
        let search = self.search.to_lowercase();
        let unit_filter = self.unit_filter.to_lowercase();
        let mut items: Vec<AnomalyEvent> = self
            .render
            .anomalies
            .iter()
            .filter(|a| self.level_filter.matches(a.level))
            .filter(|a| {
                unit_filter.is_empty()
                    || a.unit
                        .as_deref()
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains(&unit_filter)
            })
            .filter(|a| {
                search.is_empty()
                    || a.template_text.to_lowercase().contains(&search)
                    || a.sample_message.to_lowercase().contains(&search)
            })
            .cloned()
            .collect();

        match self.sort_key {
            SortKey::Time => items.sort_by_key(|a| std::cmp::Reverse(a.timestamp_us)),
            SortKey::Score => items.sort_by(|a, b| {
                b.breakdown
                    .combined
                    .partial_cmp(&a.breakdown.combined)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
        }
        items
    }
}

impl eframe::App for LogsentryApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.refresh();

        // Nur während des Reconnects erneut aufwecken, damit die
        // Sekundenanzeige der Wartezeit weiterläuft, ohne in den
        // Continuous-Modus zu wechseln (Regel 20).
        if !matches!(self.render.connection, Some(ConnectionState::Connected { .. })) {
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        ctx.set_visuals(if self.dark_mode {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });

        self.draw_header(ctx);
        self.draw_system_panel(ctx);
        self.draw_detail_panel(ctx);
        self.draw_central(ctx);
    }
}

impl LogsentryApp {
    fn draw_header(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let host = self.render.hostname.as_deref().unwrap_or("–");
                ui.heading(format!("logsentry · {host}"));
                ui.separator();

                if let Some(snapshot) = &self.render.snapshot {
                    ui.label(format!("Uptime {}s", snapshot.daemon_uptime_secs));
                    ui.separator();
                    ui.label(format!("Entropie {:.2} bit", snapshot.window.entropy_bits));
                    ui.separator();
                    ui.label(format!("Verworfen {}", snapshot.stats.dropped_overflow));
                    ui.separator();
                    if snapshot.learning.active {
                        let remaining = snapshot
                            .learning
                            .remaining_secs
                            .map(|s| format!("{s}s"))
                            .unwrap_or_else(|| "?".to_string());
                        ui.colored_label(egui::Color32::YELLOW, format!("Lernphase ({remaining})"));
                    } else {
                        ui.label("Lernphase beendet");
                    }
                    ui.separator();
                    if snapshot.replay {
                        ui.colored_label(egui::Color32::LIGHT_BLUE, "Replay");
                        ui.separator();
                    }
                } else {
                    ui.label("noch keine Daten vom Daemon");
                    ui.separator();
                }

                ui.label(connection_label(self.render.connection.as_ref()));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(if self.dark_mode { "☀ Hell" } else { "🌙 Dunkel" })
                        .clicked()
                    {
                        self.dark_mode = !self.dark_mode;
                    }
                    let pause_label = if self.paused { "▶ Fortsetzen" } else { "⏸ Pause" };
                    if ui.button(pause_label).clicked() {
                        self.paused = !self.paused;
                    }
                });
            });
        });
    }

    fn draw_system_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("system_panel")
            .resizable(true)
            .default_width(260.0)
            .show(ctx, |ui| {
                ui.heading("Systemzustand");
                match self.render.snapshot.as_ref().and_then(|s| s.system.as_ref()) {
                    None => {
                        ui.label("noch keine Messung");
                    }
                    Some(system) => {
                        ui.label(format!("CPU {:.1} %", system.cpu.global_usage_percent));
                        ui.label(format!("RAM {:.1} %", system.memory.used_percent));
                        ui.label(format!(
                            "Load {:.2} / {:.2} / {:.2}",
                            system.load.one, system.load.five, system.load.fifteen
                        ));
                        if !system.temperatures.is_empty() {
                            ui.separator();
                            for temp in &system.temperatures {
                                ui.label(format!("{}: {:.1} °C", temp.label, temp.celsius));
                            }
                        }
                        if !system.disks.is_empty() {
                            ui.separator();
                            for disk in &system.disks {
                                ui.label(format!(
                                    "{}: {:.0} % belegt",
                                    disk.mount_point, disk.used_percent
                                ));
                            }
                        }
                        if !system.units.is_empty() {
                            ui.separator();
                            ui.label("Units:");
                            for unit in &system.units {
                                ui.label(format!(
                                    "{} — {}/{}",
                                    unit.name, unit.active_state, unit.sub_state
                                ));
                            }
                        }
                    }
                }

                if !self.render.log.is_empty() {
                    ui.separator();
                    ui.heading("Meldungen");
                    egui::ScrollArea::vertical()
                        .id_salt("log_scroll")
                        .max_height(150.0)
                        .show(ui, |ui| {
                            for line in self.render.log.iter().rev() {
                                ui.label(line);
                            }
                        });
                }
            });
    }

    fn draw_detail_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("detail_panel")
            .resizable(true)
            .default_width(360.0)
            .show(ctx, |ui| {
                ui.heading("Detail");
                let Some(selected_id) = self.selected_anomaly else {
                    ui.label("Anomalie in der Liste auswählen");
                    return;
                };
                let Some(anomaly) = self
                    .render
                    .anomalies
                    .iter()
                    .find(|a| a.id == selected_id)
                    .cloned()
                else {
                    ui.label("Anomalie nicht mehr im Speicher");
                    return;
                };

                ui.label(format!("ID {}", anomaly.id));
                ui.label(format!("Unit: {}", anomaly.unit.as_deref().unwrap_or("–")));
                ui.label(format!("PID: {}", anomaly.pid.map_or("–".to_string(), |p| p.to_string())));
                ui.label(format!("Level: {:?}", anomaly.level));
                ui.label(format!("Template: {}", anomaly.template_text));
                ui.label(format!("Beispielzeile: {}", anomaly.sample_message));
                if anomaly.suppressed_since_last > 0 {
                    ui.label(format!(
                        "Seit letzter Meldung unterdrückt: {}",
                        anomaly.suppressed_since_last
                    ));
                }

                ui.separator();
                ui.label("Score-Aufschlüsselung:");
                let b = &anomaly.breakdown;
                ui.label(format!("Gesamt: {:.3}", b.combined));
                ui.label(format!("Rate-Z: {:.2} ({:?})", b.rate_z, b.rate_source));
                ui.label(format!("Surprisal: {:.2} bit", b.surprisal_bits));
                ui.label(format!("Entropie-Z: {:.2}", b.entropy_z));

                ui.separator();
                let connected = is_connected(self.render.connection.as_ref());
                if ui
                    .add_enabled(connected, egui::Button::new("Kontext laden"))
                    .clicked()
                {
                    self.request_context(&anomaly);
                }

                // Nur eine Antwort anzeigen, die tatsächlich zur zuletzt
                // gestellten Anfrage dieser Auswahl gehört -- sonst könnte
                // nach einem Auswahlwechsel kurzzeitig der Kontext der
                // vorherigen Anomalie erscheinen (`GuiState::context_reply`
                // kennt die aktuelle Auswahl nicht, nur `request_id`).
                let matching_context = self
                    .render
                    .context
                    .as_ref()
                    .filter(|reply| Some(reply.request_id) == self.pending_context_request);

                match matching_context {
                    Some(reply) => {
                        if reply.truncated {
                            ui.colored_label(egui::Color32::YELLOW, "Ausschnitt gekürzt");
                        }
                        egui::ScrollArea::vertical()
                            .id_salt("context_scroll")
                            .max_height(300.0)
                            .show(ui, |ui| {
                                for line in &reply.lines {
                                    ui.monospace(&line.message);
                                }
                            });
                    }
                    None => {
                        ui.label("noch kein Kontext geladen");
                    }
                }
            });
    }

    fn draw_central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Verlauf (Entropie)");
            let points: PlotPoints = self.render.history.iter().copied().collect();
            Plot::new("entropy_plot")
                .height(180.0)
                .allow_scroll(false)
                .show(ui, |plot_ui| {
                    plot_ui.line(Line::new(points));
                });

            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Suche:");
                ui.text_edit_singleline(&mut self.search);
                ui.label("Unit:");
                ui.text_edit_singleline(&mut self.unit_filter);
                egui::ComboBox::from_label("Level")
                    .selected_text(self.level_filter.label())
                    .show_ui(ui, |ui| {
                        for option in [
                            LevelFilter::All,
                            LevelFilter::Info,
                            LevelFilter::Warn,
                            LevelFilter::Critical,
                        ] {
                            ui.selectable_value(&mut self.level_filter, option, option.label());
                        }
                    });
                ui.selectable_value(&mut self.sort_key, SortKey::Time, "nach Zeit");
                ui.selectable_value(&mut self.sort_key, SortKey::Score, "nach Score");
            });

            ui.separator();

            let selected_before = self.selected_anomaly;
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("anomaly_grid")
                    .striped(true)
                    .num_columns(5)
                    .show(ui, |ui| {
                        ui.strong("Level");
                        ui.strong("Unit");
                        ui.strong("Score");
                        ui.strong("Template");
                        ui.strong("");
                        ui.end_row();

                        for anomaly in self.filtered_anomalies() {
                            let level_color = match anomaly.level {
                                AnomalyLevel::Info => egui::Color32::LIGHT_BLUE,
                                AnomalyLevel::Warn => egui::Color32::YELLOW,
                                AnomalyLevel::Critical => egui::Color32::LIGHT_RED,
                            };
                            ui.colored_label(level_color, format!("{:?}", anomaly.level));
                            ui.label(anomaly.unit.as_deref().unwrap_or("–"));
                            ui.label(format!("{:.2}", anomaly.breakdown.combined));
                            ui.label(&anomaly.template_text);
                            if ui.button("Details").clicked() {
                                self.selected_anomaly = Some(anomaly.id);
                            }
                            ui.end_row();
                        }
                    });
            });

            // Auswahl hat sich geändert (neuer Klick oben): vorherige
            // Kontextantwort ist für die neue Anomalie irrelevant.
            if self.selected_anomaly != selected_before {
                self.pending_context_request = None;
            }
        });
    }
}
