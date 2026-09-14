//! Geteilter Zustand des Daemons: der einzige Ort, an dem die
//! Verarbeitungs-Pipeline (`pipeline.rs`) und die Client-Verbindungen
//! (Schritt 5) aufeinandertreffen.
//!
//! Normativ: `docs/phase6-protokoll.md` Abschnitt 3 (`watch` für
//! Snapshots -- ein langsamer Client bekommt immer nur den neuesten
//! Stand; `broadcast` für Anomalien -- Lag ist sichtbar, nichts wird
//! stillschweigend verworfen) und Abschnitt 7.
//!
//! `#![allow(dead_code)]`: siehe Begründung in `wire.rs` -- dieses Modul
//! wird erst ab Schritt 5 (`server.rs`/`client_task.rs`) und Schritt 6
//! (Pipeline-Anbindung) tatsächlich verwendet.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{broadcast, watch};

use logsentry_proto::{AnomalyEvent, Snapshot};

use crate::wire::ContextEntry;

/// Kapazität des Broadcast-Kanals für Anomalien. Größer als die üblichen
/// Bursts (z. B. ein SSH-Bruteforce-Replay), damit `Lagged` in normalem
/// Betrieb die Ausnahme bleibt und nicht der Regelfall bei zwei, drei
/// gleichzeitigen Clients.
const ANOMALY_BROADCAST_CAPACITY: usize = 256;

/// Geteilter Zustand des Daemons für einen Collector-Lauf (eine
/// Prozess-Lebensdauer). `session_id` ändert sich bei jedem Neustart, damit
/// Clients erkennen, dass zuvor gesehene Anomalie-`id`s neu vergeben sein
/// könnten (sie sind nur innerhalb einer Session eindeutig).
pub struct SharedState {
    snapshot_tx: watch::Sender<Arc<Snapshot>>,
    anomaly_tx: broadcast::Sender<Arc<AnomalyEvent>>,
    recent: Mutex<VecDeque<Arc<AnomalyEvent>>>,
    recent_capacity: usize,
    context: Mutex<ContextRing>,
    next_anomaly_id: AtomicU64,
    pub session_id: u64,
}

impl SharedState {
    /// Baut den Zustand. `initial_snapshot` ist das, was neu verbundene
    /// Clients sehen, bevor die Pipeline den ersten echten Snapshot
    /// veröffentlicht hat (Schritt 6 baut ihn in `main.rs`/`pipeline.rs`).
    pub fn new(
        initial_snapshot: Snapshot,
        recent_capacity: usize,
        context_capacity: usize,
    ) -> Self {
        let (snapshot_tx, _) = watch::channel(Arc::new(initial_snapshot));
        let (anomaly_tx, _) = broadcast::channel(ANOMALY_BROADCAST_CAPACITY);
        Self {
            snapshot_tx,
            anomaly_tx,
            recent: Mutex::new(VecDeque::with_capacity(recent_capacity.min(4096))),
            recent_capacity: recent_capacity.max(1),
            context: Mutex::new(ContextRing::new(context_capacity)),
            next_anomaly_id: AtomicU64::new(0),
            session_id: random_session_id(),
        }
    }

    /// Veröffentlicht einen neuen Snapshot. Ersetzt den vorherigen für
    /// alle Abonnenten -- kein Client sieht je eine Warteschlange
    /// veralteter Snapshots (Regel: latest-value statt Queue).
    pub fn publish_snapshot(&self, snapshot: Snapshot) {
        // send_replace statt send: es ist uns egal, ob gerade jemand
        // abonniert hat, der Wert soll trotzdem aktuell sein, wenn sich
        // gleich jemand verbindet (subscribe() liest den zuletzt
        // gesendeten Wert, auch ohne aktive Abonnenten in diesem Moment).
        self.snapshot_tx.send_replace(Arc::new(snapshot));
    }

    /// Liefert den zuletzt veröffentlichten Snapshot (für einen frisch
    /// verbundenen Client, bevor er selbst einen `watch`-Abonnenten hält).
    pub fn latest_snapshot(&self) -> Arc<Snapshot> {
        self.snapshot_tx.borrow().clone()
    }

    /// Neuer `watch`-Abonnent für Snapshots.
    pub fn subscribe_snapshot(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.snapshot_tx.subscribe()
    }

    /// Nächste, innerhalb dieser Session eindeutige Anomalie-ID, beginnend
    /// bei 1 (Regel: `AnomalyEvent::id` ist "monoton steigend pro Session,
    /// ab 1").
    pub fn next_anomaly_id(&self) -> u64 {
        self.next_anomaly_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Veröffentlicht eine Anomalie: in den `RecentAnomalies`-Ring
    /// aufnehmen (ältesten verdrängen, wenn voll) und an alle
    /// Broadcast-Abonnenten senden. Gibt es keine Abonnenten gerade, ist
    /// das kein Fehler -- der Ring hält sie trotzdem für neu verbindende
    /// Clients vor.
    pub fn publish_anomaly(&self, event: AnomalyEvent) {
        let event = Arc::new(event);
        {
            let mut recent = self.recent.lock().unwrap_or_else(PoisonError::into_inner);
            if recent.len() >= self.recent_capacity {
                recent.pop_front();
            }
            recent.push_back(event.clone());
        }
        let _ = self.anomaly_tx.send(event);
    }

    /// Die zuletzt gemeldeten Anomalien dieser Session, älteste zuerst --
    /// für `RecentAnomalies` direkt nach dem Handshake.
    pub fn recent_anomalies(&self) -> Vec<Arc<AnomalyEvent>> {
        self.recent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// Neuer Broadcast-Abonnent für Anomalien. Der Aufrufer muss
    /// `RecvError::Lagged` behandeln (siehe `docs/phase6-protokoll.md`
    /// Abschnitt 4, Regel 5) statt den Fehler zu ignorieren.
    pub fn subscribe_anomalies(&self) -> broadcast::Receiver<Arc<AnomalyEvent>> {
        self.anomaly_tx.subscribe()
    }

    /// Nimmt eine Rohzeile in den Kontext-Ring auf (für spätere
    /// `GetContext`-Anfragen).
    pub fn push_context(&self, entry: ContextEntry) {
        self.context
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry);
    }

    /// Fragt den Kontext-Ring ab. Siehe [`ContextRing::query`].
    pub fn query_context(
        &self,
        timestamp_us: u64,
        before: u16,
        after: u16,
        unit: Option<&str>,
    ) -> (Vec<ContextEntry>, bool) {
        self.context
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .query(timestamp_us, before, after, unit)
    }
}

/// Erzeugt eine Session-ID, die sich zwischen Daemon-Starts unterscheidet.
/// Kein Sicherheitsmerkmal (keine Kryptografie nötig) -- dient nur dazu,
/// dass ein Client nach einem Daemon-Neustart erkennt, dass zuvor
/// gesehene Anomalie-`id`s neu beginnen. Deshalb keine zusätzliche
/// RNG-Abhängigkeit: Systemzeit, Prozess-ID und eine Stack-Adresse (ASLR)
/// als Entropiequellen reichen für diesen Zweck aus.
fn random_session_id() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Ok(elapsed) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        elapsed.hash(&mut hasher);
    }
    std::process::id().hash(&mut hasher);
    let stack_marker = 0u8;
    (std::ptr::addr_of!(stack_marker) as usize).hash(&mut hasher);
    hasher.finish()
}

/// Bounded Ring roher Journal-Zeilen für `GetContext` (Detail-Panel:
/// Rohzeilen um eine Anomalie herum). Getrennt von [`SharedState`]s
/// übrigen Feldern dokumentiert, weil die Abfragelogik (`query`)
/// eigenständig testbar sein soll.
pub struct ContextRing {
    entries: VecDeque<ContextEntry>,
    capacity: usize,
}

impl ContextRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity.min(4096)),
            capacity: capacity.max(1),
        }
    }

    /// Nimmt `entry` auf; ist der Ring voll, weicht der älteste Eintrag
    /// (Regel 18: keine unbeschränkt wachsende Struktur).
    pub fn push(&mut self, entry: ContextEntry) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Liefert bis zu `before` Einträge vor und `after` Einträge nach dem
    /// Eintrag, dessen Zeitstempel `timestamp_us` am nächsten kommt (bei
    /// Gleichstand gewinnt der frühere Index), optional auf `unit`
    /// gefiltert. Der zweite Rückgabewert ist `true`, wenn weniger
    /// Einträge geliefert wurden als angefragt, weil der Ring (nach
    /// Filterung) nicht genug hergab -- das Klemmen von `before`/`after`
    /// auf `context_max_lines` aus der Konfiguration ist Sache des
    /// Aufrufers (Schritt 5), nicht dieser Methode.
    pub fn query(
        &self,
        timestamp_us: u64,
        before: u16,
        after: u16,
        unit: Option<&str>,
    ) -> (Vec<ContextEntry>, bool) {
        let filtered: Vec<&ContextEntry> = match unit {
            Some(u) => self
                .entries
                .iter()
                .filter(|e| e.unit.as_deref() == Some(u))
                .collect(),
            None => self.entries.iter().collect(),
        };
        if filtered.is_empty() {
            return (Vec::new(), before > 0 || after > 0);
        }

        let mut idx = 0usize;
        let mut best_diff = u64::MAX;
        for (i, e) in filtered.iter().enumerate() {
            let diff = e.timestamp_us.abs_diff(timestamp_us);
            if diff < best_diff {
                best_diff = diff;
                idx = i;
            }
        }

        let before = before as usize;
        let after = after as usize;
        let truncated = idx < before || (filtered.len() - idx - 1) < after;

        let start = idx.saturating_sub(before);
        let end = (idx + after + 1).min(filtered.len());
        let lines = filtered[start..end].iter().map(|e| (*e).clone()).collect();
        (lines, truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ts: u64, unit: Option<&str>, msg: &str) -> ContextEntry {
        ContextEntry {
            timestamp_us: ts,
            unit: unit.map(Arc::from),
            pid: Some(1),
            priority: Some(6),
            message: Arc::from(msg),
        }
    }

    fn minimal_snapshot() -> Snapshot {
        Snapshot {
            timestamp_us: 0,
            daemon_uptime_secs: 0,
            learning: logsentry_proto::LearningState {
                active: true,
                remaining_secs: None,
            },
            stats: logsentry_proto::PipelineStats {
                events: 0,
                parse_errors: 0,
                dropped_overflow: 0,
                templates: 0,
                anomalies_emitted: 0,
                suppressed: 0,
                suppressed_learning: 0,
                events_per_sec: 0.0,
                clients: 0,
            },
            window: logsentry_proto::WindowStats {
                window_secs: 60,
                entropy_bits: 0.0,
                entropy_z: 0.0,
                events_in_window: 0,
                distinct_templates: 0,
            },
            system: None,
            replay: false,
        }
    }

    fn sample_anomaly_event(id: u64) -> AnomalyEvent {
        AnomalyEvent {
            id,
            timestamp_us: id,
            template_id: 1,
            template_text: "x".into(),
            sample_message: "x".into(),
            unit: None,
            pid: None,
            priority: None,
            level: logsentry_proto::AnomalyLevel::Warn,
            breakdown: logsentry_proto::ScoreBreakdown {
                rate_z: 0.0,
                surprisal_bits: 0.0,
                entropy_z: 0.0,
                rate_component: 0.0,
                surprisal_component: 0.0,
                entropy_component: 0.0,
                combined: 0.0,
                rate_source: logsentry_proto::RateSource::None,
            },
            suppressed_since_last: 0,
        }
    }

    #[test]
    fn snapshot_watch_liefert_immer_nur_den_neuesten_wert() {
        let state = SharedState::new(minimal_snapshot(), 10, 10);
        let mut initial = minimal_snapshot();
        initial.timestamp_us = 1;
        state.publish_snapshot(initial);
        let mut second = minimal_snapshot();
        second.timestamp_us = 2;
        state.publish_snapshot(second);

        assert_eq!(state.latest_snapshot().timestamp_us, 2);
        let rx = state.subscribe_snapshot();
        assert_eq!(rx.borrow().timestamp_us, 2);
    }

    #[test]
    fn zwei_daemon_instanzen_erhalten_unterschiedliche_session_ids() {
        let a = SharedState::new(minimal_snapshot(), 10, 10);
        let b = SharedState::new(minimal_snapshot(), 10, 10);
        // Nicht garantiert kollisionsfrei, aber praktisch nie gleich --
        // ein Fehlschlag hier wäre ein Hinweis auf einen echten Bug in
        // random_session_id(), nicht auf Flakiness.
        assert_ne!(a.session_id, b.session_id);
    }

    #[test]
    fn anomaly_id_ist_monoton_steigend_ab_eins() {
        let state = SharedState::new(minimal_snapshot(), 10, 10);
        assert_eq!(state.next_anomaly_id(), 1);
        assert_eq!(state.next_anomaly_id(), 2);
        assert_eq!(state.next_anomaly_id(), 3);
    }

    #[test]
    fn recent_ring_verdraengt_aeltesten_bei_ueberlauf() {
        let state = SharedState::new(minimal_snapshot(), 2, 10);
        state.publish_anomaly(sample_anomaly_event(1));
        state.publish_anomaly(sample_anomaly_event(2));
        state.publish_anomaly(sample_anomaly_event(3));

        let recent = state.recent_anomalies();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].id, 2);
        assert_eq!(recent[1].id, 3);
    }

    #[tokio::test]
    async fn broadcast_liefert_anomalien_an_abonnenten() {
        let state = SharedState::new(minimal_snapshot(), 10, 10);
        let mut rx = state.subscribe_anomalies();
        state.publish_anomaly(sample_anomaly_event(1));
        let received = rx.recv().await.unwrap();
        assert_eq!(received.id, 1);
    }

    #[test]
    fn context_ring_verdraengt_aeltesten_bei_kapazitaet() {
        let mut ring = ContextRing::new(2);
        ring.push(entry(1, None, "eins"));
        ring.push(entry(2, None, "zwei"));
        ring.push(entry(3, None, "drei"));
        assert_eq!(ring.len(), 2);
        let (lines, _) = ring.query(2, 5, 5, None);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].message.as_ref(), "zwei");
        assert_eq!(lines[1].message.as_ref(), "drei");
    }

    #[test]
    fn context_query_findet_naechstgelegenen_zeitstempel_und_umgebung() {
        let mut ring = ContextRing::new(100);
        for i in 0..10u64 {
            ring.push(entry(i * 10, None, &format!("zeile-{i}")));
        }
        // Zeitstempel 55 liegt exakt zwischen Index 5 (50) und 6 (60) --
        // bei Gleichstand gewinnt laut Dokumentation der frühere Index (5).
        let (lines, truncated) = ring.query(55, 2, 2, None);
        assert_eq!(
            lines.iter().map(|e| e.message.as_ref()).collect::<Vec<_>>(),
            vec!["zeile-3", "zeile-4", "zeile-5", "zeile-6", "zeile-7"]
        );
        assert!(!truncated);
    }

    #[test]
    fn context_query_markiert_truncated_wenn_rand_erreicht_wird() {
        let mut ring = ContextRing::new(100);
        for i in 0..5u64 {
            ring.push(entry(i, None, &format!("zeile-{i}")));
        }
        // Treffer ist der erste Eintrag; 3 davor gibt es nicht.
        let (lines, truncated) = ring.query(0, 3, 1, None);
        assert!(truncated);
        assert_eq!(
            lines.iter().map(|e| e.message.as_ref()).collect::<Vec<_>>(),
            vec!["zeile-0", "zeile-1"]
        );
    }

    #[test]
    fn context_query_filtert_nach_unit() {
        let mut ring = ContextRing::new(100);
        ring.push(entry(1, Some("sshd.service"), "ssh-1"));
        ring.push(entry(2, Some("cron.service"), "cron-1"));
        ring.push(entry(3, Some("sshd.service"), "ssh-2"));

        let (lines, _) = ring.query(2, 5, 5, Some("sshd.service"));
        assert_eq!(
            lines.iter().map(|e| e.message.as_ref()).collect::<Vec<_>>(),
            vec!["ssh-1", "ssh-2"]
        );
    }

    #[test]
    fn context_query_auf_leerem_ring_liefert_leere_liste() {
        let ring = ContextRing::new(10);
        let (lines, truncated) = ring.query(0, 5, 5, None);
        assert!(lines.is_empty());
        assert!(truncated);
    }
}
