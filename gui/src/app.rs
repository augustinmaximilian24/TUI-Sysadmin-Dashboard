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

use logsentry_proto::{
    ActionKind, ActionRequest, AnomalyEvent, AnomalyLevel, ClientMessage, ConnectionState,
    MuteScope, Snapshot, UnitName,
};

use crate::client::{
    action_kind_label, connection_label, format_action_outcome, is_connected, GuiState,
    HistoryPoint,
};
use crate::theme;

/// Eine per Button ausgelöste, aber noch nicht bestätigte Aktion (Regel 12:
/// kein Ein-Klick-Vollzug). Der Klick baut schon die fertige
/// `ActionRequest` -- der Dialog zeigt nur noch die Vorschau und wartet auf
/// Bestätigung oder Abbruch.
struct PendingConfirmation {
    description: String,
    action: ActionRequest,
}

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

/// Welches Format `export_anomalies` erzeugen soll. Die eigentliche
/// Serialisierung passiert erst im Hintergrund-Thread (siehe dort), damit
/// weder sie noch der anschließende Schreibvorgang den Render-Thread
/// blockieren (Regel 21).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportFormat {
    Json,
    Csv,
}

/// Momentaufnahme des geteilten Zustands für einen Frame, damit
/// `Pause` das Mutex nicht bei jedem Redraw erneut sperren muss und die
/// UI-Logik auf Klondaten statt einer gehaltenen Sperre arbeitet.
#[derive(Default)]
struct RenderSnapshot {
    connection: Option<ConnectionState>,
    hostname: Option<String>,
    allowed_actions: Vec<ActionKind>,
    dry_run: bool,
    snapshot: Option<Snapshot>,
    history: Vec<HistoryPoint>,
    anomalies: Vec<AnomalyEvent>,
    context: Option<logsentry_proto::ContextReply>,
    log: Vec<String>,
    action_log: Vec<(u64, logsentry_proto::ActionOutcome)>,
    dropped_local: u64,
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
    /// Per Button vorbereitete, aber noch nicht bestätigte Aktion
    /// (Regel 12: Bestätigungsdialog mit Vorschau, kein Ein-Klick-Vollzug).
    pending_confirmation: Option<PendingConfirmation>,
    /// Rückmeldung des letzten Export-Versuchs (Phase 10, optional) --
    /// rein lokaler UI-Zustand, nicht Teil von `GuiState`. Geteilt statt
    /// direkt gehalten, weil der eigentliche Schreibvorgang in einem
    /// Hintergrund-Thread läuft (Regel 21) und sein Ergebnis von dort aus
    /// zurückmelden muss.
    export_message: Arc<Mutex<Option<String>>>,
    /// Eingabefeld für `ActionRequest::BlockIp` (Phase 8) -- diese Aktion
    /// hängt an keinem Feld der Anomalie selbst (anders als Unit/PID), die
    /// IP muss deshalb manuell eingegeben werden.
    block_ip_input: String,
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
            pending_confirmation: None,
            export_message: Arc::new(Mutex::new(None)),
            block_ip_input: String::new(),
            render: RenderSnapshot::default(),
        }
    }

    /// Übernimmt einen Klon des geteilten Zustands für diesen Frame.
    ///
    /// Verbindungsstatus, erlaubte Aktionsarten und der Dry-Run-Schalter
    /// werden auch bei `paused` aktualisiert: sie steuern, ob/wie eine
    /// Aktion überhaupt ausgelöst werden darf (Regel 12), und dürfen daher
    /// nie veraltet sein. Ohne das bliebe der Kopfbereich z. B. während
    /// eines Daemon-Neustarts fälschlich auf "verbunden" stehen, eine im
    /// pausierten Zustand bestätigte Aktion würde serverseitig warten und
    /// erst bei der nächsten echten Verbindung verzögert ausgeführt.
    /// Anomalie-Liste, Verlauf und Meldungen bleiben dagegen bei `paused`
    /// bewusst stehen, damit sich die angezeigte Liste beim Betrachten
    /// nicht unter der Maus verändert, während der Hintergrund-Task weiter
    /// empfängt.
    fn refresh(&mut self) {
        let guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.render.connection = guard.connection.clone();
        self.render.hostname = guard.hostname.clone();
        self.render.allowed_actions = guard.allowed_actions.clone();
        self.render.dry_run = guard.dry_run;
        self.render.dropped_local = guard.dropped_local;

        if self.paused {
            return;
        }
        self.render.snapshot = guard.snapshot.clone();
        self.render.history = guard.entropy_history.iter().copied().collect();
        self.render.anomalies = guard.anomalies.iter().cloned().collect();
        self.render.context = guard.context_reply.clone();
        self.render.log = guard.log.iter().cloned().collect();
        self.render.action_log = guard.action_log.iter().cloned().collect();
    }

    /// Sendet die bestätigte Aktion und räumt den Dialog weg. `try_send`
    /// statt `.await`: die Warteschlange ist bounded (Regel 17), läuft sie
    /// voll, verliert der Client nur diese eine Anfrage statt die UI zu
    /// blockieren (Regel 21).
    fn confirm_pending_action(&mut self) {
        let Some(pending) = self.pending_confirmation.take() else {
            return;
        };
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        let _ = self.outbound.try_send(ClientMessage::Action {
            request_id,
            action: pending.action,
        });
    }

    /// Serialisiert die aktuell geladenen Anomalien in `format` und
    /// schreibt sie in eine neue Datei im Home-Verzeichnis (Fallback:
    /// aktuelles Arbeitsverzeichnis, falls `$HOME` fehlt); merkt sich das
    /// Ergebnis für die Anzeige. Kein Datei-Dialog -- dafür bräuchte es
    /// eine zusätzliche Abhängigkeit (z. B. `rfd`), die für dieses
    /// optionale Phase-10-Feature nicht gerechtfertigt ist.
    ///
    /// Sowohl die Serialisierung als auch der Schreibvorgang laufen in
    /// einem eigenen `std::thread`, nicht hier: `update()` läuft auf dem
    /// Render-Thread, `serde_json::to_string_pretty` über tausende
    /// Anomalien ist bei einem JSON-Export der teurere Teil, `fs::write`
    /// bei langsamem/vollem Datenträger der andere (Regel 21: die GUI
    /// blockiert nie auf I/O; dasselbe gilt sinngemäß für spürbare
    /// CPU-Arbeit auf dem Render-Thread). `ctx` wird geklont, um den
    /// Hintergrund-Thread nach Abschluss gezielt einen Repaint auslösen zu
    /// lassen, statt auf den nächsten ohnehin fälligen Redraw zu warten.
    fn export_anomalies(&mut self, ctx: &egui::Context, format: ExportFormat) {
        let anomalies = self.render.anomalies.clone();
        let dir = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let extension = match format {
            ExportFormat::Json => "json",
            ExportFormat::Csv => "csv",
        };
        let path = dir.join(format!("logsentry-export-{timestamp}.{extension}"));

        let export_message = Arc::clone(&self.export_message);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let content = match format {
                ExportFormat::Json => match crate::export::anomalies_to_json(&anomalies) {
                    Ok(json) => json,
                    Err(err) => {
                        *export_message
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = Some(format!(
                            "Export fehlgeschlagen: Serialisierung nicht möglich ({err})"
                        ));
                        ctx.request_repaint();
                        return;
                    }
                },
                ExportFormat::Csv => crate::export::anomalies_to_csv(&anomalies),
            };
            let message = match std::fs::write(&path, content) {
                Ok(()) => format!("Exportiert nach {}", path.display()),
                Err(err) => format!("Export nach {} fehlgeschlagen: {err}", path.display()),
            };
            *export_message
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(message);
            ctx.request_repaint();
        });
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
        if !matches!(
            self.render.connection,
            Some(ConnectionState::Connected { .. })
        ) {
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        theme::apply(ctx, self.dark_mode);

        self.draw_header(ctx);
        self.draw_system_panel(ctx);
        self.draw_detail_panel(ctx);
        self.draw_central(ctx);
        self.draw_confirmation_dialog(ctx);
    }
}

impl LogsentryApp {
    fn draw_header(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("header")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(12.0, 10.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let host = self.render.hostname.as_deref().unwrap_or("–");
                    ui.heading(egui::RichText::new("logsentry").strong());
                    ui.label(egui::RichText::new(host).color(theme::TEXT_MUTED));
                    ui.add_space(6.0);

                    theme::chip(
                        ui,
                        connection_label(self.render.connection.as_ref()),
                        theme::connection_color(self.render.connection.as_ref()),
                    );

                    if let Some(snapshot) = &self.render.snapshot {
                        theme::neutral_chip(ui, format!("Uptime {}s", snapshot.daemon_uptime_secs));
                        theme::neutral_chip(
                            ui,
                            format!("Entropie {:.2} bit", snapshot.window.entropy_bits),
                        );
                        if snapshot.stats.dropped_overflow > 0 {
                            theme::chip(
                                ui,
                                format!("Verworfen {}", snapshot.stats.dropped_overflow),
                                theme::LEVEL_WARN,
                            );
                        }
                        if self.render.dropped_local > 0 {
                            // Regel 17: clientseitig (nicht serverseitig)
                            // verworfene Nachrichten müssen sichtbar sein --
                            // die GUI kam mit dem Abholen nicht hinterher.
                            theme::chip(
                                ui,
                                format!("Lokal verworfen {}", self.render.dropped_local),
                                theme::LEVEL_WARN,
                            );
                        }
                        if snapshot.learning.active {
                            let remaining = snapshot
                                .learning
                                .remaining_secs
                                .map(|s| format!("{s}s"))
                                .unwrap_or_else(|| "?".to_string());
                            theme::chip(ui, format!("Lernphase ({remaining})"), theme::LEVEL_WARN);
                        }
                        if snapshot.replay {
                            theme::chip(ui, "Replay", theme::ACCENT);
                        }
                    } else {
                        theme::neutral_chip(ui, "noch keine Daten vom Daemon");
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(if self.dark_mode {
                                "☀ Hell"
                            } else {
                                "🌙 Dunkel"
                            })
                            .clicked()
                        {
                            self.dark_mode = !self.dark_mode;
                        }
                        let pause_label = if self.paused {
                            "▶ Fortsetzen"
                        } else {
                            "⏸ Pause"
                        };
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
            .default_width(300.0)
            .show(ctx, |ui| {
                ui.add_space(2.0);
                ui.heading("Systemzustand");
                ui.add_space(4.0);

                match self
                    .render
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.system.as_ref())
                {
                    None => {
                        ui.label(egui::RichText::new("noch keine Messung").color(theme::TEXT_MUTED));
                    }
                    Some(system) => {
                        let core_count = system.cpu.per_core_usage_percent.len().max(1) as f64;

                        theme::card(ui, |ui| {
                            theme::section_heading(ui, "Auslastung");
                            ui.add_space(4.0);
                            theme::usage_bar(
                                ui,
                                "CPU",
                                system.cpu.global_usage_percent / 100.0,
                                format!("{:.0} %", system.cpu.global_usage_percent),
                            );
                            theme::usage_bar(
                                ui,
                                "RAM",
                                system.memory.used_percent / 100.0,
                                format!("{:.0} %", system.memory.used_percent),
                            );
                            theme::usage_bar(
                                ui,
                                "Load 1m",
                                (system.load.one / core_count) as f32,
                                format!(
                                    "{:.2} / {:.2} / {:.2}",
                                    system.load.one, system.load.five, system.load.fifteen
                                ),
                            );
                        });

                        if !system.temperatures.is_empty() {
                            ui.add_space(6.0);
                            theme::card(ui, |ui| {
                                theme::section_heading(ui, "Temperaturen");
                                ui.add_space(4.0);
                                for temp in &system.temperatures {
                                    theme::usage_bar(
                                        ui,
                                        &temp.label,
                                        temp.celsius / 100.0,
                                        format!("{:.1} °C", temp.celsius),
                                    );
                                }
                            });
                        }

                        if !system.disks.is_empty() {
                            ui.add_space(6.0);
                            theme::card(ui, |ui| {
                                theme::section_heading(ui, "Speicher");
                                ui.add_space(4.0);
                                for disk in &system.disks {
                                    theme::usage_bar(
                                        ui,
                                        &disk.mount_point,
                                        disk.used_percent / 100.0,
                                        format!("{:.0} %", disk.used_percent),
                                    );
                                }
                            });
                        }

                        if !system.units.is_empty() {
                            ui.add_space(6.0);
                            theme::card(ui, |ui| {
                                theme::section_heading(ui, "Units");
                                ui.add_space(4.0);
                                for unit in &system.units {
                                    let active = unit.active_state == "active";
                                    let dot_color = if active { theme::OK } else { theme::TEXT_MUTED };
                                    ui.horizontal(|ui| {
                                        ui.colored_label(dot_color, "●");
                                        ui.label(&unit.name);
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{}/{}",
                                                unit.active_state, unit.sub_state
                                            ))
                                            .color(theme::TEXT_MUTED),
                                        );
                                    });
                                }
                            });
                        }
                    }
                }

                if !self.render.log.is_empty() {
                    ui.add_space(6.0);
                    theme::card(ui, |ui| {
                        theme::section_heading(ui, "Meldungen");
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .id_salt("log_scroll")
                            .max_height(150.0)
                            .show(ui, |ui| {
                                for line in self.render.log.iter().rev() {
                                    ui.label(line);
                                }
                            });
                    });
                }

                // Audit-Ansicht (Phase 8, Schritt 9): das Protokoll bietet
                // keinen Abruf historischer Einträge aus der serverseitigen
                // Audit-Datei -- diese Liste zeigt nur Ergebnisse von
                // Aktionen, die diese GUI-Sitzung selbst ausgelöst hat.
                if !self.render.action_log.is_empty() {
                    ui.add_space(6.0);
                    theme::card(ui, |ui| {
                        theme::section_heading(ui, "Aktionen dieser Sitzung");
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .id_salt("action_log_scroll")
                            .max_height(150.0)
                            .show(ui, |ui| {
                                for (request_id, outcome) in self.render.action_log.iter().rev() {
                                    ui.label(format!(
                                        "#{request_id}: {}",
                                        format_action_outcome(outcome)
                                    ));
                                }
                            });
                    });
                }
            });
    }

    fn draw_detail_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("detail_panel")
            .resizable(true)
            .default_width(380.0)
            .show(ctx, |ui| {
                ui.add_space(2.0);
                ui.heading("Detail");
                ui.add_space(4.0);
                let Some(selected_id) = self.selected_anomaly else {
                    ui.label(
                        egui::RichText::new("Anomalie in der Liste auswählen")
                            .color(theme::TEXT_MUTED),
                    );
                    return;
                };
                let Some(anomaly) = self
                    .render
                    .anomalies
                    .iter()
                    .find(|a| a.id == selected_id)
                    .cloned()
                else {
                    ui.label(
                        egui::RichText::new("Anomalie nicht mehr im Speicher")
                            .color(theme::TEXT_MUTED),
                    );
                    return;
                };

                ui.horizontal(|ui| {
                    theme::level_badge(ui, anomaly.level);
                    ui.label(egui::RichText::new(format!("#{}", anomaly.id)).color(theme::TEXT_MUTED));
                });
                ui.add_space(6.0);

                theme::card(ui, |ui| {
                    theme::section_heading(ui, "Übersicht");
                    ui.add_space(4.0);
                    ui.label(format!("Unit: {}", anomaly.unit.as_deref().unwrap_or("–")));
                    ui.label(format!(
                        "PID: {}",
                        anomaly.pid.map_or("–".to_string(), |p| p.to_string())
                    ));
                    ui.label(format!("Template: {}", anomaly.template_text));
                    ui.label(format!("Beispielzeile: {}", anomaly.sample_message));
                    if anomaly.suppressed_since_last > 0 {
                        ui.colored_label(
                            theme::LEVEL_WARN,
                            format!(
                                "Seit letzter Meldung unterdrückt: {}",
                                anomaly.suppressed_since_last
                            ),
                        );
                    }
                });

                ui.add_space(6.0);
                theme::card(ui, |ui| {
                    theme::section_heading(ui, "Score");
                    ui.add_space(4.0);
                    let b = &anomaly.breakdown;
                    theme::usage_bar(
                        ui,
                        "Gesamt",
                        b.combined as f32,
                        format!("{:.3}", b.combined),
                    );
                    ui.label(format!("Rate-Z: {:.2} ({:?})", b.rate_z, b.rate_source));
                    ui.label(format!("Surprisal: {:.2} bit", b.surprisal_bits));
                    ui.label(format!("Entropie-Z: {:.2}", b.entropy_z));
                });

                ui.add_space(6.0);
                let connected = is_connected(self.render.connection.as_ref());
                if ui
                    .add_enabled(connected, egui::Button::new("Kontext laden"))
                    .clicked()
                {
                    self.request_context(&anomaly);
                }

                ui.add_space(6.0);
                theme::card(ui, |ui| {
                    theme::section_heading(ui, "Aktionen");
                    ui.add_space(4.0);
                    self.draw_action_buttons(ui, &anomaly, connected);
                });

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

                ui.add_space(6.0);
                theme::card(ui, |ui| {
                    theme::section_heading(ui, "Kontext");
                    ui.add_space(4.0);
                    match matching_context {
                        Some(reply) => {
                            if reply.truncated {
                                ui.colored_label(theme::LEVEL_WARN, "Ausschnitt gekürzt");
                            }
                            if ui.button("In Zwischenablage kopieren").clicked() {
                                let text = reply
                                    .lines
                                    .iter()
                                    .map(|line| line.message.as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                ui.ctx().copy_text(text);
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
                            ui.label(
                                egui::RichText::new("noch kein Kontext geladen")
                                    .color(theme::TEXT_MUTED),
                            );
                        }
                    }
                });
            });
    }

    /// Zeigt Buttons für die Aktionen, die für diese Anomalie inhaltlich
    /// Sinn ergeben (Unit vorhanden -> Neustart/Stopp, PID vorhanden ->
    /// Prozess beenden, immer -> Stummschalten) und laut `Hello` erlaubt
    /// sind. Ein Klick öffnet nur den Bestätigungsdialog (Regel 12) --
    /// gesendet wird erst nach Bestätigung dort.
    fn draw_action_buttons(&mut self, ui: &mut egui::Ui, anomaly: &AnomalyEvent, connected: bool) {
        let allowed = &self.render.allowed_actions;

        if let Some(unit) = anomaly.unit.as_deref() {
            if allowed.contains(&ActionKind::RestartUnit) {
                if let Ok(unit_name) = UnitName::parse(unit) {
                    if ui
                        .add_enabled(
                            connected,
                            egui::Button::new(action_kind_label(ActionKind::RestartUnit)),
                        )
                        .clicked()
                    {
                        self.pending_confirmation = Some(PendingConfirmation {
                            description: format!("Unit „{unit}“ jetzt neu starten?"),
                            action: ActionRequest::RestartUnit { unit: unit_name },
                        });
                    }
                }
            }
            if allowed.contains(&ActionKind::StopUnit) {
                if let Ok(unit_name) = UnitName::parse(unit) {
                    if ui
                        .add_enabled(
                            connected,
                            egui::Button::new(action_kind_label(ActionKind::StopUnit)),
                        )
                        .clicked()
                    {
                        self.pending_confirmation = Some(PendingConfirmation {
                            description: format!("Unit „{unit}“ jetzt stoppen?"),
                            action: ActionRequest::StopUnit { unit: unit_name },
                        });
                    }
                }
            }
        }

        if let Some(pid) = anomaly.pid {
            if allowed.contains(&ActionKind::TerminateProcess)
                && ui
                    .add_enabled(
                        connected,
                        egui::Button::new(action_kind_label(ActionKind::TerminateProcess)),
                    )
                    .clicked()
            {
                self.pending_confirmation = Some(PendingConfirmation {
                    description: format!("Prozess PID {pid} jetzt beenden (SIGTERM, danach SIGKILL nach Ablauf der Gnadenfrist)?"),
                    action: ActionRequest::TerminateProcess {
                        pid,
                        grace_secs: 5,
                    },
                });
            }
        }

        if allowed.contains(&ActionKind::BlockIp) {
            ui.label(format!("{}:", action_kind_label(ActionKind::BlockIp)));
            let parsed_ip = self.block_ip_input.trim().parse::<std::net::IpAddr>();
            ui.horizontal(|ui| {
                ui.label("IP:");
                ui.text_edit_singleline(&mut self.block_ip_input);
                for (label, duration_secs) in [
                    ("1 Stunde", 3_600u32),
                    ("1 Tag", 86_400u32),
                    ("7 Tage", 604_800u32),
                ] {
                    if ui
                        .add_enabled(connected && parsed_ip.is_ok(), egui::Button::new(label))
                        .clicked()
                    {
                        if let Ok(ip) = parsed_ip {
                            self.pending_confirmation = Some(PendingConfirmation {
                                description: format!("IP „{ip}“ für {label} sperren?"),
                                action: ActionRequest::BlockIp { ip, duration_secs },
                            });
                        }
                    }
                }
            });
            if !self.block_ip_input.trim().is_empty() && parsed_ip.is_err() {
                ui.colored_label(theme::LEVEL_CRITICAL, "keine gültige IP-Adresse");
            }
        }

        if allowed.contains(&ActionKind::MuteAnomaly) {
            ui.label(format!("{}:", action_kind_label(ActionKind::MuteAnomaly)));
            ui.horizontal(|ui| {
                for (label, scope) in [
                    ("1 Stunde", MuteScope::OneHour),
                    ("1 Tag", MuteScope::OneDay),
                    ("dauerhaft", MuteScope::Permanent),
                ] {
                    if ui
                        .add_enabled(connected, egui::Button::new(label))
                        .clicked()
                    {
                        self.pending_confirmation = Some(PendingConfirmation {
                            description: format!(
                                "Template „{}“{} für {label} stummschalten?",
                                anomaly.template_text,
                                anomaly
                                    .unit
                                    .as_deref()
                                    .map(|u| format!(" für Unit „{u}“"))
                                    .unwrap_or_default()
                            ),
                            action: ActionRequest::MuteAnomaly {
                                template_id: anomaly.template_id,
                                unit: anomaly.unit.clone(),
                                scope,
                            },
                        });
                    }
                }
            });
        }

        if allowed.is_empty() {
            ui.label("(keine Aktionsart vom Daemon erlaubt)");
        }
    }

    /// Bestätigungsdialog mit Vorschau (Regel 12). Modal genug für den
    /// Zweck: `egui::Window` mit `collapsible(false)`, `resizable(false)`;
    /// ein echtes Overlay, das Klicks dahinter blockiert, ist für ein
    /// Single-Window-Tool wie dieses nicht nötig.
    fn draw_confirmation_dialog(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.pending_confirmation else {
            return;
        };
        let description = pending.description.clone();
        let dry_run = self.render.dry_run;
        // Ohne diese Prüfung ließe sich "Bestätigen" auch anklicken, während
        // die Verbindung inzwischen (z. B. während des Dialogs) getrennt
        // wurde: die Aktion bliebe dann in der ausgehenden Warteschlange
        // liegen und würde erst bei der nächsten echten Verbindung -- unter
        // Umständen Minuten später gegen einen ganz anderen Systemzustand --
        // ausgeführt.
        let connected = is_connected(self.render.connection.as_ref());
        let mut confirm = false;
        let mut cancel = false;

        egui::Window::new("Aktion bestätigen")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(&description);
                if dry_run {
                    ui.colored_label(
                        theme::ACCENT,
                        "Dry-Run aktiv: der Daemon protokolliert nur, führt aber nichts aus.",
                    );
                }
                if !connected {
                    ui.colored_label(
                        theme::LEVEL_CRITICAL,
                        "Nicht verbunden -- Bestätigen ist deaktiviert.",
                    );
                }
                ui.horizontal(|ui| {
                    if ui.button("Abbrechen").clicked() {
                        cancel = true;
                    }
                    if ui
                        .add_enabled(connected, egui::Button::new("Bestätigen"))
                        .clicked()
                    {
                        confirm = true;
                    }
                });
            });

        if confirm {
            self.confirm_pending_action();
        } else if cancel {
            self.pending_confirmation = None;
        }
    }

    fn draw_central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            theme::card(ui, |ui| {
                theme::section_heading(ui, "Verlauf (Entropie)");
                ui.add_space(4.0);
                let points: PlotPoints = self.render.history.iter().copied().collect();
                Plot::new("entropy_plot")
                    .height(160.0)
                    .allow_scroll(false)
                    .show(ui, |plot_ui| {
                        plot_ui.line(Line::new(points).color(theme::ACCENT));
                    });
            });

            ui.add_space(8.0);

            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("🔍");
                    ui.text_edit_singleline(&mut self.search);
                    ui.separator();
                    ui.label("Unit:");
                    ui.text_edit_singleline(&mut self.unit_filter);
                    ui.separator();
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
                    ui.separator();
                    ui.selectable_value(&mut self.sort_key, SortKey::Time, "nach Zeit");
                    ui.selectable_value(&mut self.sort_key, SortKey::Score, "nach Score");

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("CSV").clicked() {
                            self.export_anomalies(ctx, ExportFormat::Csv);
                        }
                        if ui.button("JSON").clicked() {
                            self.export_anomalies(ctx, ExportFormat::Json);
                        }
                        ui.label(egui::RichText::new("Export:").color(theme::TEXT_MUTED));
                    });
                });
                let export_message = self
                    .export_message
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                if let Some(message) = export_message {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(message).color(theme::TEXT_MUTED));
                }
            });

            ui.add_space(8.0);

            let selected_before = self.selected_anomaly;
            let filtered = self.filtered_anomalies();
            theme::section_heading(ui, &format!("Anomalien ({})", filtered.len()));
            ui.add_space(4.0);
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("anomaly_grid")
                    .striped(true)
                    .num_columns(5)
                    .spacing(egui::vec2(12.0, 6.0))
                    .show(ui, |ui| {
                        ui.strong("Level");
                        ui.strong("Unit");
                        ui.strong("Score");
                        ui.strong("Template");
                        ui.strong("");
                        ui.end_row();

                        for anomaly in filtered {
                            theme::level_badge(ui, anomaly.level);
                            ui.label(anomaly.unit.as_deref().unwrap_or("–"));
                            ui.colored_label(
                                theme::level_color(anomaly.level),
                                format!("{:.2}", anomaly.breakdown.combined),
                            );
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
