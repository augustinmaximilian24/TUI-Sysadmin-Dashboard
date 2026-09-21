//! Wissensgraph-Tab (Phase 11, optional): zeigt die von `graphify`
//! erzeugte `graph.json` als langsam rotierende, interaktive 3D-Ansicht
//! direkt in der GUI, statt dass dafür `graph.html` im Browser geöffnet
//! werden muss.
//!
//! Architektur (siehe auch [`camera`], [`layout`], [`data`]):
//! - Ein Hintergrund-`std::thread` pollt die `mtime` von `graph_json_path`
//!   (Regel 21: nie auf I/O im Render-Thread blockieren) und lädt bei einer
//!   Änderung -- z. B. durch ein `graphify --update` -- neu, inklusive
//!   Neuberechnung des 3D-Layouts. Ergebnis landet in `Arc<Mutex<Shared>>`,
//!   danach `ctx.request_repaint()` (dasselbe Muster wie
//!   `client::spawn_bridge`/`app::export_anomalies`).
//! - `show()` läuft auf dem Render-Thread, klont nur den aktuellen
//!   `Shared`-Zustand (kleine Datenmenge, siehe `LoadedGraph`) und
//!   zeichnet daraus jeden Frame neu -- reine Projektion, keine
//!   Neuberechnung des Layouts.
//! - Automatische Rotation läuft über `ctx.request_repaint_after` mit
//!   fester Zielrate (Regel 20), nicht im Continuous-Modus.

mod camera;
mod data;
mod layout;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, Color32, Pos2, Sense, Stroke};

use camera::{project_point, Camera, Projected};
use data::{GraphHyperedge, GraphJson, GraphNode};
use layout::{layout_3d, LayoutParams, Vec3};

use logsentry_core::config::KnowledgeGraphConfig;

/// Zielrate für die automatische Rotation (Regel 20: reaktiv statt
/// Continuous-Modus, aber genug für eine ruckelfreie Drehung).
const REPAINT_INTERVAL: Duration = Duration::from_millis(120);

/// Tableau-10-artige Palette, identisch zur Farbwahl in graphify's
/// `graph.html` (`COMMUNITY_COLORS`), damit dieselbe Community in Tab und
/// Browser-Ansicht dieselbe Farbe hat.
const COMMUNITY_COLORS: [Color32; 8] = [
    Color32::from_rgb(0x4E, 0x79, 0xA7),
    Color32::from_rgb(0xF2, 0x8E, 0x2B),
    Color32::from_rgb(0xE1, 0x57, 0x59),
    Color32::from_rgb(0x76, 0xB7, 0xB2),
    Color32::from_rgb(0x59, 0xA1, 0x4F),
    Color32::from_rgb(0xED, 0xC9, 0x48),
    Color32::from_rgb(0xB0, 0x7A, 0xA1),
    Color32::from_rgb(0xFF, 0x9D, 0xA7),
];

fn community_color(community: i64) -> Color32 {
    let idx = community.rem_euclid(COMMUNITY_COLORS.len() as i64) as usize;
    COMMUNITY_COLORS[idx]
}

struct ResolvedEdge {
    a: usize,
    b: usize,
    relation: String,
    confidence: String,
}

struct ResolvedHyperedge {
    label: String,
    members: Vec<usize>,
}

/// Vollständig geladener und layouteter Graph -- alles, was `show()` zum
/// Zeichnen eines Frames braucht, ohne erneut JSON zu parsen oder das
/// Layout neu zu berechnen.
struct LoadedGraph {
    nodes: Vec<GraphNode>,
    positions: Vec<Vec3>,
    edges: Vec<ResolvedEdge>,
    hyperedges: Vec<ResolvedHyperedge>,
    degree: Vec<u32>,
    max_degree: u32,
    community_labels: HashMap<i64, String>,
}

#[derive(Default)]
struct Shared {
    /// `Arc`, damit `show()` den aktuellen Graphen unter kurzem Halten des
    /// Mutex nur klonen (Refcount, keine Kopie der Knoten-/Kantenlisten)
    /// und danach ohne gehaltene Sperre zeichnen kann.
    graph: Option<Arc<LoadedGraph>>,
    error: Option<String>,
}

/// Liest `.graphify_labels.json` (Community-ID -> Klartext-Label) aus
/// demselben Verzeichnis wie `graph.json`, falls vorhanden. Fehlt die Datei
/// oder ist sie nicht lesbar, ist das kein Fehler (Regel 16) -- die
/// Community wird dann nur mit ihrer Nummer angezeigt.
fn load_community_labels(graph_dir: &Path) -> HashMap<i64, String> {
    let path = graph_dir.join(".graphify_labels.json");
    let Ok(raw) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&raw) else {
        return HashMap::new();
    };
    map.into_iter()
        .filter_map(|(key, value)| key.parse::<i64>().ok().map(|id| (id, value)))
        .collect()
}

fn load_and_layout(path: &Path, iterations: usize) -> Result<LoadedGraph, data::LoadError> {
    let raw = GraphJson::load(path)?;
    let id_to_index: HashMap<&str, usize> = raw
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.as_str(), i))
        .collect();

    let edges: Vec<ResolvedEdge> = raw
        .edges
        .iter()
        .filter_map(|e| {
            let a = *id_to_index.get(e.source.as_str())?;
            let b = *id_to_index.get(e.target.as_str())?;
            Some(ResolvedEdge {
                a,
                b,
                relation: e.relation.clone(),
                confidence: e.confidence.clone(),
            })
        })
        .collect();

    let mut degree = vec![0u32; raw.nodes.len()];
    for edge in &edges {
        degree[edge.a] += 1;
        degree[edge.b] += 1;
    }
    let max_degree = degree.iter().copied().max().unwrap_or(1).max(1);

    let edge_pairs: Vec<(usize, usize)> = edges.iter().map(|e| (e.a, e.b)).collect();
    let params = LayoutParams::with_iterations(iterations);
    let positions = layout_3d(raw.nodes.len(), &edge_pairs, &params);

    let hyperedges: Vec<ResolvedHyperedge> = raw
        .hyperedges
        .iter()
        .filter_map(resolve_hyperedge(&id_to_index))
        .collect();

    let community_labels = path.parent().map(load_community_labels).unwrap_or_default();

    Ok(LoadedGraph {
        nodes: raw.nodes,
        positions,
        edges,
        hyperedges,
        degree,
        max_degree,
        community_labels,
    })
}

fn resolve_hyperedge<'a>(
    id_to_index: &'a HashMap<&'a str, usize>,
) -> impl Fn(&GraphHyperedge) -> Option<ResolvedHyperedge> + 'a {
    |h: &GraphHyperedge| {
        let members: Vec<usize> = h
            .nodes
            .iter()
            .filter_map(|id| id_to_index.get(id.as_str()).copied())
            .collect();
        if members.len() >= 3 {
            Some(ResolvedHyperedge {
                label: h.label.clone(),
                members,
            })
        } else {
            None
        }
    }
}

fn spawn_watcher(
    path: PathBuf,
    iterations: usize,
    poll_interval: Duration,
    shared: Arc<Mutex<Shared>>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let mut last_mtime: Option<SystemTime> = None;
        let mut first = true;
        loop {
            let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            if first || mtime != last_mtime {
                first = false;
                last_mtime = mtime;
                let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
                match load_and_layout(&path, iterations) {
                    Ok(loaded) => {
                        guard.graph = Some(Arc::new(loaded));
                        guard.error = None;
                    }
                    Err(err) => {
                        guard.error = Some(err.to_string());
                    }
                }
                drop(guard);
                ctx.request_repaint();
            }
            std::thread::sleep(poll_interval);
        }
    });
}

/// Zustand des Wissensgraph-Tabs: Kamera, Interaktion und der geteilte
/// Zustand, den der Hintergrund-Thread befüllt.
pub struct KnowledgeGraphTab {
    shared: Arc<Mutex<Shared>>,
    camera: Camera,
    last_interaction: Instant,
    last_frame: Instant,
    selected_node: Option<usize>,
    rotation_degrees_per_sec: f32,
    idle_resume_secs: f32,
    drag_sensitivity_deg_per_px: f32,
}

impl KnowledgeGraphTab {
    /// Startet den Hintergrund-Watcher und liefert den Tab-Zustand.
    /// `~` in `config.graph_json_path` wird über `$HOME` aufgelöst (siehe
    /// [`data::expand_home`]) -- fehlt `$HOME`, bleibt der Pfad wörtlich
    /// stehen und der Tab zeigt beim ersten Ladeversuch einen Lesefehler
    /// statt abzustürzen.
    pub fn new(config: &KnowledgeGraphConfig, ctx: &egui::Context) -> Self {
        let home = std::env::var("HOME").ok();
        let path = data::expand_home(&config.graph_json_path, home.as_deref());
        let shared = Arc::new(Mutex::new(Shared::default()));
        spawn_watcher(
            path,
            config.layout_iterations,
            Duration::from_secs(config.poll_interval_secs.max(1)),
            Arc::clone(&shared),
            ctx.clone(),
        );
        Self {
            shared,
            camera: Camera::new(420.0, 700.0),
            last_interaction: Instant::now(),
            last_frame: Instant::now(),
            selected_node: None,
            rotation_degrees_per_sec: config.rotation_degrees_per_sec,
            idle_resume_secs: config.idle_resume_secs.max(0.0),
            drag_sensitivity_deg_per_px: config.drag_sensitivity_deg_per_px,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;

        let (error, current) = {
            let guard = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
            (guard.error.clone(), guard.graph.clone())
        };

        if let Some(err) = error {
            ui.colored_label(
                crate::theme::LEVEL_WARN,
                format!("Wissensgraph konnte nicht geladen werden: {err}"),
            );
        }
        let Some(graph) = current else {
            ui.label(
                egui::RichText::new("Noch kein Graph geladen (warte auf graph.json) …")
                    .color(crate::theme::TEXT_MUTED),
            );
            ui.ctx().request_repaint_after(REPAINT_INTERVAL);
            return;
        };

        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());

        if response.dragged() {
            let delta = response.drag_delta();
            self.camera
                .apply_drag(delta.x, delta.y, self.drag_sensitivity_deg_per_px);
            self.last_interaction = now;
        }

        // Scrollen über dem Graphen zoomt (Kamera-Abstand), begrenzt auf
        // einen sinnvollen Bereich, damit man weder in den Ursprung
        // hineinzoomen noch den Graphen zu einem Punkt schrumpfen lassen
        // kann.
        if response.hovered() {
            let scroll = ui.ctx().input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > f32::EPSILON {
                self.camera.distance = (self.camera.distance - scroll).clamp(150.0, 2000.0);
                self.last_interaction = now;
            }
        }

        let idle_for = now.duration_since(self.last_interaction).as_secs_f32();
        if idle_for >= self.idle_resume_secs {
            self.camera
                .advance_auto_rotation(self.rotation_degrees_per_sec, dt);
        }

        self.draw_graph(ui, rect, &response, &graph);

        // Kontinuierliche, aber gedrosselte Aktualisierung für die
        // Rotation (Regel 20: feste Zielrate statt Continuous-Modus).
        ui.ctx().request_repaint_after(REPAINT_INTERVAL);
    }

    fn draw_graph(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        response: &egui::Response,
        graph: &LoadedGraph,
    ) {
        let painter = ui.painter_at(rect);
        let center = rect.center();

        let projected: Vec<Option<Projected>> = graph
            .positions
            .iter()
            .map(|p| project_point(*p, &self.camera))
            .collect();

        let to_screen = |p: &Projected| Pos2::new(center.x + p.x, center.y - p.y);

        // Hyperkanten zuerst als transluzente konvexe Hülle über die
        // aktuell sichtbaren Mitglieder -- dieselbe visuelle Idee wie
        // graphify's `graph.html` (dort per Canvas-Overlay über die
        // Live-Pixelpositionen).
        for hyperedge in &graph.hyperedges {
            let points: Vec<Pos2> = hyperedge
                .members
                .iter()
                .filter_map(|&idx| projected.get(idx).and_then(|p| p.as_ref()))
                .map(to_screen)
                .collect();
            if points.len() < 3 {
                continue;
            }
            let hull = convex_hull(&points);
            if hull.len() >= 3 {
                let centroid =
                    hull.iter().fold(Pos2::ZERO, |acc, p| acc + p.to_vec2()) / hull.len() as f32;
                painter.add(egui::Shape::convex_polygon(
                    hull,
                    Color32::from_rgba_unmultiplied(0x63, 0x66, 0xF1, 24),
                    Stroke::new(
                        1.0_f32,
                        Color32::from_rgba_unmultiplied(0x63, 0x66, 0xF1, 90),
                    ),
                ));
                painter.text(
                    centroid,
                    egui::Align2::CENTER_CENTER,
                    &hyperedge.label,
                    egui::FontId::proportional(11.0),
                    Color32::from_rgba_unmultiplied(0xC7, 0xC9, 0xFF, 180),
                );
            }
        }

        // Kanten nach Tiefe sortiert (grob, per Mittelpunkt) zeichnen,
        // damit nähere Kanten weiter entfernte optisch überdecken.
        let mut edge_order: Vec<usize> = (0..graph.edges.len()).collect();
        edge_order.sort_by(|&i, &j| {
            let depth = |idx: usize| -> f32 {
                let e = &graph.edges[idx];
                let da = projected[e.a].as_ref().map_or(f32::MAX, |p| p.depth);
                let db = projected[e.b].as_ref().map_or(f32::MAX, |p| p.depth);
                (da + db) / 2.0
            };
            depth(j)
                .partial_cmp(&depth(i))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for idx in edge_order {
            let edge = &graph.edges[idx];
            let (Some(pa), Some(pb)) = (&projected[edge.a], &projected[edge.b]) else {
                continue;
            };
            let extracted = edge.confidence == "EXTRACTED";
            let alpha = if extracted { 140 } else { 60 };
            let width: f32 = if extracted { 1.6 } else { 1.0 };
            painter.line_segment(
                [to_screen(pa), to_screen(pb)],
                Stroke::new(width, Color32::from_rgba_unmultiplied(150, 156, 165, alpha)),
            );
        }

        // Knoten nach Tiefe sortiert (fern -> nah) für einfaches
        // Malerprinzip, damit nähere Knoten weiter entfernte verdecken.
        let mut node_order: Vec<usize> = (0..graph.nodes.len()).collect();
        node_order.sort_by(|&i, &j| {
            let da = projected[i].as_ref().map_or(f32::MAX, |p| p.depth);
            let db = projected[j].as_ref().map_or(f32::MAX, |p| p.depth);
            db.partial_cmp(&da).unwrap_or(std::cmp::Ordering::Equal)
        });

        let pointer = response.hover_pos();
        let mut hovered: Option<usize> = None;

        for &idx in &node_order {
            let Some(p) = &projected[idx] else { continue };
            let screen = to_screen(p);
            let perspective_scale = self.camera.distance / p.depth;
            let base_radius = 4.0 + 8.0 * (graph.degree[idx] as f32 / graph.max_degree as f32);
            let radius = (base_radius * perspective_scale).clamp(1.5, 26.0);

            if let Some(pointer) = pointer {
                if pointer.distance(screen) <= radius + 3.0 {
                    hovered = Some(idx);
                }
            }

            let color = community_color(graph.nodes[idx].community);
            let is_selected = self.selected_node == Some(idx);
            painter.circle_filled(screen, radius, color);
            if is_selected || hovered == Some(idx) {
                painter.circle_stroke(screen, radius + 2.0, Stroke::new(2.0_f32, Color32::WHITE));
            }
        }

        if let Some(idx) = hovered {
            if let Some(p) = &projected[idx] {
                let screen = to_screen(p);
                painter.text(
                    screen + egui::vec2(10.0, -10.0),
                    egui::Align2::LEFT_BOTTOM,
                    &graph.nodes[idx].label,
                    egui::FontId::proportional(13.0),
                    Color32::WHITE,
                );
            }
        }

        if response.clicked() {
            self.selected_node = hovered.or(None);
        }

        if let Some(idx) = self.selected_node {
            self.draw_detail_overlay(ui, rect, graph, idx);
        }
    }

    fn draw_detail_overlay(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        graph: &LoadedGraph,
        idx: usize,
    ) {
        let node = &graph.nodes[idx];
        let mut close = false;
        egui::Area::new(egui::Id::new("knowledge_graph_detail"))
            .fixed_pos(rect.left_bottom() + egui::vec2(12.0, -220.0))
            .show(ui.ctx(), |ui| {
                crate::theme::card(ui, |ui| {
                    ui.set_max_width(320.0);
                    ui.horizontal(|ui| {
                        crate::theme::section_heading(ui, &node.label);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                            if ui.small_button("×").clicked() {
                                close = true;
                            }
                        });
                    });
                    ui.add_space(4.0);
                    let community_label = graph
                        .community_labels
                        .get(&node.community)
                        .cloned()
                        .unwrap_or_else(|| format!("Community {}", node.community));
                    ui.label(
                        egui::RichText::new(community_label).color(community_color(node.community)),
                    );
                    ui.label(
                        egui::RichText::new(format!("Typ: {}", node.file_type))
                            .color(crate::theme::TEXT_MUTED)
                            .size(11.0),
                    );
                    if let Some(source) = &node.source_file {
                        ui.label(
                            egui::RichText::new(format!("Quelle: {source}"))
                                .color(crate::theme::TEXT_MUTED)
                                .size(11.0),
                        );
                    }
                    if let Some(rationale) = &node.rationale {
                        ui.add_space(6.0);
                        ui.label(rationale);
                    }

                    let connections: Vec<(bool, &str, &GraphNode)> = graph
                        .edges
                        .iter()
                        .filter_map(|edge| {
                            if edge.a == idx {
                                Some((true, edge.relation.as_str(), &graph.nodes[edge.b]))
                            } else if edge.b == idx {
                                Some((false, edge.relation.as_str(), &graph.nodes[edge.a]))
                            } else {
                                None
                            }
                        })
                        .collect();
                    if !connections.is_empty() {
                        ui.add_space(6.0);
                        crate::theme::section_heading(ui, "Verbindungen");
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .id_salt("knowledge_graph_connections")
                            .max_height(140.0)
                            .show(ui, |ui| {
                                for (outgoing, relation, other) in &connections {
                                    let arrow = if *outgoing { "→" } else { "←" };
                                    ui.label(format!("{arrow} {relation} {arrow} {}", other.label));
                                }
                            });
                    }
                });
            });
        if close {
            self.selected_node = None;
        }
    }
}

/// Konvexe Hülle einer Punktmenge (Andrew's Monotone-Chain, wie im
/// graphify-`graph.html`-Overlay für Hyperkanten). Liefert die Hülle im
/// Uhrzeigersinn; bei weniger als 3 unterschiedlichen Punkten die
/// Eingabe unverändert.
fn convex_hull(points: &[Pos2]) -> Vec<Pos2> {
    let mut pts: Vec<Pos2> = points.to_vec();
    pts.sort_by(|a, b| {
        a.x.partial_cmp(&b.x)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.y.partial_cmp(&b.y).unwrap_or(std::cmp::Ordering::Equal))
    });
    pts.dedup_by(|a, b| (a.x - b.x).abs() < 1e-4 && (a.y - b.y).abs() < 1e-4);
    if pts.len() < 3 {
        return pts;
    }

    fn cross(o: Pos2, a: Pos2, b: Pos2) -> f32 {
        (a.x - o.x) * (b.y - o.y) - (a.y - o.y) * (b.x - o.x)
    }

    let mut lower: Vec<Pos2> = Vec::new();
    for &p in &pts {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Vec<Pos2> = Vec::new();
    for &p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn community_color_wrappt_bei_vielen_communities() {
        assert_eq!(community_color(0), community_color(8));
        assert_eq!(community_color(3), COMMUNITY_COLORS[3]);
    }

    #[test]
    fn load_and_layout_parst_und_layoutet_kleinen_graphen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graph.json");
        std::fs::write(
            &path,
            r#"{
                "nodes": [
                    {"id": "a", "label": "A", "file_type": "concept", "community": 0},
                    {"id": "b", "label": "B", "file_type": "concept", "community": 1},
                    {"id": "c", "label": "C", "file_type": "concept", "community": 1}
                ],
                "links": [
                    {"source": "a", "target": "b", "relation": "references", "confidence": "EXTRACTED"},
                    {"source": "b", "target": "c", "relation": "references", "confidence": "INFERRED"}
                ],
                "hyperedges": [
                    {"id": "h1", "label": "Alle drei", "nodes": ["a", "b", "c"], "relation": "form"}
                ]
            }"#,
        )
        .expect("schreiben");

        let loaded = load_and_layout(&path, 20).expect("laden");
        assert_eq!(loaded.nodes.len(), 3);
        assert_eq!(loaded.positions.len(), 3);
        assert_eq!(loaded.edges.len(), 2);
        assert_eq!(loaded.degree, vec![1, 2, 1]);
        assert_eq!(loaded.max_degree, 2);
        assert_eq!(loaded.hyperedges.len(), 1);
        assert_eq!(loaded.hyperedges[0].members.len(), 3);
    }

    #[test]
    fn load_and_layout_ignoriert_kanten_mit_unbekannter_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graph.json");
        std::fs::write(
            &path,
            r#"{
                "nodes": [{"id": "a", "label": "A", "file_type": "concept", "community": 0}],
                "links": [{"source": "a", "target": "geistknoten", "relation": "references", "confidence": "EXTRACTED"}],
                "hyperedges": []
            }"#,
        )
        .expect("schreiben");

        let loaded = load_and_layout(&path, 5).expect("laden trotz kaputter Kante");
        assert_eq!(loaded.nodes.len(), 1);
        assert!(loaded.edges.is_empty());
    }

    #[test]
    fn convex_hull_von_quadrat_liefert_vier_ecken() {
        let points = vec![
            Pos2::new(0.0, 0.0),
            Pos2::new(10.0, 0.0),
            Pos2::new(10.0, 10.0),
            Pos2::new(0.0, 10.0),
            Pos2::new(5.0, 5.0), // innerer Punkt, darf nicht auf der Hülle landen
        ];
        let hull = convex_hull(&points);
        assert_eq!(hull.len(), 4);
        assert!(!hull.contains(&Pos2::new(5.0, 5.0)));
    }

    #[test]
    fn convex_hull_mit_weniger_als_drei_punkten_gibt_eingabe_zurueck() {
        let points = vec![Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)];
        assert_eq!(convex_hull(&points), points);
    }
}
