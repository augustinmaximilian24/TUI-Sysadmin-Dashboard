//! Verarbeitungspipeline des Daemons: nimmt Journal-Ereignisse aus dem
//! Ring-Kanal, normalisiert sie zu Templates, führt die Anomalie-Analyse
//! aus und sichert Baselines und Template-Registry periodisch.
//!
//! Läuft als eigener Task neben der Ingestion. Der Snapshot wird bewusst
//! im selben Task geschrieben (ein Schreiber, kein geteilter Zustand; siehe
//! `docs/phase4-baselines.md` Abschnitt 6.4).
//!
//! Anomalien werden in dieser Phase nur protokolliert; die Übergabe an den
//! Socket-Client folgt in Phase 6.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use logsentry_core::analysis::{AnalysisEngine, AnalysisInput};
use logsentry_core::baseline::{BaselineDb, PersistedState};
use logsentry_core::{Config, JournalEvent, RingReceiver, TemplateEngine};

/// Zustand der Pipeline, aus persistierten Daten oder leer aufgebaut.
pub struct Pipeline {
    templates: TemplateEngine,
    engine: AnalysisEngine,
    db: Option<BaselineDb>,
    snapshot_interval: Duration,
    events: u64,
    anomalies: u64,
}

impl Pipeline {
    /// Baut die Pipeline aus der Konfiguration und -- falls vorhanden -- dem
    /// beim Start geladenen Bestand. Wurden vertrauenswürdige Baselines
    /// geladen, wird die globale Lernphase übersprungen.
    pub fn new(config: &Config, db: Option<BaselineDb>, restored: Option<PersistedState>) -> Self {
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
            snapshot_interval: Duration::from_secs(
                config.persistence.snapshot_interval_minutes.max(1) * 60,
            ),
            events: 0,
            anomalies: 0,
        }
    }

    /// Verarbeitet ein einzelnes Ereignis.
    fn handle(&mut self, event: &JournalEvent) {
        self.events += 1;
        let matched = self
            .templates
            .process(&event.message, event.realtime_timestamp_us);
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
        }
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

    /// Hauptschleife: Ereignisse verarbeiten, periodisch sichern, beim
    /// Schließen des Kanals ein letztes Mal sichern.
    pub async fn run(
        mut self,
        mut receiver: RingReceiver<JournalEvent>,
        parse_error_counter: Arc<AtomicU64>,
    ) -> PipelineSummary {
        let mut ticker = tokio::time::interval(self.snapshot_interval);
        // Der erste Tick eines Intervalls feuert sofort; den wollen wir
        // nicht, sonst würde direkt nach dem Start ein leerer Snapshot
        // geschrieben.
        ticker.tick().await;

        loop {
            tokio::select! {
                maybe_event = receiver.recv() => {
                    match maybe_event {
                        Some(event) => self.handle(&event),
                        None => break,
                    }
                }
                _ = ticker.tick() => {
                    self.snapshot("periodisch");
                }
            }
        }

        self.snapshot("beenden");

        let stats = self.engine.stats();
        PipelineSummary {
            events: self.events,
            anomalies: self.anomalies,
            templates: self.templates.template_count(),
            dropped_overflow: receiver.dropped_count(),
            parse_errors: parse_error_counter.load(Ordering::Relaxed),
            suppressed: stats.suppressed,
            suppressed_learning: stats.suppressed_learning,
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
}
