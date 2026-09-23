//! Drehbare 3D-Weltkugel im Systemzustands-Panel (Phase 12, optional):
//! zeigt, in welche Länder gerade aktive ausgehende TCP-Verbindungen
//! dieses Rechners gehen. Per Maus-Drag frei drehbar (siehe [`globe`],
//! dasselbe Kameramuster wie [`crate::knowledge_graph`]); dreht nach einer
//! Leerlaufzeit von selbst zur "Zuhause"-Ursprungsansicht zurück.
//!
//! Architektur (dasselbe Muster wie [`crate::knowledge_graph`]): ein
//! Hintergrund-`std::thread` scannt in `poll_interval_seconds`
//! `/proc/net/tcp` ([`connections`]) und löst jede neue Gegenstelle über
//! die lokale `GeoIP.dat` ([`geoip`]) in ein Land auf -- keine
//! Netzwerk-Anfrage pro Verbindung, keine Cloud-Abhängigkeit. Ergebnis
//! landet in `Arc<Mutex<Shared>>`, danach `ctx.request_repaint()`.
//! `show()` läuft auf dem Render-Thread, klont nur den kleinen aktuellen
//! Zustand und zeichnet daraus -- keine I/O im Render-Pfad (Regel 21).
//!
//! Ein **zweiter** Hintergrund-Thread misst optional den tatsächlichen Weg
//! zu jeder Gegenstelle ([`traceroute`]), sodass die Karte die
//! Zwischenstationen zeigt ("Zuhause -> Frankfurt -> Vereinigte Staaten ->
//! Ziel") statt einer geraden Linie. Das ist der einzige Teil dieses
//! Moduls, der selbst Pakete sendet -- deshalb per Konfiguration
//! abschaltbar (`traceroute_enabled`) und in einem eigenen Thread, damit
//! ein langsamer Messlauf die Verbindungsanzeige nicht ausbremst.

pub mod connections;
pub mod geoip;
mod globe;
mod land;
mod precise_location;
mod traceroute;

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke};

use geoip::CountryDb;
use globe::{GlobeCamera, Vec3};
use logsentry_core::config::NetworkMapConfig;

/// Zielrate für die Puls-/Zoom-Animation (Regel 20: reaktiv statt
/// Continuous-Modus).
const REPAINT_INTERVAL: Duration = Duration::from_millis(120);

/// Wie lange eine Gegenstelle nach dem letzten Sichten noch angezeigt
/// bleibt, bevor sie aus der Karte verschwindet -- ohne das würde jede
/// kurz geschlossene Verbindung sofort wieder verschwinden und die Karte
/// würde bei jedem Poll-Intervall unruhig flackern.
const CONNECTION_HOLD: Duration = Duration::from_secs(20);

/// Feste Höhe der scrollbaren Verbindungs-/Routenliste unter der Karte.
/// Seit dort unter jeder Verbindung ihr vollständiger Weg steht, ist sie
/// höher als zuvor -- alles darüber hinaus scrollt, statt die Karte zu
/// verdrängen (Regel 18).
const CONNECTION_LIST_HEIGHT_PX: f32 = 150.0;

/// Harte Obergrenze des Routen-Caches (Regel 18). Ist sie erreicht, kommen
/// keine neuen Routen mehr dazu -- die Karte zeichnet für weitere Ziele
/// dann wieder die direkte Linie, statt dass der Cache unbegrenzt wächst.
/// Bei typischerweise einigen Dutzend gleichzeitigen Gegenstellen wird das
/// im Alltag nicht erreicht.
const MAX_CACHED_ROUTES: usize = 256;

/// Wartezeit zwischen zwei Messläufen des Routen-Threads. Bewusst träge:
/// ein Messlauf sendet Pakete, und die Route eines Ziels ändert sich
/// ungleich seltener als die Verbindungsliste selbst.
const ROUTE_SCAN_INTERVAL: Duration = Duration::from_secs(3);

/// Ein Zwischenschritt auf dem Weg zu einer Gegenstelle (siehe
/// [`traceroute`]) -- so vollständig, wie er messbar war.
///
/// Auch Hops ohne Karten-Position werden behalten: ein Router, der nicht
/// antwortet, im lokalen Netz steht oder in der GeoIP-Datenbank fehlt, ist
/// trotzdem Teil des Weges. Er taucht in der Routenliste auf, nur eben
/// nicht als Punkt auf der Kugel.
#[derive(Clone)]
struct RouteHop {
    /// `None`, wenn der Hop nicht geantwortet hat.
    ip: Option<Ipv4Addr>,
    /// Kartenposition, `None` bei stillen, privaten oder nicht
    /// zuzuordnenden Adressen.
    position: Option<(f32, f32)>,
    /// Ort wie in [`ConnectionPoint::place`]: Stadt + Land, wenn
    /// Reverse-DNS etwas hergab, sonst der ausgeschriebene Ländername.
    /// `None`, wenn sich nicht einmal ein Land bestimmen ließ.
    place: Option<String>,
}

impl RouteHop {
    /// Eine Zeile für die Routenliste unter der Karte: Ort und Adresse,
    /// soweit bekannt. Ein stiller Hop wird ausdrücklich als solcher
    /// benannt, statt einfach zu fehlen.
    fn describe(&self) -> String {
        match (&self.place, self.ip) {
            (Some(place), Some(ip)) => format!("{place} ({ip})"),
            (None, Some(ip)) => format!("{ip} (Ort unbekannt)"),
            (_, None) => "keine Antwort".to_string(),
        }
    }
}

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
    /// Programm, das laut [`connections::resolve_program_names`] gerade
    /// diese Verbindung hält -- `None`, solange sich der Prozess nicht
    /// (mehr) zuordnen ließ (z. B. Verbindung eines fremden Benutzers,
    /// oder Prozess bereits beendet).
    program: Option<String>,
    /// Gemessener Weg zum Ziel (siehe [`traceroute`]), aus dem Routen-Cache
    /// übernommen. Leer, solange noch nicht gemessen -- dann zeichnet die
    /// Karte wie zuvor die direkte Verbindung. `Arc`, weil dieselbe Route
    /// bei jedem Frame mitkopiert wird und sich nie mehr ändert.
    route: Arc<Vec<RouteHop>>,
    last_seen: Instant,
}

impl ConnectionPoint {
    /// Ort als lesbarer Text für Tooltip, Karten-Label und die
    /// Verbindungsliste unter der Karte: Stadt + Land, wenn die
    /// Reverse-DNS-Nachschärfung etwas gefunden hat, sonst der
    /// ausgeschriebene Ländername, sonst (unbekannter Code) der rohe
    /// GeoIP-Code als letzter Rückfall.
    fn place(&self) -> String {
        let country = geoip::country_name(self.country).unwrap_or(self.country);
        match &self.precise_city {
            Some(city) => format!("{city}, {country}"),
            None => country.to_string(),
        }
    }
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
    /// Trianguliert Länder (siehe [`land`]), einmalig beim Start des
    /// Hintergrund-Threads berechnet -- ändert sich danach nie mehr, liegt
    /// aber im selben `Mutex` wie der Rest, weil es vom selben
    /// Hintergrund-Thread geschrieben und vom Render-Thread gelesen wird.
    countries: Arc<Vec<land::Country>>,
    /// Gemessene Route je Ziel-IP, gefüllt vom Routen-Thread (siehe
    /// [`spawn_route_watcher`]). Überdauert bewusst das Verschwinden einer
    /// Verbindung aus [`Self::points`] ([`CONNECTION_HOLD`]): eine
    /// flatternde Verbindung soll nicht immer wieder neu vermessen werden.
    /// Begrenzt durch [`MAX_CACHED_ROUTES`].
    routes: HashMap<Ipv4Addr, Arc<Vec<RouteHop>>>,
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
        // Ear-Clipping-Triangulierung aller ~180 Länder kann spürbar
        // dauern, ändert ihr Ergebnis danach aber nie wieder.
        let countries = Arc::new(land::load_countries());
        {
            let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
            guard.countries = countries;
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
            // Wie die Gegenstellen selbst: nicht unter der Sperre laufen
            // lassen (Regel 21) -- ein `/proc`-Scan über alle Prozesse
            // kostet spürbar mehr als der reine `/proc/net/tcp`-Read.
            let program_names = connections::resolve_program_names();

            let new_ips: Vec<Ipv4Addr> = {
                let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
                // Getrennte Feldreferenzen, damit `points` veränderlich und
                // `routes` gleichzeitig lesbar ist -- über den `MutexGuard`
                // selbst wären beide Zugriffe ein Borrow-Konflikt.
                let state = &mut *guard;
                state.points.retain(|_, p| now.duration_since(p.last_seen) < CONNECTION_HOLD);
                let routes = &state.routes;
                seen.into_iter()
                    .filter(|ip| match state.points.get_mut(ip) {
                        Some(point) => {
                            point.last_seen = now;
                            // Zum Sichtungszeitpunkt konnte der Prozess
                            // sein Fd evtl. noch nicht offen haben --
                            // bei jedem weiteren Poll nachschärfen, bis
                            // es klappt, statt dauerhaft unbekannt zu
                            // bleiben.
                            if point.program.is_none() {
                                point.program = program_names.get(ip).cloned();
                            }
                            // Dasselbe für die Route: sie wird erst
                            // Sekunden nach dem ersten Sichten gemessen
                            // (eigener Thread, siehe
                            // `spawn_route_watcher`).
                            if point.route.is_empty() {
                                if let Some(route) = routes.get(ip) {
                                    point.route = Arc::clone(route);
                                }
                            }
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
                            program: program_names.get(&ip).cloned(),
                            route: Arc::new(Vec::new()),
                            last_seen: now,
                        });
                    }
                    None => unresolved += 1,
                }
            }

            let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
            for mut point in newly_resolved {
                // Ein zuvor schon einmal vermessenes Ziel bekommt seine
                // Route sofort zurück, statt erneut gemessen zu werden.
                if let Some(route) = guard.routes.get(&point.ip) {
                    point.route = Arc::clone(route);
                }
                guard.points.insert(point.ip, point);
            }
            guard.unresolved_count = unresolved;
            drop(guard);
            ctx.request_repaint();
            std::thread::sleep(poll_interval);
        }
    });
}

/// Misst in einem **eigenen** Thread den Weg zu jeder neu gesehenen
/// Gegenstelle.
///
/// Bewusst getrennt vom Verbindungs-Thread: ein Messlauf dauert je nach
/// Strecke Sekunden bis zum Zeitdeckel, und die Verbindungsliste soll
/// derweil weiterlaufen statt einzufrieren. Es läuft immer nur **ein**
/// Messlauf gleichzeitig und danach eine Pause ([`ROUTE_SCAN_INTERVAL`]) --
/// bei einem Dutzend frischer Verbindungen sollen nicht ebenso viele
/// Messungen gleichzeitig los.
fn spawn_route_watcher(
    db_path: std::path::PathBuf,
    max_hops: u8,
    timeout_secs: u64,
    shared: Arc<Mutex<Shared>>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        // Eigene Instanz der Länderdatenbank statt einer geteilten: die
        // Datei ist wenige MB groß, und so braucht es keine Kopplung an
        // die Ladereihenfolge des Verbindungs-Threads. Ein Fehler wurde
        // dort bereits gemeldet -- hier bleibt es still, statt dieselbe
        // Meldung ein zweites Mal zu schreiben.
        let db = CountryDb::load(&db_path).ok();

        loop {
            let target = {
                let guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
                if guard.routes.len() >= MAX_CACHED_ROUTES {
                    None
                } else {
                    // Kleinste noch unvermessene IP zuerst -- nur damit
                    // die Reihenfolge deterministisch ist und nicht von
                    // der HashMap-Iterationsreihenfolge abhängt.
                    guard
                        .points
                        .keys()
                        .filter(|ip| !guard.routes.contains_key(ip))
                        .min()
                        .copied()
                }
            };

            let Some(target) = target else {
                std::thread::sleep(ROUTE_SCAN_INTERVAL);
                continue;
            };

            // Messlauf und Namensauflösung ohne gehaltene Sperre
            // (Regel 21): beides dauert Sekunden.
            let hops = traceroute::trace(target, max_hops, timeout_secs);
            let stops = Arc::new(resolve_route_hops(&hops, db.as_ref()));

            {
                let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
                let state = &mut *guard;
                // Auch eine leere Route wird eingetragen: sie bedeutet
                // "gemessen, nichts Verortbares dabei" und verhindert, dass
                // dasselbe Ziel bei jedem Durchlauf erneut vermessen wird.
                state.routes.insert(target, Arc::clone(&stops));
                if let Some(point) = state.points.get_mut(&target) {
                    point.route = stops;
                }
            }
            ctx.request_repaint();
            std::thread::sleep(ROUTE_SCAN_INTERVAL);
        }
    });
}

/// Verortet jeden gemessenen Hop, soweit möglich -- ohne einen davon
/// wegzulassen.
///
/// Bewusst **kein** Zusammenfassen gleicher Orte: mehrere Router im selben
/// Land sind mehrere echte Zwischenschritte, auch wenn sie mangels
/// Stadtauflösung auf denselben Länder-Mittelpunkt fallen. Auf der Karte
/// würden sie sonst verschwinden; in der Routenliste unter der Karte
/// stehen sie ohnehin einzeln.
fn resolve_route_hops(hops: &[traceroute::Hop], db: Option<&CountryDb>) -> Vec<RouteHop> {
    hops.iter()
        .map(|hop| {
            let Some(ip) = *hop else {
                return RouteHop { ip: None, position: None, place: None };
            };
            if !connections::is_routable_public(ip) {
                // Der eigene Router gehört zum Weg, hat aber keine
                // sinnvolle Position auf einer Weltkarte.
                return RouteHop {
                    ip: Some(ip),
                    position: None,
                    place: Some("lokales Netz".to_string()),
                };
            }
            let located = db
                .and_then(|db| db.lookup_v4(ip))
                .and_then(|code| geoip::country_centroid(code).map(|c| (code, c)));
            let Some((code, (country_lat, country_lon))) = located else {
                return RouteHop { ip: Some(ip), position: None, place: None };
            };
            let (lat, lon, city) = match precise_location::resolve_precise(ip) {
                Some((lat, lon, city)) => (lat, lon, Some(city)),
                None => (country_lat, country_lon, None),
            };
            let country = geoip::country_name(code).unwrap_or(code);
            let place = match city {
                Some(city) => format!("{city}, {country}"),
                None => country.to_string(),
            };
            RouteHop { ip: Some(ip), position: Some((lat, lon)), place: Some(place) }
        })
        .collect()
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

/// Fällt zurück auf ein Fleckchen Atlantik vor Westafrika, wenn kein
/// "Zuhause" ermittelt werden konnte (siehe [`guess_home_country`]) --
/// dieselbe Ausgangs-Ausrichtung, die die Karte vor der Umstellung auf die
/// Kugel als Kartenmittelpunkt hatte, nur jetzt als Kamera-Ursprung.
const FALLBACK_HOME_LAT_LON: (f32, f32) = (20.0, 10.0);

pub struct NetworkMapPanel {
    shared: Arc<Mutex<Shared>>,
    camera: GlobeCamera,
    ease_per_second: f32,
    idle_resume_secs: f32,
    drag_sensitivity_deg_per_px: f32,
    zoom_sensitivity: f32,
    home: Option<(String, f32, f32)>,
    last_interaction: Instant,
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
        if config.traceroute_enabled {
            spawn_route_watcher(
                std::path::PathBuf::from(&config.geoip_database_path),
                config.traceroute_max_hops,
                config.traceroute_timeout_seconds.max(1),
                Arc::clone(&shared),
                ctx.clone(),
            );
        }

        let home_code = if config.home_country_override.is_empty() {
            guess_home_country()
        } else {
            Some(config.home_country_override.to_uppercase())
        };
        // Genaue Koordinaten aus der Konfiguration haben Vorrang vor dem
        // Länder-Mittelpunkt: der liegt je nach Land weit vom
        // tatsächlichen Anschluss entfernt, und alle Routen beginnen an
        // diesem Punkt.
        let home = home_code.and_then(|code| {
            match (config.home_latitude, config.home_longitude) {
                (Some(lat), Some(lon)) => Some((code, lat, lon)),
                _ => geoip::country_centroid(&code).map(|(lat, lon)| (code, lat, lon)),
            }
        });

        let (home_lat, home_lon) =
            home.as_ref().map(|(_, lat, lon)| (*lat, *lon)).unwrap_or(FALLBACK_HOME_LAT_LON);

        Self {
            shared,
            camera: GlobeCamera::centered_on(home_lat, home_lon),
            ease_per_second: config.camera_ease_per_second.max(0.05),
            idle_resume_secs: config.idle_resume_secs.max(0.0),
            drag_sensitivity_deg_per_px: config.drag_sensitivity_deg_per_px,
            zoom_sensitivity: config.zoom_sensitivity,
            home,
            last_interaction: Instant::now(),
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

        let (points, error, unresolved, countries) = {
            let guard = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
            let mut points: Vec<ConnectionPoint> = guard.points.values().cloned().collect();
            points.sort_by_key(|a| a.ip);
            (
                points,
                guard.error.clone(),
                guard.unresolved_count,
                Arc::clone(&guard.countries),
            )
        };

        if let Some(err) = &error {
            ui.colored_label(theme_warn(), err);
        }

        // `available_height` ist vor der Überschrift und der Fußzeile
        // gemessen -- deren Platz hier grob abziehen, statt dass die Karte
        // über den freien Bereich hinausragt. Die Fußzeile besteht aus der
        // Zusammenfassungszeile ("N aktive Verbindungen") plus der
        // Verbindungsliste (Programm -> Ort), die deshalb eine feste,
        // scrollbare Höhe statt unbegrenzten Wachstums bekommt (siehe
        // `CONNECTION_LIST_HEIGHT_PX` unten) -- sonst schiebt eine lange
        // Verbindungsliste die "Details"-Knöpfe der Karte quasi genauso aus
        // dem sichtbaren Panel wie die Anomalien-Grid-Spalte es vor dem Fix
        // vom 2026-09-21 tat.
        const CHROME_RESERVE_PX: f32 = 48.0;
        let desired_height =
            (available_height - CHROME_RESERVE_PX - CONNECTION_LIST_HEIGHT_PX).max(220.0);
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), desired_height),
            Sense::click_and_drag(),
        );

        if response.dragged() {
            let delta = response.drag_delta();
            self.camera.apply_drag(delta.x, delta.y, self.drag_sensitivity_deg_per_px);
            if dt > 0.0 {
                self.camera.set_drag_velocity(delta.x / dt, delta.y / dt, self.drag_sensitivity_deg_per_px);
            }
            self.last_interaction = now;
        } else if self.camera.has_momentum() {
            // Freies Nachdrehen nach einem "Wurf" (Maus mit Schwung
            // losgelassen) -- klingt exponentiell ab
            // (`GlobeCamera::apply_momentum`) statt hart zu stoppen, fühlt
            // sich beim Drehen der Kugel deutlich flüssiger an. Zählt
            // weiter als Interaktion, damit die Leerlauf-Rückkehr zur
            // Ursprungsansicht erst einsetzt, wenn der Schwung ausgeklungen
            // ist, statt beide Bewegungen gegeneinander laufen zu lassen.
            self.camera.apply_momentum(dt);
            self.last_interaction = now;
        }

        if response.hovered() {
            let scroll = ui.ctx().input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > f32::EPSILON {
                self.camera.apply_zoom(scroll, self.zoom_sensitivity);
                self.last_interaction = now;
            }
        }

        let idle_for = now.duration_since(self.last_interaction).as_secs_f32();
        if idle_for >= self.idle_resume_secs {
            self.camera.ease_towards_origin(self.ease_per_second, dt);
        }

        self.hovered_ip = None;
        self.draw_map(ui, rect, &response, &points, &countries);

        // Während aktivem Drag oder Nachdreh-Schwung sofort den nächsten
        // Frame anfordern statt bis zu `REPAINT_INTERVAL` zu warten --
        // sonst wirkt die Drehung ruckelig, weil sie auf die dort
        // gedrosselte Leerlauf-Rate begrenzt wäre (Regel 20 gilt für den
        // Leerlauf, nicht für aktive Interaktion).
        if response.dragged() || self.camera.has_momentum() {
            ui.ctx().request_repaint();
        }

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

        // Nicht nur die Anzahl, sondern je Verbindung, welches Programm
        // wohin verbunden ist -- z. B. "Steam -> zu Server: 1.2.3.4 in
        // Vereinigte Staaten". `points` ist bereits nach IP sortiert
        // (siehe `show`), damit die Liste zwischen Frames nicht umspringt.
        // Feste, scrollbare Höhe statt der Liste unbegrenzt wachsen zu
        // lassen (Regel 18: keine unbeschränkt wachsende Fläche).
        egui::ScrollArea::vertical()
            .id_salt("network_map_connection_list")
            .max_height(CONNECTION_LIST_HEIGHT_PX)
            .show(ui, |ui| {
                for point in points {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} → zu Server: {} in {}",
                            point.program.as_deref().unwrap_or("Unbekanntes Programm"),
                            point.ip,
                            point.place(),
                        ))
                        .color(crate::theme::TEXT_MUTED)
                        .size(11.0),
                    );
                    // Der vollständige gemessene Weg, eingerückt unter der
                    // Verbindung: die Karte kann nur Hops mit bekannter
                    // Position zeigen, hier stehen auch die stillen und
                    // die nicht zuzuordnenden.
                    for (index, hop) in point.route.iter().enumerate() {
                        ui.label(
                            egui::RichText::new(format!(
                                "        {}. {}",
                                index + 1,
                                hop.describe()
                            ))
                            .color(MAP_HOP)
                            .size(10.0),
                        );
                    }
                }
            });

        ui.ctx().request_repaint_after(REPAINT_INTERVAL);
    }

    fn draw_map(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        response: &egui::Response,
        points: &[ConnectionPoint],
        countries: &[land::Country],
    ) {
        let painter = ui.painter_at(rect);
        painter.rect_stroke(rect, 4.0, Stroke::new(1.0_f32, crate::theme::BORDER));

        let center = rect.center();
        // Kugelradius knapp unter der halben kürzeren Kantenlänge, damit
        // etwas Luft zum Panelrand bleibt, skaliert mit dem Scroll-Zoom
        // (`GlobeCamera::zoom`). Ein herausgezoomter Radius lässt einfach
        // mehr Panel-Hintergrund um die Kugel stehen, ein hereingezoomter
        // wird von `ui.painter_at(rect)` unten am Panelrand sauber
        // abgeschnitten statt das Layout zu sprengen.
        let radius = rect.width().min(rect.height()) / 2.0 * 0.92 * self.camera.zoom;
        let view = globe::GlobeView { camera: self.camera, center: (center.x, center.y), radius_px: radius };

        // Kugel-Silhouette als "Wasser"-Grundfläche -- alles danach
        // Gezeichnete liegt darüber.
        painter.circle_filled(center, radius, MAP_OCEAN);
        painter.circle_stroke(center, radius, Stroke::new(1.0_f32, MAP_LIMB));

        // Länder als gefüllte Fläche: alle sichtbaren Dreiecke (siehe
        // `land`) landen in einem einzigen `Mesh` statt in vielen
        // einzelnen `convex_polygon`-Aufrufen -- so entstehen keine
        // sichtbaren Nähte zwischen benachbarten Dreiecken derselben
        // Landmasse. Ein Dreieck mit mindestens einer rückseitigen Ecke
        // wird einfach verworfen statt exakt am Sichtkreis geclippt --
        // der dadurch leicht gezackte Rand ist bei der Dreiecksgröße
        // dieses Datensatzes nicht auffällig.
        let mut land_mesh = egui::Mesh::default();
        for country in countries {
            for tri in &country.triangles {
                let (ax, ay, a_vis) = view.project(tri[0]);
                let (bx, by, b_vis) = view.project(tri[1]);
                let (cx, cy, c_vis) = view.project(tri[2]);
                if !(a_vis && b_vis && c_vis) {
                    continue;
                }
                let idx = land_mesh.vertices.len() as u32;
                land_mesh.colored_vertex(Pos2::new(ax, ay), MAP_LAND);
                land_mesh.colored_vertex(Pos2::new(bx, by), MAP_LAND);
                land_mesh.colored_vertex(Pos2::new(cx, cy), MAP_LAND);
                land_mesh.add_triangle(idx, idx + 1, idx + 2);
            }
        }
        if !land_mesh.is_empty() {
            painter.add(egui::Shape::mesh(land_mesh));
        }

        // Ländergrenzen: die echte Außenkontur jedes Landes, nicht die
        // inneren Kanten der Fülldreiecke -- dieselbe
        // Sichtkante-Abbruch-Technik wie bei den Verbindungsrouten
        // (`draw_connection_route`), nur ohne Wölbung, weil hier schon die
        // tatsächliche Geometrie vorliegt statt einer synthetischen Kurve.
        // Länder-Codes (ISO-3166-1-Alpha-3, z. B. "DEU") statt
        // ausgeschriebener Namen -- kompakter, weniger Clutter bei bis zu
        // 180 gleichzeitig sichtbaren Labels. Erst gesammelt, nicht sofort
        // gezeichnet -- sie sollen
        // beim gemeinsamen Entzerren (`declutter_labels` unten) den
        // Verbindungs-Labels den Vortritt lassen, nicht umgekehrt.
        let mut country_label_anchors = Vec::new();
        let mut country_label_texts = Vec::new();
        for country in countries {
            for ring in &country.outline_rings {
                draw_horizon_clipped_line(&painter, &view, ring, true, MAP_BORDER);
            }

            let (x, y, visible) = view.project(country.label_point);
            if visible {
                country_label_anchors.push(Pos2::new(x, y));
                country_label_texts.push(country.code.clone());
            }
        }

        let home_point = self
            .home
            .as_ref()
            .map(|(code, lat, lon)| (code.clone(), globe::lat_lon_to_unit_sphere(*lat, *lon)));

        // Wie bei `declump`/`declutter_labels` zuvor: erst alle sichtbaren
        // Ziele einsammeln (samt Bögen und Markern), Labels danach in
        // einem zweiten Durchgang überlappungsfrei platzieren.
        let mut label_anchors = Vec::new();
        let mut label_texts = Vec::new();
        let mut visible_targets: Vec<(&ConnectionPoint, Pos2)> = Vec::new();
        // Jeder verortbare Hop bekommt einen eigenen Marker -- mehrere
        // Router im selben Land sind mehrere echte Zwischenschritte.
        // Beschriftet wird dagegen je Ort nur einmal: fast alle
        // Verbindungen dieses Anschlusses teilen sich die ersten Hops, und
        // fünfmal derselbe Ortsname übereinander wäre unlesbar.
        let mut hop_positions: Vec<Pos2> = Vec::new();
        let mut hop_labels: Vec<(Pos2, &str)> = Vec::new();
        let mut seen_hop_places: std::collections::HashSet<&str> = std::collections::HashSet::new();

        for point in points {
            let target_sphere = globe::lat_lon_to_unit_sphere(point.lat, point.lon);
            let (x, y, target_visible) = view.project(target_sphere);

            if let Some((_, home_sphere)) = &home_point {
                let stops = route_waypoints(&point.route, point.ip);
                let mut waypoints = Vec::with_capacity(stops.len() + 2);
                waypoints.push(*home_sphere);
                waypoints.extend(
                    stops.iter().map(|(lat, lon, _)| globe::lat_lon_to_unit_sphere(*lat, *lon)),
                );
                waypoints.push(target_sphere);

                // Bewusst unabhängig davon, ob das Ziel selbst sichtbar
                // ist: liegt es hinter der Kugel, ist der vordere Teil der
                // Route trotzdem zu sehen (die Linie bricht an der
                // Sichtkante von selbst ab, siehe `draw_visible_runs`).
                draw_connection_route(&painter, &view, &waypoints, self.pulse_phase);

                for ((_, _, place), sphere) in stops.iter().zip(&waypoints[1..]) {
                    let (hx, hy, hop_visible) = view.project(*sphere);
                    if !hop_visible {
                        continue;
                    }
                    let pos = Pos2::new(hx, hy);
                    hop_positions.push(pos);
                    if seen_hop_places.insert(place) {
                        hop_labels.push((pos, place));
                    }
                }
            }

            if target_visible {
                visible_targets.push((point, Pos2::new(x, y)));
            }
        }

        // Unter den Ziel-Markern gezeichnet: eine Zwischenstation ist der
        // Weg, nicht das Ergebnis. `declump` zieht Hops auseinander, die
        // auf demselben Länder-Mittelpunkt liegen -- sonst wären mehrere
        // aufeinanderfolgende Router eines Landes ein einziger Punkt.
        for pos in declump(&hop_positions) {
            painter.circle_filled(pos, 2.0, MAP_HOP);
        }

        // Bögen liegen unter den Markern, Marker unter ihren Labels --
        // deshalb erst jetzt, in einem eigenen Durchgang, Punkte und
        // Label-Anker sammeln.
        let screen_positions = declump(&visible_targets.iter().map(|(_, p)| *p).collect::<Vec<_>>());
        for ((point, _), &target) in visible_targets.iter().zip(&screen_positions) {
            painter.circle_filled(target, 3.0, crate::theme::ACCENT);
            painter.circle_stroke(target, 3.0, Stroke::new(1.0_f32, MAP_OCEAN));
            label_anchors.push(target);
            label_texts.push(point.place());

            let hover_rect = Rect::from_center_size(target, egui::vec2(12.0, 12.0));
            if response.hovered() {
                if let Some(mouse) = response.hover_pos() {
                    if hover_rect.contains(mouse) {
                        self.hovered_ip = Some(point.ip);
                    }
                }
            }
        }

        if let Some((code, home_sphere)) = &home_point {
            let (x, y, visible) = view.project(*home_sphere);
            if visible {
                let p = Pos2::new(x, y);
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

        // Verbindungs-Labels zuerst einreihen (siehe oben: sie sollen ihren
        // bevorzugten Platz behalten), Länder-Labels danach -- Reihenfolge
        // in `declutter_labels` entscheidet, wer bei Überlappung zuerst
        // seinen Wunschplatz bekommt.
        let connection_label_count = label_anchors.len();
        for (pos, place) in &hop_labels {
            label_anchors.push(*pos);
            label_texts.push((*place).to_string());
        }
        let hop_label_end = label_anchors.len();
        label_anchors.extend(country_label_anchors);
        label_texts.extend(country_label_texts);

        for (i, (pos, text)) in declutter_labels(&label_anchors, &label_texts)
            .into_iter()
            .zip(&label_texts)
            .enumerate()
        {
            if i < connection_label_count {
                painter.text(
                    pos,
                    egui::Align2::LEFT_CENTER,
                    text,
                    egui::FontId::proportional(9.5),
                    crate::theme::TEXT_HIGH_CONTRAST,
                );
            } else if i < hop_label_end {
                draw_text_with_halo(
                    &painter,
                    pos,
                    text,
                    egui::FontId::proportional(9.0),
                    MAP_HOP,
                    MAP_LABEL_HALO,
                );
            } else {
                draw_text_with_halo(
                    &painter,
                    pos,
                    text,
                    egui::FontId::proportional(9.0),
                    MAP_LABEL_TEXT,
                    MAP_LABEL_HALO,
                );
            }
        }

        if let Some(ip) = self.hovered_ip {
            if let Some((point, &pos)) = visible_targets
                .iter()
                .map(|(p, _)| *p)
                .zip(&screen_positions)
                .find(|(p, _)| p.ip == ip)
            {
                egui::show_tooltip_at(
                    ui.ctx(),
                    ui.layer_id(),
                    egui::Id::new(("network_map_tooltip", point.ip)),
                    pos,
                    |ui| {
                        ui.label(format!(
                            "{} ({}, {})",
                            point.program.as_deref().unwrap_or("unbekanntes Programm"),
                            point.ip,
                            point.place()
                        ));
                    },
                );
            }
        }
    }
}

/// Wasserfarbe, direkt aus dem echten `mapbox/dark-v11`-Style-JSON
/// (Layer `water`, `hsl(0, 0%, 12%)`) übernommen statt geschätzt -- der
/// Nutzer wollte explizit genau diesen Stil.
const MAP_OCEAN: Color32 = Color32::from_rgb(0x1f, 0x1f, 0x1f);

/// Landmasse-Farbe: nah am `dark-v11`-Landlayer (`hsl(0, 0%, 16%)`), aber
/// leicht angehoben, damit die durchgezogene Füllfläche gegenüber dem
/// Wasser (`MAP_OCEAN`, `hsl(0, 0%, 12%)`) klar als eigene Fläche lesbar
/// bleibt.
const MAP_LAND: Color32 = Color32::from_rgb(0x33, 0x33, 0x33);

/// Kugelrand ("Limb") -- aus `dark-v11`s Ländergrenzfarbe
/// (`admin-0-boundary`, `hsl(0, 0%, 41%)`), gedimmt, als dezenter
/// Silhouetten-Ring statt als volle Kontur.
const MAP_LIMB: Color32 = Color32::from_rgb(0x35, 0x35, 0x35);

/// Ländergrenzen-Farbe -- direkt aus `dark-v11`s `admin-0-boundary`-Layer
/// (`hsl(0, 0%, 41%)`), hier in voller Deckkraft statt gedimmt wie
/// [`MAP_LIMB`], da sie als eigene Informationsebene (Landesgrenzen)
/// erkennbar sein soll.
const MAP_BORDER: Color32 = Color32::from_rgb(0x69, 0x69, 0x69);

/// Ländernamen-Textfarbe, aus `dark-v11`s `settlement-major-label`-Layer
/// (`hsl(0, 0%, 66%)`).
const MAP_LABEL_TEXT: Color32 = Color32::from_rgb(0xa8, 0xa8, 0xa8);

/// Zwischenstationen einer Route: erkennbar der Akzentfarbe der
/// Verbindungen zugehörig, aber deutlich zurückgenommen -- der Weg soll
/// die Endpunkte nicht überstrahlen.
const MAP_HOP: Color32 = Color32::from_rgb(0x4e, 0x8f, 0x8c);

/// Halo hinter den Ländernamen (siehe [`draw_text_with_halo`]), aus
/// demselben Layer (`hsl(0, 0%, 3%)`) -- ohne Halo wären die vielen
/// Labels über wechselnd hellem/dunklem Untergrund streckenweise
/// unlesbar.
const MAP_LABEL_HALO: Color32 = Color32::from_rgb(0x08, 0x08, 0x08);


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

/// Zeilenhöhe und (grob geschätzte, proportionale) Zeichenbreite für die
/// Kollisionsprüfung in [`declutter_labels`] -- keine echte Font-Metrik
/// (die hängt vom `egui::Painter` ab und ist dort erst beim Zeichnen
/// verfügbar), sondern eine bewusst leicht großzügige Schätzung, die
/// lieber ein Label zu früh verschiebt als eine Überlappung zu übersehen.
const LABEL_ROW_HEIGHT_PX: f32 = 12.0;
const LABEL_CHAR_WIDTH_PX: f32 = 5.4;

/// Schiebt Orts-Labels, deren geschätzte Textfläche eine bereits platzierte
/// überlappen würde, zeilenweise nach unten -- sonst verschmelzen die
/// Namen mehrerer Verbindungen in derselben Region (z. B. drei
/// US-Server) zu unlesbarem Text. `anchors`/`texts` müssen gleich lang
/// sein und in derselben Reihenfolge stehen wie die zugehörigen
/// Kartenpunkte, damit die Zuordnung beim Zeichnen stimmt.
fn declutter_labels(anchors: &[Pos2], texts: &[String]) -> Vec<Pos2> {
    let mut placed_rects: Vec<Rect> = Vec::with_capacity(anchors.len());
    let mut positions = Vec::with_capacity(anchors.len());
    for (anchor, text) in anchors.iter().zip(texts) {
        let width = (text.chars().count() as f32 * LABEL_CHAR_WIDTH_PX).max(1.0);
        let mut pos = *anchor + egui::vec2(6.0, 1.0);
        loop {
            let label_rect = Rect::from_center_size(pos, egui::vec2(width, LABEL_ROW_HEIGHT_PX));
            if !placed_rects.iter().any(|r| r.intersects(label_rect)) {
                placed_rects.push(label_rect);
                positions.push(pos);
                break;
            }
            pos.y += LABEL_ROW_HEIGHT_PX;
        }
    }
    positions
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

/// Wie hoch sich ein Verbindungsbogen über die Kugeloberfläche wölbt, als
/// Anteil des Kugelradius (siehe [`globe::arc_point`]).
const ARC_BULGE: f32 = 0.18;

/// Zeichnet den Weg zu einer Gegenstelle als Kette über die Kugel
/// gewölbter Großkreis-Abschnitte (siehe [`globe::arc_point`]), mit einem
/// über die **gesamte** Route laufenden Lichtpunkt.
///
/// `waypoints` ist die vollständige Kette Zuhause -> Zwischenstationen ->
/// Ziel. Ohne gemessene Zwischenstationen (Messung deaktiviert, noch nicht
/// gelaufen oder kein Hop verortbar) sind das schlicht zwei Punkte und es
/// entsteht wieder die frühere direkte Verbindung.
///
/// Läuft ein Abschnitt über die Sichtkante der Kugel, bricht die
/// gezeichnete Linie dort ab, statt (falsch) quer durch die Kugel hindurch
/// weiterzulaufen -- [`globe::Rotated::visible`] wird pro Stützpunkt
/// geprüft, sichtbare Abschnitte werden als eigene Teilstrecken gezeichnet.
fn draw_connection_route(
    painter: &egui::Painter,
    view: &globe::GlobeView,
    waypoints: &[Vec3],
    phase: f32,
) {
    const STEPS_PER_SEGMENT: usize = 24;
    let Some(segments) = waypoints.len().checked_sub(1).filter(|&n| n > 0) else {
        return;
    };
    let stroke = Stroke::new(1.0_f32, crate::theme::ACCENT.gamma_multiply(0.4));

    for pair in waypoints.windows(2) {
        let bulge = globe::segment_bulge(pair[0], pair[1], ARC_BULGE);
        let samples: Vec<(Pos2, bool)> = (0..=STEPS_PER_SEGMENT)
            .map(|i| {
                let t = i as f32 / STEPS_PER_SEGMENT as f32;
                let point = globe::arc_point(pair[0], pair[1], t, bulge);
                let (x, y, visible) = view.project(point);
                (Pos2::new(x, y), visible)
            })
            .collect();
        draw_visible_runs(painter, &samples, stroke);
    }

    // Ein Lichtpunkt für die ganze Strecke statt einer je Abschnitt: er
    // soll den Weg entlangwandern, nicht überall gleichzeitig blinken.
    let progress = (phase / std::f32::consts::TAU).fract() * segments as f32;
    let index = (progress as usize).min(segments - 1);
    let (from, to) = (waypoints[index], waypoints[index + 1]);
    let bulge = globe::segment_bulge(from, to, ARC_BULGE);
    let point = globe::arc_point(from, to, progress - index as f32, bulge);
    let (x, y, visible) = view.project(point);
    if visible {
        painter.circle_filled(Pos2::new(x, y), 2.0, crate::theme::ACCENT);
    }
}

/// Die zeichenbaren Zwischenstationen einer Route: alle Hops mit bekannter
/// Position, außer dem Ziel selbst.
///
/// Das Ziel steht als letzter Hop in jeder Messung und wird vom Aufrufer
/// ohnehin als Endpunkt gezeichnet -- ohne diesen Ausschluss läge ein
/// Wegpunkt exakt unter dem Ziel-Marker. Verglichen wird dafür die
/// Adresse, nicht der Ortsname: ein Hop in derselben Stadt wie das Ziel
/// ist ein echter eigener Zwischenschritt und bleibt erhalten.
fn route_waypoints(route: &[RouteHop], target: Ipv4Addr) -> Vec<(f32, f32, &str)> {
    route
        .iter()
        .filter(|hop| hop.ip != Some(target))
        .filter_map(|hop| {
            let (lat, lon) = hop.position?;
            Some((lat, lon, hop.place.as_deref().unwrap_or("unbekannter Ort")))
        })
        .collect()
}

/// Projiziert eine Punktfolge auf der Kugel und zeichnet sie als Linie,
/// die an der Sichtkante abbricht (siehe [`draw_visible_runs`]) --
/// dieselbe Technik wie [`draw_connection_route`], hier aber ohne Wölbung
/// (`globe::arc_point`), weil `points_3d` schon die tatsächliche
/// Ländergrenzen-Geometrie ist statt einer synthetischen Kurve.
/// `closed`: verbindet zusätzlich den letzten mit dem ersten Punkt (für
/// geschlossene Ringe).
fn draw_horizon_clipped_line(
    painter: &egui::Painter,
    view: &globe::GlobeView,
    points_3d: &[Vec3],
    closed: bool,
    color: Color32,
) {
    if points_3d.len() < 2 {
        return;
    }
    let mut samples: Vec<(Pos2, bool)> = points_3d
        .iter()
        .map(|&p| {
            let (x, y, visible) = view.project(p);
            (Pos2::new(x, y), visible)
        })
        .collect();
    if closed {
        samples.push(samples[0]);
    }
    draw_visible_runs(painter, &samples, Stroke::new(1.0_f32, color));
}

/// Zerlegt eine Folge projizierter Punkte in zusammenhängende sichtbare
/// Teilstrecken und zeichnet jede als eigene Linie -- läuft die Linie über
/// die Sichtkante der Kugel (`globe::Rotated::visible`), bricht sie dort
/// sauber ab, statt (falsch) quer durch die Kugel hindurch weiterzulaufen.
fn draw_visible_runs(painter: &egui::Painter, samples: &[(Pos2, bool)], stroke: Stroke) {
    let mut run: Vec<Pos2> = Vec::new();
    for &(pos, visible) in samples {
        if visible {
            run.push(pos);
        } else if run.len() >= 2 {
            painter.add(egui::Shape::line(std::mem::take(&mut run), stroke));
        } else {
            run.clear();
        }
    }
    if run.len() >= 2 {
        painter.add(egui::Shape::line(run, stroke));
    }
}

/// Zeichnet Text mit einem einfarbigen Rand ("Halo") dahinter -- vier
/// leicht versetzte Kopien in der Halo-Farbe, dann der eigentliche Text
/// obenauf. `egui::Painter::text` kennt keinen eingebauten Textrand; ohne
/// das wären die bis zu 180 Länder-Labels über wechselnd hellem/dunklem
/// Untergrund streckenweise unlesbar.
fn draw_text_with_halo(
    painter: &egui::Painter,
    pos: Pos2,
    text: &str,
    font: egui::FontId,
    color: Color32,
    halo_color: Color32,
) {
    let offsets = [egui::vec2(-1.0, 0.0), egui::vec2(1.0, 0.0), egui::vec2(0.0, -1.0), egui::vec2(0.0, 1.0)];
    for offset in offsets {
        painter.text(pos + offset, egui::Align2::LEFT_CENTER, text, font.clone(), halo_color);
    }
    painter.text(pos, egui::Align2::LEFT_CENTER, text, font, color);
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

    #[test]
    fn declutter_labels_verschiebt_ueberlappende_beschriftungen() {
        let anchors = vec![Pos2::new(100.0, 100.0), Pos2::new(102.0, 100.0), Pos2::new(98.0, 101.0)];
        let texts = vec!["United States".to_string(); 3];
        let result = declutter_labels(&anchors, &texts);

        let width = "United States".chars().count() as f32 * LABEL_CHAR_WIDTH_PX;
        let rects: Vec<Rect> = result
            .iter()
            .map(|&p| Rect::from_center_size(p, egui::vec2(width, LABEL_ROW_HEIGHT_PX)))
            .collect();
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                assert!(!rects[i].intersects(rects[j]), "Labels {i} und {j} überlappen noch");
            }
        }
    }

    fn hop(ip: [u8; 4], place: Option<&str>, position: Option<(f32, f32)>) -> RouteHop {
        RouteHop {
            ip: Some(Ipv4Addr::from(ip)),
            position,
            place: place.map(str::to_string),
        }
    }

    #[test]
    fn route_waypoints_laesst_das_ziel_selbst_aus() {
        let route = vec![
            hop([62, 155, 242, 122], Some("Deutschland"), Some((51.0, 9.0))),
            hop([8, 8, 8, 8], Some("Vereinigte Staaten"), Some((38.0, -97.0))),
        ];
        let result = route_waypoints(&route, Ipv4Addr::new(8, 8, 8, 8));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].2, "Deutschland");
    }

    #[test]
    fn route_waypoints_behaelt_mehrere_hops_am_selben_ort() {
        // Kern der Anforderung "alle Hops zeigen": drei Router im selben
        // Land sind drei Zwischenschritte, nicht einer.
        let route = vec![
            hop([62, 155, 242, 122], Some("Deutschland"), Some((51.0, 9.0))),
            hop([80, 157, 207, 46], Some("Deutschland"), Some((51.0, 9.0))),
            hop([80, 157, 131, 165], Some("Deutschland"), Some((51.0, 9.0))),
        ];
        assert_eq!(route_waypoints(&route, Ipv4Addr::new(8, 8, 8, 8)).len(), 3);
    }

    #[test]
    fn route_waypoints_ueberspringt_hops_ohne_position() {
        let route = vec![
            RouteHop { ip: None, position: None, place: None },
            hop([192, 168, 178, 1], Some("lokales Netz"), None),
            hop([62, 155, 242, 122], Some("Deutschland"), Some((51.0, 9.0))),
        ];
        assert_eq!(route_waypoints(&route, Ipv4Addr::new(8, 8, 8, 8)).len(), 1);
    }

    #[test]
    fn resolve_route_hops_behaelt_jeden_hop_auch_ohne_verortung() {
        // Ohne GeoIP-Datenbank ist keine Position bestimmbar -- die Hops
        // selbst müssen trotzdem alle erhalten bleiben, sonst fehlen sie
        // auch in der Routenliste.
        let hops = vec![
            Some(Ipv4Addr::new(192, 168, 178, 1)),
            None,
            Some(Ipv4Addr::new(8, 8, 8, 8)),
        ];
        let resolved = resolve_route_hops(&hops, None);
        assert_eq!(resolved.len(), 3);
        assert!(resolved.iter().all(|h| h.position.is_none()));
        assert_eq!(resolved[0].place.as_deref(), Some("lokales Netz"));
        assert_eq!(resolved[1].describe(), "keine Antwort");
        assert_eq!(resolved[2].describe(), "8.8.8.8 (Ort unbekannt)");
    }

    #[test]
    fn declutter_labels_laesst_weit_entfernte_labels_an_ihrem_anker() {
        let anchors = vec![Pos2::new(0.0, 0.0), Pos2::new(500.0, 500.0)];
        let texts = vec!["Berlin".to_string(), "Tokio".to_string()];
        let result = declutter_labels(&anchors, &texts);
        assert_eq!(result, vec![anchors[0] + egui::vec2(6.0, 1.0), anchors[1] + egui::vec2(6.0, 1.0)]);
    }
}
