//! Integrationstest: verarbeitet 31 reale Beispielzeilen (Fixture) durch die
//! Template-Engine und prüft, dass strukturell gleiche Zeilen auf dasselbe
//! Template abgebildet werden (Kompression) und die Registry deutlich
//! weniger Templates enthält als es Eingabezeilen gab.

use logsentry_core::TemplateEngine;
use std::fs;
use std::path::Path;

fn load_corpus() -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/normalize_corpus.txt");
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("Fixture {path:?} konnte nicht gelesen werden: {err}"))
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn kompression_ueber_reale_beispielzeilen() {
    let lines = load_corpus();
    assert_eq!(lines.len(), 31, "Fixture sollte genau 31 Zeilen enthalten");

    let mut engine = TemplateEngine::new(0.7, 1000);
    let ids: Vec<_> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| engine.process(line, 1000 + i as u64))
        .map(|m| m.id)
        .collect();

    // Deutliche Kompression: strukturell gleiche Zeilen (unterschiedliche
    // IPs/Ports/Usernamen) sollten auf spürbar weniger Templates abbilden
    // als es Eingabezeilen gab.
    assert!(
        engine.template_count() < lines.len(),
        "Template-Anzahl ({}) sollte kleiner als Zeilenanzahl ({}) sein",
        engine.template_count(),
        lines.len()
    );
    assert!(
        engine.template_count() <= 22,
        "erwartete deutlich weniger als 30 Templates, tatsächlich: {}",
        engine.template_count()
    );

    // Gezielte Stichproben bekannter Duplikat-Paare (0-indiziert):
    // Zeile 0/1: "Accepted publickey ... from <verschiedene IP>" -> gleiche IDs.
    assert_eq!(ids[0], ids[1], "unterschiedliche IPs sollten dasselbe Template ergeben");

    // Zeile 2-5: "Failed password ..." mit wechselnder IP/Port/Nutzername.
    assert_eq!(ids[2], ids[3]);
    assert_eq!(ids[2], ids[4]);
    assert_eq!(ids[2], ids[5], "ein abweichendes Token von mehreren sollte noch mergen");

    // Zeile 6/7: "sshd[PID]: Connection closed ..." unterschiedliche PID/Port.
    assert_eq!(ids[6], ids[7]);

    // Zeile 8/9: "cron[PID]: (root) CMD (<Pfad>)" unterschiedliche PID/Pfad.
    assert_eq!(ids[8], ids[9]);

    // Zeile 14/15: kernel I/O error, unterschiedlicher Sektor.
    assert_eq!(ids[14], ids[15]);

    // Zeile 23-25: "Started backup job for user <name>", drei verschiedene Namen.
    assert_eq!(ids[23], ids[24]);
    assert_eq!(ids[23], ids[25]);

    // Zeile 26/27: "New session <n> of user <name>".
    assert_eq!(ids[26], ids[27]);

    // Zeile 29/30: nginx-Zugriffslog, nach Maskierung von Pfad/Statuscode identisch.
    assert_eq!(ids[29], ids[30]);
}
