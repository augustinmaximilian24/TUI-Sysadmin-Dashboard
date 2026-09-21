//! Landmassen als Punktraster für den Kartenhintergrund: statt eines
//! leeren Koordinatengitters zeigt die Karte, wo tatsächlich Kontinente
//! liegen -- im selben "digitalen Punktwolken"-Stil wie gängige
//! Netzwerk-HUD-Karten (Kontinente als Feld einzelner Leuchtpunkte statt
//! durchgezogener Flächen).
//!
//! Datenquelle: `assets/world_countries_110m.geo.json`, eine öffentliche,
//! stark vereinfachte 110m-Ländergrenzen-Datei (MIT/Unlicense,
//! `johan/world.geo.json`, abgeleitet von Natural Earth), zur Build-Zeit
//! per `include_str!` eingebettet -- keine Laufzeit-Abhängigkeit von
//! einem Dateipfad, kein Netzwerk-Download.
//!
//! Das Punktraster wird einmalig im Hintergrund-Thread berechnet (Regel
//! 21: keine Sekunden dauernde Punkt-in-Polygon-Prüfung im Render-Pfad)
//! und danach nur noch projiziert.

use serde::Deserialize;

const WORLD_GEOJSON: &str = include_str!("../../assets/world_countries_110m.geo.json");

/// Abstand des Rasters in Grad. Kleiner = dichter/detaillierter, aber
/// mehr Punkt-in-Polygon-Prüfungen beim einmaligen Aufbau und mehr
/// gezeichnete Punkte pro Frame danach.
const GRID_STEP_DEGREES: f32 = 1.25;

type Ring = Vec<[f64; 2]>;

#[derive(Deserialize)]
struct GeoJsonRoot {
    features: Vec<GeoJsonFeature>,
}

#[derive(Deserialize)]
struct GeoJsonFeature {
    geometry: GeoJsonGeometry,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "coordinates")]
enum GeoJsonGeometry {
    Polygon(Vec<Ring>),
    MultiPolygon(Vec<Vec<Ring>>),
    // Andere Geometrietypen kommen in dieser Datei nicht vor; ein
    // unbekannter Typ scheitert beim Parsen des gesamten Features nicht
    // (Regel 16), er liefert nur keine Ringe.
    #[serde(other)]
    Other,
}

/// Ein Land als Liste seiner Polygone (Außenring + evtl. Löcher je
/// Polygon), plus eine grobe Bounding-Box für schnelles Verwerfen beim
/// Punkt-in-Polygon-Test.
struct Landmass {
    polygons: Vec<Vec<Ring>>,
    min_lon: f64,
    max_lon: f64,
    min_lat: f64,
    max_lat: f64,
}

fn bounding_box(polygons: &[Vec<Ring>]) -> (f64, f64, f64, f64) {
    let mut min_lon = f64::MAX;
    let mut max_lon = f64::MIN;
    let mut min_lat = f64::MAX;
    let mut max_lat = f64::MIN;
    for polygon in polygons {
        for ring in polygon {
            for &[lon, lat] in ring {
                min_lon = min_lon.min(lon);
                max_lon = max_lon.max(lon);
                min_lat = min_lat.min(lat);
                max_lat = max_lat.max(lat);
            }
        }
    }
    (min_lon, max_lon, min_lat, max_lat)
}

/// Ray-Casting-Test (even-odd rule) für einen einzelnen Ring.
fn point_in_ring(lon: f64, lat: f64, ring: &Ring) -> bool {
    let mut inside = false;
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (ring[i][0], ring[i][1]);
        let (xj, yj) = (ring[j][0], ring[j][1]);
        if (yi > lat) != (yj > lat) {
            let x_at_lat = (xj - xi) * (lat - yi) / (yj - yi) + xi;
            if lon < x_at_lat {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

/// Ein Polygon besteht aus einem Außenring (Index 0) und optionalen
/// Loch-Ringen (Seen, Enklaven) -- innerhalb eines Lochs gilt der Punkt
/// nicht als Land.
fn point_in_polygon(lon: f64, lat: f64, polygon: &[Ring]) -> bool {
    let Some(outer) = polygon.first() else {
        return false;
    };
    if !point_in_ring(lon, lat, outer) {
        return false;
    }
    !polygon[1..].iter().any(|hole| point_in_ring(lon, lat, hole))
}

fn parse_landmasses(raw: &str) -> Vec<Landmass> {
    let Ok(root) = serde_json::from_str::<GeoJsonRoot>(raw) else {
        return Vec::new();
    };
    root.features
        .into_iter()
        .filter_map(|feature| {
            let polygons = match feature.geometry {
                GeoJsonGeometry::Polygon(rings) => vec![rings],
                GeoJsonGeometry::MultiPolygon(polygons) => polygons,
                GeoJsonGeometry::Other => return None,
            };
            if polygons.is_empty() {
                return None;
            }
            let (min_lon, max_lon, min_lat, max_lat) = bounding_box(&polygons);
            Some(Landmass { polygons, min_lon, max_lon, min_lat, max_lat })
        })
        .collect()
}

/// Baut das Punktraster aller Landmassen aus der eingebetteten GeoJSON.
/// Liefert `(lat, lon)`-Paare, dieselbe Konvention wie im Rest von
/// [`super`]. Eine defekte/leere eingebettete Datei ist kein Absturz
/// (Regel 16) -- die Karte zeigt dann nur das Koordinatengitter ohne
/// Landmassen-Punkte.
pub fn build_dot_grid() -> Vec<(f32, f32)> {
    let landmasses = parse_landmasses(WORLD_GEOJSON);
    let mut dots = Vec::new();

    let mut lat = -85.0_f64;
    while lat <= 85.0 {
        let mut lon = -180.0_f64;
        while lon <= 180.0 {
            for land in &landmasses {
                if lon < land.min_lon || lon > land.max_lon || lat < land.min_lat || lat > land.max_lat
                {
                    continue;
                }
                if land.polygons.iter().any(|p| point_in_polygon(lon, lat, p)) {
                    dots.push((lat as f32, lon as f32));
                    break;
                }
            }
            lon += GRID_STEP_DEGREES as f64;
        }
        lat += GRID_STEP_DEGREES as f64;
    }
    dots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_in_ring_erkennt_einfaches_quadrat() {
        let square: Ring = vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0]];
        assert!(point_in_ring(2.0, 2.0, &square));
        assert!(!point_in_ring(5.0, 2.0, &square));
    }

    #[test]
    fn point_in_polygon_respektiert_loch() {
        let outer: Ring = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        let hole: Ring = vec![[3.0, 3.0], [7.0, 3.0], [7.0, 7.0], [3.0, 7.0]];
        let polygon = vec![outer, hole];
        assert!(point_in_polygon(1.0, 1.0, &polygon));
        assert!(!point_in_polygon(5.0, 5.0, &polygon), "Punkt liegt im Loch");
    }

    #[test]
    fn eingebettete_datei_ist_gueltiges_geojson_mit_landmassen() {
        let landmasses = parse_landmasses(WORLD_GEOJSON);
        assert!(
            landmasses.len() > 100,
            "erwarte grob 180 Länder, waren {}",
            landmasses.len()
        );
    }

    #[test]
    fn kaputte_geojson_liefert_leere_liste_statt_zu_paniken() {
        assert!(parse_landmasses("{ kein json").is_empty());
        assert!(build_dot_grid_from("{}").is_empty());
    }

    fn build_dot_grid_from(raw: &str) -> Vec<Landmass> {
        parse_landmasses(raw)
    }

    #[test]
    fn deutschland_liegt_im_raster_australien_nicht_versehentlich_am_nullmeridian() {
        // Berlin ungefähr: 52.5N, 13.4E -- muss von irgendeinem Land
        // getroffen werden (Deutschland selbst reicht als Nachweis, dass
        // die Datei nicht leer/verschoben geladen wurde).
        let landmasses = parse_landmasses(WORLD_GEOJSON);
        let hits_berlin = landmasses
            .iter()
            .any(|l| l.polygons.iter().any(|p| point_in_polygon(13.4, 52.5, p)));
        assert!(hits_berlin, "Berlin sollte auf Land liegen");

        // Mitten im Atlantik darf kein Land sein.
        let hits_ocean = landmasses
            .iter()
            .any(|l| l.polygons.iter().any(|p| point_in_polygon(-30.0, 30.0, p)));
        assert!(!hits_ocean, "mitten im Atlantik sollte kein Land liegen");
    }
}
