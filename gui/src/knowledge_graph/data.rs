//! Lädt und parst die von `graphify` erzeugte `graph.json` (siehe
//! `~/.claude/activity-log/graphify-out/graph.json`).
//!
//! `graph.json` ist ein NetworkX-`node_link_data`-Export: Kanten liegen
//! unter dem Schlüssel `links`, nicht `edges`. `relation` und `confidence`
//! werden bewusst als `String` statt als geschlossenes `enum` geparst --
//! graphify erweitert dieses Vokabular fortlaufend (z. B. `AGGREGATED` bei
//! Meta-Kanten), ein unbekannter Wert soll das Laden nicht scheitern lassen
//! (dasselbe Prinzip wie `ActionsConfig::allowed_kinds`, siehe
//! `logsentry_core::config`).

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// Fehler beim Laden oder Parsen von `graph.json`.
#[derive(Debug, Error)]
pub enum LoadError {
    #[error("graph.json konnte nicht gelesen werden: {0}")]
    Read(#[from] std::io::Error),
    #[error("graph.json konnte nicht geparst werden: {0}")]
    Parse(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub file_type: String,
    #[serde(default)]
    pub community: i64,
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub source_file: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub relation: String,
    #[serde(default)]
    pub confidence: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphHyperedge {
    #[serde(default)]
    pub label: String,
    pub nodes: Vec<String>,
}

/// Entspricht dem Wurzelobjekt von `graph.json`. `directed`, `multigraph`
/// und das verschachtelte `graph`-Objekt (dessen `hyperedges` laut
/// graphify-Quellcode byte-identisch zum Top-Level-Feld ist) werden nicht
/// gebraucht und von `serde_json` automatisch ignoriert.
#[derive(Debug, Clone, Deserialize)]
pub struct GraphJson {
    #[serde(default)]
    pub nodes: Vec<GraphNode>,
    #[serde(default, rename = "links")]
    pub edges: Vec<GraphEdge>,
    #[serde(default)]
    pub hyperedges: Vec<GraphHyperedge>,
}

impl GraphJson {
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        let raw = std::fs::read_to_string(path)?;
        let parsed: Self = serde_json::from_str(&raw)?;
        Ok(parsed)
    }
}

/// Ersetzt ein führendes `~` durch das übergebene Home-Verzeichnis (analog
/// zu `export_anomalies` in `app.rs`). Rein/parametrisiert statt direkt
/// `std::env::var` zu lesen, damit die Funktion ohne Umgebungsvariablen
/// testbar ist.
pub fn expand_home(path: &str, home: Option<&str>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_ersetzt_tilde() {
        let path = expand_home("~/.claude/graph.json", Some("/home/max"));
        assert_eq!(path, PathBuf::from("/home/max/.claude/graph.json"));
    }

    #[test]
    fn expand_home_laesst_absolute_pfade_unveraendert() {
        let path = expand_home("/etc/logsentry/graph.json", Some("/home/max"));
        assert_eq!(path, PathBuf::from("/etc/logsentry/graph.json"));
    }

    #[test]
    fn expand_home_ohne_home_env_laesst_tilde_stehen() {
        let path = expand_home("~/graph.json", None);
        assert_eq!(path, PathBuf::from("~/graph.json"));
    }

    #[test]
    fn parst_minimalen_graphen() {
        let raw = r#"{
            "directed": false,
            "multigraph": false,
            "graph": {"hyperedges": []},
            "nodes": [
                {"id": "a", "label": "A", "file_type": "concept", "community": 0, "rationale": null, "source_file": "x.md"},
                {"id": "b", "label": "B", "file_type": "document", "community": 1}
            ],
            "links": [
                {"source": "a", "target": "b", "relation": "references", "confidence": "EXTRACTED", "confidence_score": 1.0, "weight": 1.0}
            ],
            "hyperedges": []
        }"#;
        let graph: GraphJson = serde_json::from_str(raw).expect("gueltiges JSON");
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].source, "a");
        assert_eq!(graph.edges[0].target, "b");
        assert_eq!(graph.nodes[1].community, 1);
        assert!(graph.nodes[1].rationale.is_none());
    }

    #[test]
    fn unbekanntes_confidence_vokabular_scheitert_nicht() {
        let raw = r#"{
            "nodes": [],
            "links": [
                {"source": "a", "target": "b", "relation": "future_relation", "confidence": "AGGREGATED"}
            ],
            "hyperedges": []
        }"#;
        let graph: GraphJson = serde_json::from_str(raw).expect("gueltiges JSON");
        assert_eq!(graph.edges[0].confidence, "AGGREGATED");
    }
}
