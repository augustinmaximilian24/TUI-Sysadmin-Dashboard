//! Holo-Darstellung des Graphen (schwarzer Hintergrund, cyanfarbene
//! Linien, Glas-Panels): Übersicht mit großen Gruppen-Knoten ("Ordner"),
//! Mitglieder nur als kleine Punkte; Klick auf eine Gruppe zoomt hinein und
//! zeigt ihre wichtigen Knoten (Unterordner, Hubs) deutlich.

use eframe::egui::{self, Color32, Pos2, Stroke, Vec2};

use super::camera::{project_point, Projected};
use super::layout::Vec3;
use super::{draw_node_glow, Group, KnowledgeGraphTab, LoadedGraph};

/// Fast-schwarzer Hintergrund der Graph-Ansicht.
const BG: Color32 = Color32::from_rgb(3, 6, 10);
/// Grundton für Linien, Ringe und Panel-Rahmen.
const CYAN: Color32 = Color32::from_rgb(90, 214, 236);
const TEXT: Color32 = Color32::from_rgb(190, 235, 245);
/// Kante, die direkt aus den Quellen extrahiert wurde (verlässlich).
const EXTRACTED_COLOR: Color32 = Color32::from_rgb(90, 235, 150);
/// Kante, die nur erschlossen/vermutet wurde.
const INFERRED_COLOR: Color32 = Color32::from_rgb(255, 176, 64);

/// Linienfarbe einer Kante nach `confidence`; unbekannte Werte bleiben
/// neutral, damit neues graphify-Vokabular nicht falsch eingefärbt wird.
fn edge_color(confidence: &str) -> Color32 {
    match confidence {
        "EXTRACTED" => EXTRACTED_COLOR,
        "INFERRED" => INFERRED_COLOR,
        _ => Color32::from_rgb(120, 150, 165),
    }
}

/// Mischfarbe einer Gruppenverbindung aus dem Anteil verlässlicher Kanten.
fn group_edge_color(total: u32, extracted: u32) -> Color32 {
    let t = if total == 0 { 0.0 } else { extracted as f32 / total as f32 };
    let mix = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t) as u8;
    Color32::from_rgb(
        mix(INFERRED_COLOR.r(), EXTRACTED_COLOR.r()),
        mix(INFERRED_COLOR.g(), EXTRACTED_COLOR.g()),
        mix(INFERRED_COLOR.b(), EXTRACTED_COLOR.b()),
    )
}

/// Farbton einer Gruppe: Variationen von Cyan/Türkis/Blau, damit das Bild
/// einheitlich "holografisch" bleibt, Gruppen sich aber unterscheiden.
pub(super) fn holo_color(group: usize) -> Color32 {
    let hue = 0.47 + 0.17 * ((group as f32 * 0.618_034) % 1.0);
    egui::ecolor::Hsva::new(hue, 0.55, 0.95, 1.0).into()
}

fn with_alpha(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

/// Sechseck-Eckpunkte um `center` (für Symbole und schwebende Körper).
fn hexagon(center: Pos2, radius: f32, spin: f32) -> Vec<Pos2> {
    (0..6)
        .map(|i| {
            let a = spin + i as f32 * std::f32::consts::FRAC_PI_3;
            center + Vec2::new(a.cos(), a.sin()) * radius
        })
        .collect()
}

/// Punkt auf einer um `rotation` gedrehten Ellipse (Halbachsen `rx`/`ry`).
fn ring_point(center: Pos2, rx: f32, ry: f32, rotation: f32, angle: f32) -> Pos2 {
    let (sin_r, cos_r) = rotation.sin_cos();
    let (x, y) = (angle.cos() * rx, angle.sin() * ry);
    center + Vec2::new(x * cos_r - y * sin_r, x * sin_r + y * cos_r)
}

/// Schräge Ellipse (Ring um einen Gruppen-Knoten) als Punktliste.
fn ring_points(center: Pos2, rx: f32, ry: f32, rotation: f32) -> Vec<Pos2> {
    (0..40)
        .map(|i| ring_point(center, rx, ry, rotation, i as f32 / 40.0 * std::f32::consts::TAU))
        .collect()
}

/// Glas-Panel im Holo-Stil an Bildschirmposition `pos` (mit `pivot` als Anker).
fn holo_panel(
    ctx: &egui::Context,
    id: &str,
    pivot: egui::Align2,
    pos: Pos2,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    let out = egui::Area::new(egui::Id::new(id))
        .order(egui::Order::Foreground)
        .pivot(pivot)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            egui::Frame::none()
                .fill(Color32::from_rgba_unmultiplied(6, 24, 34, 205))
                .stroke(Stroke::new(1.0_f32, with_alpha(CYAN, 110)))
                .rounding(4.0)
                .inner_margin(egui::Margin::same(10.0))
                .show(ui, |ui| {
                    ui.set_max_width(340.0);
                    ui.style_mut().override_font_id = Some(egui::FontId::monospace(12.0));
                    ui.visuals_mut().override_text_color = Some(TEXT);
                    add_contents(ui);
                });
        });
    // Eck-Markierungen wie bei einem Sci-Fi-Display.
    let rect = out.response.rect;
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new(id).with("corners"),
    ));
    let stroke = Stroke::new(1.6_f32, CYAN);
    let l = 9.0;
    for (corner, dx, dy) in [
        (rect.left_top(), 1.0, 1.0),
        (rect.right_top(), -1.0, 1.0),
        (rect.left_bottom(), 1.0, -1.0),
        (rect.right_bottom(), -1.0, -1.0),
    ] {
        painter.line_segment([corner, corner + Vec2::new(l * dx, 0.0)], stroke);
        painter.line_segment([corner, corner + Vec2::new(0.0, l * dy)], stroke);
    }
}

/// Gemeinsamer Zeichenkontext eines Frames (Painter, projizierte Punkte,
/// Mausposition, Projektion auf Bildschirmkoordinaten).
struct Scene<'a, F: Fn(&Projected) -> Pos2> {
    painter: &'a egui::Painter,
    projected: &'a [Option<Projected>],
    pointer: Option<Pos2>,
    to_screen: F,
}

impl KnowledgeGraphTab {
    pub(super) fn draw_graph(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        response: &egui::Response,
        graph: &LoadedGraph,
    ) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, BG);

        let center = rect.center();
        let zoom = self.camera.zoom;
        let (pan_x, pan_y) = (self.camera.pan_x, self.camera.pan_y);
        let to_screen = |p: &Projected| {
            Pos2::new(center.x + pan_x + p.x * zoom, center.y + pan_y - p.y * zoom)
        };
        let persp = |p: &Projected| self.camera.distance / p.depth;

        // Effektive Positionen: im Fokus fahren die Mitglieder der Gruppe aus
        // der kompakten Wolke in ihr aufgefächertes Eigenlayout.
        let expand = self.focus.map_or(0.0, |_| {
            let t = (self.focus_anim / 0.9).clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t)
        });
        let positions: Vec<Vec3> = match self.focus {
            Some(fg) if expand > 0.0 => {
                let centroid = graph.groups[fg].centroid;
                let mut effective = graph.positions.clone();
                for &i in &graph.groups[fg].members {
                    let target = centroid.add(graph.local_positions[i]);
                    effective[i] = graph.positions[i].add(target.sub(graph.positions[i]).scale(expand));
                }
                effective
            }
            _ => graph.positions.clone(),
        };
        let projected: Vec<Option<Projected>> = positions
            .iter()
            .map(|p| project_point(*p, &self.camera))
            .collect();
        // Tiefe 0 (nah) .. 1 (fern) für Abdunklung und Größe.
        let (near, far) = projected
            .iter()
            .flatten()
            .fold((f32::MAX, f32::MIN), |(lo, hi), p| (lo.min(p.depth), hi.max(p.depth)));
        let near_factor = |depth: f32| 1.0 - ((depth - near) / (far - near).max(1.0)).clamp(0.0, 1.0);
        let hubs: Vec<Option<(Pos2, f32, f32)>> = graph
            .groups
            .iter()
            .map(|g| {
                project_point(g.centroid, &self.camera)
                    .map(|p| (to_screen(&p), persp(&p), near_factor(p.depth)))
            })
            .collect();
        let max_members = graph.groups.iter().map(|g| g.members.len()).max().unwrap_or(1) as f32;
        let max_shared = graph.group_edges.iter().map(|e| e.2).max().unwrap_or(1) as f32;
        let pointer = response.hover_pos();
        let pulse = self.pulse_phase;
        let spin = self.spin_phase;
        let focus = self.focus;
        let mut hovered_hub: Option<usize> = None;
        let mut hovered_node: Option<usize> = None;

        self.draw_decor(&painter, graph, &to_screen, focus.is_some());

        // --- Verbindungen zwischen Gruppen (zusammengefasst) ---
        for (k, &(a, b, count, extracted)) in graph.group_edges.iter().enumerate() {
            if focus.is_some_and(|f| f != a && f != b) {
                continue;
            }
            let (Some((pa, _, da)), Some((pb, _, db))) = (hubs[a], hubs[b]) else {
                continue;
            };
            let depth_fade = 0.35 + 0.65 * (da + db) / 2.0;
            let share = count as f32 / max_shared;
            let color = group_edge_color(count, extracted);
            let alpha = (if focus.is_some() { 55.0 } else { 60.0 + 90.0 * share } * depth_fade) as u8;
            painter.line_segment([pa, pb], Stroke::new(3.0 + 3.0 * share, with_alpha(color, 14)));
            painter.line_segment([pa, pb], Stroke::new(0.7 + 1.6 * share, with_alpha(color, alpha)));
            // Kleiner Datenpunkt, der entlang der Verbindung wandert.
            let t = (pulse / std::f32::consts::TAU + k as f32 * 0.17) % 1.0;
            painter.circle_filled(pa + (pb - pa) * t, 2.0, with_alpha(CYAN, 190));
        }

        // --- Mitglieder ---
        if let Some(fg) = focus {
            let scene = Scene {
                painter: &painter,
                projected: &projected,
                pointer,
                to_screen: &to_screen,
            };
            hovered_node = self.draw_focused_members(&scene, graph, fg);
            let group = &graph.groups[fg];
            if let Some((hub, ps, _)) = hubs[fg] {
                let ring = group.focus_radius * ps * (self.camera.focal_length / self.camera.distance) * zoom * 1.12;
                painter.circle_stroke(hub, ring, Stroke::new(1.0_f32, with_alpha(holo_color(fg), 45)));
                painter.text(
                    hub + Vec2::new(0.0, -ring - 6.0),
                    egui::Align2::CENTER_BOTTOM,
                    &group.label,
                    egui::FontId::proportional(16.0),
                    with_alpha(TEXT, 230),
                );
            }
        } else {
            // Übersicht: alle Knoten nur als winzige Punkte, Unterordner
            // minimal größer.
            for (i, p) in projected.iter().enumerate() {
                let Some(p) = p else { continue };
                let is_dir = graph.nodes[i].file_type == "dir";
                let (radius, alpha) = if is_dir { (2.0, 150.0) } else { (1.2, 70.0) };
                let df = near_factor(p.depth);
                painter.circle_filled(
                    to_screen(p),
                    radius * persp(p) * zoom.sqrt(),
                    with_alpha(holo_color(graph.group_of[i]), (alpha * (0.3 + 0.7 * df)) as u8),
                );
            }
        }

        // --- Gruppen-Knoten ("Ordner") ---
        let mut hub_labels: Vec<(Pos2, f32, usize, bool)> = Vec::new();
        let mut hub_order: Vec<usize> = (0..graph.groups.len()).collect();
        hub_order.sort_by(|&a, &b| {
            let df = |g: usize| hubs[g].map_or(0.0, |h| h.2);
            df(a).partial_cmp(&df(b)).unwrap_or(std::cmp::Ordering::Equal)
        });
        for g in hub_order {
            let group = &graph.groups[g];
            let Some((pos, ps, df)) = hubs[g] else { continue };
            let is_focus = focus == Some(g);
            let dimmed = focus.is_some() && !is_focus;
            let base = if dimmed {
                7.0
            } else if is_focus {
                0.0
            } else {
                16.0 + 22.0 * (group.members.len() as f32 / max_members).sqrt()
            };
            if base == 0.0 {
                continue;
            }
            let radius = (base * ps * (0.7 + 0.6 * df) * zoom.powf(0.35)).clamp(6.0, 64.0);
            let hit = pointer.is_some_and(|p| p.distance(pos) <= radius + 4.0);
            if hit {
                hovered_hub = Some(g);
            }
            let color = holo_color(g);
            let strength = if dimmed { 0.25 } else { 0.55 } + if hit { 0.9 } else { 0.0 };
            draw_node_glow(&painter, pos, radius.min(20.0), color, strength * 0.7 * (0.4 + 0.6 * df));
            painter.circle_filled(pos, radius, Color32::from_rgba_unmultiplied(6, 22, 30, 235));
            // Kugel-Wirkung: Glanzpunkt oben links und ein schräger Ring.
            painter.circle_filled(
                pos + Vec2::new(-radius * 0.35, -radius * 0.35),
                radius * 0.22,
                with_alpha(color, (70.0 * (0.4 + 0.6 * df)) as u8),
            );
            if !dimmed {
                // Fester, schräger Ring; nur ein kleiner Punkt umkreist ihn.
                let ring = ring_points(pos, radius * 1.65, radius * 0.5, -0.35);
                painter.add(egui::Shape::closed_line(
                    ring.clone(),
                    Stroke::new(1.0_f32, with_alpha(color, (120.0 * (0.3 + 0.7 * df)) as u8)),
                ));
                let orbit = ring_point(pos, radius * 1.65, radius * 0.5, -0.35, spin + g as f32);
                painter.circle_filled(orbit, 2.2, with_alpha(CYAN, (220.0 * (0.4 + 0.6 * df)) as u8));
            }
            painter.circle_stroke(
                pos,
                radius,
                Stroke::new(
                    if hit { 2.4_f32 } else { 1.6_f32 },
                    with_alpha(color, if dimmed { 150 } else { (130.0 + 125.0 * df) as u8 }),
                ),
            );
            painter.add(egui::Shape::closed_line(
                hexagon(pos, radius * 0.5, spin * 0.25),
                Stroke::new(1.0_f32, with_alpha(color, if dimmed { 110 } else { 210 })),
            ));
            hub_labels.push((pos, radius, g, dimmed));
        }
        // Beschriftungen zuletzt, damit keine Kreise darüber liegen.
        for (pos, radius, g, dimmed) in hub_labels {
            let group = &graph.groups[g];
            let label_alpha = if dimmed { 150 } else { 240 };
            painter.text(
                pos + Vec2::new(radius + 8.0, -2.0),
                egui::Align2::LEFT_BOTTOM,
                &group.label,
                egui::FontId::proportional(if dimmed { 11.0 } else { 14.0 }),
                with_alpha(TEXT, label_alpha),
            );
            if !dimmed {
                painter.text(
                    pos + Vec2::new(radius + 8.0, 0.0),
                    egui::Align2::LEFT_TOP,
                    format!("{} Knoten", group.members.len()),
                    egui::FontId::proportional(10.5),
                    with_alpha(TEXT, 130),
                );
            }
        }

        // --- Klicks ---
        if response.clicked() {
            if let Some(n) = hovered_node {
                self.selected_node = Some(n);
            } else if let Some(g) = hovered_hub.filter(|&g| focus != Some(g)) {
                self.focus_group(g);
            } else if self.selected_node.is_some() {
                self.selected_node = None;
            } else if focus.is_some() {
                self.unfocus();
            }
        }
        if response.hovered() && (hovered_hub.is_some() || hovered_node.is_some()) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        self.draw_panels(ui, rect, graph);
    }

    /// Schwebende Wireframe-Körper als Dekoration im Hintergrund.
    fn draw_decor(
        &self,
        painter: &egui::Painter,
        graph: &LoadedGraph,
        to_screen: &impl Fn(&Projected) -> Pos2,
        focused: bool,
    ) {
        let radius = graph.extent * 1.45;
        for k in 0..8usize {
            let a = k as f32 * 2.399;
            let pos = Vec3::new(
                radius * a.cos(),
                radius * 0.55 * (k as f32 * 1.7).sin(),
                radius * a.sin(),
            );
            let Some(p) = project_point(pos, &self.camera) else { continue };
            let size = (9.0 + (k % 3) as f32 * 6.0) * (self.camera.distance / p.depth);
            let center = to_screen(&p);
            let spin = self.spin_phase * 0.5 + k as f32;
            let alpha = if focused { 22 } else { 55 };
            let stroke = Stroke::new(1.0_f32, with_alpha(CYAN, alpha));
            let outline = hexagon(center, size, spin);
            painter.add(egui::Shape::closed_line(outline.clone(), stroke));
            for i in 0..3 {
                painter.line_segment([outline[i * 2], outline[(i * 2 + 2) % 6]], stroke);
                painter.line_segment([center, outline[i * 2]], stroke);
            }
        }
    }

    /// Zeichnet die Mitglieder der fokussierten Gruppe `fg`; liefert den
    /// Knoten unter dem Mauszeiger (falls einer).
    fn draw_focused_members<F: Fn(&Projected) -> Pos2>(
        &self,
        scene: &Scene<'_, F>,
        graph: &LoadedGraph,
        fg: usize,
    ) -> Option<usize> {
        let Scene { painter, projected, pointer, to_screen } = scene;
        let mut hovered: Option<usize> = None;
        let group: &Group = &graph.groups[fg];
        let color = holo_color(fg);
        let zoom = self.camera.zoom;

        // Kanten innerhalb der Gruppe.
        for edge in &graph.edges {
            if graph.group_of[edge.a] != fg || graph.group_of[edge.b] != fg {
                continue;
            }
            let (Some(pa), Some(pb)) = (&projected[edge.a], &projected[edge.b]) else {
                continue;
            };
            let (a, b) = (to_screen(pa), to_screen(pb));
            let strong = edge.confidence == "EXTRACTED";
            let line_color = edge_color(&edge.confidence);
            if strong {
                painter.line_segment([a, b], Stroke::new(3.5_f32, with_alpha(line_color, 18)));
            }
            painter.line_segment([a, b], Stroke::new(1.1_f32, with_alpha(line_color, if strong { 150 } else { 120 })));
        }

        // Wichtige Knoten: Unterordner + obere 20 % nach Verbindungsgrad.
        let mut degrees: Vec<u32> = group.members.iter().map(|&i| graph.degree[i]).collect();
        degrees.sort_unstable_by(|a, b| b.cmp(a));
        let threshold = degrees.get(group.members.len() / 5).copied().unwrap_or(0).max(2);
        let is_key = |i: usize| graph.nodes[i].file_type == "dir" || graph.degree[i] >= threshold;

        let mut placed: Vec<egui::Rect> = Vec::new();
        let mut order: Vec<usize> = group.members.clone();
        order.sort_by(|&i, &j| {
            let depth = |n: usize| projected[n].as_ref().map_or(f32::MAX, |p| p.depth);
            depth(j).partial_cmp(&depth(i)).unwrap_or(std::cmp::Ordering::Equal)
        });
        for &i in &order {
            let Some(p) = &projected[i] else { continue };
            let screen = to_screen(p);
            let ps = self.camera.distance / p.depth;
            let key = is_key(i);
            let base = if key {
                5.0 + 5.0 * (graph.degree[i] as f32 / graph.max_degree as f32)
            } else {
                3.2
            };
            let radius = (base * ps * zoom.sqrt()).clamp(2.0, 26.0);
            if pointer.is_some_and(|ptr| ptr.distance(screen) <= radius + 5.0) {
                hovered = Some(i);
            }
            let is_hover = hovered == Some(i);
            let selected = self.selected_node == Some(i);
            if key || is_hover || selected {
                let strength = 0.16 + if is_hover || selected { 1.3 } else { 0.0 };
                draw_node_glow(painter, screen, radius.min(7.0), color, strength);
            }
            painter.circle_filled(screen, radius, if key { color } else { with_alpha(color, 170) });
            if is_hover || selected {
                painter.circle_stroke(screen, radius + 2.5, Stroke::new(1.6_f32, Color32::WHITE));
            }
            if key || is_hover || selected || zoom > 4.5 {
                let font = egui::FontId::proportional(if key { 12.0 } else { 10.5 });
                let anchor = screen + Vec2::new(radius + 5.0, -radius + 1.0);
                let galley = painter.layout_no_wrap(graph.nodes[i].label.clone(), font, Color32::WHITE);
                let rect = egui::Rect::from_min_size(
                    Pos2::new(anchor.x, anchor.y - galley.size().y),
                    galley.size(),
                );
                // Überlappende Beschriftungen weglassen (Hover/Auswahl immer zeigen).
                let forced = is_hover || selected;
                if forced || !placed.iter().any(|r| r.intersects(rect.expand(1.0))) {
                    placed.push(rect);
                    painter.galley(rect.min + Vec2::new(1.0, 1.0), galley.clone(), Color32::from_rgba_unmultiplied(0, 0, 0, 200));
                    painter.galley(
                        rect.min,
                        painter.layout_no_wrap(
                            graph.nodes[i].label.clone(),
                            egui::FontId::proportional(if key { 12.0 } else { 10.5 }),
                            with_alpha(TEXT, if forced { 255 } else { 220 }),
                        ),
                        TEXT,
                    );
                }
            }
        }
        hovered
    }

    fn draw_panels(&mut self, ui: &mut egui::Ui, rect: egui::Rect, graph: &LoadedGraph) {
        let ctx = ui.ctx().clone();
        let title = self.title.clone();
        let focus = self.focus;
        let mut go_back = false;
        let mut pick: Option<usize> = None;

        holo_panel(
            &ctx,
            &format!("holo_title_{title}"),
            egui::Align2::LEFT_TOP,
            rect.left_top() + Vec2::new(14.0, 14.0),
            |ui| {
                ui.label(egui::RichText::new(&title).color(CYAN).strong());
                ui.label(format!(
                    "{} Gruppen · {} Knoten · {} Kanten",
                    graph.groups.len(),
                    graph.nodes.len(),
                    graph.edges.len()
                ));
                let extracted = graph.edges.iter().filter(|e| e.confidence == "EXTRACTED").count();
                let inferred = graph.edges.iter().filter(|e| e.confidence == "INFERRED").count();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("━ EXTRACTED {extracted}")).color(EXTRACTED_COLOR));
                    ui.label(egui::RichText::new(format!("━ INFERRED {inferred}")).color(INFERRED_COLOR));
                });
                let hint = if focus.is_some() {
                    "Esc / Klick ins Leere: zurück"
                } else {
                    "Klick auf Gruppe: hineinzoomen"
                };
                ui.label(egui::RichText::new(hint).color(with_alpha(TEXT, 130)).size(11.0));
            },
        );

        if let Some(fg) = focus {
            let group = &graph.groups[fg];
            let mut members: Vec<usize> = group.members.clone();
            members.sort_by_key(|&i| {
                (graph.nodes[i].file_type != "dir", std::cmp::Reverse(graph.degree[i]))
            });
            holo_panel(
                &ctx,
                &format!("holo_group_{title}"),
                egui::Align2::RIGHT_TOP,
                rect.right_top() + Vec2::new(-14.0, 14.0),
                |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&group.label).color(holo_color(fg)).strong());
                        if ui.small_button("← Übersicht").clicked() {
                            go_back = true;
                        }
                    });
                    ui.label(format!(
                        "{} Knoten · {} Unterordner",
                        group.members.len(),
                        group.members.iter().filter(|&&i| graph.nodes[i].file_type == "dir").count()
                    ));
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .id_salt(format!("holo_members_{title}"))
                        .max_height(260.0)
                        .show(ui, |ui| {
                            for &i in members.iter().take(200) {
                                let node = &graph.nodes[i];
                                let marker = if node.file_type == "dir" { "▣" } else { "·" };
                                let text = format!("{marker} {}", node.label);
                                if ui.selectable_label(self.selected_node == Some(i), text).clicked() {
                                    pick = Some(i);
                                }
                            }
                        });
                },
            );
        }

        if let Some(idx) = self.selected_node {
            self.draw_detail_panel(&ctx, rect, graph, idx);
        }
        if let Some(i) = pick {
            self.selected_node = Some(i);
        }
        if go_back {
            self.unfocus();
        }
    }

    fn draw_detail_panel(&mut self, ctx: &egui::Context, rect: egui::Rect, graph: &LoadedGraph, idx: usize) {
        let node = &graph.nodes[idx];
        let mut close = false;
        holo_panel(
            ctx,
            &format!("holo_detail_{}", self.title),
            egui::Align2::LEFT_BOTTOM,
            rect.left_bottom() + Vec2::new(14.0, -14.0),
            |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&node.label).color(CYAN).strong());
                    if ui.small_button("×").clicked() {
                        close = true;
                    }
                });
                let group = &graph.groups[graph.group_of[idx]];
                ui.label(egui::RichText::new(&group.label).color(holo_color(graph.group_of[idx])));
                ui.label(egui::RichText::new(format!("Typ: {}", node.file_type)).color(with_alpha(TEXT, 140)).size(11.0));
                if let Some(source) = &node.source_file {
                    ui.label(egui::RichText::new(format!("Quelle: {source}")).color(with_alpha(TEXT, 140)).size(11.0));
                }
                if let Some(rationale) = &node.rationale {
                    ui.add_space(4.0);
                    ui.label(rationale);
                }
                let connections: Vec<(bool, &str, &str, &str)> = graph
                    .edges
                    .iter()
                    .filter_map(|e| {
                        if e.a == idx {
                            Some((true, e.relation.as_str(), graph.nodes[e.b].label.as_str(), e.confidence.as_str()))
                        } else if e.b == idx {
                            Some((false, e.relation.as_str(), graph.nodes[e.a].label.as_str(), e.confidence.as_str()))
                        } else {
                            None
                        }
                    })
                    .collect();
                if !connections.is_empty() {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("VERBINDUNGEN").color(CYAN).size(11.0));
                    egui::ScrollArea::vertical()
                        .id_salt("holo_connections")
                        .max_height(140.0)
                        .show(ui, |ui| {
                            for (out, relation, other, confidence) in &connections {
                                let arrow = if *out { "→" } else { "←" };
                                ui.label(
                                    egui::RichText::new(format!("{arrow} {relation} {arrow} {other}"))
                                        .color(edge_color(confidence)),
                                );
                            }
                        });
                }
            },
        );
        if close {
            self.selected_node = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holo_farben_bleiben_im_cyan_blau_bereich() {
        for g in 0..40 {
            let c = holo_color(g);
            assert!(c.b() > c.r(), "Gruppe {g}: Blauanteil muss überwiegen ({c:?})");
        }
    }

    #[test]
    fn kantenfarbe_unterscheidet_extracted_und_inferred() {
        assert_eq!(edge_color("EXTRACTED"), EXTRACTED_COLOR);
        assert_eq!(edge_color("INFERRED"), INFERRED_COLOR);
        assert_ne!(edge_color("EXTRACTED"), edge_color("INFERRED"));
        assert_ne!(edge_color("AMBIGUOUS"), edge_color("INFERRED"));
    }

    #[test]
    fn gruppenverbindung_mischt_nach_extracted_anteil() {
        assert_eq!(group_edge_color(4, 0), INFERRED_COLOR);
        assert_eq!(group_edge_color(4, 4), EXTRACTED_COLOR);
        assert_eq!(group_edge_color(0, 0), INFERRED_COLOR);
    }

    #[test]
    fn hexagon_hat_sechs_ecken_auf_dem_radius() {
        let hex = hexagon(Pos2::new(10.0, 10.0), 5.0, 0.3);
        assert_eq!(hex.len(), 6);
        for p in hex {
            assert!((p.distance(Pos2::new(10.0, 10.0)) - 5.0).abs() < 1e-4);
        }
    }
}
