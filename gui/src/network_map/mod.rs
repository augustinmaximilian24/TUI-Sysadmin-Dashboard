//! Weltkarte im Systemzustands-Panel (Phase 12, optional): zeigt, in
//! welche Länder gerade aktive ausgehende TCP-Verbindungen dieses
//! Rechners gehen, und zoomt automatisch auf den Bereich, der "Zuhause"
//! und alle aktiven Gegenstellen umfasst.
//!
//! Architektur (dasselbe Muster wie [`crate::knowledge_graph`]): ein
//! Hintergrund-`std::thread` scannt in `poll_interval_seconds`
//! `/proc/net/tcp` ([`connections`]) und löst jede neue Gegenstelle über
//! die lokale `GeoIP.dat` ([`geoip`]) in ein Land auf -- keine
//! Netzwerk-Anfrage pro Verbindung, keine Cloud-Abhängigkeit. Ergebnis
//! landet in `Arc<Mutex<Shared>>`, danach `ctx.request_repaint()`.
//! `show()` läuft auf dem Render-Thread, klont nur den kleinen aktuellen
//! Zustand und zeichnet daraus -- keine I/O im Render-Pfad (Regel 21).

pub mod connections;
pub mod geoip;
mod precise_location;
mod world;

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke};

use geoip::CountryDb;
use logsentry_core::config::NetworkMapConfig;

/// Zielrate für die Puls-/Zoom-Animation (Regel 20: reaktiv statt
/// Continuous-Modus).
const REPAINT_INTERVAL: Duration = Duration::from_millis(120);

/// Wie lange eine Gegenstelle nach dem letzten Sichten noch angezeigt
/// bleibt, bevor sie aus der Karte verschwindet -- ohne das würde jede
/// kurz geschlossene Verbindung sofort wieder verschwinden und die Karte
/// würde bei jedem Poll-Intervall unruhig flackern.
const CONNECTION_HOLD: Duration = Duration::from_secs(20);

#[derive(Clone)]
struct ConnectionPoint {
    ip: Ipv4Addr,
    country: &'static str,
    lat: f32,
    lon: f32,
    /// Stadt, falls [`precise_location::resolve_precise`] über Reverse-DNS
    /// einen bekannten Flughafencode im Hostnamen gefunden hat -- sonst
    /// `None`, dann sind `lat`/`lon` nur der Länder-Mittelpunkt.
    precise_city: Option<String>,
    last_seen: Instant,
}

#[derive(Default)]
struct Shared {
    /// `ip -> ConnectionPoint`, damit ein Re-Scan bestehende Einträge nur
    /// auffrischt statt sie neu anzulegen (hält `last_seen` stabil für
    /// [`CONNECTION_HOLD`]).
    points: HashMap<Ipv4Addr, ConnectionPoint>,
    /// Gegenstellen, deren Land sich nicht auflösen ließ (privat gefiltert
    /// schon in [`connections`], aber z. B. nicht in der GeoIP-Datenbank
    /// gelistete Adressen) -- tauchen nicht auf der Karte auf, sollen aber
    /// nicht kommentarlos verschwinden.
    unresolved_count: usize,
    error: Option<String>,
    /// Punktraster der Landmassen (siehe [`world`]), einmalig beim Start
    /// des Hintergrund-Threads berechnet -- ändert sich danach nie mehr,
    /// liegt aber im selben `Mutex` wie der Rest, weil es vom selben
    /// Hintergrund-Thread geschrieben und vom Render-Thread gelesen wird.
    world_dots: Arc<Vec<(f32, f32)>>,
}

fn spawn_watcher(
    db_path: std::path::PathBuf,
    poll_interval: Duration,
    shared: Arc<Mutex<Shared>>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let db = match CountryDb::load(&db_path) {
            Ok(db) => Some(db),
            Err(err) => {
                let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
                guard.error = Some(format!(
                    "GeoIP-Datenbank {} konnte nicht geladen werden: {err}",
                    db_path.display()
                ));
                None
            }
        };

        // Einmalig und nicht im Render-Pfad (Regel 21): die
        // Punkt-in-Polygon-Prüfung über alle ~180 Länder kann spürbar
        // dauern, ändert ihr Ergebnis danach aber nie wieder.
        let dots = Arc::new(world::build_dot_grid());
        {
            let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
            guard.world_dots = dots;
        }
        ctx.request_repaint();

        loop {
            let now = Instant::now();
            let seen = connections::read_established_remote_ipv4();

            // Nur kurz sperren, um bekannte Gegenstellen aufzufrischen und
            // neue zu erkennen -- Länder-Lookup ist zwar schnell, aber
            // `precise_location::resolve_precise` ruft `getent` als
            // Subprozess auf und kann spürbar dauern. Würde die Sperre
            // darüber gehalten, hinge `show()` auf dem Render-Thread bei
            // jedem neuen Verbindungsziel kurz fest (Regel 21).
            let new_ips: Vec<Ipv4Addr> = {
                let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
                guard.points.retain(|_, p| now.duration_since(p.last_seen) < CONNECTION_HOLD);
                seen.into_iter()
                    .filter(|ip| match guard.points.get_mut(ip) {
                        Some(point) => {
                            point.last_seen = now;
                            false
                        }
                        None => true,
                    })
                    .collect()
            };

            let mut unresolved = 0usize;
            let mut newly_resolved = Vec::new();
            for ip in new_ips {
                let resolved = db.as_ref().and_then(|db| db.lookup_v4(ip)).and_then(|code| {
                    geoip::country_centroid(code).map(|(lat, lon)| (code, lat, lon))
                });
                match resolved {
                    Some((country, country_lat, country_lon)) => {
                        let (lat, lon, precise_city) =
                            match precise_location::resolve_precise(ip) {
                                Some((lat, lon, city)) => (lat, lon, Some(city)),
                                None => (country_lat, country_lon, None),
                            };
                        newly_resolved.push(ConnectionPoint {
                            ip,
                            country,
                            lat,
                            lon,
                            precise_city,
                            last_seen: now,
                        });
                    }
                    None => unresolved += 1,
                }
            }

            let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
            for point in newly_resolved {
                guard.points.insert(point.ip, point);
            }
            guard.unresolved_count = unresolved;
            drop(guard);
            ctx.request_repaint();
            std::thread::sleep(poll_interval);
        }
    });
}

/// Kamera als reines Gleichgewinkel-("Plattkarte"-)Fenster: Mittelpunkt in
/// Grad plus **ein einziger** Maßstab für beide Achsen -- bewusst nicht
/// pro Achse getrennt: unterschiedliche Maßstäbe für Breite/Höhe stauchen
/// oder strecken die Kontinente und sehen in einem hochkanten Panel
/// sofort sichtbar verzerrt aus. Dass bei einem Seitenverhältnis, das
/// nicht zur Welt (360°×180°) passt, ein Rand stehen bleibt, ist
/// hingenommen -- das ist dasselbe Verhalten wie bei jeder unverzerrten
/// Karte in einem nicht passenden Rahmen (siehe [`fit_camera`] für die
/// Begrenzung, wie groß dieser Rand im schlimmsten Fall wird).
#[derive(Clone, Copy)]
struct MapCamera {
    center_lat: f32,
    center_lon: f32,
    degrees_per_px: f32,
}

impl MapCamera {
    const DEFAULT: MapCamera = MapCamera { center_lat: 20.0, center_lon: 10.0, degrees_per_px: 0.5 };

    fn project(&self, rect: Rect, lat: f32, lon: f32) -> Pos2 {
        let center = rect.center();
        Pos2::new(
            center.x + (lon - self.center_lon) / self.degrees_per_px,
            center.y - (lat - self.center_lat) / self.degrees_per_px,
        )
    }

    /// Frame-raten-unabhängige exponentielle Annäherung an `target`
    /// (dasselbe Glättungsmuster wie an anderer Stelle für Kamera-/
    /// Wertübergänge üblich: `1 - exp(-ease*dt)` statt eines festen
    /// linearen Schritts pro Frame).
    fn ease_towards(&mut self, target: MapCamera, ease_per_second: f32, dt: f32) {
        let t = 1.0 - (-ease_per_second * dt).exp();
        self.center_lat += (target.center_lat - self.center_lat) * t;
        self.center_lon += (target.center_lon - self.center_lon) * t;
        self.degrees_per_px += (target.degrees_per_px - self.degrees_per_px) * t;
    }
}

/// Bestimmt Zielkamera, die `points` (inklusive optionalem Zuhause-Punkt)
/// mit Rand `padding_fraction` in `rect` einpasst. Berücksichtigt **nicht**
/// das Überqueren der 180°-Datumsgrenze (eine Verbindung nach Fidschi
/// gemeinsam mit einer nach Alaska würde die ganze Weltbreite einschließen
/// statt nur den kurzen Weg über den Pazifik) -- ein Heim-PC verbindet sich
/// so gut wie nie gleichzeitig in beide Richtungen um die Datumsgrenze
/// herum, und eine korrekte Behandlung bräuchte eine zirkuläre statt einer
/// linearen Bounding-Box. Ceiling: bei Bedarf min/max über die kürzere der
/// beiden Richtungen um 180°/-180° bestimmen statt über rohe Werte.
fn fit_camera(
    points: &[(f32, f32)],
    rect: Rect,
    padding_fraction: f32,
    min_zoom_span_degrees: f32,
) -> MapCamera {
    if points.is_empty() {
        return MapCamera::DEFAULT;
    }
    let (mut min_lat, mut max_lat) = (points[0].0, points[0].0);
    let (mut min_lon, mut max_lon) = (points[0].1, points[0].1);
    for &(lat, lon) in &points[1..] {
        min_lat = min_lat.min(lat);
        max_lat = max_lat.max(lat);
        min_lon = min_lon.min(lon);
        max_lon = max_lon.max(lon);
    }

    let pad = 1.0 + 2.0 * padding_fraction.max(0.0);
    let span_lat = ((max_lat - min_lat) * pad).max(min_zoom_span_degrees);
    let span_lon = ((max_lon - min_lon) * pad).max(min_zoom_span_degrees);

    let height = rect.height().max(1.0);
    let width = rect.width().max(1.0);

    // Ein Maßstab für beide Achsen -- an der jeweils enger begrenzten
    // Achse orientiert, damit beide Spannen hineinpassen. Zusätzlich
    // gedeckelt auf das, was die Welt tatsächlich hergibt (180°/360°):
    // ohne diese Deckelung könnte ein sehr schmaler, hoher Kasten (wenig
    // `width` bei kleinem `span_lon`) einen Maßstab erzwingen, der
    // senkrecht weit über die Pole hinaus "leerzoomt" -- der ungenutzte
    // Rand oben/unten wäre dann unnötig groß.
    let degrees_per_px = (span_lat / height)
        .max(span_lon / width)
        .min(180.0 / height)
        .min(360.0 / width);

    MapCamera {
        center_lat: (min_lat + max_lat) / 2.0,
        center_lon: (min_lon + max_lon) / 2.0,
        degrees_per_px,
    }
}

/// Rät den "Zuhause"-Ländercode aus der Locale-Umgebung (`LC_ALL`/`LANG`,
/// z. B. `de_DE.UTF-8` -> `DE`). Liefert `None` bei einer Locale ohne
/// Länderanteil (`C`, `POSIX`) oder fehlenden Variablen -- dann zeigt die
/// Karte einfach keinen Zuhause-Punkt, statt zu raten.
fn guess_home_country() -> Option<String> {
    let locale = std::env::var("LC_ALL")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("LANG").ok().filter(|v| !v.is_empty()))?;
    let territory = locale.split('_').nth(1)?;
    let code = territory.split('.').next()?.to_uppercase();
    (code.len() == 2).then_some(code)
}

pub struct NetworkMapPanel {
    shared: Arc<Mutex<Shared>>,
    camera: MapCamera,
    ease_per_second: f32,
    min_zoom_span_degrees: f32,
    zoom_padding_fraction: f32,
    home: Option<(String, f32, f32)>,
    last_frame: Instant,
    pulse_phase: f32,
    hovered_ip: Option<Ipv4Addr>,
}

impl NetworkMapPanel {
    pub fn new(config: &NetworkMapConfig, ctx: &egui::Context) -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        spawn_watcher(
            std::path::PathBuf::from(&config.geoip_database_path),
            Duration::from_secs(config.poll_interval_seconds.max(1)),
            Arc::clone(&shared),
            ctx.clone(),
        );

        let home_code = if config.home_country_override.is_empty() {
            guess_home_country()
        } else {
            Some(config.home_country_override.to_uppercase())
        };
        let home = home_code.and_then(|code| {
            geoip::country_centroid(&code).map(|(lat, lon)| (code, lat, lon))
        });

        Self {
            shared,
            camera: MapCamera::DEFAULT,
            ease_per_second: config.camera_ease_per_second.max(0.05),
            min_zoom_span_degrees: config.min_zoom_span_degrees.max(1.0),
            zoom_padding_fraction: config.zoom_padding_fraction.max(0.0),
            home,
            last_frame: Instant::now(),
            pulse_phase: 0.0,
            hovered_ip: None,
        }
    }

    /// `available_height`: die Höhe, die die Karte ausfüllen soll. Wird
    /// vom Aufrufer *vor* dem Aufteilen in Spalten gemessen und explizit
    /// übergeben statt hier per `ui.available_height()` neu ermittelt --
    /// eine frische `Ui`-Spalte aus `egui::Ui::columns` berichtet dafür
    /// nicht zuverlässig die tatsächlich verfügbare Resthöhe des
    /// umgebenden Panels (anders als z. B. eine `ScrollArea`, die intern
    /// anders rechnet), wodurch die Karte sonst spürbar kleiner als der
    /// freie Bereich neben der Anomalie-Liste ausfiel.
    pub fn show(&mut self, ui: &mut egui::Ui, available_height: f32) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32().min(0.5);
        self.last_frame = now;
        self.pulse_phase = (self.pulse_phase + dt * 1.4) % std::f32::consts::TAU;

        let (points, error, unresolved, world_dots) = {
            let guard = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
            let mut points: Vec<ConnectionPoint> = guard.points.values().cloned().collect();
            points.sort_by_key(|a| a.ip);
            (
                points,
                guard.error.clone(),
                guard.unresolved_count,
                Arc::clone(&guard.world_dots),
            )
        };

        if let Some(err) = &error {
            ui.colored_label(theme_warn(), err);
        }

        // `available_height` ist vor der Überschrift und der
        // Fußzeile ("N aktive Verbindungen") gemessen -- deren Platz hier
        // grob abziehen, statt dass die Karte über den freien Bereich
        // hinausragt.
        const CHROME_RESERVE_PX: f32 = 48.0;
        let desired_height = (available_height - CHROME_RESERVE_PX).max(220.0);
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), desired_height), Sense::hover());

        let mut bounds: Vec<(f32, f32)> = points.iter().map(|p| (p.lat, p.lon)).collect();
        if let Some((_, lat, lon)) = &self.home {
            bounds.push((*lat, *lon));
        }
        let target = fit_camera(
            &bounds,
            rect,
            self.zoom_padding_fraction,
            self.min_zoom_span_degrees,
        );
        self.camera.ease_towards(target, self.ease_per_second, dt);

        self.hovered_ip = None;
        self.draw_map(ui, rect, &response, &points, &world_dots);

        ui.add_space(4.0);
        if !points.is_empty() || unresolved > 0 {
            ui.label(
                egui::RichText::new(format!(
                    "{} aktive Verbindung(en){}",
                    points.len(),
                    if unresolved > 0 {
                        format!(", {unresolved} ohne Kartenposition")
                    } else {
                        String::new()
                    }
                ))
                .color(crate::theme::TEXT_MUTED)
                .size(11.0),
            );
        }

        ui.ctx().request_repaint_after(REPAINT_INTERVAL);
    }

    fn draw_map(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        response: &egui::Response,
        points: &[ConnectionPoint],
        world_dots: &[(f32, f32)],
    ) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, Color32::from_rgb(6, 7, 9));
        painter.rect_stroke(rect, 4.0, Stroke::new(1.0_f32, crate::theme::BORDER));

        self.draw_graticule(&painter, rect);
        self.draw_world_dots(&painter, rect, world_dots);

        if let Some((code, lat, lon)) = &self.home {
            let p = self.camera.project(rect, *lat, *lon);
            if rect.contains(p) {
                draw_pulsing_marker(&painter, p, crate::theme::ACCENT, self.pulse_phase);
                painter.text(
                    p + egui::vec2(7.0, -7.0),
                    egui::Align2::LEFT_BOTTOM,
                    format!("Zuhause ({code})"),
                    egui::FontId::proportional(10.0),
                    crate::theme::TEXT_MUTED,
                );
            }
        }

        // Mehrere Verbindungen ins selbe Land (oder sogar dieselbe Stadt)
        // projizieren sonst auf denselben Bildpunkt und verdecken sich
        // gegenseitig -- es sähe dann so aus, als gäbe es nur eine
        // Verbindung. `declump` spreizt Punkte, die zu nah beieinander
        // liegen, in einem kleinen Kreis auseinander, in Bildschirm- statt
        // Kartenkoordinaten, damit der Abstand bei jedem Zoom gleich
        // aussieht.
        let base_positions: Vec<Pos2> = points
            .iter()
            .map(|p| self.camera.project(rect, p.lat, p.lon))
            .collect();
        let screen_positions = declump(&base_positions);

        for (point, &target) in points.iter().zip(&screen_positions) {
            if !rect.contains(target) {
                continue;
            }
            if let Some((_, home_lat, home_lon)) = &self.home {
                let origin = self.camera.project(rect, *home_lat, *home_lon);
                draw_connection_arc(&painter, origin, target, self.pulse_phase);
            }
            painter.circle_filled(target, 3.0, crate::theme::OK);
            painter.circle_stroke(target, 3.0, Stroke::new(1.0_f32, Color32::from_rgb(6, 7, 9)));

            let hover_rect = Rect::from_center_size(target, egui::vec2(12.0, 12.0));
            if response.hovered() {
                if let Some(mouse) = response.hover_pos() {
                    if hover_rect.contains(mouse) {
                        self.hovered_ip = Some(point.ip);
                    }
                }
            }
        }

        if let Some(ip) = self.hovered_ip {
            if let Some((point, &pos)) = points
                .iter()
                .zip(&screen_positions)
                .find(|(p, _)| p.ip == ip)
            {
                egui::show_tooltip_at(
                    ui.ctx(),
                    ui.layer_id(),
                    egui::Id::new(("network_map_tooltip", point.ip)),
                    pos,
                    |ui| {
                        let where_ = point
                            .precise_city
                            .as_deref()
                            .map(|city| format!("{city}, {}", point.country))
                            .unwrap_or_else(|| point.country.to_string());
                        ui.label(format!("{} ({where_})", point.ip));
                    },
                );
            }
        }
    }

    fn draw_graticule(&self, painter: &egui::Painter, rect: Rect) {
        let grid_color = Color32::from_rgba_unmultiplied(0x3d, 0xdc, 0xff, 22);
        let mut lat = -90.0_f32;
        while lat <= 90.0 {
            let a = self.camera.project(rect, lat, -180.0);
            let b = self.camera.project(rect, lat, 180.0);
            painter.line_segment([a, b], Stroke::new(1.0_f32, grid_color));
            lat += 30.0;
        }
        let mut lon = -180.0_f32;
        while lon <= 180.0 {
            let a = self.camera.project(rect, -90.0, lon);
            let b = self.camera.project(rect, 90.0, lon);
            painter.line_segment([a, b], Stroke::new(1.0_f32, grid_color));
            lon += 30.0;
        }
        painter.hline(
            rect.x_range(),
            self.camera.project(rect, 0.0, 0.0).y,
            Stroke::new(1.0_f32, grid_color.gamma_multiply(1.6)),
        );
    }

    /// Zeichnet die Kontinente als Feld einzelner Punkte (siehe
    /// [`world`]) statt als durchgezogene Fläche -- der "digitale
    /// Punktwolken"-Look, den auch die vom Nutzer verlinkte Referenz
    /// zeigt, und der zugleich das darunterliegende Gradnetz durchscheinen
    /// lässt.
    fn draw_world_dots(&self, painter: &egui::Painter, rect: Rect, world_dots: &[(f32, f32)]) {
        let dot_color = Color32::from_rgba_unmultiplied(0x4d, 0xd8, 0xff, 130);
        // Bei starkem Herauszoomen (viel Kartenbreite pro Pixel) liegen
        // mehrere Raster-Punkte auf demselben Pixel -- ein etwas
        // kleinerer Radius hält die Kontinente dann als erkennbare Form
        // statt als einheitlich helle Fläche.
        let radius = if self.camera.degrees_per_px > 1.5 { 0.9 } else { 1.4 };
        for &(lat, lon) in world_dots {
            let p = self.camera.project(rect, lat, lon);
            if rect.contains(p) {
                painter.circle_filled(p, radius, dot_color);
            }
        }
    }
}

/// Wie nah zwei projizierte Punkte (in Pixeln) beieinander liegen dürfen,
/// bevor sie als "derselbe Fleck" gelten und auseinandergezogen werden.
const DECLUMP_MIN_SEPARATION_PX: f32 = 7.0;

/// Spreizt Punkte, die auf demselben Fleck landen (z. B. drei
/// Verbindungen, deren Länder-Mittelpunkt identisch ist), in einem
/// kleinen Kreis um ihre gemeinsame Ausgangsposition. Deterministisch
/// über die Eingabereihenfolge (dieselbe Sortierung wie `points` in
/// [`NetworkMapPanel::show`]), damit sich nichts zwischen Frames
/// unbegründet neu anordnet.
fn declump(base_positions: &[Pos2]) -> Vec<Pos2> {
    let mut placed: Vec<Pos2> = Vec::with_capacity(base_positions.len());
    for &base in base_positions {
        let mut candidate = base;
        let mut ring = 0u32;
        while placed.iter().any(|&p| p.distance(candidate) < DECLUMP_MIN_SEPARATION_PX) {
            ring += 1;
            let angle = ring as f32 * 2.399_963; // Goldener Winkel: keine sich überlagernden Spiralarme
            let radius = DECLUMP_MIN_SEPARATION_PX * 0.9 * (ring as f32).sqrt();
            candidate = base + egui::vec2(angle.cos(), angle.sin()) * radius;
        }
        placed.push(candidate);
    }
    placed
}

fn theme_warn() -> Color32 {
    crate::theme::LEVEL_WARN
}

fn draw_pulsing_marker(painter: &egui::Painter, center: Pos2, color: Color32, phase: f32) {
    let pulse = (phase.sin() * 0.5 + 0.5) * 6.0;
    painter.circle_stroke(
        center,
        5.0 + pulse,
        Stroke::new(1.0_f32, color.gamma_multiply(0.5)),
    );
    painter.circle_filled(center, 3.5, color);
}

/// Leicht gebogene Verbindungslinie mit einem mitlaufenden Lichtpunkt, der
/// entlang der Kurve wandert -- soll "Datenfluss" andeuten, ohne eine
/// echte Traceroute zu simulieren (siehe Modul-Kommentar: nur Start/Ziel
/// sind bekannt, keine tatsächlichen Zwischen-Hops).
fn draw_connection_arc(painter: &egui::Painter, from: Pos2, to: Pos2, phase: f32) {
    let mid = Pos2::new((from.x + to.x) / 2.0, (from.y + to.y) / 2.0);
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let bow = Pos2::new(mid.x - dy * 0.12, mid.y + dx * 0.12);

    let steps = 24;
    let curve: Vec<Pos2> = (0..=steps)
        .map(|i| {
            let t = i as f32 / steps as f32;
            let inv = 1.0 - t;
            Pos2::new(
                inv * inv * from.x + 2.0 * inv * t * bow.x + t * t * to.x,
                inv * inv * from.y + 2.0 * inv * t * bow.y + t * t * to.y,
            )
        })
        .collect();
    painter.add(egui::Shape::line(
        curve.clone(),
        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(0x4d, 0xff, 0xb0, 70)),
    ));

    let t = (phase / std::f32::consts::TAU).fract();
    let idx = ((t * steps as f32) as usize).min(steps);
    if let Some(&pulse_pos) = curve.get(idx) {
        painter.circle_filled(pulse_pos, 2.0, Color32::from_rgb(0x4d, 0xff, 0xb0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declump_laesst_weit_entfernte_punkte_unveraendert() {
        let positions = vec![Pos2::new(0.0, 0.0), Pos2::new(200.0, 200.0)];
        let result = declump(&positions);
        assert_eq!(result, positions);
    }

    #[test]
    fn declump_trennt_identische_punkte() {
        let positions = vec![Pos2::new(50.0, 50.0); 4];
        let result = declump(&positions);
        for i in 0..result.len() {
            for j in (i + 1)..result.len() {
                assert!(
                    result[i].distance(result[j]) >= DECLUMP_MIN_SEPARATION_PX - 0.01,
                    "Punkte {i} und {j} liegen noch zu nah beieinander: {:?} / {:?}",
                    result[i],
                    result[j]
                );
            }
        }
    }

    #[test]
    fn declump_ist_deterministisch() {
        let positions = vec![Pos2::new(10.0, 10.0); 5];
        assert_eq!(declump(&positions), declump(&positions));
    }
}
