//! Verbindung zum Daemon: bündelt `logsentry_proto::client` mit einem
//! Hintergrund-Task, der eingehende Nachrichten in einen geteilten,
//! begrenzten Zustand (Regel 18) einsortiert und die GUI zum Neuzeichnen
//! aufweckt (Regel 20: kein Continuous-Repaint, nur bei Events).
//!
//! Die GUI selbst liest `GuiState` nur unter kurzer Sperre (Regel 21: kein
//! Blockieren auf I/O) -- die eigentliche Socket-Kommunikation läuft
//! vollständig im Hintergrund-Task auf dem separat gestarteten
//! Tokio-Runtime (siehe `main.rs`).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::mpsc;

use logsentry_proto::{
    spawn, ActionKind, ActionOutcome, AnomalyEvent, ClientConfig, ConnectionState, ContextReply,
    ErrorCode, GoodbyeReason, ServerMessage, Snapshot, Subscription,
};

/// Obergrenze der im Speicher gehaltenen Anomalien (Regel 18). Die
/// `RecentAnomalies`-Antwort des Daemons ist ohnehin schon begrenzt
/// (`recent_anomalies` in der Daemon-Konfiguration); dieser Wert schützt
/// zusätzlich gegen sehr lange laufende GUI-Sitzungen mit vielen
/// nachfolgend gestreamten `Anomaly`-Ereignissen.
const ANOMALY_CAP: usize = 1000;

/// Obergrenze der Verlaufspunkte für den Live-Graph (bei 1 Hz Snapshot-Rate
/// entspricht das gut 30 Minuten).
const HISTORY_CAP: usize = 1800;

/// Obergrenze für angezeigte Protokoll-/Verbindungsfehler.
const LOG_CAP: usize = 100;

/// Obergrenze für die im Speicher gehaltene Aktions-Historie dieser
/// Sitzung (Regel 18). Das ist keine vollständige Audit-Ansicht der
/// serverseitigen Datei -- das Protokoll bietet keinen Abruf historischer
/// Einträge, nur `ActionResult` für in dieser Sitzung selbst gestellte
/// Anfragen (Phase 8, Schritt 9).
const ACTION_LOG_CAP: usize = 200;

/// Ein Punkt im Entropie-Verlauf: (Sekunden seit Programmstart, Bit).
pub type HistoryPoint = [f64; 2];

/// Geteilter, von der GUI unter kurzer Sperre gelesener Zustand. Wird
/// ausschließlich vom Hintergrund-Task in `spawn_bridge` geschrieben.
#[derive(Default)]
pub struct GuiState {
    pub connection: Option<ConnectionState>,
    pub hostname: Option<String>,
    /// Vom Daemon im `Hello` gemeldete erlaubte Aktionsarten (Phase 8) --
    /// die GUI zeigt nur Buttons für Aktionen an, die hier auftauchen; die
    /// endgültige Prüfung bleibt aber immer beim Daemon.
    pub allowed_actions: Vec<ActionKind>,
    /// Globaler Dry-Run-Schalter des Daemons, für einen Hinweis im
    /// Bestätigungsdialog.
    pub dry_run: bool,
    pub snapshot: Option<Snapshot>,
    pub entropy_history: VecDeque<HistoryPoint>,
    pub anomalies: VecDeque<AnomalyEvent>,
    pub context_reply: Option<ContextReply>,
    pub log: VecDeque<String>,
    /// Ergebnisse selbst gestellter Aktionsanfragen dieser Sitzung
    /// (`request_id`, Ergebnis), neueste zuletzt.
    pub action_log: VecDeque<(u64, ActionOutcome)>,
}

impl GuiState {
    fn push_anomaly(&mut self, event: AnomalyEvent) {
        if self.anomalies.len() >= ANOMALY_CAP {
            self.anomalies.pop_front();
        }
        self.anomalies.push_back(event);
    }

    fn push_action_result(&mut self, request_id: u64, outcome: ActionOutcome) {
        if self.action_log.len() >= ACTION_LOG_CAP {
            self.action_log.pop_front();
        }
        self.action_log.push_back((request_id, outcome));
    }

    fn push_log(&mut self, line: String) {
        if self.log.len() >= LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    fn apply_snapshot(&mut self, snapshot: Snapshot, started_at: std::time::Instant) {
        let seconds = started_at.elapsed().as_secs_f64();
        if self.entropy_history.len() >= HISTORY_CAP {
            self.entropy_history.pop_front();
        }
        self.entropy_history
            .push_back([seconds, snapshot.window.entropy_bits]);
        self.snapshot = Some(snapshot);
    }
}

/// Baut die Verbindung auf und startet den Hintergrund-Task. Muss innerhalb
/// eines Tokio-Runtime-Kontexts aufgerufen werden (siehe `main.rs`).
/// Liefert den geteilten Zustand sowie den Sender für Anfragen
/// (`GetContext`, `Ping`) von der UI aus.
pub fn spawn_bridge(
    socket_path: PathBuf,
    ctx: eframe::egui::Context,
) -> (Arc<Mutex<GuiState>>, mpsc::Sender<logsentry_proto::ClientMessage>) {
    let state = Arc::new(Mutex::new(GuiState::default()));
    let (outbound, mut inbound, mut connection_state) = spawn(ClientConfig {
        socket_path,
        client_name: "logsentry-gui".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        subscription: Subscription::default(),
    });

    let bridge_state = Arc::clone(&state);
    tokio::spawn(async move {
        let started_at = std::time::Instant::now();
        loop {
            tokio::select! {
                changed = connection_state.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let current = connection_state.borrow().clone();
                    let mut guard = bridge_state.lock().unwrap_or_else(PoisonError::into_inner);
                    if let ConnectionState::Denied(err) = &current {
                        guard.push_log(format!("Zugriff verweigert: {err}"));
                    }
                    guard.connection = Some(current);
                    drop(guard);
                    ctx.request_repaint();
                }
                message = inbound.recv() => {
                    let Some(message) = message else {
                        return;
                    };
                    let mut guard = bridge_state.lock().unwrap_or_else(PoisonError::into_inner);
                    apply_message(&mut guard, message, started_at);
                    drop(guard);
                    ctx.request_repaint();
                }
            }
        }
    });

    (state, outbound)
}

/// Sortiert eine eingehende Nachricht in den geteilten Zustand ein.
fn apply_message(state: &mut GuiState, message: ServerMessage, started_at: std::time::Instant) {
    match message {
        ServerMessage::Hello {
            hostname,
            allowed_actions,
            dry_run,
            ..
        } => {
            state.hostname = Some(hostname);
            state.allowed_actions = allowed_actions;
            state.dry_run = dry_run;
        }
        ServerMessage::RecentAnomalies { anomalies } => {
            for anomaly in anomalies {
                state.push_anomaly(anomaly);
            }
        }
        ServerMessage::Snapshot(snapshot) => state.apply_snapshot(snapshot, started_at),
        ServerMessage::Anomaly(event) => state.push_anomaly(event),
        ServerMessage::Context(reply) => state.context_reply = Some(reply),
        ServerMessage::ActionResult { request_id, outcome } => {
            state.push_action_result(request_id, outcome);
        }
        ServerMessage::Pong { .. } => {}
        ServerMessage::Error { code, message, .. } => {
            state.push_log(format_error(&code, &message));
        }
        ServerMessage::Goodbye { reason } => {
            state.push_log(format_goodbye(&reason));
        }
    }
}

fn format_error(code: &ErrorCode, message: &str) -> String {
    match code {
        ErrorCode::Lagged { missed } => {
            format!("Anomalien verpasst ({missed} übersprungen): {message}")
        }
        _ => format!("Protokollfehler ({code:?}): {message}"),
    }
}

fn format_goodbye(reason: &GoodbyeReason) -> String {
    match reason {
        GoodbyeReason::Shutdown => "Daemon fährt herunter".to_string(),
        GoodbyeReason::HelloTimeout => "Handshake nicht rechtzeitig abgeschlossen".to_string(),
        GoodbyeReason::LineTooLong => "Verbindung wegen überlanger Nachricht getrennt".to_string(),
        GoodbyeReason::TooManyClients { max } => {
            format!("Zu viele gleichzeitige Clients (Limit {max})")
        }
        GoodbyeReason::VersionMismatch { expected, got } => {
            format!("Protokoll-Version {got} passt nicht zu erwarteter Version {expected}")
        }
    }
}

/// Menschlich lesbarer Verbindungsstatus für die Kopfzeile.
pub fn connection_label(state: Option<&ConnectionState>) -> String {
    match state {
        None => "verbinde …".to_string(),
        Some(ConnectionState::Connecting { attempt }) if *attempt == 0 => "verbinde …".to_string(),
        Some(ConnectionState::Connecting { attempt }) => format!("verbinde … (Versuch {attempt})"),
        Some(ConnectionState::Connected { session_id }) => {
            format!("verbunden (Sitzung {session_id})")
        }
        Some(ConnectionState::Denied(err)) => format!("verweigert: {err}"),
        Some(ConnectionState::Disconnected { retry_in }) => {
            format!("getrennt, neuer Versuch in {:.0}s", retry_in.as_secs_f64())
        }
    }
}

/// Ob die Verbindung aktuell steht (für die Aktivierung von UI-Elementen,
/// die eine Anfrage an den Daemon schicken).
pub fn is_connected(state: Option<&ConnectionState>) -> bool {
    matches!(state, Some(ConnectionState::Connected { .. }))
}

/// Menschlich lesbarer Name einer Aktionsart für Buttons und Audit-Ansicht.
pub fn action_kind_label(kind: ActionKind) -> &'static str {
    match kind {
        ActionKind::RestartUnit => "Unit neu starten",
        ActionKind::StopUnit => "Unit stoppen",
        ActionKind::TerminateProcess => "Prozess beenden",
        ActionKind::BlockIp => "IP sperren",
        ActionKind::MuteAnomaly => "Anomalie stummschalten",
    }
}

/// Menschlich lesbares Ergebnis einer Aktionsanfrage für die Audit-Ansicht.
pub fn format_action_outcome(outcome: &ActionOutcome) -> String {
    match outcome {
        ActionOutcome::Accepted => "angenommen".to_string(),
        ActionOutcome::Completed { message, dry_run } => {
            if *dry_run {
                format!("Dry-Run: {message}")
            } else {
                format!("ausgeführt: {message}")
            }
        }
        ActionOutcome::Denied { reason } => format!("abgelehnt: {reason:?}"),
        ActionOutcome::Failed { message } => format!("fehlgeschlagen: {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use logsentry_proto::DenyReason;

    #[test]
    fn format_action_outcome_markiert_dry_run_deutlich() {
        let dry = ActionOutcome::Completed {
            message: "sshd.service neu gestartet".to_string(),
            dry_run: true,
        };
        let real = ActionOutcome::Completed {
            message: "sshd.service neu gestartet".to_string(),
            dry_run: false,
        };
        assert!(format_action_outcome(&dry).starts_with("Dry-Run:"));
        assert!(format_action_outcome(&real).starts_with("ausgeführt:"));
    }

    #[test]
    fn format_action_outcome_zeigt_ablehnungsgrund() {
        let outcome = ActionOutcome::Denied {
            reason: DenyReason::RateLimited {
                retry_after_secs: 42,
            },
        };
        let text = format_action_outcome(&outcome);
        assert!(text.contains("abgelehnt"));
        assert!(text.contains("42"));
    }

    #[test]
    fn action_kind_label_deckt_alle_varianten_ab() {
        for kind in [
            ActionKind::RestartUnit,
            ActionKind::StopUnit,
            ActionKind::TerminateProcess,
            ActionKind::BlockIp,
            ActionKind::MuteAnomaly,
        ] {
            assert!(!action_kind_label(kind).is_empty());
        }
    }

    #[test]
    fn is_connected_erkennt_nur_den_connected_zustand() {
        assert!(!is_connected(None));
        assert!(!is_connected(Some(&ConnectionState::Connecting { attempt: 0 })));
        assert!(is_connected(Some(&ConnectionState::Connected { session_id: 1 })));
    }
}
