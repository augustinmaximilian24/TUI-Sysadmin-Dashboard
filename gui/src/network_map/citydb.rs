//! Offline-Stadtauflösung über eine Datenbank im MaxMind-DB-Format
//! (`.mmdb`), z. B. die kostenlose *DB-IP City Lite* (CC BY 4.0, Quelle:
//! <https://db-ip.com/db/download/ip-to-city-lite>) oder *GeoLite2-City*.
//!
//! Ergänzt [`super::geoip`]: Die Country-Datenbank kennt nur das Land, diese
//! hier zusätzlich Stadt und Koordinaten. Das ist der einzige Weg, bei einem
//! privaten Anschluss (Reverse-DNS ohne Ortshinweis, z. B.
//! `p5dc….dip0.t-ipconnect.de`) überhaupt auf eine Stadt zu kommen.
//!
//! Alles bleibt lokal: eine Datei, die der Betreiber selbst herunterlädt, und
//! kein Netzwerkzugriff pro Verbindung. Die Daten sind **Näherungswerte** --
//! bei Privatanschlüssen oft nur der Einwahlknoten des Anbieters, im
//! ungünstigen Fall dutzende Kilometer daneben; hinter VPN, Tunnel oder
//! Carrier-Grade-NAT steht dort der Endpunkt des Tunnels, nicht der Mensch.
//! Die Anzeige kennzeichnet solche Orte deshalb als "ca." (siehe
//! `ConnectionPoint::place` in `mod.rs`).
//!
//! Die Datei wird komplett in den Speicher gelesen (`Reader::open_readfile`):
//! das Crate-Feature `mmap` bräuchte `unsafe` auf unserer Seite. Dafür wird
//! sie nur einmal geladen und von beiden Hintergrund-Threads gemeinsam
//! genutzt ([`SharedCityDb`]).

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use maxminddb::{geoip2, MaxMindDbError, Reader};

use super::geoip;

/// Ergebnis eines Lookups: Land (immer, sonst gibt es keinen Treffer) und,
/// falls die Datenbank eine Stadt kennt, deren Name samt Koordinaten.
#[derive(Debug, Clone, PartialEq)]
pub struct CityHit {
    /// ISO-3166-1-Alpha-2-Code in der Schreibweise aus [`super::geoip`], damit
    /// Ländername und Mittelpunkt dort nachgeschlagen werden können.
    pub country: &'static str,
    /// Stadtname, falls bekannt. Deutsch, wenn die Datenbank ihn liefert,
    /// sonst Englisch.
    pub city: Option<String>,
    /// Breite/Länge der Stadt. Bewusst nur gesetzt, wenn auch ein Stadtname
    /// da ist: Koordinaten ohne Stadt wären nur ein Landes-Platzhalter der
    /// Datenbank und dürften nicht als Genauigkeit erscheinen.
    pub position: Option<(f32, f32)>,
    /// Erste Verwaltungsebene (Bundesland, Staat), falls die Datenbank sie
    /// kennt. Deutsch, sonst Englisch.
    pub region: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CityDbError {
    #[error("Stadt-Datenbank konnte nicht gelesen werden: {0}")]
    Read(#[from] MaxMindDbError),
}

/// Geladene Stadt-Datenbank.
pub struct CityDb {
    reader: Reader<Vec<u8>>,
}

impl CityDb {
    /// Lädt die Datei in den Speicher. Läuft im Hintergrund-Thread (siehe
    /// [`SharedCityDb`]), nie auf dem Render-Thread (Regel 21).
    pub fn load(path: &Path) -> Result<Self, CityDbError> {
        Ok(Self {
            reader: Reader::open_readfile(path)?,
        })
    }

    /// Schlägt eine IPv4-Adresse nach. `None` bei fehlendem Eintrag,
    /// unbekanntem Ländercode oder beschädigtem Datensatz -- nie ein Panic
    /// (Regel 16).
    pub fn lookup_v4(&self, ip: Ipv4Addr) -> Option<CityHit> {
        let result = self.reader.lookup(IpAddr::V4(ip)).ok()?;
        if !result.has_data() {
            return None;
        }
        let record: geoip2::City<'_> = result.decode().ok()??;
        hit_from_record(&record)
    }
}

/// Übersetzt einen Datensatz in einen [`CityHit`]. Als eigene, reine Funktion,
/// damit die Auswahlregeln ohne Datenbankdatei testbar sind.
fn hit_from_record(record: &geoip2::City<'_>) -> Option<CityHit> {
    let country = geoip::canonical_country_code(record.country.iso_code?)?;

    let city = pick_name(record.city.names.german, record.city.names.english);
    let region = record
        .subdivisions
        .first()
        .and_then(|sub| pick_name(sub.names.german, sub.names.english));

    let position = city.as_ref().and_then(|_| {
        let lat = record.location.latitude?;
        let lon = record.location.longitude?;
        valid_position(lat, lon)
    });
    // Ohne gültige Koordinaten wäre ein Stadtname nutzlos für die Karte und
    // würde den Ort nur behaupten, ohne ihn zu belegen.
    let city = position.is_some().then_some(city).flatten();

    Some(CityHit {
        country,
        city,
        position,
        region,
    })
}

/// Deutscher Name, sonst englischer; leere Namen zählen als fehlend.
fn pick_name(german: Option<&str>, english: Option<&str>) -> Option<String> {
    german
        .or(english)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// Prüft Wertebereich und Endlichkeit, bevor eine Koordinate auf die Kugel
/// gelegt wird -- eine beschädigte Datei darf keine NaN-Position erzeugen.
fn valid_position(lat: f64, lon: f64) -> Option<(f32, f32)> {
    let valid = lat.is_finite()
        && lon.is_finite()
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lon);
    valid.then_some((lat as f32, lon as f32))
}

/// Zustand der gemeinsam genutzten Datenbank.
pub enum CityDbState {
    /// Die Datei existiert nicht -- der Normalfall, solange der Betreiber sie
    /// nicht heruntergeladen hat. Kein Fehler, die Karte bleibt beim
    /// bisherigen Verhalten (Regel 16).
    Absent,
    /// Die Datei ist da, ließ sich aber nicht lesen. Der Text geht als
    /// Hinweis ins Panel.
    Failed(String),
    Ready(CityDb),
}

/// Lädt die Datenbank genau einmal und gibt sie an alle Threads weiter.
///
/// Der Verbindungs-Thread und der Routen-Thread brauchen beide dieselbe
/// Datei. Mit zwei eigenen Instanzen läge sie doppelt im Speicher. Wer
/// zuerst [`SharedCityDb::get`] aufruft, lädt; der andere wartet kurz und
/// bekommt dasselbe Ergebnis ([`OnceLock::get_or_init`]).
pub struct SharedCityDb {
    path: Option<PathBuf>,
    state: OnceLock<CityDbState>,
}

impl SharedCityDb {
    /// `path = None` (leere Konfiguration) schaltet die Stadtauflösung aus.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            state: OnceLock::new(),
        }
    }

    /// Blockiert beim ersten Aufruf, bis geladen ist -- deshalb nur aus
    /// Hintergrund-Threads aufrufen (Regel 21).
    pub fn get(&self) -> &CityDbState {
        self.state.get_or_init(|| match &self.path {
            None => CityDbState::Absent,
            Some(path) if !path.exists() => CityDbState::Absent,
            Some(path) => match CityDb::load(path) {
                Ok(db) => CityDbState::Ready(db),
                Err(err) => CityDbState::Failed(format!("{}: {err}", path.display())),
            },
        })
    }

    /// Die Datenbank, falls vorhanden und lesbar.
    pub fn db(&self) -> Option<&CityDb> {
        match self.get() {
            CityDbState::Ready(db) => Some(db),
            CityDbState::Absent | CityDbState::Failed(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maxminddb_writer::metadata::IpVersion;
    use maxminddb_writer::paths::IpAddrWithMask;
    use maxminddb_writer::Database;
    use serde::Serialize;

    /// Fehlende Sprachen werden weggelassen statt als leerer Wert geschrieben:
    /// das MMDB-Format kennt kein "null", echte Datenbanken lassen den
    /// Schlüssel einfach aus.
    #[derive(Serialize)]
    struct Names {
        #[serde(skip_serializing_if = "Option::is_none")]
        de: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        en: Option<&'static str>,
    }
    #[derive(Serialize)]
    struct City {
        names: Names,
    }
    #[derive(Serialize)]
    struct Subdivision {
        names: Names,
    }
    #[derive(Serialize)]
    struct Country {
        iso_code: &'static str,
    }
    #[derive(Serialize)]
    struct Location {
        latitude: f64,
        longitude: f64,
    }
    /// Bildet das Schema der City-Datenbanken nach (`city.names`,
    /// `country.iso_code`, `location`); fehlende Teile werden weggelassen.
    #[derive(Serialize)]
    struct Record {
        #[serde(skip_serializing_if = "Option::is_none")]
        city: Option<City>,
        country: Country,
        #[serde(skip_serializing_if = "Option::is_none")]
        location: Option<Location>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        subdivisions: Vec<Subdivision>,
    }

    /// Hängt einer Test-Region (Bundesland o. Ä.) an einen Datensatz.
    fn in_region(mut rec: Record, de: &'static str, en: &'static str) -> Record {
        rec.subdivisions.push(Subdivision {
            names: Names {
                de: (!de.is_empty()).then_some(de),
                en: Some(en),
            },
        });
        rec
    }

    fn record(
        country: &'static str,
        city: Option<(&'static str, &'static str)>,
        lat: f64,
        lon: f64,
    ) -> Record {
        Record {
            city: city.map(|(de, en)| City {
                names: Names {
                    de: (!de.is_empty()).then_some(de),
                    en: Some(en),
                },
            }),
            country: Country { iso_code: country },
            location: Some(Location {
                latitude: lat,
                longitude: lon,
            }),
            subdivisions: Vec::new(),
        }
    }

    /// Schreibt eine kleine Datenbank in eine temporäre Datei und lädt sie
    /// über denselben Weg wie die echte (`CityDb::load`).
    fn db_with(entries: &[(&str, Record)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let mut db = Database::default();
        db.metadata.ip_version = IpVersion::V4;
        db.metadata.database_type = "Test-City".to_string();
        db.metadata.languages = vec!["de".to_string(), "en".to_string()];
        db.metadata.binary_format_major_version = 2;
        db.metadata.binary_format_minor_version = 0;
        for (network, rec) in entries {
            let data = db.insert_value(rec).expect("Datensatz serialisierbar");
            db.insert_node(
                network.parse::<IpAddrWithMask>().expect("Netz gültig"),
                data,
            );
        }
        let dir = tempfile::tempdir().expect("Temp-Verzeichnis");
        let path = dir.path().join("city-test.mmdb");
        let file = std::fs::File::create(&path).expect("Datei anlegen");
        db.write_to(file).expect("Datenbank schreiben");
        (dir, path)
    }

    #[test]
    fn findet_stadt_mit_koordinaten_und_bevorzugt_deutschen_namen() {
        let (_dir, path) = db_with(&[(
            "93.0.0.0/8",
            record("DE", Some(("München", "Munich")), 48.137, 11.575),
        )]);
        let db = CityDb::load(&path).expect("laden");
        let hit = db.lookup_v4(Ipv4Addr::new(93, 1, 2, 3)).expect("Treffer");
        assert_eq!(hit.country, "DE");
        assert_eq!(hit.city.as_deref(), Some("München"));
        let (lat, lon) = hit.position.expect("Position");
        assert!((lat - 48.137).abs() < 0.001);
        assert!((lon - 11.575).abs() < 0.001);
    }

    #[test]
    fn liefert_region_deutsch_sonst_englisch_sonst_none() {
        let (_dir, path) = db_with(&[
            (
                "93.0.0.0/8",
                in_region(
                    record("DE", Some(("München", "Munich")), 48.137, 11.575),
                    "Bayern",
                    "Bavaria",
                ),
            ),
            (
                "5.0.0.0/8",
                in_region(
                    record("US", Some(("", "Austin")), 30.27, -97.74),
                    "",
                    "Texas",
                ),
            ),
            (
                "6.0.0.0/8",
                record("NL", Some(("", "Amsterdam")), 52.37, 4.9),
            ),
        ]);
        let db = CityDb::load(&path).expect("laden");
        let region = |ip| db.lookup_v4(ip).expect("Treffer").region;
        assert_eq!(region(Ipv4Addr::new(93, 1, 2, 3)).as_deref(), Some("Bayern"));
        assert_eq!(region(Ipv4Addr::new(5, 1, 2, 3)).as_deref(), Some("Texas"));
        assert_eq!(region(Ipv4Addr::new(6, 1, 2, 3)), None);
    }

    #[test]
    fn faellt_auf_englischen_namen_zurueck() {
        let (_dir, path) = db_with(&[(
            "5.0.0.0/8",
            record("NL", Some(("", "Amsterdam")), 52.37, 4.9),
        )]);
        let db = CityDb::load(&path).expect("laden");
        let hit = db.lookup_v4(Ipv4Addr::new(5, 9, 9, 9)).expect("Treffer");
        assert_eq!(hit.city.as_deref(), Some("Amsterdam"));
    }

    #[test]
    fn land_ohne_stadt_liefert_keine_position() {
        // Datenbanken tragen bei Länder-Einträgen oft trotzdem Koordinaten
        // (Landesmitte) -- die dürfen nicht als Stadt-Genauigkeit erscheinen.
        let (_dir, path) = db_with(&[("8.0.0.0/8", record("US", None, 37.75, -97.8))]);
        let db = CityDb::load(&path).expect("laden");
        let hit = db.lookup_v4(Ipv4Addr::new(8, 8, 8, 8)).expect("Treffer");
        assert_eq!(hit.country, "US");
        assert_eq!(hit.city, None);
        assert_eq!(hit.position, None);
    }

    #[test]
    fn ungueltige_koordinaten_werden_verworfen() {
        let (_dir, path) = db_with(&[(
            "9.0.0.0/8",
            record("DE", Some(("Berlin", "Berlin")), 123.0, 400.0),
        )]);
        let db = CityDb::load(&path).expect("laden");
        let hit = db.lookup_v4(Ipv4Addr::new(9, 9, 9, 9)).expect("Treffer");
        assert_eq!(
            hit.city, None,
            "Stadt ohne gültige Position wird nicht behauptet"
        );
        assert_eq!(hit.position, None);
    }

    #[test]
    fn unbekannter_ländercode_ergibt_keinen_treffer() {
        let (_dir, path) = db_with(&[(
            "7.0.0.0/8",
            record("ZZ", Some(("Nirgendwo", "Nowhere")), 1.0, 1.0),
        )]);
        let db = CityDb::load(&path).expect("laden");
        assert_eq!(db.lookup_v4(Ipv4Addr::new(7, 7, 7, 7)), None);
    }

    #[test]
    fn adresse_ohne_eintrag_ist_none() {
        let (_dir, path) = db_with(&[(
            "93.0.0.0/8",
            record("DE", Some(("Berlin", "Berlin")), 52.5, 13.4),
        )]);
        let db = CityDb::load(&path).expect("laden");
        assert_eq!(db.lookup_v4(Ipv4Addr::new(10, 0, 0, 1)), None);
    }

    #[test]
    fn valid_position_prueft_bereich_und_endlichkeit() {
        assert!(valid_position(48.1, 11.5).is_some());
        assert!(valid_position(-90.0, 180.0).is_some());
        assert!(valid_position(90.1, 0.0).is_none());
        assert!(valid_position(0.0, -180.1).is_none());
        assert!(valid_position(f64::NAN, 0.0).is_none());
        assert!(valid_position(0.0, f64::INFINITY).is_none());
    }

    #[test]
    fn laden_scheitert_sauber_bei_kaputter_datei() {
        let dir = tempfile::tempdir().expect("Temp-Verzeichnis");
        let path = dir.path().join("kaputt.mmdb");
        std::fs::write(&path, b"das ist keine mmdb-Datei").expect("schreiben");
        assert!(CityDb::load(&path).is_err());
    }

    #[test]
    fn geteilte_datenbank_unterscheidet_fehlend_defekt_und_bereit() {
        // Kein Pfad und nicht vorhandener Pfad: still ausgeschaltet.
        assert!(matches!(SharedCityDb::new(None).get(), CityDbState::Absent));
        let missing = SharedCityDb::new(Some(PathBuf::from("/nicht/vorhanden.mmdb")));
        assert!(matches!(missing.get(), CityDbState::Absent));
        assert!(missing.db().is_none());

        // Vorhanden, aber unlesbar: Fehlertext fürs Panel.
        let dir = tempfile::tempdir().expect("Temp-Verzeichnis");
        let bad = dir.path().join("kaputt.mmdb");
        std::fs::write(&bad, b"kaputt").expect("schreiben");
        let failed = SharedCityDb::new(Some(bad));
        assert!(matches!(failed.get(), CityDbState::Failed(_)));
        assert!(failed.db().is_none());

        // Gültig: bereit und abfragbar.
        let (_keep, path) = db_with(&[(
            "93.0.0.0/8",
            record("DE", Some(("Berlin", "Berlin")), 52.5, 13.4),
        )]);
        let ready = SharedCityDb::new(Some(path));
        assert!(ready.db().is_some());
    }
    /// Rauchtest gegen eine **echte** Datenbank, nicht Teil des normalen
    /// Testlaufs (die Datei ist groß und nicht im Repo, Regel 25):
    ///
    /// `LOGSENTRY_CITYDB=/pfad/dbip-city-lite.mmdb cargo test -p logsentry-gui
    /// echte_datenbank -- --ignored --nocapture`
    ///
    /// Prüft, dass das Schema der echten Datei zu unserem Lesen passt, und
    /// gibt die Treffer zur Sichtprüfung aus.
    #[test]
    #[ignore = "braucht eine echte City-Datenbank (Umgebungsvariable LOGSENTRY_CITYDB)"]
    fn echte_datenbank_loest_oeffentliche_adressen_auf() {
        let Some(path) = std::env::var_os("LOGSENTRY_CITYDB") else {
            panic!("LOGSENTRY_CITYDB ist nicht gesetzt");
        };
        let db = CityDb::load(Path::new(&path)).expect("echte Datenbank lädt");
        for ip in [
            [8, 8, 8, 8],
            [1, 1, 1, 1],
            [93, 184, 216, 34],
            [62, 155, 242, 122],
        ] {
            let ip = Ipv4Addr::from(ip);
            eprintln!("{ip}: {:?}", db.lookup_v4(ip));
        }
        let google = db
            .lookup_v4(Ipv4Addr::new(8, 8, 8, 8))
            .expect("8.8.8.8 ist verzeichnet");
        assert_eq!(google.country, "US");
        let telekom = db
            .lookup_v4(Ipv4Addr::new(62, 155, 242, 122))
            .expect("Telekom-Router");
        assert_eq!(telekom.country, "DE");
    }
}
