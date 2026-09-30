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
mod holo;
mod layout;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, Color32, Pos2, Sense};

use camera::{project_point, Camera};
use data::{GraphJson, GraphNode};
use layout::{layout_3d_grouped, LayoutParams, Vec3};

use logsentry_core::config::KnowledgeGraphConfig;

/// Zielrate für die automatische Rotation (Regel 20: reaktiv über
/// `request_repaint_after` statt Continuous-Modus). Der Tab zeichnet nur,
/// solange er sichtbar ist -- dann aber dauerhaft eine Drehung/einen Puls,
/// also kein "Idle"-Zustand im Sinne von Regel 20/der Phase-7-CPU-Vorgabe.
/// 120 ms (8.3 Hz) sahen bei stetiger Rotation sichtbar ruckelig aus (unter
/// der für flüssig wahrgenommene Bewegung nötigen Bildrate); 16 ms (~60 Hz)
/// behebt das, ohne den Idle-Zustand des restlichen Dashboards zu berühren.
/// Zoomstärke pro Scroll-Pixel (Faktor = e^(Pixel * Wert)).
const SCROLL_ZOOM_PER_PIXEL: f32 = 0.003;

/// Dauer der Zoom-Animation beim Hineinzoomen in eine Gruppe (Sekunden).
const FOCUS_ANIM_SECS: f32 = 1.2;

/// Grundgeschwindigkeit der Drehphase (rad/s).
const SPIN_SPEED_RAD_PER_SEC: f32 = 0.4;

/// Wrap der Drehphase: 12 volle Umdrehungen. Alle Drehfaktoren (0,25 / 0,5 / 1,0)
/// ergeben darauf ganzzahlige Vielfache von 2π -> kein sichtbarer Sprung.
const SPIN_WRAP: f32 = std::f32::consts::TAU * 12.0;

const REPAINT_INTERVAL: Duration = Duration::from_millis(16);

/// Wie viele konzentrische Ringe ein Knoten-Glow benutzt. Mehr Ringe ergeben
/// einen weicheren Verlauf, kosten aber mehr gezeichnete Shapes pro Frame --
/// bei den paar Dutzend Knoten eines persönlichen Wissensgraphen unkritisch.
const NODE_GLOW_RINGS: usize = 7;

/// Geschwindigkeit des leichten Leucht-Pulses (rad/s). Bewusst langsam und
/// dezent -- soll wie ein ruhig atmendes Leuchten wirken, nicht blinken.
const PULSE_SPEED_RAD_PER_SEC: f32 = 1.1;

/// Zeichnet einen weichen Halo aus konzentrischen, zunehmend transparenten
/// Kreisen um `center` -- eine Bloom-Annäherung ohne echten Blur-Shader
/// (den `egui::Painter` nicht anbietet). `strength` skaliert die
/// Maximalhelligkeit (z. B. höher für Hover/Auswahl/Hub-Knoten).
/// Radius und Deckkraft des `ring_index`-ten (1 = innerster) von
/// `total_rings` Glow-Ringen. Als reine Funktion ausgelagert, damit die
/// Verlaufsform ohne einen `egui::Painter` testbar ist.
fn glow_ring(ring_index: usize, total_rings: usize, base_radius: f32, strength: f32) -> (f32, u8) {
    let t = ring_index as f32 / total_rings as f32;
    let ring_radius = base_radius * (1.0 + 2.6 * t);
    let alpha = (strength * 100.0 * (1.0 - t).powf(1.7)).clamp(0.0, 255.0) as u8;
    (ring_radius, alpha)
}

fn draw_node_glow(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32, strength: f32) {
    for i in (1..=NODE_GLOW_RINGS).rev() {
        let (ring_radius, alpha) = glow_ring(i, NODE_GLOW_RINGS, radius, strength);
        if alpha == 0 {
            continue;
        }
        painter.circle_filled(
            center,
            ring_radius,
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha),
        );
    }
}

/// Mischt zwei Farben zu gleichen Teilen -- für den Kanten-Glow zwischen
/// zwei unterschiedlich gefärbten Communities.
fn blend_color(a: Color32, b: Color32) -> Color32 {
    Color32::from_rgb(
        ((a.r() as u16 + b.r() as u16) / 2) as u8,
        ((a.g() as u16 + b.g() as u16) / 2) as u8,
        ((a.b() as u16 + b.b() as u16) / 2) as u8,
    )
}

struct ResolvedEdge {
    a: usize,
    b: usize,
    relation: String,
    confidence: String,
}

/// Vollständig geladener und layouteter Graph -- alles, was `show()` zum
/// Zeichnen eines Frames braucht, ohne erneut JSON zu parsen oder das
/// Layout neu zu berechnen.
struct LoadedGraph {
    nodes: Vec<GraphNode>,
    positions: Vec<Vec3>,
    edges: Vec<ResolvedEdge>,
    degree: Vec<u32>,
    max_degree: u32,
    /// Gruppen (= Communities, z. B. Projektordner) mit Schwerpunkt.
    groups: Vec<Group>,
    /// Gruppen-Index je Knoten.
    group_of: Vec<usize>,
    /// Zusammengefasste Kanten zwischen verschiedenen Gruppen
    /// `(gruppe_a, gruppe_b, anzahl)` mit `gruppe_a < gruppe_b`.
    group_edges: Vec<(usize, usize, u32)>,
    /// Grobe Ausdehnung des Graphen (größter Abstand Schwerpunkt + Radius).
    extent: f32,
}

/// Eine Community als "Ordner": Mitglieder, Schwerpunkt im Layout-Raum
/// und Radius der Wolke.
struct Group {
    label: String,
    members: Vec<usize>,
    centroid: Vec3,
    radius: f32,
}

/// Kürzt Pfad-/Langnamen für Beschriftungen: letzter Pfadteil, max. 26 Zeichen.
fn short_label(raw: &str) -> String {
    let last = raw.rsplit('/').next().unwrap_or(raw);
    if last.chars().count() > 26 {
        last.chars().take(25).collect::<String>() + "…"
    } else {
        last.to_string()
    }
}

/// Zieht die Gruppen-Wolken auseinander und macht sie kompakter: Schwerpunkte
/// um `GROUP_SPREAD` weiter auseinander, Mitglieder näher an ihrem
/// Schwerpunkt (`GROUP_COMPACT`) -- so bleiben die "Ordner" in der
/// Übersicht klar getrennt und lesbar.
fn spread_groups(positions: &mut [Vec3], group_of: &[usize], group_count: usize) {
    let mut sums = vec![Vec3::ZERO; group_count];
    let mut counts = vec![0usize; group_count];
    for (p, &g) in positions.iter().zip(group_of) {
        sums[g] = sums[g].add(*p);
        counts[g] += 1;
    }
    for (p, &g) in positions.iter_mut().zip(group_of) {
        let centroid = sums[g].scale(1.0 / counts[g].max(1) as f32);
        *p = centroid
            .scale(GROUP_SPREAD)
            .add(p.sub(centroid).scale(GROUP_COMPACT));
    }
}

/// Faktor, um den die Gruppen-Schwerpunkte auseinandergezogen werden.
const GROUP_SPREAD: f32 = 1.8;
/// Faktor, um den die Mitglieder zum eigenen Gruppenschwerpunkt rücken.
const GROUP_COMPACT: f32 = 0.7;

fn build_groups(
    nodes: &[GraphNode],
    community_ids: &[i64],
    group_of: &[usize],
    positions: &[Vec3],
    labels: &HashMap<i64, String>,
) -> Vec<Group> {
    community_ids
        .iter()
        .enumerate()
        .map(|(g, &community)| {
            let members: Vec<usize> = (0..nodes.len()).filter(|&i| group_of[i] == g).collect();
            let sum = members.iter().fold(Vec3::ZERO, |acc, &i| acc.add(positions[i]));
            let centroid = sum.scale(1.0 / members.len().max(1) as f32);
            // 90. Perzentil statt Maximum: einzelne Ausreißer sollen weder den
            // Ring noch den Fokus-Zoom bestimmen.
            let mut distances: Vec<f32> = members
                .iter()
                .map(|&i| positions[i].sub(centroid).length())
                .collect();
            distances.sort_unstable_by(f32::total_cmp);
            let radius = distances
                .get(distances.len().saturating_sub(1) * 9 / 10)
                .map_or(12.0, |d| (d * 1.15).max(12.0));
            let label = labels
                .get(&community)
                .cloned()
                .or_else(|| members.iter().find_map(|&i| nodes[i].community_name.clone()))
                .map_or_else(|| format!("Community {community}"), |l| short_label(&l));
            Group {
                label,
                members,
                centroid,
                radius,
            }
        })
        .collect()
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
    let mut community_ids: Vec<i64> = raw.nodes.iter().map(|n| n.community).collect();
    community_ids.sort_unstable();
    community_ids.dedup();
    let group_of: Vec<usize> = raw
        .nodes
        .iter()
        .map(|n| community_ids.binary_search(&n.community).unwrap_or(0))
        .collect();
    let mut positions = layout_3d_grouped(raw.nodes.len(), &edge_pairs, Some(&group_of), &params);
    spread_groups(&mut positions, &group_of, community_ids.len());

    let community_labels = path.parent().map(load_community_labels).unwrap_or_default();
    let groups = build_groups(&raw.nodes, &community_ids, &group_of, &positions, &community_labels);
    let extent = groups
        .iter()
        .map(|g| g.centroid.length() + g.radius)
        .fold(60.0_f32, f32::max);
    let mut aggregated: HashMap<(usize, usize), u32> = HashMap::new();
    for edge in &edges {
        let (ga, gb) = (group_of[edge.a], group_of[edge.b]);
        if ga != gb {
            *aggregated.entry((ga.min(gb), ga.max(gb))).or_insert(0) += 1;
        }
    }
    let mut group_edges: Vec<(usize, usize, u32)> =
        aggregated.into_iter().map(|((a, b), n)| (a, b, n)).collect();
    group_edges.sort_unstable();

    Ok(LoadedGraph {
        nodes: raw.nodes,
        positions,
        edges,
        degree,
        max_degree,
        groups,
        group_of,
        group_edges,
        extent,
    })
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
    /// Phase des dezenten Leucht-Pulses (rad, läuft frei weiter). An `dt`
    /// gekoppelt statt an die Wanduhr, damit sie sich an dieselbe reaktive
    /// Repaint-Rate hält wie die Auto-Rotation.
    pulse_phase: f32,
    /// Kontinuierliche Drehphase für Dekoration und Ringe. Läuft nahtlos:
    /// der Wrap liegt bei einem ganzzahligen Vielfachen aller verwendeten
    /// Drehfaktoren (siehe `SPIN_WRAP`), damit es keinen Sprung gibt.
    spin_phase: f32,
    /// Überschrift im Holo-Panel ("WISSENSGRAPH" / "HOME-ÜBERSICHT").
    title: String,
    /// Aktuell hineingezoomte Gruppe (Index in `LoadedGraph::groups`).
    focus: Option<usize>,
    /// Sekunden seit dem Fokussieren (steuert die Zoom-Animation).
    focus_anim: f32,
    /// Restdauer der Rückkehr-Animation nach dem Verlassen einer Gruppe.
    return_anim: f32,
    /// `extent` des Graphen, auf den Kamera-Abstand und Start-Zoom zuletzt
    /// angepasst wurden (0 = noch nicht).
    fitted_extent: f32,
    /// Start-Zoom, bei dem der ganze Graph sichtbar ist (Ziel der Rückkehr).
    fit_zoom: f32,
}

impl KnowledgeGraphTab {
    /// Setzt die Überschrift des Holo-Panels.
    pub fn with_title(mut self, title: &str) -> Self {
        self.title = title.to_string();
        self
    }

    fn focus_group(&mut self, group: usize) {
        self.focus = Some(group);
        self.focus_anim = 0.0;
        self.selected_node = None;
    }

    fn unfocus(&mut self) {
        if self.focus.take().is_some() {
            self.return_anim = 1.2;
        }
        self.selected_node = None;
    }

    /// Fährt die Kamera weich auf die fokussierte Gruppe (Mitte + Zoom) bzw.
    /// nach dem Verlassen zurück auf die Übersicht.
    fn update_focus_camera(&mut self, graph: &LoadedGraph, rect: egui::Rect, dt: f32, idle: bool) {
        let ease = 1.0 - (-dt * 6.0).exp();
        if let Some(g) = self.focus {
            self.focus_anim += dt;
            let animating = self.focus_anim < FOCUS_ANIM_SECS;
            // Nach der Animation nur nachführen, solange die Ansicht von
            // selbst rotiert -- bei manuellem Zoomen/Verschieben bleibt sie
            // wo der Benutzer sie hingelegt hat.
            if !animating && !idle {
                return;
            }
            let group = &graph.groups[g];
            if let Some(p) = project_point(group.centroid, &self.camera) {
                let zoom = self.camera.zoom;
                self.camera.pan_x += (-p.x * zoom - self.camera.pan_x) * ease;
                self.camera.pan_y += (p.y * zoom - self.camera.pan_y) * ease;
                if animating {
                    let rad_px = (group.radius * self.camera.distance / p.depth).max(8.0);
                    let target = (0.40 * rect.width().min(rect.height()) / rad_px).clamp(1.2, 9.0);
                    self.camera.zoom += (target - self.camera.zoom) * ease;
                }
            }
        } else if self.return_anim > 0.0 {
            self.return_anim -= dt;
            self.camera.zoom += (self.fit_zoom - self.camera.zoom) * ease;
            self.camera.pan_x -= self.camera.pan_x * ease;
            self.camera.pan_y -= self.camera.pan_y * ease;
        }
    }

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
            camera: {
                let mut camera = Camera::new(420.0, 700.0);
                // Leicht von oben geneigt, damit die Drehung räumlich wirkt.
                camera.pitch = 0.3;
                camera
            },
            last_interaction: Instant::now(),
            last_frame: Instant::now(),
            selected_node: None,
            rotation_degrees_per_sec: config.rotation_degrees_per_sec,
            idle_resume_secs: config.idle_resume_secs.max(0.0),
            drag_sensitivity_deg_per_px: config.drag_sensitivity_deg_per_px,
            pulse_phase: 0.0,
            spin_phase: 0.0,
            title: "WISSENSGRAPH".to_string(),
            focus: None,
            focus_anim: 0.0,
            return_anim: 0.0,
            fitted_extent: 0.0,
            fit_zoom: 1.0,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;
        self.pulse_phase = (self.pulse_phase + dt * PULSE_SPEED_RAD_PER_SEC)
            % (std::f32::consts::TAU);
        self.spin_phase = (self.spin_phase + dt * SPIN_SPEED_RAD_PER_SEC) % SPIN_WRAP;

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

        // Kamera einmalig (und nach Graph-Änderungen) so einstellen, dass der
        // ganze Graph in den Zeichenbereich passt.
        if (graph.extent - self.fitted_extent).abs() > 1.0 {
            self.fitted_extent = graph.extent;
            self.camera.distance = (graph.extent * 2.4).max(300.0);
            let projected_extent = graph.extent * self.camera.focal_length / self.camera.distance;
            self.fit_zoom = (0.55 * rect.width().min(rect.height()) / projected_extent).clamp(0.5, 3.0);
            self.camera.zoom = self.fit_zoom;
            self.camera.pan_x = 0.0;
            self.camera.pan_y = 0.0;
        }

        if response.dragged_by(egui::PointerButton::Secondary)
            || response.dragged_by(egui::PointerButton::Middle)
        {
            // Rechte/mittlere Maustaste verschiebt die Ansicht.
            let delta = response.drag_delta();
            self.camera.pan_by(delta.x, delta.y);
            self.last_interaction = now;
        } else if response.dragged() {
            let delta = response.drag_delta();
            self.camera
                .apply_drag(delta.x, delta.y, self.drag_sensitivity_deg_per_px);
            self.last_interaction = now;
        }
        if response.double_clicked() {
            self.camera.reset_view();
            self.fitted_extent = 0.0; // beim nächsten Frame neu einpassen
            self.focus = None;
            self.return_anim = 0.0;
        }
        if self.focus.is_some_and(|g| g >= graph.groups.len()) {
            self.focus = None;
        }
        if self.selected_node.is_some_and(|n| n >= graph.nodes.len()) {
            self.selected_node = None;
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.selected_node.is_some() {
                self.selected_node = None;
            } else {
                self.unfocus();
            }
        }
        let idle = now.duration_since(self.last_interaction).as_secs_f32() >= self.idle_resume_secs;
        self.update_focus_camera(&graph, rect, dt, idle);

        // Scrollen über dem Graphen zoomt auf die Mausposition (nicht auf
        // die Mitte); die Grenzen stehen in `camera::MIN_ZOOM/MAX_ZOOM`.
        if let Some(pointer) = response.hover_pos() {
            let scroll = ui.ctx().input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > f32::EPSILON {
                let rel = pointer - rect.center();
                self.camera
                    .zoom_at(rel.x, rel.y, (scroll * SCROLL_ZOOM_PER_PIXEL).exp());
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

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glow_ring_wird_nach_aussen_schwaecher_und_groesser() {
        let (inner_radius, inner_alpha) = glow_ring(1, NODE_GLOW_RINGS, 10.0, 1.0);
        let (outer_radius, outer_alpha) = glow_ring(NODE_GLOW_RINGS, NODE_GLOW_RINGS, 10.0, 1.0);
        assert!(outer_radius > inner_radius, "äußerer Ring muss größer sein");
        assert!(
            outer_alpha < inner_alpha,
            "äußerer Ring muss durchsichtiger sein (innen {inner_alpha}, außen {outer_alpha})"
        );
    }

    #[test]
    fn glow_ring_alpha_bleibt_immer_im_gueltigen_byte_bereich() {
        for strength in [0.0_f32, 0.5, 1.0, 5.0, 100.0] {
            for i in 1..=NODE_GLOW_RINGS {
                let (_, alpha) = glow_ring(i, NODE_GLOW_RINGS, 10.0, strength);
                assert!((0..=255).contains(&(alpha as i32)));
            }
        }
    }

    #[test]
    fn blend_color_mischt_zu_gleichen_teilen() {
        let mixed = blend_color(Color32::from_rgb(0, 0, 0), Color32::from_rgb(200, 100, 40));
        assert_eq!(mixed, Color32::from_rgb(100, 50, 20));
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
        assert_eq!(loaded.groups.len(), 2);
        assert_eq!(loaded.group_edges, vec![(0, 1, 1)]);
        assert_eq!(loaded.groups[1].members, vec![1, 2]);
    }

    #[test]
    fn spread_groups_vergroessert_gruppenabstand_und_verkleinert_wolken() {
        let mut positions = vec![
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(10.0, 0.0, 0.0),
            Vec3::new(100.0, 0.0, 0.0),
            Vec3::new(110.0, 0.0, 0.0),
        ];
        spread_groups(&mut positions, &[0, 0, 1, 1], 2);
        let inside = positions[1].sub(positions[0]).length();
        let between = positions[2].sub(positions[0]).length();
        assert!(inside < 10.0, "Wolke muss kompakter werden: {inside}");
        assert!(between > 100.0, "Gruppen müssen weiter auseinander: {between}");
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
}
