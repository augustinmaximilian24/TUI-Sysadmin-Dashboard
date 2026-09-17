//! Verarbeitungspipeline des Daemons: nimmt Journal-Ereignisse aus dem
//! Ring-Kanal, normalisiert sie zu Templates, führt die Anomalie-Analyse
//! aus und sichert Baselines und Template-Registry periodisch.
//!
//! Läuft als eigener Task neben der Ingestion. Der Snapshot wird bewusst
//! im selben Task geschrieben (ein Schreiber, kein geteilter Zustand; siehe
//! `docs/phase4-baselines.md` Abschnitt 6.4).
//!
//! Seit Phase 6 Schritt 6 fließen Anomalien und periodische Snapshots
//! zusätzlich in den geteilten Zustand (`state.rs`), von wo sie an
//! verbundene Clients gehen; die `tracing`-Protokollierung bleibt für den
//! Betrieb ohne GUI (Server-Log) bestehen.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use logsentry_core::analysis::{AnalysisEngine, AnalysisInput};
use logsentry_core::baseline::{BaselineDb, PersistedState};
use logsentry_core::{Config, JournalEvent, RingReceiver, TemplateEngine};
use logsentry_proto::{LearningState, PipelineStats, Snapshot, WindowStats};

use crate::now_us;
use crate::state::SharedState;
use crate::wire::{self, AnomalyEventContext, ContextEntry};

/// Zustand der Pipeline, aus persistierten Daten oder leer aufgebaut.
pub struct Pipeline {
    templates: TemplateEngine,
    engine: AnalysisEngine,
    db: Option<BaselineDb>,
    state: Arc<SharedState>,
    persist_interval: Duration,
    snapshot_interval: Duration,
    replay: bool,
    started_at: Instant,
    events: u64,
    anomalies: u64,
    /// Wegen Selbstfilter (Regel 13) von der Analyse ausgenommene
    /// Ereignisse -- Folgezeilen einer eigenen `RestartUnit`/`StopUnit`/
    /// `TerminateProcess`-Aktion.
    self_filtered: u64,
    /// Wegen `ActionRequest::MuteAnomaly` von der Analyse ausgenommene
    /// Ereignisse.
    muted: u64,
    /// Ereigniszähler beim letzten veröffentlichten Snapshot, für
    /// `events_per_sec` (Regel: Magic Numbers gehören in Config, nicht der
    /// Zähler selbst -- der bleibt reiner Laufzeitzustand).
    events_at_last_snapshot: u64,
}

impl Pipeline {
    /// Baut die Pipeline aus der Konfiguration, dem geteilten Zustand für
    /// die Socket-Verteilung (Phase 6) und -- falls vorhanden -- dem beim
    /// Start geladenen Bestand. Wurden vertrauenswürdige Baselines geladen,
    /// wird die globale Lernphase übersprungen. `replay` steuert nur das
    /// `Snapshot::replay`-Feld für die GUI-Anzeige, nicht das Verhalten der
    /// Analyse.
    pub fn new(
        config: &Config,
        db: Option<BaselineDb>,
        restored: Option<PersistedState>,
        state: Arc<SharedState>,
        replay: bool,
    ) -> Self {
        let similarity = config.analysis.template_similarity_threshold;
        let max_templates = config.analysis.max_templates;

        let mut engine =
            AnalysisEngine::with_baseline_config(config.analysis.clone(), config.baseline.clone());

        let templates = match restored {
            Some(state) => {
                let template_count = state.templates.clusters.len();
                let histogram_count = state.baselines.histograms.len();
                engine.restore_baselines(state.baselines);
                let trusted = engine.has_trusted_baselines();
                if trusted {
                    engine.skip_learning_phase();
                }
                tracing::info!(
                    templates = template_count,
                    histogramme = histogram_count,
                    lernphase_uebersprungen = trusted,
                    "persistierten Bestand geladen"
                );
                TemplateEngine::restore(state.templates, similarity, max_templates)
            }
            None => TemplateEngine::new(similarity, max_templates),
        };

        Self {
            templates,
            engine,
            db,
            state,
            persist_interval: Duration::from_secs(
                config.persistence.snapshot_interval_minutes.max(1) * 60,
            ),
            snapshot_interval: Duration::from_millis(
                u64::from(config.socket.snapshot_interval_ms).max(50),
            ),
            replay,
            started_at: Instant::now(),
            events: 0,
            anomalies: 0,
            self_filtered: 0,
            muted: 0,
            events_at_last_snapshot: 0,
        }
    }

    /// Verarbeitet ein einzelnes Ereignis.
    fn handle(&mut self, event: &JournalEvent) {
        self.events += 1;
        let matched = self
            .templates
            .process(&event.message, event.realtime_timestamp_us);

        self.state.push_context(ContextEntry {
            timestamp_us: event.realtime_timestamp_us,
            unit: event.systemd_unit.as_deref().map(Arc::from),
            pid: event.pid,
            priority: event.priority,
            message: Arc::from(event.message.as_str()),
        });

        // Selbstfilter (Regel 13) vor Mute geprüft: eine per Aktion
        // ausgelöste Folgezeile soll auch dann nicht als Anomalie
        // erscheinen, wenn zufällig kein Mute für ihr Template existiert.
        // Beide Prüfungen laufen unabhängig vom Analyseergebnis -- der
        // `ContextRing` oben hat die Zeile bereits unbedingt aufgenommen
        // (Audit-Sichtbarkeit, `docs/phase6-protokoll.md` Abschnitt 9).
        if self.state.is_self_filtered(
            event.systemd_unit.as_deref(),
            event.pid,
            event.realtime_timestamp_us,
        ) {
            self.self_filtered += 1;
            return;
        }
        if self.state.is_muted(
            matched.id.0,
            event.systemd_unit.as_deref(),
            event.realtime_timestamp_us,
        ) {
            self.muted += 1;
            return;
        }

        let result = self.engine.process(AnalysisInput {
            timestamp_us: event.realtime_timestamp_us,
            template_id: matched.id,
            unit: event.systemd_unit.as_deref(),
        });
        if let Some(anomaly) = result {
            self.anomalies += 1;
            tracing::warn!(
                level = anomaly.level.as_str(),
                unit = anomaly.unit.as_deref().unwrap_or("<none>"),
                template = %anomaly.template_id,
                score = format!("{:.3}", anomaly.breakdown.combined),
                rate_z = format!("{:.2}", anomaly.breakdown.rate_z),
                rate_source = ?anomaly.breakdown.rate_source,
                surprisal = format!("{:.2}", anomaly.breakdown.surprisal_bits),
                unterdrueckt_seit_letzter = anomaly.suppressed_since_last,
                nachricht = %event.message,
                "Anomalie"
            );

            let event_ctx = AnomalyEventContext {
                template_text: &matched.template,
                raw_message: &event.message,
                pid: event.pid,
                priority: event.priority,
            };
            if let Some(wire_event) =
                wire::build_anomaly_event(self.state.next_anomaly_id(), &anomaly, event_ctx)
            {
                self.state.publish_anomaly(wire_event);
            }
        }
    }

    /// Baut und veröffentlicht einen aktuellen Snapshot im geteilten
    /// Zustand. `system` kommt vom unabhängigen Systemzustands-Task
    /// (`main.rs::run_system_monitor`), der zuletzt gemessene Stand wird
    /// hier nur durchgereicht.
    fn publish_snapshot(&mut self, timestamp_us: u64, parse_errors: u64, dropped_overflow: u64) {
        let stats = self.engine.stats();
        let elapsed_secs = self.snapshot_interval.as_secs_f64().max(0.001);
        let events_per_sec =
            (self.events.saturating_sub(self.events_at_last_snapshot)) as f64 / elapsed_secs;
        self.events_at_last_snapshot = self.events;

        let learning_remaining = self.engine.learning_remaining_secs(timestamp_us);
        let snapshot = Snapshot {
            timestamp_us,
            daemon_uptime_secs: self.started_at.elapsed().as_secs(),
            learning: LearningState {
                active: learning_remaining.is_some(),
                remaining_secs: learning_remaining,
            },
            stats: PipelineStats {
                events: self.events,
                parse_errors,
                dropped_overflow,
                templates: self.templates.template_count() as u64,
                anomalies_emitted: self.anomalies,
                suppressed: stats.suppressed,
                suppressed_learning: stats.suppressed_learning,
                events_per_sec,
                clients: self.state.client_count(),
            },
            window: WindowStats {
                window_secs: self.engine.window_seconds() as u32,
                entropy_bits: self.engine.current_entropy(),
                entropy_z: self.engine.current_entropy_z(),
                events_in_window: self.engine.window_len() as u64,
                distinct_templates: self.engine.distinct_templates() as u64,
            },
            system: self.state.latest_system_snapshot(),
            replay: self.replay,
        };
        self.state.publish_snapshot(snapshot);
    }

    /// Sichert Baselines und Template-Registry, falls Persistenz aktiv ist.
    fn snapshot(&mut self, grund: &str) {
        let Some(db) = &self.db else {
            return;
        };
        self.engine.enforce_baseline_limits();
        let state = PersistedState {
            baselines: self.engine.baseline_snapshot(),
            templates: self.templates.snapshot(),
        };
        match db.save(&state) {
            Ok(()) => tracing::info!(
                grund,
                histogramme = state.baselines.histograms.len(),
                templates = state.templates.clusters.len(),
                pfad = %db.path().display(),
                "Snapshot gesichert"
            ),
            Err(err) => tracing::error!(fehler = %err, "Snapshot fehlgeschlagen"),
        }
    }

    /// Hauptschleife: Ereignisse verarbeiten, periodisch Baselines sichern
    /// und einen Socket-Snapshot veröffentlichen, beim Schließen des Kanals
    /// ein letztes Mal beides.
    pub async fn run(
        mut self,
        mut receiver: RingReceiver<JournalEvent>,
        parse_error_counter: Arc<AtomicU64>,
    ) -> PipelineSummary {
        let mut persist_ticker = tokio::time::interval(self.persist_interval);
        let mut snapshot_ticker = tokio::time::interval(self.snapshot_interval);
        // Der erste Tick eines Intervalls feuert sofort; den wollen wir für
        // die Baseline-Persistenz nicht, sonst würde direkt nach dem Start
        // ein leerer Bestand geschrieben. Der erste Socket-Snapshot darf
        // dagegen sofort raus, damit ein früh verbundener Client nicht bis
        // zum ersten Intervall auf Daten wartet.
        persist_ticker.tick().await;

        loop {
            tokio::select! {
                maybe_event = receiver.recv() => {
                    match maybe_event {
                        Some(event) => self.handle(&event),
                        None => break,
                    }
                }
                _ = persist_ticker.tick() => {
                    self.snapshot("periodisch");
                }
                _ = snapshot_ticker.tick() => {
                    self.publish_snapshot(
                        now_us(),
                        parse_error_counter.load(Ordering::Relaxed),
                        receiver.dropped_count(),
                    );
                }
            }
        }

        self.snapshot("beenden");
        self.publish_snapshot(
            now_us(),
            parse_error_counter.load(Ordering::Relaxed),
            receiver.dropped_count(),
        );

        let stats = self.engine.stats();
        PipelineSummary {
            events: self.events,
            anomalies: self.anomalies,
            templates: self.templates.template_count(),
            dropped_overflow: receiver.dropped_count(),
            parse_errors: parse_error_counter.load(Ordering::Relaxed),
            suppressed: stats.suppressed,
            suppressed_learning: stats.suppressed_learning,
            self_filtered: self.self_filtered,
            muted: self.muted,
        }
    }
}

/// Kennzahlen am Ende eines Pipeline-Laufs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineSummary {
    /// Verarbeitete Ereignisse.
    pub events: u64,
    /// Gemeldete Anomalien.
    pub anomalies: u64,
    /// Templates in der Registry.
    pub templates: usize,
    /// Wegen Kanal-Überlauf verworfene Ereignisse.
    pub dropped_overflow: u64,
    /// Nicht parsbare Journal-Zeilen.
    pub parse_errors: u64,
    /// Wegen Sperrzeit unterdrückte Meldungen.
    pub suppressed: u64,
    /// Während der Lernphase unterdrückte Meldungen.
    pub suppressed_learning: u64,
    /// Wegen Selbstfilter (Regel 13) von der Analyse ausgenommene
    /// Ereignisse.
    pub self_filtered: u64,
    /// Wegen `MuteAnomaly` von der Analyse ausgenommene Ereignisse.
    pub muted: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_snapshot() -> logsentry_proto::Snapshot {
        logsentry_proto::Snapshot {
            timestamp_us: 0,
            daemon_uptime_secs: 0,
            learning: logsentry_proto::LearningState {
                active: false,
                remaining_secs: None,
            },
            stats: PipelineStats {
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
            window: WindowStats {
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

    fn test_pipeline() -> Pipeline {
        let config = Config::default();
        let state = Arc::new(SharedState::new(minimal_snapshot(), 10, 10));
        Pipeline::new(&config, None, None, state, false)
    }

    fn event(unit: &str, pid: i32, message: &str, timestamp_us: u64) -> JournalEvent {
        JournalEvent {
            realtime_timestamp_us: timestamp_us,
            systemd_unit: Some(unit.to_string()),
            pid: Some(pid),
            priority: Some(6),
            message: message.to_string(),
            message_was_binary: false,
            hostname: None,
        }
    }

    #[test]
    fn selbstgefilterte_unit_wird_gezaehlt_aber_nicht_analysiert() {
        let mut pipeline = test_pipeline();
        pipeline.state.suppress_unit("sshd.service", u64::MAX);

        pipeline.handle(&event("sshd.service", 100, "Failed password for root", 1));

        assert_eq!(pipeline.self_filtered, 1);
        assert_eq!(
            pipeline.events, 1,
            "Ereignis zählt trotzdem als verarbeitet"
        );
        assert_eq!(
            pipeline.engine.stats().processed,
            0,
            "Analyse-Engine darf ein selbstgefiltertes Ereignis nie sehen"
        );
        // Audit-Sichtbarkeit bleibt erhalten (docs/phase6-protokoll.md
        // Abschnitt 9): der ContextRing bekommt die Zeile trotzdem.
        let (lines, _) = pipeline.state.query_context(1, 1, 1, None);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn gemutetes_template_wird_gezaehlt_aber_nicht_analysiert() {
        let mut pipeline = test_pipeline();
        let message = "immer dieselbe Testnachricht";
        // Dieselbe Maskierung/ID-Bildung wie in `handle`, um die Mute-ID
        // vorab zu kennen -- weißes Kästchen-Wissen über `templates` ist
        // hier bewusst, da beide Aufrufe im selben Prozess auf derselben
        // Registry laufen und für dieselbe Nachricht deterministisch
        // dieselbe ID liefern.
        let template_id = pipeline.templates.process(message, 0).id.0;
        pipeline.state.mute(template_id, None, u64::MAX);

        pipeline.handle(&event("cron.service", 200, message, 1));

        assert_eq!(pipeline.muted, 1);
        assert_eq!(
            pipeline.engine.stats().processed,
            0,
            "Analyse-Engine darf ein gemutetes Ereignis nie sehen"
        );
    }

    #[test]
    fn unbeteiligtes_ereignis_wird_normal_analysiert() {
        let mut pipeline = test_pipeline();
        pipeline.state.suppress_unit("anderer.service", u64::MAX);
        pipeline.state.mute(999, None, u64::MAX);

        pipeline.handle(&event("sshd.service", 100, "eine ganz normale Zeile", 1));

        assert_eq!(pipeline.self_filtered, 0);
        assert_eq!(pipeline.muted, 0);
        assert_eq!(pipeline.engine.stats().processed, 1);
    }
}
