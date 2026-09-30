//! Anwendungszustand des TUI-Clients: reine Datenhaltung und
//! Nachrichtenverarbeitung, ohne Terminal-/Rendering-Code (der lebt in
//! `ui.rs`) -- damit beides unabhängig testbar ist.

use std::collections::VecDeque;

use logsentry_proto::{AnomalyEvent, ConnectionState, ServerMessage, Snapshot};

/// Obergrenze der im Speicher gehaltenen Anomalien (Regel 18), analog zur
/// GUI (`gui::client::ANOMALY_CAP`).
const ANOMALY_CAP: usize = 500;

#[derive(Default)]
pub struct App {
    pub hostname: Option<String>,
    pub connection: Option<ConnectionState>,
    pub snapshot: Option<Snapshot>,
    /// Neueste Anomalie zuerst (Index 0 = neueste), damit die Anzeige ohne
    /// Umkehrung an der Reihenfolge der `VecDeque` hängen kann.
    pub anomalies: VecDeque<AnomalyEvent>,
    pub selected: usize,
    pub should_quit: bool,
    /// Letzte Protokoll-/Verbindungsmeldung (Error/Goodbye) für die
    /// Statuszeile.
    pub status: Option<String>,
}

impl App {
    pub fn apply_message(&mut self, message: ServerMessage) {
        match message {
            ServerMessage::Hello { hostname, .. } => self.hostname = Some(hostname),
            ServerMessage::RecentAnomalies { anomalies } => {
                // Älteste zuerst in der Nachricht; `push_anomaly` fügt
                // jeweils vorne ein, wodurch die zuletzt eingefügte
                // (jüngste) Anomalie am Ende automatisch ganz vorne
                // landet -- kein `.rev()` nötig, das würde die
                // Reihenfolge umdrehen.
                for anomaly in anomalies {
                    self.push_anomaly(anomaly);
                }
            }
            ServerMessage::Snapshot(snapshot) => self.snapshot = Some(snapshot),
            ServerMessage::Anomaly(event) => self.push_anomaly(event),
            ServerMessage::Context(_)
            | ServerMessage::ActionResult { .. }
            | ServerMessage::Pong { .. }
            | ServerMessage::LanDevices { .. } => {
                // Der TUI-Client fordert weder Kontext noch Aktionen an
                // (Phase 10: reiner, lesender zweiter Konsument) und
                // abonniert `lan` gar nicht erst (Subscription::default()
                // in main.rs) -- LanDevices träfe hier ohnehin nie ein, der
                // Arm ist nur für Erschöpfungsvollständigkeit da.
            }
            ServerMessage::Error { message, .. } => {
                self.status = Some(format!("Fehler: {message}"));
            }
            ServerMessage::Goodbye { reason } => {
                self.status = Some(format!("Verbindung beendet: {reason:?}"));
            }
        }
    }

    fn push_anomaly(&mut self, event: AnomalyEvent) {
        self.anomalies.push_front(event);
        if self.anomalies.len() > ANOMALY_CAP {
            self.anomalies.pop_back();
        }
        // Neue Anomalie oben eingefügt: die aktuelle Auswahl soll auf
        // demselben *Eintrag* bleiben, nicht auf demselben Index springen.
        if self.selected > 0 {
            self.selected += 1;
        }
    }

    pub fn select_next(&mut self) {
        if self.selected + 1 < self.anomalies.len() {
            self.selected += 1;
        }
    }

    pub fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn selected_anomaly(&self) -> Option<&AnomalyEvent> {
        self.anomalies.get(self.selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use logsentry_proto::{AnomalyLevel, RateSource, ScoreBreakdown};

    fn anomaly(id: u64) -> AnomalyEvent {
        AnomalyEvent {
            id,
            timestamp_us: id,
            template_id: 1,
            template_text: "x".to_string(),
            sample_message: "x".to_string(),
            unit: None,
            pid: None,
            priority: None,
            level: AnomalyLevel::Info,
            breakdown: ScoreBreakdown {
                rate_z: 0.0,
                surprisal_bits: 0.0,
                entropy_z: 0.0,
                rate_component: 0.0,
                surprisal_component: 0.0,
                entropy_component: 0.0,
                combined: 0.0,
                rate_source: RateSource::None,
            },
            suppressed_since_last: 0,
        }
    }

    #[test]
    fn neue_anomalien_landen_vorne() {
        let mut app = App::default();
        app.apply_message(ServerMessage::Anomaly(anomaly(1)));
        app.apply_message(ServerMessage::Anomaly(anomaly(2)));
        assert_eq!(app.anomalies[0].id, 2);
        assert_eq!(app.anomalies[1].id, 1);
    }

    #[test]
    fn obergrenze_wird_respektiert() {
        let mut app = App::default();
        for i in 0..(ANOMALY_CAP as u64 + 10) {
            app.apply_message(ServerMessage::Anomaly(anomaly(i)));
        }
        assert_eq!(app.anomalies.len(), ANOMALY_CAP);
        // Die neuesten müssen erhalten bleiben, die ältesten fallen raus.
        assert_eq!(app.anomalies[0].id, ANOMALY_CAP as u64 + 9);
    }

    #[test]
    fn select_next_und_previous_bleiben_in_grenzen() {
        let mut app = App::default();
        app.apply_message(ServerMessage::Anomaly(anomaly(1)));
        app.apply_message(ServerMessage::Anomaly(anomaly(2)));
        assert_eq!(app.selected, 0);
        app.select_previous();
        assert_eq!(app.selected, 0, "darf nicht unter 0 fallen");
        app.select_next();
        assert_eq!(app.selected, 1);
        app.select_next();
        assert_eq!(app.selected, 1, "darf nicht über das Ende hinaus");
    }

    #[test]
    fn auswahl_bleibt_am_eintrag_wenn_neue_anomalie_oben_eingefuegt_wird() {
        let mut app = App::default();
        app.apply_message(ServerMessage::Anomaly(anomaly(1)));
        app.apply_message(ServerMessage::Anomaly(anomaly(2)));
        app.select_next(); // zeigt jetzt auf id=1 (Index 1)
        assert_eq!(app.selected_anomaly().unwrap().id, 1);

        app.apply_message(ServerMessage::Anomaly(anomaly(3)));
        // id=1 ist jetzt an Index 2 statt 1 -- die Auswahl muss mitwandern.
        assert_eq!(app.selected_anomaly().unwrap().id, 1);
    }

    #[test]
    fn recent_anomalies_erhaelt_reihenfolge_neueste_zuerst() {
        let mut app = App::default();
        app.apply_message(ServerMessage::RecentAnomalies {
            anomalies: vec![anomaly(1), anomaly(2), anomaly(3)],
        });
        assert_eq!(app.anomalies[0].id, 3);
        assert_eq!(app.anomalies[1].id, 2);
        assert_eq!(app.anomalies[2].id, 1);
    }

    #[test]
    fn goodbye_setzt_statuszeile() {
        let mut app = App::default();
        app.apply_message(ServerMessage::Goodbye {
            reason: logsentry_proto::GoodbyeReason::Shutdown,
        });
        assert!(app.status.unwrap().contains("Shutdown"));
    }
}
