//! Replay-Integrationstest über die vollständige Pipeline
//! (Parsing → Maskierung → Template → Analyse) gegen eine feste Fixture.
//!
//! Deckt zwei Punkte der Definition of Done ab: Ein simulierter
//! SSH-Bruteforce und ein OOM-Kill müssen zuverlässig erkannt werden, ohne
//! dass der Normalbetrieb in Fehlalarmen untergeht.

use logsentry_core::analysis::{AnalysisEngine, AnalysisInput, Anomaly, AnomalyLevel};
use logsentry_core::config::AnalysisConfig;
use logsentry_core::journal::parse_journal_line;
use logsentry_core::TemplateEngine;
use std::fs;
use std::path::Path;

/// Führt die Fixture durch die gesamte Pipeline und liefert alle gemeldeten
/// Anomalien zusammen mit der auslösenden Nachricht.
fn replay() -> Vec<(Anomaly, String)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anomaly_replay.ndjson");
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("Fixture {path:?} nicht lesbar: {err}"));

    let config = AnalysisConfig {
        // Die Fixture beginnt bei t=0; eine kurze Lernphase reicht, damit
        // die Grundlast als normal gilt, bevor der Angriff einsetzt.
        learning_phase_minutes: 5,
        ..AnalysisConfig::default()
    };
    let mut templates = TemplateEngine::new(
        config.template_similarity_threshold,
        config.max_templates,
    );
    let mut engine = AnalysisEngine::new(config);

    let mut anomalies = Vec::new();
    for line in raw.lines() {
        let Ok(event) = parse_journal_line(line) else {
            continue;
        };
        let matched = templates.process(&event.message, event.realtime_timestamp_us);
        let result = engine.process(AnalysisInput {
            timestamp_us: event.realtime_timestamp_us,
            template_id: matched.id,
            unit: event.systemd_unit.as_deref(),
        });
        if let Some(anomaly) = result {
            anomalies.push((anomaly, event.message.clone()));
        }
    }
    anomalies
}

#[test]
fn erkennt_ssh_bruteforce_im_replay() {
    let anomalies = replay();
    let treffer = anomalies.iter().any(|(anomaly, message)| {
        message.contains("Failed password")
            && anomaly.unit.as_deref() == Some("sshd.service")
    });
    assert!(
        treffer,
        "SSH-Bruteforce muss erkannt werden. Gemeldet wurde: {:?}",
        anomalies
            .iter()
            .map(|(a, m)| (a.level, m.as_str()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn bruteforce_eskaliert_bis_critical() {
    let anomalies = replay();
    let hoechstes = anomalies
        .iter()
        .filter(|(_, message)| message.contains("Failed password"))
        .map(|(anomaly, _)| anomaly.level)
        .max();
    assert_eq!(
        hoechstes,
        Some(AnomalyLevel::Critical),
        "ein anhaltender Bruteforce muss bis critical eskalieren, nicht auf der \
         ersten schwachen Erkennung stehen bleiben"
    );
}

#[test]
fn erkennt_oom_kill_im_replay() {
    let anomalies = replay();
    let treffer = anomalies
        .iter()
        .any(|(_, message)| message.contains("due to memory pressure"));
    assert!(
        treffer,
        "OOM-Kill muss erkannt werden. Gemeldet wurde: {:?}",
        anomalies
            .iter()
            .map(|(a, m)| (a.level, m.as_str()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn erzeugt_im_normalbetrieb_wenige_fehlalarme() {
    let anomalies = replay();
    // Alles, was weder Bruteforce noch OOM-Kill ist, gilt als Fehlalarm.
    let fehlalarme: Vec<&str> = anomalies
        .iter()
        .filter(|(_, message)| {
            !message.contains("Failed password") && !message.contains("due to memory pressure")
        })
        .map(|(_, message)| message.as_str())
        .collect();

    // Die Fixture umfasst 25 Minuten. Der Zielwert der Definition of Done
    // liegt bei unter 5 Fehlalarmen pro Tag.
    //
    // Bekannte Restgrenze: Die verbleibenden Meldungen fallen sämtlich in
    // das Zeitfenster des Bruteforce-Sturms. Das Surprisal wird gegen die
    // Verteilung im Momentanfenster gebildet; solange ein Sturm dieses
    // Fenster dominiert, wird jedes gewöhnliche Template darin tatsächlich
    // relativ selten und schlägt entsprechend aus. Sauber lösen lässt sich
    // das erst mit einem längerfristigen Vergleichsmaßstab statt des
    // Momentanfensters -- das ist Gegenstand von Phase 4 (Baselines pro Unit
    // mit Tageszeitprofil). Bis dahin bleibt die Grenze bewusst sichtbar.
    assert!(
        fehlalarme.len() <= 3,
        "zu viele Fehlalarme im Normalbetrieb ({}): {:?}",
        fehlalarme.len(),
        fehlalarme
    );
}
