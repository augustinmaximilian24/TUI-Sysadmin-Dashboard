//! Fleet-Tab (Phase 13, optional, außerhalb des ursprünglichen
//! v1.0-Scopes): read-only Übersicht mehrerer logsentry-Daemon-Instanzen
//! (lokal + konfigurierte entfernte Hosts) nebeneinander in einem eigenen
//! Tab. Zugriff auf entfernte Hosts läuft ausschließlich über einen
//! SSH-Tunnel ([`tunnel`]) auf deren Unix-Socket, niemals über ein zweites,
//! roh erreichbares TCP-Listening des Daemons -- das Protokoll hat bewusst
//! keine eigene Authentisierung/Verschlüsselung (nur Unix-Socket-Rechte,
//! Regel 11).
//!
//! **Read-only**: der ausgehende Kanal der Proto-Client-Bridge
//! ([`spawn_host_bridge`]) wird nie zum Senden benutzt (kein
//! `Action`/`GetContext`) -- das setzt "keine Aktionen auf entfernte Hosts"
//! auf Code-Ebene durch, nicht nur als UI-Beschränkung.
//!
//! Architektur (analog `knowledge_graph`): pro Host ein Tunnel-Supervisor
//! plus ein Proto-Client, beide schreiben in denselben `Arc<Mutex<Shared>>`,
//! `show()` liest davon nur einen kurzen Klon.

mod tunnel;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use eframe::egui;

use logsentry_core::config::{FleetConfig, RemoteHost};
use logsentry_proto::{
    spawn, AnomalyEvent, AnomalyLevel, ClientConfig, ConnectionState, Endpoint, ServerMessage,
    Subscription,
};

use tunnel::{supervise_tunnel, TunnelState};

/// Wie viele zuletzt gesehene Anomalien pro Host im Speicher gehalten werden
/// (Regel 18) -- eine Übersicht, keine vollständige Historie pro Host.
const RECENT_ANOMALIES_CAP: usize = 20;

/// Abstand zwischen zwei Snapshots über den Fleet-Tunnel: bewusst gröber als
/// die 1000ms des lokalen Dashboards (`gui/src/client.rs`), um Bandbreite
/// und CPU über mehrere gleichzeitige Tunnel zu sparen -- eine grobe
/// Übersicht braucht keine flüssige Live-Graph-Rate.
const FLEET_SNAPSHOT_INTERVAL_MS: u32 = 2000;

/// Verbindungsstatus eines einzelnen Hosts, wie er im Tab angezeigt wird.
/// Trennt Tunnel- und Daemon-Zustand -- ein Tunnel-Fehler hat immer Vorrang
/// vor einem veralteten Daemon-Status, sonst zeigte der Tab nach einem
/// abgebrochenen Tunnel fälschlich noch "verbunden" an.
#[derive(Debug, Clone, PartialEq)]
pub enum HostConnectionState {
    TunnelConnecting,
    TunnelFailed {
        message: String,
        retry_in: Duration,
    },
    DaemonConnecting {
        attempt: u32,
    },
    DaemonConnected,
    DaemonDisconnected {
        retry_in: Duration,
    },
    DaemonDenied,
}

/// Bildet Tunnel- und Daemon-Zustand auf einen einzelnen, für die UI
/// gedachten Zustand ab. Reine Funktion, unabhängig von `egui`/`tokio`,
/// daher isoliert testbar.
fn derive_state(tunnel: &TunnelState, connection: &ConnectionState) -> HostConnectionState {
    if let TunnelState::Failed { message, retry_in } = tunnel {
        return HostConnectionState::TunnelFailed {
            message: message.clone(),
            retry_in: *retry_in,
        };
    }
    match connection {
        ConnectionState::Connecting { attempt } => HostConnectionState::DaemonConnecting {
            attempt: *attempt,
        },
        ConnectionState::Connected { .. } => HostConnectionState::DaemonConnected,
        ConnectionState::Denied(_) => HostConnectionState::DaemonDenied,
        ConnectionState::Disconnected { retry_in, .. } => HostConnectionState::DaemonDisconnected {
            retry_in: *retry_in,
        },
    }
}

/// Laufende Zähler seit Verbindungsaufbau, nicht persistiert -- eine
/// Übersicht für "ist gerade etwas los", keine Statistik über die Zeit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnomalyCounts {
    pub info: u64,
    pub warn: u64,
    pub critical: u64,
}

impl AnomalyCounts {
    fn record(&mut self, level: AnomalyLevel) {
        match level {
            AnomalyLevel::Info => self.info += 1,
            AnomalyLevel::Warn => self.warn += 1,
            AnomalyLevel::Critical => self.critical += 1,
        }
    }
}

/// Sichtbarer Zustand eines einzelnen entfernten Hosts. Bewusst schlank (nur
/// Zusammenfassung, keine volle Historie/Graphen wie beim lokalen
/// Dashboard) -- Übersicht, kein zweites Dashboard pro Host.
#[derive(Debug, Clone)]
pub struct HostSummary {
    pub name: String,
    pub state: HostConnectionState,
    pub hostname: Option<String>,
    pub uptime_secs: Option<u64>,
    pub entropy_bits: Option<f64>,
    pub anomaly_counts: AnomalyCounts,
    pub recent_anomalies: VecDeque<AnomalyEvent>,
}

impl HostSummary {
    fn pending(host: &RemoteHost) -> Self {
        Self {
            name: host.name.clone(),
            state: HostConnectionState::TunnelConnecting,
            hostname: None,
            uptime_secs: None,
            entropy_bits: None,
            anomaly_counts: AnomalyCounts::default(),
            recent_anomalies: VecDeque::new(),
        }
    }

    fn push_anomaly(&mut self, anomaly: AnomalyEvent) {
        self.anomaly_counts.record(anomaly.level);
        if self.recent_anomalies.len() >= RECENT_ANOMALIES_CAP {
            self.recent_anomalies.pop_front();
        }
        self.recent_anomalies.push_back(anomaly);
    }
}

struct Shared {
    hosts: Vec<HostSummary>,
}

fn update_host(shared: &Arc<Mutex<Shared>>, index: usize, f: impl FnOnce(&mut HostSummary)) {
    let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(host) = guard.hosts.get_mut(index) {
        f(host);
    }
}

fn apply_message(shared: &Arc<Mutex<Shared>>, index: usize, msg: ServerMessage) {
    update_host(shared, index, move |host| match msg {
        ServerMessage::Hello { hostname, .. } => {
            host.hostname = Some(hostname);
        }
        ServerMessage::Snapshot(snapshot) => {
            host.uptime_secs = Some(snapshot.daemon_uptime_secs);
            host.entropy_bits = Some(snapshot.window.entropy_bits);
        }
        ServerMessage::Anomaly(anomaly) => host.push_anomaly(anomaly),
        ServerMessage::RecentAnomalies { anomalies } => {
            for anomaly in anomalies {
                host.push_anomaly(anomaly);
            }
        }
        // Context/ActionResult/Pong/Error/Goodbye: fuer die Uebersicht ohne
        // Belang (read-only v1 fordert ohnehin nie Context/Actions an).
        _ => {}
    });
}

/// Verbindet sich über den (bereits per [`supervise_tunnel`] offengehaltenen)
/// lokalen Tunnel-Port zum entfernten Daemon und spiegelt eingehende
/// Nachrichten in `shared.hosts[index]`.
fn spawn_host_bridge(
    index: usize,
    local_port: u16,
    tunnel_state: Arc<Mutex<TunnelState>>,
    shared: Arc<Mutex<Shared>>,
    ctx: egui::Context,
) {
    let (outbound, mut inbound, mut connection_state) = spawn(ClientConfig {
        endpoint: Endpoint::Tcp(SocketAddr::from(([127, 0, 0, 1], local_port))),
        client_name: "logsentry-fleet".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        subscription: Subscription {
            snapshot_interval_ms: FLEET_SNAPSHOT_INTERVAL_MS,
            ..Subscription::default()
        },
    });
    // `outbound` wird absichtlich nie zum Senden benutzt (kein
    // Action/GetContext) -- das ist die konkrete Durchsetzung von
    // "read-only v1" auf Code-Ebene, nicht nur eine UI-Beschränkung. Der
    // Sender muss trotzdem am Leben gehalten werden: liesse man ihn fallen,
    // wertet `connection_loop` (proto::client) das als "Anwendung will
    // nicht mehr senden" und beendet die Session dauerhaft.
    let _outbound_keepalive = outbound;

    tokio::spawn(async move {
        loop {
            tokio::select! {
                changed = connection_state.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let connection = connection_state.borrow().clone();
                    let tunnel = tunnel_state.lock().unwrap_or_else(PoisonError::into_inner).clone();
                    update_host(&shared, index, |host| {
                        host.state = derive_state(&tunnel, &connection);
                    });
                    ctx.request_repaint();
                }
                msg = inbound.recv() => {
                    let Some(msg) = msg else { return; };
                    apply_message(&shared, index, msg);
                    ctx.request_repaint();
                }
            }
        }
    });
}

/// Zustand des Fleet-Tabs.
pub struct FleetTab {
    shared: Arc<Mutex<Shared>>,
}

impl FleetTab {
    /// Startet pro konfiguriertem Host einen SSH-Tunnel-Supervisor und eine
    /// Proto-Client-Bridge im Hintergrund auf der bereits laufenden
    /// Tokio-Runtime (siehe `gui/src/main.rs`).
    pub fn new(config: &FleetConfig, ctx: &egui::Context) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            hosts: config.hosts.iter().map(HostSummary::pending).collect(),
        }));
        for (index, host) in config.hosts.iter().cloned().enumerate() {
            let local_port = tunnel::resolve_local_port(&host, index);
            let tunnel_state = Arc::new(Mutex::new(TunnelState::Starting));
            tokio::spawn(supervise_tunnel(
                host,
                local_port,
                Arc::clone(&tunnel_state),
                ctx.clone(),
            ));
            spawn_host_bridge(
                index,
                local_port,
                tunnel_state,
                Arc::clone(&shared),
                ctx.clone(),
            );
        }
        Self { shared }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        let hosts = {
            let guard = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
            guard.hosts.clone()
        };

        if hosts.is_empty() {
            ui.label(
                egui::RichText::new("Keine Fleet-Hosts konfiguriert.")
                    .color(crate::theme::TEXT_MUTED),
            );
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            for host in &hosts {
                crate::theme::card(ui, |ui| draw_host_card(ui, host));
                ui.add_space(8.0);
            }
        });
    }
}

fn draw_host_card(ui: &mut egui::Ui, host: &HostSummary) {
    ui.horizontal(|ui| {
        crate::theme::section_heading(ui, &host.name);
        ui.add_space(8.0);
        draw_state_chip(ui, &host.state);
    });
    if let Some(hostname) = &host.hostname {
        ui.label(
            egui::RichText::new(format!("Hostname: {hostname}")).color(crate::theme::TEXT_MUTED),
        );
    }
    ui.horizontal(|ui| {
        if let Some(uptime) = host.uptime_secs {
            ui.label(format!("Laufzeit: {}", format_uptime(uptime)));
        }
        if let Some(entropy) = host.entropy_bits {
            ui.label(format!("Entropie: {entropy:.2} bit"));
        }
    });
    ui.horizontal(|ui| {
        ui.colored_label(
            crate::theme::LEVEL_INFO,
            format!("Info: {}", host.anomaly_counts.info),
        );
        ui.colored_label(
            crate::theme::LEVEL_WARN,
            format!("Warn: {}", host.anomaly_counts.warn),
        );
        ui.colored_label(
            crate::theme::LEVEL_CRITICAL,
            format!("Kritisch: {}", host.anomaly_counts.critical),
        );
    });
    if !host.recent_anomalies.is_empty() {
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .id_salt(format!("fleet_recent_{}", host.name))
            .max_height(120.0)
            .show(ui, |ui| {
                for anomaly in host.recent_anomalies.iter().rev() {
                    ui.label(format!(
                        "{}: {}",
                        level_label(anomaly.level),
                        anomaly.template_text
                    ));
                }
            });
    }
}

fn draw_state_chip(ui: &mut egui::Ui, state: &HostConnectionState) {
    let (text, color) = match state {
        HostConnectionState::TunnelConnecting => (
            "Tunnel wird aufgebaut …".to_string(),
            crate::theme::TEXT_MUTED,
        ),
        HostConnectionState::TunnelFailed { message, retry_in } => (
            format!(
                "Tunnel fehlgeschlagen: {message} (neuer Versuch in {}s)",
                retry_in.as_secs()
            ),
            crate::theme::LEVEL_CRITICAL,
        ),
        HostConnectionState::DaemonConnecting { attempt } => (
            format!("Verbinde zum Daemon … (Versuch {attempt})"),
            crate::theme::TEXT_MUTED,
        ),
        HostConnectionState::DaemonConnected => ("Verbunden".to_string(), crate::theme::LEVEL_INFO),
        HostConnectionState::DaemonDisconnected { retry_in } => (
            format!("Getrennt (neuer Versuch in {}s)", retry_in.as_secs()),
            crate::theme::LEVEL_WARN,
        ),
        HostConnectionState::DaemonDenied => {
            ("Zugriff verweigert".to_string(), crate::theme::LEVEL_CRITICAL)
        }
    };
    ui.colored_label(color, text);
}

fn level_label(level: AnomalyLevel) -> &'static str {
    match level {
        AnomalyLevel::Info => "Info",
        AnomalyLevel::Warn => "Warn",
        AnomalyLevel::Critical => "Kritisch",
    }
}

fn format_uptime(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    format!("{hours}h {minutes}m")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_fehler_hat_vorrang_vor_daemon_status() {
        let tunnel = TunnelState::Failed {
            message: "Permission denied".to_string(),
            retry_in: Duration::from_secs(5),
        };
        let connection = ConnectionState::Connected { session_id: 1 };
        assert!(matches!(
            derive_state(&tunnel, &connection),
            HostConnectionState::TunnelFailed { .. }
        ));
    }

    #[test]
    fn verbundener_tunnel_zeigt_daemon_status() {
        let tunnel = TunnelState::Up;
        assert_eq!(
            derive_state(&tunnel, &ConnectionState::Connected { session_id: 42 }),
            HostConnectionState::DaemonConnected
        );
        assert_eq!(
            derive_state(
                &tunnel,
                &ConnectionState::Connecting { attempt: 3 }
            ),
            HostConnectionState::DaemonConnecting { attempt: 3 }
        );
    }

    #[test]
    fn tunnel_im_aufbau_ueberschreibt_daemon_status_nicht() {
        // Starting ist kein Failed -- der Daemon-Status (hier: noch nie
        // verbunden) bleibt massgeblich.
        let tunnel = TunnelState::Starting;
        assert_eq!(
            derive_state(&tunnel, &ConnectionState::Connecting { attempt: 0 }),
            HostConnectionState::DaemonConnecting { attempt: 0 }
        );
    }
}
