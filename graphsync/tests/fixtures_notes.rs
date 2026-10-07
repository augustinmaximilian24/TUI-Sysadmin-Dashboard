//! Durchlauf gegen feste Beispielnotizen unter `tests/fixtures/notes`
//! (Regel 25: keine Live-Daten in Tests). Die KI-Stufe ist hier
//! abgeschaltet; sie hat eigene Tests mit einem Test-Runner.

use std::collections::BTreeSet;
use std::path::PathBuf;

use logsentry_core::config::GraphSyncConfig;
use logsentry_graphsync::graph::{OutGraph, NODE_ID_PREFIX};
use logsentry_graphsync::sync::Syncer;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/notes")
}

fn run() -> OutGraph {
    let state_dir = tempfile::tempdir().expect("tempdir");
    let mut config = GraphSyncConfig {
        source_dirs: vec![fixture_root().display().to_string()],
        state_path: state_dir.path().join("state.json").display().to_string(),
        output_path: state_dir.path().join("links.json").display().to_string(),
        ..GraphSyncConfig::default()
    };
    config.llm.enabled = false;
    let mut syncer = Syncer::new(config, None);
    let report = syncer.scan_once(0);
    assert_eq!(report.added, 6);
    syncer.write_output().expect("schreiben");
    let raw = std::fs::read(syncer.output_path()).expect("lesen");
    serde_json::from_slice(&raw).expect("gültiges graph.json")
}

fn rel(id: &str) -> String {
    let root = fixture_root().display().to_string();
    id.trim_start_matches(NODE_ID_PREFIX)
        .trim_start_matches(&root)
        .trim_start_matches('/')
        .to_string()
}

#[test]
fn erwartete_kanten_aus_den_beispielnotizen() {
    let graph = run();
    let edges: BTreeSet<(String, String, String, String)> = graph
        .links
        .iter()
        .map(|e| {
            let (a, b) = (rel(&e.source), rel(&e.target));
            let (a, b) = if a < b { (a, b) } else { (b, a) };
            (a, b, e.relation.clone(), e.confidence.clone())
        })
        .collect();
    let expected: BTreeSet<(String, String, String, String)> = [
        ("index.md", "projekte/logsentry.md", "links_to", "EXTRACTED"),
        (
            "index.md",
            "projekte/heimserver.md",
            "links_to",
            "EXTRACTED",
        ),
        ("index.md", "projekte/backup.md", "links_to", "EXTRACTED"),
        (
            "projekte/heimserver.md",
            "projekte/logsentry.md",
            "links_to",
            "EXTRACTED",
        ),
        (
            "projekte/backup.md",
            "projekte/heimserver.md",
            "links_to",
            "EXTRACTED",
        ),
        // #nas + #homelab gemeinsam -> 4 Punkte
        (
            "projekte/backup.md",
            "tagebuch/2026-10-01.md",
            "shares_tag",
            "INFERRED",
        ),
        (
            "projekte/heimserver.md",
            "tagebuch/2026-10-01.md",
            "shares_tag",
            "INFERRED",
        ),
        // Überschriftenwörter "tagebuch" + "oktober" -> 2 Punkte. Bei einem
        // echten Tagebuch mit mehr als max_term_docs Einträgen entfällt das.
        (
            "tagebuch/2026-10-01.md",
            "tagebuch/2026-10-02.md",
            "shares_topic",
            "INFERRED",
        ),
    ]
    .iter()
    .map(|(a, b, r, c)| (a.to_string(), b.to_string(), r.to_string(), c.to_string()))
    .collect();
    assert_eq!(edges, expected);
}

#[test]
fn knoten_haben_titel_gruppen_und_quellpfad() {
    let graph = run();
    assert_eq!(graph.nodes.len(), 6);
    let node = graph
        .nodes
        .iter()
        .find(|n| n.source_file.ends_with("projekte/logsentry.md"))
        .expect("Knoten vorhanden");
    assert_eq!(node.label, "logsentry Dashboard");
    assert_eq!(node.community_name, "notes/projekte");
    assert_eq!(node.file_type, "document");
}
