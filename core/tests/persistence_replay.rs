//! Zweifacher Replay über die Phase-3-Fixture mit Persistenz dazwischen
//! (Regel 28, Aufgabe 10 in `docs/phase4-baselines.md`).
//!
//! Lauf 1 lernt aus dem Nichts und sichert seinen Stand in eine
//! redb-Datei. Lauf 2 startet aus dieser Datei. Geprüft wird:
//! 1. Lauf 2 hat keine Lernphase (vertrauenswürdige Baselines geladen).
//! 2. Lauf 2 aus der **Datei** liefert exakt dieselben Anomalien wie ein
//!    Lauf 2 aus einem reinen **In-Memory**-Snapshot -- der Datei-Roundtrip
//!    ist also verlustfrei. Das ist die präzise Fassung von "die Ergebnisse
//!    müssen identisch sein": Ein Neustart darf sich von einem
//!    ununterbrochenen Prozess nur um das unterscheiden, was der Snapshot
//!    bewusst nicht enthält (offener Bucket, Kurzzeit-Historie), und genau
//!    das ist in beiden Varianten gleich.
//! 3. Bruteforce und OOM-Kill werden auch in Lauf 2 erkannt, der
//!    Bruteforce eskaliert weiterhin bis `critical`.

use logsentry_core::analysis::{AnalysisEngine, AnalysisInput, AnomalyLevel};
use logsentry_core::baseline::{BaselineDb, PersistedState};
use logsentry_core::config::Config;
use logsentry_core::journal::{parse_journal_line, JournalEvent};
use logsentry_core::TemplateEngine;
use std::fs;
use std::path::Path;

fn fixture_events() -> Vec<JournalEvent> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anomaly_replay.ndjson");
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("Fixture {path:?} nicht lesbar: {err}"));
    raw.lines()
        .filter_map(|line| parse_journal_line(line).ok())
        .collect()
}

fn test_config() -> Config {
    let mut config = Config::default();
    config.analysis.learning_phase_minutes = 5;
    config
}

/// Gemeldete Anomalien eines Laufs, als vergleichbare Tupel.
type Report = Vec<(u64, AnomalyLevel, String)>;

struct RunResult {
    report: Report,
    learning_at_first_event: bool,
    state: PersistedState,
}

/// Führt die Fixture durch die Pipeline. `restored` ist entweder `None`
/// (Lauf aus dem Nichts) oder ein zuvor gesicherter Bestand.
fn run(config: &Config, restored: Option<PersistedState>) -> RunResult {
    let events = fixture_events();
    let mut engine =
        AnalysisEngine::with_baseline_config(config.analysis.clone(), config.baseline.clone());
    let mut templates = match restored {
        Some(state) => {
            engine.restore_baselines(state.baselines);
            if engine.has_trusted_baselines() {
                engine.skip_learning_phase();
            }
            TemplateEngine::restore(
                state.templates,
                config.analysis.template_similarity_threshold,
                config.analysis.max_templates,
            )
        }
        None => TemplateEngine::new(
            config.analysis.template_similarity_threshold,
            config.analysis.max_templates,
        ),
    };

    let first_ts = events.first().map_or(0, |e| e.realtime_timestamp_us);
    let learning_at_first_event = engine.in_learning_phase(first_ts);

    let mut report = Vec::new();
    for event in &events {
        let matched = templates.process(&event.message, event.realtime_timestamp_us);
        if let Some(anomaly) = engine.process(AnalysisInput {
            timestamp_us: event.realtime_timestamp_us,
            template_id: matched.id,
            unit: event.systemd_unit.as_deref(),
        }) {
            report.push((anomaly.timestamp_us, anomaly.level, event.message.clone()));
        }
    }

    engine.enforce_baseline_limits();
    RunResult {
        report,
        learning_at_first_event,
        state: PersistedState {
            baselines: engine.baseline_snapshot(),
            templates: templates.snapshot(),
        },
    }
}

fn assert_angriffe_erkannt(report: &Report, lauf: &str) {
    let bruteforce_max = report
        .iter()
        .filter(|(_, _, m)| m.contains("Failed password"))
        .map(|(_, level, _)| *level)
        .max();
    assert_eq!(
        bruteforce_max,
        Some(AnomalyLevel::Critical),
        "{lauf}: Bruteforce muss bis critical eskalieren"
    );
    assert!(
        report
            .iter()
            .any(|(_, _, m)| m.contains("due to memory pressure")),
        "{lauf}: OOM-Kill muss erkannt werden"
    );
}

#[test]
fn zweiter_lauf_aus_persistenz_ist_identisch_zum_in_memory_lauf_und_ohne_lernphase() {
    let config = test_config();

    // Lauf 1: aus dem Nichts.
    let erster = run(&config, None);
    assert!(
        erster.learning_at_first_event,
        "Lauf 1 muss mit Lernphase beginnen"
    );
    assert_angriffe_erkannt(&erster.report, "Lauf 1");
    assert!(
        !erster.state.baselines.profiles.is_empty(),
        "nach Lauf 1 müssen Unit-Profile existieren"
    );

    // Bestand über eine echte redb-Datei sichern und wieder laden.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("baselines.redb");
    {
        let db = BaselineDb::open(&path, "testhost", 5).expect("öffnen");
        db.save(&erster.state).expect("sichern");
    }
    let geladen = {
        let db = BaselineDb::open(&path, "testhost", 5).expect("erneut öffnen");
        db.load().expect("laden").expect("Bestand vorhanden")
    };

    // Lauf 2a: aus der Datei. Lauf 2b: aus dem In-Memory-Snapshot, ohne
    // Umweg über die Platte. Beide müssen sich exakt gleich verhalten.
    let aus_datei = run(&config, Some(geladen));
    let aus_speicher = run(&config, Some(erster.state.clone()));

    assert!(
        !aus_datei.learning_at_first_event,
        "Lauf 2 darf keine Lernphase haben, wenn vertrauenswürdige Baselines geladen wurden"
    );
    assert_eq!(
        aus_datei.report, aus_speicher.report,
        "Lauf aus der Datei muss dem Lauf aus dem In-Memory-Snapshot gleichen \
         (Datei-Roundtrip muss verlustfrei sein)"
    );
    assert_angriffe_erkannt(&aus_datei.report, "Lauf 2");

    // Der Zustand nach Lauf 2 muss ebenfalls in beiden Varianten gleich
    // sein -- sonst wäre die Divergenz nur noch nicht sichtbar geworden.
    let mut h1 = aus_datei.state.baselines.histograms.clone();
    let mut h2 = aus_speicher.state.baselines.histograms.clone();
    h1.sort_by_key(|(k, _)| (k.unit_key, k.template_id.0, k.slot.0));
    h2.sort_by_key(|(k, _)| (k.unit_key, k.template_id.0, k.slot.0));
    assert_eq!(h1, h2, "Baseline-Zustand nach Lauf 2 divergiert");
    assert_eq!(
        aus_datei.state.templates, aus_speicher.state.templates,
        "Template-Registry nach Lauf 2 divergiert"
    );
}

#[test]
fn zweiter_lauf_erzeugt_nicht_mehr_fehlalarme_als_der_erste() {
    let config = test_config();
    let erster = run(&config, None);
    let zweiter = run(&config, Some(erster.state.clone()));

    let fehlalarme = |report: &Report| {
        report
            .iter()
            .filter(|(_, _, m)| {
                !m.contains("Failed password") && !m.contains("due to memory pressure")
            })
            .count()
    };
    assert!(
        fehlalarme(&zweiter.report) <= fehlalarme(&erster.report),
        "mehr Wissen darf nicht zu mehr Fehlalarmen führen: Lauf 1 = {}, Lauf 2 = {}",
        fehlalarme(&erster.report),
        fehlalarme(&zweiter.report)
    );
}
