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

use std::collections::{HashMap, HashSet};
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
    /// Klartextname der Community (von `homegraph.py`/graphify geliefert).
    #[serde(default)]
    pub community_name: Option<String>,
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

/// Entspricht dem Wurzelobjekt von `graph.json`. `directed`, `multigraph`
/// und das verschachtelte `graph`-Objekt (dessen `hyperedges` laut
/// graphify-Quellcode byte-identisch zum Top-Level-Feld ist) werden nicht
/// gebraucht und von `serde_json` automatisch ignoriert.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GraphJson {
    #[serde(default)]
    pub nodes: Vec<GraphNode>,
    #[serde(default, rename = "links")]
    pub edges: Vec<GraphEdge>,
}

impl GraphJson {
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        let raw = std::fs::read_to_string(path)?;
        let parsed: Self = serde_json::from_str(&raw)?;
        Ok(parsed)
    }
}

/// Statistik einer Overlay-Zusammenführung.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeStats {
    /// Overlay-Knoten, die einem bestehenden Knoten zugeordnet wurden.
    pub matched: usize,
    /// Neu hinzugefügte Knoten.
    pub added: usize,
    /// Übernommene Kanten.
    pub edges: usize,
}

/// Mischt ein Overlay (z. B. die Ausgabe von `logsentry-graphsync`) in
/// den Basisgraphen.
///
/// Overlay-Knoten werden über ihren Dateipfad (`source_file`) einem
/// Basisknoten zugeordnet: gleicher Pfad, oder ein relativer Basispfad,
/// auf den der Overlay-Pfad endet. Bei mehreren Treffern (graphify legt
/// mehrere Konzepte pro Datei an) gewinnt der Knoten, dessen Label dem
/// Dateinamen entspricht. Nicht zugeordnete Knoten werden höchstens
/// `max_added_nodes` mal übernommen (0 = keine; das 3D-Layout ist
/// quadratisch in der Knotenzahl, Regel 18) -- bevorzugt die mit den
/// meisten Overlay-Kanten -- mit eigenen Community-IDs oberhalb der
/// bestehenden. Pro Knotenpaar bleibt die Kante des Basisgraphen erhalten.
pub fn merge_overlay(
    base: &mut GraphJson,
    overlay: GraphJson,
    max_added_nodes: usize,
) -> MergeStats {
    let mut stats = MergeStats::default();
    let mut by_file_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, node) in base.nodes.iter().enumerate() {
        for candidate in [node.source_file.as_deref(), Some(node.id.as_str())]
            .into_iter()
            .flatten()
        {
            if let Some(name) = Path::new(candidate).file_name().and_then(|n| n.to_str()) {
                let list = by_file_name.entry(name.to_string()).or_default();
                if !list.contains(&i) {
                    list.push(i);
                }
            }
        }
    }
    let community_offset = base
        .nodes
        .iter()
        .map(|n| n.community)
        .max()
        .map_or(0, |m| m + 1);
    let mut known_ids: HashSet<String> = base.nodes.iter().map(|n| n.id.clone()).collect();

    let mut overlay_degree: HashMap<&str, usize> = HashMap::new();
    for edge in &overlay.edges {
        *overlay_degree.entry(edge.source.as_str()).or_default() += 1;
        *overlay_degree.entry(edge.target.as_str()).or_default() += 1;
    }
    let mut id_map: HashMap<String, String> = HashMap::new();
    let mut unmatched: Vec<(usize, GraphNode)> = Vec::new();
    for node in overlay.nodes {
        let path = node.source_file.clone().unwrap_or_else(|| node.id.clone());
        if let Some(i) = match_base_node(base, &by_file_name, &path) {
            id_map.insert(node.id, base.nodes[i].id.clone());
            stats.matched += 1;
        } else {
            let degree = overlay_degree.get(node.id.as_str()).copied().unwrap_or(0);
            unmatched.push((degree, node));
        }
    }
    // Stabil sortiert: bei gleichem Grad bleibt die Overlay-Reihenfolge.
    unmatched.sort_by_key(|(degree, _)| std::cmp::Reverse(*degree));
    for (_, node) in unmatched {
        if stats.added >= max_added_nodes {
            break;
        }
        if known_ids.insert(node.id.clone()) {
            id_map.insert(node.id.clone(), node.id.clone());
            base.nodes.push(GraphNode {
                community: community_offset + node.community,
                ..node
            });
            stats.added += 1;
        }
    }

    let pair = |a: &str, b: &str| {
        if a <= b {
            (a.to_string(), b.to_string())
        } else {
            (b.to_string(), a.to_string())
        }
    };
    let mut pairs: HashSet<(String, String)> = base
        .edges
        .iter()
        .map(|e| pair(&e.source, &e.target))
        .collect();
    for edge in overlay.edges {
        let (Some(source), Some(target)) = (id_map.get(&edge.source), id_map.get(&edge.target))
        else {
            continue;
        };
        if source == target || !pairs.insert(pair(source, target)) {
            continue;
        }
        base.edges.push(GraphEdge {
            source: source.clone(),
            target: target.clone(),
            ..edge
        });
        stats.edges += 1;
    }
    stats
}

fn match_base_node(
    base: &GraphJson,
    by_file_name: &HashMap<String, Vec<usize>>,
    path: &str,
) -> Option<usize> {
    let file = Path::new(path);
    let name = file.file_name()?.to_str()?;
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let matches_path = |candidate: &str| {
        candidate == path
            || (!candidate.starts_with('/') && path.ends_with(&format!("/{candidate}")))
    };
    let candidates: Vec<usize> = by_file_name
        .get(name)?
        .iter()
        .copied()
        .filter(|&i| {
            let node = &base.nodes[i];
            node.source_file.as_deref().is_some_and(matches_path) || matches_path(&node.id)
        })
        .collect();
    candidates
        .iter()
        .copied()
        .find(|&i| {
            let label = base.nodes[i].label.to_lowercase();
            label == name.to_lowercase() || label == stem.to_lowercase()
        })
        .or_else(|| candidates.first().copied())
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

    fn node(id: &str, label: &str, community: i64, source: Option<&str>) -> GraphNode {
        GraphNode {
            id: id.into(),
            label: label.into(),
            file_type: String::new(),
            community,
            community_name: None,
            rationale: None,
            source_file: source.map(Into::into),
        }
    }

    fn edge(a: &str, b: &str, confidence: &str) -> GraphEdge {
        GraphEdge {
            source: a.into(),
            target: b.into(),
            relation: "r".into(),
            confidence: confidence.into(),
        }
    }

    #[test]
    fn overlay_ordnet_ueber_pfad_zu_und_ergaenzt_fehlende_knoten() {
        let mut base = GraphJson {
            nodes: vec![
                node("konzept", "Irgendwas", 0, Some("notes/a.md")),
                node("a_md", "a", 0, Some("notes/a.md")),
                node("/home/max/notes/b.md", "B", 3, None),
            ],
            edges: vec![edge("a_md", "/home/max/notes/b.md", "EXTRACTED")],
        };
        let overlay = GraphJson {
            nodes: vec![
                node("gs:a", "A", 0, Some("/home/max/notes/a.md")),
                node("gs:b", "B", 0, Some("/home/max/notes/b.md")),
                node("gs:c", "C", 1, Some("/home/max/notes/c.md")),
                node("gs:other", "a", 1, Some("/anders/a.md")),
            ],
            edges: vec![
                edge("gs:a", "gs:b", "INFERRED"),
                edge("gs:a", "gs:c", "INFERRED"),
                edge("gs:c", "gs:fehlt", "INFERRED"),
            ],
        };
        let stats = merge_overlay(&mut base, overlay, 10);
        assert_eq!(
            stats,
            MergeStats {
                matched: 2,
                added: 2,
                edges: 1
            }
        );
        // a.md -> Knoten mit passendem Label, nicht das erste Konzept.
        assert_eq!(base.edges[1].source, "a_md");
        assert_eq!(base.edges[1].target, "gs:c");
        assert_eq!(base.edges[1].confidence, "INFERRED");
        // Basis-Kante a<->b bleibt EXTRACTED, keine Doppelung.
        assert_eq!(base.edges.len(), 2);
        // Neue Communities liegen oberhalb der bestehenden (max 3).
        let c = base.nodes.iter().find(|n| n.id == "gs:c").expect("c");
        assert_eq!(c.community, 5);
    }

    #[test]
    fn overlay_ohne_neue_knoten_verbindet_nur_bestehende() {
        let mut base = GraphJson {
            nodes: vec![node("/n/a.md", "a", 0, None), node("/n/b.md", "b", 0, None)],
            edges: vec![],
        };
        let overlay = GraphJson {
            nodes: vec![
                node("gs:a", "a", 0, Some("/n/a.md")),
                node("gs:b", "b", 0, Some("/n/b.md")),
                node("gs:c", "c", 0, Some("/n/c.md")),
            ],
            edges: vec![
                edge("gs:a", "gs:b", "EXTRACTED"),
                edge("gs:b", "gs:c", "INFERRED"),
            ],
        };
        let stats = merge_overlay(&mut base, overlay, 0);
        assert_eq!(
            stats,
            MergeStats {
                matched: 2,
                added: 0,
                edges: 1
            }
        );
        assert_eq!(base.nodes.len(), 2);
    }

    #[test]
    fn neue_knoten_sind_begrenzt_und_vernetzte_haben_vorrang() {
        let mut base = GraphJson::default();
        let overlay = GraphJson {
            nodes: vec![
                node("einsam", "E", 0, Some("/n/e.md")),
                node("x", "X", 0, Some("/n/x.md")),
                node("y", "Y", 0, Some("/n/y.md")),
            ],
            edges: vec![edge("x", "y", "EXTRACTED")],
        };
        let stats = merge_overlay(&mut base, overlay, 2);
        assert_eq!(
            stats,
            MergeStats {
                matched: 0,
                added: 2,
                edges: 1
            }
        );
        let ids: Vec<&str> = base.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["x", "y"]);
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
