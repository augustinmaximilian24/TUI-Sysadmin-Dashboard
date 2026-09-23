//! Länder als gefüllte Flächen (nicht mehr als Punktraster, siehe
//! [`super::world`], das dieses Modul ablöst): parst dieselbe eingebettete
//! GeoJSON-Datei, zerlegt jedes Land per Ear-Clipping-Triangulierung in
//! Dreiecke auf der Einheitskugel und liefert zusätzlich die
//! Außenkontur (für Ländergrenzen) und einen Beschriftungspunkt (für den
//! Ländernamen).
//!
//! Warum kein `egui`-Bordmittel: `epaint`s `PathShape`-Füllung
//! trianguliert über einen einfachen Fächer vom ersten Punkt aus (siehe
//! `epaint::tessellator::fill_closed_path`) -- das ist nur für konvexe
//! Formen korrekt. Eine echte Küstenlinie ist alles andere als konvex,
//! ein Fächer würde sichtbar falsche/überlappende Dreiecke erzeugen.
//! Deshalb hier eine eigene, getestete Ear-Clipping-Triangulierung.
//!
//! Läuft einmalig in einem Hintergrund-Thread (Regel 21, dasselbe Muster
//! wie [`super::world::build_dot_grid`]) -- die Dreiecke selbst ändern
//! sich danach nie mehr, nur ihre Sichtbarkeit (per Kamera-Rotation) wird
//! jeden Frame neu geprüft.

use serde::Deserialize;

use super::globe::{self, Vec3};

const WORLD_GEOJSON: &str = include_str!("../../assets/world_countries_110m.geo.json");

type Ring = Vec<[f64; 2]>;

#[derive(Deserialize)]
struct GeoJsonRoot {
    features: Vec<GeoJsonFeature>,
}

#[derive(Deserialize)]
struct GeoJsonFeature {
    /// ISO-3166-1-Alpha-3-Code (`"DEU"`, `"USA"`, ...) -- in dieser Datei
    /// auf der Feature-Ebene, nicht unter `properties`. Wird als
    /// Kartenlabel verwendet statt des ausgeschriebenen Namens: kürzer
    /// und eine allgemein bekannte Abkürzung, ohne dass dafür eine
    /// zusätzliche Name->Code-Zuordnung gepflegt werden müsste (anders
    /// als [`super::geoip::country_name`], das umgekehrt Alpha-2-Codes in
    /// Namen übersetzt und dafür eine eigene Tabelle mitbringt).
    id: String,
    geometry: GeoJsonGeometry,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "coordinates")]
enum GeoJsonGeometry {
    Polygon(Vec<Ring>),
    MultiPolygon(Vec<Vec<Ring>>),
    #[serde(other)]
    Other,
}

/// Ein Land: vorberechnete Füll-Dreiecke, Außenkonturen (für die
/// Ländergrenzen-Linie) und ein Punkt fürs Namens-Label -- alles bereits
/// als Punkte auf der Einheitskugel, nicht mehr als lat/lon.
pub struct Country {
    /// ISO-3166-1-Alpha-3-Code, siehe [`GeoJsonFeature::id`].
    pub code: String,
    pub triangles: Vec<[Vec3; 3]>,
    pub outline_rings: Vec<Vec<Vec3>>,
    pub label_point: Vec3,
}

/// Lädt und trianguliert alle Länder aus der eingebetteten GeoJSON. Eine
/// defekte/leere Datei liefert einfach eine leere Liste statt eines
/// Absturzes (Regel 16) -- die Kugel zeigt dann nur Wasser.
pub fn load_countries() -> Vec<Country> {
    let Ok(root) = serde_json::from_str::<GeoJsonRoot>(WORLD_GEOJSON) else {
        return Vec::new();
    };
    root.features.into_iter().filter_map(build_country).collect()
}

fn build_country(feature: GeoJsonFeature) -> Option<Country> {
    let polygons = match feature.geometry {
        GeoJsonGeometry::Polygon(rings) => vec![rings],
        GeoJsonGeometry::MultiPolygon(polygons) => polygons,
        GeoJsonGeometry::Other => return None,
    };
    if polygons.is_empty() {
        return None;
    }

    let mut triangles = Vec::new();
    let mut outline_rings = Vec::new();
    let mut label_sum = Vec3::new(0.0, 0.0, 0.0);
    let mut label_count = 0usize;

    for polygon in &polygons {
        // Löcher (z. B. Enklaven, Binnenseen) werden bewusst nicht von der
        // Füllfläche abgezogen -- in diesem Datensatz betrifft das genau
        // einen Ring von 180 Ländern (geprüft beim Erstellen dieses
        // Moduls), der optische Unterschied ist vernachlässigbar gegen
        // den Aufwand einer Loch-Verschneidung (Ring-Bridging).
        // ponytail: falls das doch mal auffällt, betroffenes Land in
        // `polygon[1..]` als echtes Loch aus den Dreiecken herausschneiden
        // statt es hier weiter zu ignorieren.
        let Some(outer) = polygon.first() else { continue };
        let unwrapped = unwrap_antimeridian(outer);
        for &[lon, lat] in &unwrapped {
            let p = globe::lat_lon_to_unit_sphere(lat as f32, lon as f32);
            label_sum = Vec3::new(label_sum.x + p.x, label_sum.y + p.y, label_sum.z + p.z);
            label_count += 1;
        }

        let sphere_ring: Vec<Vec3> = outer
            .iter()
            .map(|&[lon, lat]| globe::lat_lon_to_unit_sphere(lat as f32, lon as f32))
            .collect();
        outline_rings.push(sphere_ring);

        for [a, b, c] in triangulate_ring(&unwrapped) {
            let to_vec3 = |[lon, lat]: [f64; 2]| globe::lat_lon_to_unit_sphere(lat as f32, lon as f32);
            triangles.push([to_vec3(a), to_vec3(b), to_vec3(c)]);
        }
    }

    if label_count == 0 {
        return None;
    }
    let label_point = normalize(Vec3::new(
        label_sum.x / label_count as f32,
        label_sum.y / label_count as f32,
        label_sum.z / label_count as f32,
    ));

    Some(Country { code: feature.id, triangles, outline_rings, label_point })
}

fn normalize(v: Vec3) -> Vec3 {
    let len = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt().max(1e-6);
    Vec3::new(v.x / len, v.y / len, v.z / len)
}

/// Ringe, die die 180°-Datumsgrenze überqueren (z. B. Russland, Fidschi,
/// die Aleuten), springen in der Rohdatei von nahe +180° auf nahe -180°
/// -- eine flache 2D-Triangulierung in Grad würde das als riesigen
/// "Ohr"-Ausschlag quer über die ganze Karte missverstehen. Erkennung:
/// Punkte, die mehr als 180° vom ersten Punkt entfernt liegen, werden um
/// 360° verschoben, bis alle auf einer durchgehenden Seite liegen.
fn unwrap_antimeridian(ring: &Ring) -> Vec<[f64; 2]> {
    let Some(&[first_lon, _]) = ring.first() else {
        return Vec::new();
    };
    ring.iter()
        .map(|&[lon, lat]| {
            let mut lon = lon;
            while lon - first_lon > 180.0 {
                lon -= 360.0;
            }
            while lon - first_lon < -180.0 {
                lon += 360.0;
            }
            [lon, lat]
        })
        .collect()
}

fn signed_area(points: &[[f64; 2]]) -> f64 {
    let n = points.len();
    let mut area = 0.0;
    for i in 0..n {
        let [x0, y0] = points[i];
        let [x1, y1] = points[(i + 1) % n];
        area += x0 * y1 - x1 * y0;
    }
    area / 2.0
}

fn cross(o: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

fn point_in_triangle(p: [f64; 2], a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> bool {
    let d1 = cross(a, b, p);
    let d2 = cross(b, c, p);
    let d3 = cross(c, a, p);
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_neg && has_pos)
}

/// Ear-Clipping-Triangulierung eines einfachen (nicht selbstschneidenden)
/// Polygons in flachen 2D-Koordinaten (Länge/Breite -- für die
/// *Topologie* der Triangulierung reicht das; siehe Modul-Kommentar für
/// die Antimeridian-Behandlung davor). Liefert Dreiecke als
/// Punkt-Tripel. Bricht bei entartetem/selbstschneidendem Input sauber ab
/// (Regel 16) statt in eine Endlosschleife zu laufen -- ein bereits
/// unvollständig trianguliertes Land zeigt dann nur einen Teil seiner
/// Fläche gefüllt, stürzt aber nicht ab.
fn triangulate_ring(ring: &[[f64; 2]]) -> Vec<[[f64; 2]; 3]> {
    let mut points: Vec<[f64; 2]> = ring.to_vec();
    if points.len() >= 2 && points.first() == points.last() {
        points.pop();
    }
    if points.len() < 3 {
        return Vec::new();
    }
    if signed_area(&points) < 0.0 {
        points.reverse();
    }

    let mut triangles = Vec::new();
    // Harte Obergrenze an Iterationen statt einer reinen
    // `while points.len() > 3`-Schleife: verhindert eine Endlosschleife,
    // falls in einem einzelnen Durchlauf nie ein gültiges "Ohr" gefunden
    // wird (siehe unten).
    let mut guard = points.len() * points.len() + 8;
    while points.len() > 3 && guard > 0 {
        guard -= 1;
        let n = points.len();
        let mut clipped = false;
        for i in 0..n {
            let prev = points[(i + n - 1) % n];
            let curr = points[i];
            let next = points[(i + 1) % n];
            if cross(prev, curr, next) <= 0.0 {
                continue; // konkave Ecke, kann kein Ohr sein
            }
            let is_ear = !points
                .iter()
                .enumerate()
                .any(|(j, &p)| {
                    j != (i + n - 1) % n
                        && j != i
                        && j != (i + 1) % n
                        && point_in_triangle(p, prev, curr, next)
                });
            if is_ear {
                triangles.push([prev, curr, next]);
                points.remove(i);
                clipped = true;
                break;
            }
        }
        if !clipped {
            break; // entarteter/selbstschneidender Rest -- sauber abbrechen
        }
    }
    if points.len() == 3 {
        triangles.push([points[0], points[1], points[2]]);
    }
    triangles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triangulate_ring_quadrat_ergibt_zwei_dreiecke() {
        let square = vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0]];
        let triangles = triangulate_ring(&square);
        assert_eq!(triangles.len(), 2);
        let total_area: f64 = triangles
            .iter()
            .map(|t| signed_area(&[t[0], t[1], t[2]]).abs())
            .sum();
        assert!((total_area - 16.0).abs() < 1e-9, "Flächensumme war {total_area}");
    }

    #[test]
    fn triangulate_ring_l_form_bleibt_konkav_korrekt() {
        // L-Form: konkaves Polygon, ein naiver Fächer vom ersten Punkt
        // aus würde hier sichtbar falsch triangulieren.
        let l_shape = vec![
            [0.0, 0.0],
            [4.0, 0.0],
            [4.0, 2.0],
            [2.0, 2.0],
            [2.0, 4.0],
            [0.0, 4.0],
        ];
        let triangles = triangulate_ring(&l_shape);
        assert_eq!(triangles.len(), 4);
        let total_area: f64 = triangles
            .iter()
            .map(|t| signed_area(&[t[0], t[1], t[2]]).abs())
            .sum();
        // Fläche der L-Form: 4x4-Quadrat minus 2x2-Ecke = 16 - 4 = 12.
        assert!((total_area - 12.0).abs() < 1e-9, "Flächensumme war {total_area}");
    }

    #[test]
    fn triangulate_ring_geschlossener_ring_ohne_duplikat_gleiches_ergebnis() {
        let with_dup = vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0], [0.0, 0.0]];
        let without_dup = vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0]];
        assert_eq!(triangulate_ring(&with_dup).len(), triangulate_ring(&without_dup).len());
    }

    #[test]
    fn triangulate_ring_entartetes_polygon_bricht_sauber_ab() {
        // Alle Punkte auf einer Linie -- keine gültigen Ohren, darf nicht
        // hängen bleiben.
        let degenerate = vec![[0.0, 0.0], [1.0, 0.0], [2.0, 0.0], [3.0, 0.0]];
        let triangles = triangulate_ring(&degenerate);
        assert!(triangles.len() <= 2);
    }

    #[test]
    fn unwrap_antimeridian_verschiebt_sprung_ueber_180_grad() {
        let ring: Ring = vec![[179.0, 10.0], [-179.0, 10.0], [-179.0, 20.0], [179.0, 20.0]];
        let unwrapped = unwrap_antimeridian(&ring);
        let lons: Vec<f64> = unwrapped.iter().map(|p| p[0]).collect();
        let span = lons.iter().cloned().fold(f64::MIN, f64::max)
            - lons.iter().cloned().fold(f64::MAX, f64::min);
        assert!(span.abs() < 10.0, "Längengrade sollten nah beieinander liegen, Spanne: {span}");
    }

    #[test]
    fn unwrap_antimeridian_laesst_normale_ringe_unveraendert() {
        let ring: Ring = vec![[10.0, 50.0], [12.0, 50.0], [12.0, 52.0], [10.0, 52.0]];
        assert_eq!(unwrap_antimeridian(&ring), ring);
    }

    #[test]
    fn eingebettete_datei_liefert_plausibel_viele_laender() {
        let countries = load_countries();
        assert!(countries.len() > 100, "erwarte grob 180 Länder, waren {}", countries.len());
        assert!(countries.iter().any(|c| c.code == "DEU"));
        for country in &countries {
            assert!(!country.triangles.is_empty(), "{} hat keine Dreiecke", country.code);
        }
    }
}
