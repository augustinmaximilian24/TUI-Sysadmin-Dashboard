//! Baut aus dem Zustand die Ausgabe im `graph.json`-Format (NetworkX
//! `node_link_data`, wie es die GUI bereits liest).
//!
//! Drei Kantenquellen, in dieser Priorität (bei Erreichen von `max_edges`
//! fällt die schwächste Quelle zuerst weg):
//! 1. Explizite Links -> `confidence = "EXTRACTED"`
//! 2. KI-Verknüpfungen -> `confidence = "INFERRED"`
//! 3. Gemeinsame seltene Begriffe/Tags -> `confidence = "INFERRED"`
//!
//! Pro Dateipaar entsteht höchstens eine Kante (die GUI zeichnet
//! ungerichtet); die stärkere Quelle gewinnt.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::extract::LinkRef;
use crate::state::State;

/// Präfix der Knoten-IDs, damit sie nie mit IDs anderer Quellen kollidieren.
pub const NODE_ID_PREFIX: &str = "graphsync:";

/// Parameter des Graph-Aufbaus (Ausschnitt aus der Konfiguration).
#[derive(Debug, Clone)]
pub struct BuildParams {
    pub extensions: Vec<String>,
    pub max_term_docs: usize,
    pub min_shared_score: u32,
    pub max_inferred_per_file: usize,
    pub max_edges: usize,
}

/// Ausgabe-Wurzelobjekt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutGraph {
    pub directed: bool,
    pub multigraph: bool,
    pub nodes: Vec<OutNode>,
    pub links: Vec<OutEdge>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutNode {
    pub id: String,
    pub label: String,
    pub file_type: String,
    pub community: i64,
    pub community_name: String,
    pub source_file: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutEdge {
    pub source: String,
    pub target: String,
    pub relation: String,
    pub confidence: String,
    /// Herkunft der Kante (`links`, `llm`, `terms`) -- nur zur Nachvollziehbarkeit.
    pub origin: String,
}

/// Erzeugt die Ausgabe aus dem Zustand. `roots` sind die (expandierten)
/// Quellwurzeln in Konfigurationsreihenfolge; sie bestimmen die Gruppen.
pub fn build(state: &State, roots: &[PathBuf], params: &BuildParams) -> OutGraph {
    let paths: Vec<&String> = state.files.keys().collect();
    let index = Index::new(&paths, &params.extensions);
    let ids: Vec<String> = paths
        .iter()
        .map(|p| format!("{NODE_ID_PREFIX}{p}"))
        .collect();

    let mut edges: Vec<OutEdge> = Vec::new();
    let mut pairs: HashSet<(usize, usize)> = HashSet::new();
    let mut push = |a: usize,
                    b: usize,
                    relation: &str,
                    confidence: &str,
                    origin: &str,
                    edges: &mut Vec<OutEdge>| {
        if a == b || edges.len() >= params.max_edges || !pairs.insert((a.min(b), a.max(b))) {
            return false;
        }
        edges.push(OutEdge {
            source: ids[a].clone(),
            target: ids[b].clone(),
            relation: relation.to_string(),
            confidence: confidence.to_string(),
            origin: origin.to_string(),
        });
        true
    };

    // 1. Explizite Links.
    for (a, record) in state.files.values().enumerate() {
        for link in &record.links {
            if let Some(b) = index.resolve(a, link) {
                push(a, b, "links_to", "EXTRACTED", "links", &mut edges);
            }
        }
    }

    // 2. KI-Verknüpfungen.
    for (a, record) in state.files.values().enumerate() {
        for edge in &record.llm_edges {
            if let Some(&b) = index.by_path.get(edge.target.as_str()) {
                push(a, b, &edge.relation, "INFERRED", "llm", &mut edges);
            }
        }
    }

    // 3. Gemeinsame seltene Begriffe.
    let mut capacity = vec![params.max_inferred_per_file; paths.len()];
    for (a, b, tag_shared) in term_candidates(state, params) {
        if capacity[a] == 0 || capacity[b] == 0 {
            continue;
        }
        let relation = if tag_shared {
            "shares_tag"
        } else {
            "shares_topic"
        };
        if push(a, b, relation, "INFERRED", "terms", &mut edges) {
            capacity[a] -= 1;
            capacity[b] -= 1;
        }
    }

    let (community, names) = communities(&paths, roots);
    let nodes = state
        .files
        .iter()
        .enumerate()
        .map(|(i, (path, record))| OutNode {
            id: ids[i].clone(),
            label: record.title.clone().unwrap_or_else(|| file_stem(path)),
            file_type: "document".to_string(),
            community: community[i],
            community_name: names[i].clone(),
            source_file: path.clone(),
        })
        .collect();

    OutGraph {
        directed: false,
        multigraph: false,
        nodes,
        links: edges,
    }
}

/// Paare mit genügend gemeinsamen seltenen Begriffen, nach Punktzahl
/// absteigend (deterministisch). Tag = 2 Punkte, Überschriftenwort = 1.
fn term_candidates(state: &State, params: &BuildParams) -> Vec<(usize, usize, bool)> {
    let mut docs_by_term: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, record) in state.files.values().enumerate() {
        for term in &record.terms {
            docs_by_term.entry(term.as_str()).or_default().push(i);
        }
    }
    let mut scores: HashMap<(usize, usize), (u32, bool)> = HashMap::new();
    for (term, docs) in &docs_by_term {
        // Zu allgemeine Begriffe ignorieren; begrenzt zugleich die
        // Paaranzahl auf max_term_docs² / 2 je Begriff.
        if docs.len() < 2 || docs.len() > params.max_term_docs {
            continue;
        }
        let is_tag = term.starts_with('#');
        let weight = if is_tag { 2 } else { 1 };
        for (k, &a) in docs.iter().enumerate() {
            for &b in &docs[k + 1..] {
                let entry = scores.entry((a, b)).or_insert((0, false));
                entry.0 += weight;
                entry.1 |= is_tag;
            }
        }
    }
    let mut out: Vec<((usize, usize), (u32, bool))> = scores
        .into_iter()
        .filter(|(_, (score, _))| *score >= params.min_shared_score)
        .collect();
    out.sort_by(|x, y| y.1 .0.cmp(&x.1 .0).then(x.0.cmp(&y.0)));
    out.into_iter()
        .map(|((a, b), (_, tag))| (a, b, tag))
        .collect()
}

/// Gruppe je Datei: erster Ordner unterhalb der Quellwurzel, sonst die
/// Wurzel selbst. Rückgabe: (Community-ID, Name) je Datei.
fn communities(paths: &[&String], roots: &[PathBuf]) -> (Vec<i64>, Vec<String>) {
    let keys: Vec<String> = paths
        .iter()
        .map(|p| {
            let path = Path::new(p.as_str());
            let root = roots
                .iter()
                .filter(|r| path.starts_with(r))
                .max_by_key(|r| r.components().count());
            match root {
                Some(root) => {
                    let rest = path.strip_prefix(root).unwrap_or(path);
                    let mut comps = rest.components();
                    let first = comps.next();
                    let root_name = root.file_name().map_or_else(
                        || root.display().to_string(),
                        |n| n.to_string_lossy().into_owned(),
                    );
                    match (first, comps.next()) {
                        (Some(dir), Some(_)) => {
                            format!("{root_name}/{}", dir.as_os_str().to_string_lossy())
                        }
                        _ => root_name,
                    }
                }
                None => path
                    .parent()
                    .map_or_else(String::new, |p| p.display().to_string()),
            }
        })
        .collect();
    let mut distinct: Vec<&String> = keys.iter().collect();
    distinct.sort();
    distinct.dedup();
    let ids: HashMap<&String, i64> = distinct
        .iter()
        .enumerate()
        .map(|(i, k)| (*k, i64::try_from(i).unwrap_or(i64::MAX)))
        .collect();
    let community = keys
        .iter()
        .map(|k| ids.get(k).copied().unwrap_or(0))
        .collect();
    (community, keys)
}

fn file_stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map_or_else(|| path.to_string(), |s| s.to_string_lossy().into_owned())
}

/// Nachschlagetabellen für die Link-Auflösung.
struct Index<'a> {
    paths: &'a [&'a String],
    by_path: HashMap<&'a str, usize>,
    /// Kleingeschriebener Dateiname ohne Endung -> Dateien.
    by_stem: HashMap<String, Vec<usize>>,
    extensions: &'a [String],
}

impl<'a> Index<'a> {
    fn new(paths: &'a [&'a String], extensions: &'a [String]) -> Self {
        let by_path = paths
            .iter()
            .enumerate()
            .map(|(i, p)| (p.as_str(), i))
            .collect();
        let mut by_stem: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, p) in paths.iter().enumerate() {
            by_stem
                .entry(file_stem(p).to_lowercase())
                .or_default()
                .push(i);
        }
        Self {
            paths,
            by_path,
            by_stem,
            extensions,
        }
    }

    fn resolve(&self, from: usize, link: &LinkRef) -> Option<usize> {
        match link {
            LinkRef::Wiki(target) => self.resolve_wiki(from, target),
            LinkRef::Path(target) => self.resolve_path(from, target),
        }
    }

    /// `[[name]]` oder `[[ordner/name]]`: über den Dateinamen; bei
    /// mehreren Treffern gewinnt derselbe Ordner, dann der kürzeste Pfad.
    fn resolve_wiki(&self, from: usize, target: &str) -> Option<usize> {
        let target = self.strip_known_extension(target.trim()).to_lowercase();
        let stem = target.rsplit('/').next()?;
        let candidates = self.by_stem.get(stem)?;
        let suffix = format!("/{target}");
        let matching: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&i| {
                !target.contains('/')
                    || self
                        .strip_known_extension(self.paths[i])
                        .to_lowercase()
                        .ends_with(&suffix)
            })
            .collect();
        let from_dir = Path::new(self.paths[from].as_str()).parent();
        matching
            .iter()
            .copied()
            .find(|&i| Path::new(self.paths[i].as_str()).parent() == from_dir)
            .or_else(|| {
                matching.iter().copied().min_by(|&a, &b| {
                    self.paths[a]
                        .len()
                        .cmp(&self.paths[b].len())
                        .then(a.cmp(&b))
                })
            })
    }

    /// `[text](pfad)`: relativ zur verweisenden Datei (oder absolut),
    /// lexikalisch normalisiert; fehlt die Endung, werden die bekannten
    /// Endungen probiert.
    fn resolve_path(&self, from: usize, target: &str) -> Option<usize> {
        let base = Path::new(self.paths[from].as_str()).parent()?;
        let joined = normalize(&base.join(target));
        let as_str = joined.to_str()?;
        if let Some(&i) = self.by_path.get(as_str) {
            return Some(i);
        }
        if joined.extension().is_none() {
            for ext in self.extensions {
                let with_ext = format!("{as_str}.{ext}");
                if let Some(&i) = self.by_path.get(with_ext.as_str()) {
                    return Some(i);
                }
            }
        }
        None
    }

    fn strip_known_extension<'s>(&self, name: &'s str) -> &'s str {
        for ext in self.extensions {
            if let Some(stripped) = name.strip_suffix(ext.as_str()) {
                if let Some(stripped) = stripped.strip_suffix('.') {
                    return stripped;
                }
            }
        }
        name
    }
}

/// Lexikalische Normalisierung (`.`/`..` auflösen) ohne Dateisystemzugriff.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{FileRecord, LlmEdge};

    fn rec(links: Vec<LinkRef>, terms: &[&str]) -> FileRecord {
        FileRecord {
            root: 0,
            size: 0,
            mtime_ns: 0,
            hash: 0,
            changed_at: 0,
            title: None,
            links,
            terms: terms.iter().map(|t| t.to_string()).collect(),
            llm_hash: None,
            llm_edges: vec![],
        }
    }

    fn params() -> BuildParams {
        BuildParams {
            extensions: vec!["md".into()],
            max_term_docs: 3,
            min_shared_score: 2,
            max_inferred_per_file: 5,
            max_edges: 100,
        }
    }

    fn edge_set(g: &OutGraph) -> Vec<(String, String, String, String)> {
        g.links
            .iter()
            .map(|e| {
                (
                    e.source.trim_start_matches(NODE_ID_PREFIX).to_string(),
                    e.target.trim_start_matches(NODE_ID_PREFIX).to_string(),
                    e.relation.clone(),
                    e.confidence.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn wiki_und_pfad_links_werden_aufgeloest() {
        let mut state = State::default();
        state.files.insert(
            "/n/a.md".into(),
            rec(
                vec![LinkRef::Wiki("B".into()), LinkRef::Path("sub/c".into())],
                &[],
            ),
        );
        state.files.insert("/n/b.md".into(), rec(vec![], &[]));
        state.files.insert(
            "/n/sub/c.md".into(),
            rec(
                vec![
                    LinkRef::Path("../a.md".into()),
                    LinkRef::Wiki("fehlt".into()),
                ],
                &[],
            ),
        );
        let g = build(&state, &[PathBuf::from("/n")], &params());
        assert_eq!(
            edge_set(&g),
            vec![
                (
                    "/n/a.md".into(),
                    "/n/b.md".into(),
                    "links_to".into(),
                    "EXTRACTED".into()
                ),
                (
                    "/n/a.md".into(),
                    "/n/sub/c.md".into(),
                    "links_to".into(),
                    "EXTRACTED".into()
                ),
            ]
        );
    }

    #[test]
    fn mehrdeutiger_wikilink_bevorzugt_denselben_ordner() {
        let mut state = State::default();
        state.files.insert(
            "/n/x/a.md".into(),
            rec(vec![LinkRef::Wiki("notiz".into())], &[]),
        );
        state.files.insert("/n/notiz.md".into(), rec(vec![], &[]));
        state.files.insert("/n/x/Notiz.md".into(), rec(vec![], &[]));
        state.files.insert(
            "/n/y/b.md".into(),
            rec(vec![LinkRef::Wiki("x/notiz".into())], &[]),
        );
        let g = build(&state, &[PathBuf::from("/n")], &params());
        let edges = edge_set(&g);
        assert!(edges.contains(&(
            "/n/x/a.md".into(),
            "/n/x/Notiz.md".into(),
            "links_to".into(),
            "EXTRACTED".into()
        )));
        // Mit Ordnerangabe eindeutig, obwohl /n/notiz.md kürzer wäre.
        assert!(edges.contains(&(
            "/n/y/b.md".into(),
            "/n/x/Notiz.md".into(),
            "links_to".into(),
            "EXTRACTED".into()
        )));
    }

    #[test]
    fn gemeinsame_seltene_tags_ergeben_inferred_kante() {
        let mut state = State::default();
        state
            .files
            .insert("/n/a.md".into(), rec(vec![], &["#nas", "allgemein"]));
        state
            .files
            .insert("/n/b.md".into(), rec(vec![], &["#nas", "allgemein"]));
        state
            .files
            .insert("/n/c.md".into(), rec(vec![], &["allgemein", "einmal"]));
        state
            .files
            .insert("/n/d.md".into(), rec(vec![], &["allgemein"]));
        // "allgemein" steckt in 4 > max_term_docs=3 Dateien und zählt nicht.
        let g = build(&state, &[PathBuf::from("/n")], &params());
        assert_eq!(
            edge_set(&g),
            vec![(
                "/n/a.md".into(),
                "/n/b.md".into(),
                "shares_tag".into(),
                "INFERRED".into()
            )]
        );
    }

    #[test]
    fn explizite_kante_hat_vorrang_vor_begriffen_und_ki() {
        let mut state = State::default();
        let mut a = rec(vec![LinkRef::Wiki("b".into())], &["#x"]);
        a.llm_edges = vec![LlmEdge {
            target: "/n/b.md".into(),
            relation: "same_project".into(),
        }];
        state.files.insert("/n/a.md".into(), a);
        state.files.insert("/n/b.md".into(), rec(vec![], &["#x"]));
        let g = build(&state, &[PathBuf::from("/n")], &params());
        assert_eq!(g.links.len(), 1);
        assert_eq!(g.links[0].confidence, "EXTRACTED");
    }

    #[test]
    fn ki_kanten_zu_unbekannten_zielen_entfallen() {
        let mut state = State::default();
        let mut a = rec(vec![], &[]);
        a.llm_edges = vec![
            LlmEdge {
                target: "/n/b.md".into(),
                relation: "depends_on".into(),
            },
            LlmEdge {
                target: "/n/geloescht.md".into(),
                relation: "x".into(),
            },
        ];
        state.files.insert("/n/a.md".into(), a);
        state.files.insert("/n/b.md".into(), rec(vec![], &[]));
        let g = build(&state, &[PathBuf::from("/n")], &params());
        assert_eq!(
            edge_set(&g),
            vec![(
                "/n/a.md".into(),
                "/n/b.md".into(),
                "depends_on".into(),
                "INFERRED".into()
            )]
        );
    }

    #[test]
    fn kantenobergrenze_und_kapazitaet_je_datei() {
        let mut state = State::default();
        for i in 0..6 {
            state
                .files
                .insert(format!("/n/{i}.md"), rec(vec![], &["#gemeinsam"]));
        }
        let mut p = params();
        p.max_term_docs = 10;
        p.max_inferred_per_file = 1;
        let g = build(&state, &[PathBuf::from("/n")], &p);
        assert_eq!(g.links.len(), 3, "jede Datei höchstens eine Begriffskante");
        p.max_inferred_per_file = 5;
        p.max_edges = 4;
        assert_eq!(build(&state, &[PathBuf::from("/n")], &p).links.len(), 4);
    }

    #[test]
    fn gruppen_nach_erstem_unterordner_und_titel_als_label() {
        let mut state = State::default();
        let mut top = rec(vec![], &[]);
        top.title = Some("Start".into());
        state.files.insert("/n/top.md".into(), top);
        state.files.insert("/n/proj/a.md".into(), rec(vec![], &[]));
        state
            .files
            .insert("/n/proj/tief/b.md".into(), rec(vec![], &[]));
        let g = build(&state, &[PathBuf::from("/n")], &params());
        let by_file: HashMap<&str, &OutNode> = g
            .nodes
            .iter()
            .map(|n| (n.source_file.as_str(), n))
            .collect();
        assert_eq!(by_file["/n/top.md"].label, "Start");
        assert_eq!(by_file["/n/top.md"].community_name, "n");
        assert_eq!(by_file["/n/proj/a.md"].community_name, "n/proj");
        assert_eq!(
            by_file["/n/proj/a.md"].community,
            by_file["/n/proj/tief/b.md"].community
        );
        assert_eq!(by_file["/n/proj/a.md"].label, "a");
    }
}
