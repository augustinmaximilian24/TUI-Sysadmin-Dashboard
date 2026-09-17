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
    let mut templates =
        TemplateEngine::new(config.template_similarity_threshold, config.max_templates);
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
        message.contains("Failed password") && anomaly.unit.as_deref() == Some("sshd.service")
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
fn sturmkorrelierte_fehlalarme_aus_phase_3_sind_verschwunden() {
    // Regressionstest für die Verbesserung aus Phase 4 Schritt 7: Diese
    // beiden Meldungen (t=944s SMART-Zeile, t=968s ChronyD-Zeile) traten im
    // reinen Phase-3-Stand auf, weil sie zufällig in das Zeitfenster des
    // Bruteforce-Sturms fielen. Sollten sie wieder auftauchen, ist die
    // Baseline-Anbindung kaputt oder ihr Effekt verschwunden.
    let anomalies = replay();
    let waehrend_sturm_faelschlich: Vec<&str> = anomalies
        .iter()
        .filter(|(anomaly, message)| {
            !message.contains("Failed password")
                && !message.contains("due to memory pressure")
                && anomaly.timestamp_us >= replay_base_us() + 900 * 1_000_000
                && anomaly.timestamp_us <= replay_base_us() + 1000 * 1_000_000
        })
        .map(|(_, message)| message.as_str())
        .collect();
    assert!(
        waehrend_sturm_faelschlich.is_empty(),
        "sturmkorrelierte Fehlalarme sind zurückgekehrt: {waehrend_sturm_faelschlich:?}"
    );
}

/// Fester Startzeitpunkt der Fixture (siehe `generate_anomaly_replay.py`).
fn replay_base_us() -> u64 {
    1_767_225_600_000_000
}

#[test]
fn erzeugt_im_normalbetrieb_kaum_fehlalarme() {
    let anomalies = replay();
    // Alles, was weder Bruteforce noch OOM-Kill ist, gilt als Fehlalarm.
    let fehlalarme: Vec<&str> = anomalies
        .iter()
        .filter(|(_, message)| {
            !message.contains("Failed password") && !message.contains("due to memory pressure")
        })
        .map(|(_, message)| message.as_str())
        .collect();

    // Historie dieser Zahl (jeweils gegen exakt dieselbe Fixture verifiziert,
    // nicht geschätzt):
    // - Reiner Phase-3-Stand (Commit d573e35, vor jeder Baseline-Anbindung):
    //   3 Fehlalarme (t=304s, t=944s, t=968s -- SMART-Zeile und ChronyD-Zeile).
    //   t=944/968 sind sturmkorreliert: Das Surprisal wurde gegen die
    //   Verteilung im 60-Sekunden-Momentanfenster gebildet, und während der
    //   Bruteforce-Sturm dieses Fenster dominiert, wird jedes gewöhnliche
    //   Template darin tatsächlich relativ selten.
    // - Mit Zeitprofil-Baselines (Phase 4, dieser Schritt): 1 Fehlalarm
    //   (t=304s). Das Surprisal kommt jetzt aus dem langfristigen
    //   Unit-Profil statt dem Momentanfenster; ein einminütiger Sturm
    //   verändert ein mit Tagen Halbwertszeit gewichtetes Profil nur
    //   marginal -- die beiden sturmkorrelierten Meldungen sind
    //   nachweislich verschwunden (mit dem reinen Phase-3-Commit
    //   gegengeprüft, nicht nur angenommen).
    //
    // Die verbliebene Meldung bei t=304s ist NICHT sturmkorreliert (der
    // Angriff beginnt erst bei t=900s) und läuft nachweislich vollständig
    // über die Kurzzeit-Pfade aus Phase 3 (RateTracker + Momentanfenster,
    // `rate_source: ShortTerm`): Zu diesem frühen Zeitpunkt hat weder die
    // Slot-Baseline noch das Unit-Profil dieser Unit die Vertrauensschwelle
    // erreicht. Es handelt sich um ein Kaltstart-Artefakt der 300-Sekunden-
    // Rate-Historie aus Phase 3 (ihre Historie füllt sich bei t~300s zum
    // ersten Mal), nicht um eine Lücke in der Phase-4-Baseline-Logik --
    // eine echte Behebung läge außerhalb des Umfangs dieses Schritts.
    assert!(
        fehlalarme.len() <= 1,
        "zu viele Fehlalarme im Normalbetrieb ({}): {:?}",
        fehlalarme.len(),
        fehlalarme
    );
}
