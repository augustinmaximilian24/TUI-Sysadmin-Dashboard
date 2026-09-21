//! Reiner Rust-Reader für die veraltete, binäre MaxMind "GeoIP Country
//! Edition" (`.dat`), wie sie das Debian/Ubuntu-Paket `geoip-database`
//! unter `/usr/share/GeoIP/GeoIP.dat` installiert.
//!
//! Absichtlich **kein** neuer Crate/keine C-Bindung: Das Format ist ein
//! simpler 32-Ebenen-Binärbaum über den IPv4-Adressraum mit 3-Byte-
//! Knotenzeigern, exakt nachgebaut nach der offiziellen Referenz
//! (`maxmind/geoip-api-c`, `libGeoIP/GeoIP.c`, `_GeoIP_seek_record_gl`
//! und `_setup_segments`). Nur die (auf einem Heimsystem übliche)
//! `GEOIP_COUNTRY_EDITION` (Typ 1) wird unterstützt -- andere Editionen
//! (City, ASN, Org, ...) und IPv6 (`GeoIPv6.dat`, eigener 128-Ebenen-Baum)
//! sind ein bewusster Scope-Schnitt für Phase "Netzwerk-Weltkarte": ein
//! Heim-PC hat weit überwiegend IPv4-Verbindungen, und ein zweiter
//! 128-Bit-Baum hätte diese Datei verdoppelt für wenig zusätzlichen
//! Nutzen. Erweiterung: `lookup_v6` nach demselben Muster wie `lookup_v4`,
//! nur mit 128 statt 32 Tiefenstufen und `GeoIPv6.dat`.
//!
//! Länder-Code-Array und Konstanten sind wortgleich aus der offiziellen
//! Quelle übernommen (nicht aus dem Gedächtnis rekonstruiert), da eine
//! falsche Reihenfolge hier lautlos falsche Länder zurückgeben würde.

use std::net::Ipv4Addr;
use std::path::Path;

/// Feste Baumgröße der Country Edition (`COUNTRY_BEGIN` in `GeoIP.c`).
/// Werte `>=` dieser Schwelle sind Blätter (Länder-Index), kleinere Werte
/// sind interne Knotenzeiger.
const COUNTRY_BEGIN: u32 = 16_776_960;
/// Bytes pro Zeiger; ein Knoten hat zwei davon (links/rechts).
const RECORD_LENGTH: usize = 3;
/// Wie viele Byte-Positionen vom Dateiende rückwärts nach dem
/// `FF FF FF`-Marker der Trailer-Struktur gesucht werden (`GeoIP.c`,
/// `STRUCTURE_INFO_MAX_SIZE`).
const STRUCTURE_INFO_MAX_SIZE: usize = 20;
/// Edition-Byte für die einfache Country-Datenbank (`GEOIP_COUNTRY_EDITION`).
const GEOIP_COUNTRY_EDITION: u8 = 1;

#[derive(Debug, thiserror::Error)]
pub enum GeoIpError {
    #[error("GeoIP-Datenbank konnte nicht gelesen werden: {0}")]
    Read(#[from] std::io::Error),
    #[error("GeoIP-Datenbank hat kein erkennbares Country-Edition-Trailer (Datei zu kurz oder falsches Format)")]
    UnrecognizedFormat,
    #[error("GeoIP-Datenbank ist Edition {0}, unterstützt wird nur Country Edition (1)")]
    UnsupportedEdition(u8),
}

/// Geladene Länder-Datenbank; hält die Rohdatei im Speicher (wenige MB,
/// unkritisch für die GUI) und beantwortet Lookups rein über
/// Index-Arithmetik ohne weitere Datei-I/O nach dem Laden.
pub struct CountryDb {
    data: Vec<u8>,
}

impl CountryDb {
    /// Lädt und validiert die Datenbank. Läuft im Hintergrund-Thread
    /// (siehe `mod.rs`), nie auf dem Render-Thread (Regel 21).
    pub fn load(path: &Path) -> Result<Self, GeoIpError> {
        let data = std::fs::read(path)?;
        let db = Self { data };
        db.verify_country_edition()?;
        Ok(db)
    }

    /// Sucht den `FF FF FF`-Marker der Trailer-Struktur rückwärts vom
    /// Dateiende (wie `_setup_segments` in `GeoIP.c`) und prüft, dass die
    /// Datei tatsächlich eine Country Edition ist -- andere Editionen
    /// haben eine andere Baumstruktur und würden sonst lautlos falsche
    /// Länder liefern.
    fn verify_country_edition(&self) -> Result<(), GeoIpError> {
        let size = self.data.len();
        if size < 4 {
            return Err(GeoIpError::UnrecognizedFormat);
        }
        let mut offset = size - 3;
        for _ in 0..STRUCTURE_INFO_MAX_SIZE {
            if self.data[offset..offset + 3] == [0xFF, 0xFF, 0xFF] {
                let Some(&raw_edition) = self.data.get(offset + 3) else {
                    return Err(GeoIpError::UnrecognizedFormat);
                };
                // Kompatibilität mit Datenbanken vor April 2003 (`GeoIP.c`).
                let edition = if raw_edition >= 106 {
                    raw_edition - 105
                } else {
                    raw_edition
                };
                return if edition == GEOIP_COUNTRY_EDITION {
                    Ok(())
                } else {
                    Err(GeoIpError::UnsupportedEdition(edition))
                };
            }
            match offset.checked_sub(1) {
                Some(next) => offset = next,
                None => break,
            }
        }
        Err(GeoIpError::UnrecognizedFormat)
    }

    /// Traversiert den Binärbaum für eine IPv4-Adresse, MSB zuerst,
    /// exakt wie `_GeoIP_seek_record_gl` in `GeoIP.c`. Gibt `None` bei
    /// nicht zugeordneten/privaten Adressen oder einer beschädigten
    /// Datenbank zurück statt zu paniken (Regel 16).
    pub fn lookup_v4(&self, ip: Ipv4Addr) -> Option<&'static str> {
        let ipnum = u32::from(ip);
        let mut node: u32 = 0;
        for depth in (0..32).rev() {
            let byte_offset = (RECORD_LENGTH * 2) as u32 * node;
            let idx = byte_offset as usize;
            if idx + RECORD_LENGTH * 2 > self.data.len() {
                return None;
            }
            let take_right = (ipnum >> depth) & 1 == 1;
            let rec_start = if take_right { idx + RECORD_LENGTH } else { idx };
            let rec = &self.data[rec_start..rec_start + RECORD_LENGTH];
            let x = rec[0] as u32 | (rec[1] as u32) << 8 | (rec[2] as u32) << 16;
            if x >= COUNTRY_BEGIN {
                let country_index = (x - COUNTRY_BEGIN) as usize;
                return COUNTRY_CODES
                    .get(country_index)
                    .copied()
                    .filter(|code| *code != "--");
            }
            node = x;
        }
        None
    }
}

/// Ungefährer geografischer Mittelpunkt eines ISO-3166-1-Alpha-2-Ländercodes
/// (Grad, WGS84). Deckt die auf einem Heim-PC typischen Ziel-/Hosting-Länder
/// ab; unbekannte Codes liefern bewusst `None` statt eines geratenen Werts
/// -- die Verbindung erscheint dann in der Liste, aber ohne Kartenmarker.
/// Quelle: <https://gist.github.com/tadast/8827699> (Alpha-2 + Mittelwert
/// Lat/Lon je Land), nicht aus dem Gedächtnis geschätzt.
pub fn country_centroid(code: &str) -> Option<(f32, f32)> {
    COUNTRY_CENTROIDS
        .iter()
        .find(|(c, _, _)| *c == code)
        .map(|(_, lat, lon)| (*lat, *lon))
}

/// Wortgleich aus `libGeoIP/GeoIP.c` (`GeoIP_country_code[256][3]`)
/// übernommen -- die Reihenfolge ist historisch gewachsen (z. B. steht
/// `"CW"` zwischen `"AM"` und `"AO"`) und **darf nicht** alphabetisch
/// sortiert oder anderweitig "aufgeräumt" werden: der Index in diesem
/// Array *ist* der Blattwert aus dem Binärbaum.
const COUNTRY_CODES: [&str; 256] = [
    "--", "AP", "EU", "AD", "AE", "AF", "AG", "AI", "AL", "AM", "CW", "AO",
    "AQ", "AR", "AS", "AT", "AU", "AW", "AZ", "BA", "BB", "BD", "BE", "BF",
    "BG", "BH", "BI", "BJ", "BM", "BN", "BO", "BR", "BS", "BT", "BV", "BW",
    "BY", "BZ", "CA", "CC", "CD", "CF", "CG", "CH", "CI", "CK", "CL", "CM",
    "CN", "CO", "CR", "CU", "CV", "CX", "CY", "CZ", "DE", "DJ", "DK", "DM",
    "DO", "DZ", "EC", "EE", "EG", "EH", "ER", "ES", "ET", "FI", "FJ", "FK",
    "FM", "FO", "FR", "SX", "GA", "GB", "GD", "GE", "GF", "GH", "GI", "GL",
    "GM", "GN", "GP", "GQ", "GR", "GS", "GT", "GU", "GW", "GY", "HK", "HM",
    "HN", "HR", "HT", "HU", "ID", "IE", "IL", "IN", "IO", "IQ", "IR", "IS",
    "IT", "JM", "JO", "JP", "KE", "KG", "KH", "KI", "KM", "KN", "KP", "KR",
    "KW", "KY", "KZ", "LA", "LB", "LC", "LI", "LK", "LR", "LS", "LT", "LU",
    "LV", "LY", "MA", "MC", "MD", "MG", "MH", "MK", "ML", "MM", "MN", "MO",
    "MP", "MQ", "MR", "MS", "MT", "MU", "MV", "MW", "MX", "MY", "MZ", "NA",
    "NC", "NE", "NF", "NG", "NI", "NL", "NO", "NP", "NR", "NU", "NZ", "OM",
    "PA", "PE", "PF", "PG", "PH", "PK", "PL", "PM", "PN", "PR", "PS", "PT",
    "PW", "PY", "QA", "RE", "RO", "RU", "RW", "SA", "SB", "SC", "SD", "SE",
    "SG", "SH", "SI", "SJ", "SK", "SL", "SM", "SN", "SO", "SR", "ST", "SV",
    "SY", "SZ", "TC", "TD", "TF", "TG", "TH", "TJ", "TK", "TM", "TN", "TO",
    "TL", "TR", "TT", "TV", "TW", "TZ", "UA", "UG", "UM", "US", "UY", "UZ",
    "VA", "VC", "VE", "VG", "VI", "VN", "VU", "WF", "WS", "YE", "YT", "RS",
    "ZA", "ZM", "ME", "ZW", "A1", "A2", "O1", "AX", "GG", "IM", "JE", "BL",
    "MF", "BQ", "SS", "O1",];

/// Ungefährer Mittelpunkt je Land, generiert aus einem öffentlichen
/// Alpha-2/Lat/Lon-Datensatz (siehe [`country_centroid`]). Pseudo-Codes aus
/// `COUNTRY_CODES` ohne reale Geografie (`--`, `AP`, `EU`, `A1`, `A2`,
/// `O1`) tauchen hier bewusst nicht auf.
const COUNTRY_CENTROIDS: &[(&str, f32, f32)] = &[
    ("AD", 42.5, 1.6), // Andorra
    ("AE", 24.0, 54.0), // United Arab Emirates
    ("AF", 33.0, 65.0), // Afghanistan
    ("AG", 17.05, -61.8), // Antigua and Barbuda
    ("AI", 18.25, -63.17), // Anguilla
    ("AL", 41.0, 20.0), // Albania
    ("AM", 40.0, 45.0), // Armenia
    ("AN", 12.25, -68.75), // Netherlands Antilles
    ("AO", -12.5, 18.5), // Angola
    ("AQ", -90.0, 0.0), // Antarctica
    ("AR", -34.0, -64.0), // Argentina
    ("AS", -14.33, -170.0), // American Samoa
    ("AT", 47.33, 13.33), // Austria
    ("AU", -27.0, 133.0), // Australia
    ("AW", 12.5, -69.97), // Aruba
    ("AX", 60.12, 19.9), // Åland Islands
    ("AZ", 40.5, 47.5), // Azerbaijan
    ("BA", 44.0, 18.0), // Bosnia and Herzegovina
    ("BB", 13.17, -59.53), // Barbados
    ("BD", 24.0, 90.0), // Bangladesh
    ("BE", 50.83, 4.0), // Belgium
    ("BF", 13.0, -2.0), // Burkina Faso
    ("BG", 43.0, 25.0), // Bulgaria
    ("BH", 26.0, 50.55), // Bahrain
    ("BI", -3.5, 30.0), // Burundi
    ("BJ", 9.5, 2.25), // Benin
    ("BL", 17.9, -62.83), // Saint Barthélemy
    ("BM", 32.33, -64.75), // Bermuda
    ("BN", 4.5, 114.7), // Brunei Darussalam
    ("BO", -17.0, -65.0), // Bolivia, Plurinational State of
    ("BQ", 12.18, -68.23), // Bonaire, Sint Eustatius and Saba
    ("BR", -10.0, -55.0), // Brazil
    ("BS", 24.25, -76.0), // Bahamas
    ("BT", 27.5, 90.5), // Bhutan
    ("BV", -54.43, 3.4), // Bouvet Island
    ("BW", -22.0, 24.0), // Botswana
    ("BY", 53.0, 28.0), // Belarus
    ("BZ", 17.25, -88.75), // Belize
    ("CA", 60.0, -95.0), // Canada
    ("CC", -12.5, 96.83), // Cocos (Keeling) Islands
    ("CD", 0.0, 25.0), // Congo, the Democratic Republic of the
    ("CF", 7.0, 21.0), // Central African Republic
    ("CG", -1.0, 15.0), // Congo
    ("CH", 47.0, 8.0), // Switzerland
    ("CI", 8.0, -5.0), // Côte d'Ivoire
    ("CK", -21.23, -159.8), // Cook Islands
    ("CL", -30.0, -71.0), // Chile
    ("CM", 6.0, 12.0), // Cameroon
    ("CN", 35.0, 105.0), // China
    ("CO", 4.0, -72.0), // Colombia
    ("CR", 10.0, -84.0), // Costa Rica
    ("CU", 21.5, -80.0), // Cuba
    ("CV", 16.0, -24.0), // Cape Verde
    ("CW", 12.17, -68.97), // Curaçao
    ("CX", -10.5, 105.7), // Christmas Island
    ("CY", 35.0, 33.0), // Cyprus
    ("CZ", 49.75, 15.5), // Czech Republic
    ("DE", 51.0, 9.0), // Germany
    ("DJ", 11.5, 43.0), // Djibouti
    ("DK", 56.0, 10.0), // Denmark
    ("DM", 15.42, -61.33), // Dominica
    ("DO", 19.0, -70.67), // Dominican Republic
    ("DZ", 28.0, 3.0), // Algeria
    ("EC", -2.0, -77.5), // Ecuador
    ("EE", 59.0, 26.0), // Estonia
    ("EG", 27.0, 30.0), // Egypt
    ("EH", 24.5, -13.0), // Western Sahara
    ("ER", 15.0, 39.0), // Eritrea
    ("ES", 40.0, -4.0), // Spain
    ("ET", 8.0, 38.0), // Ethiopia
    ("FI", 64.0, 26.0), // Finland
    ("FJ", -18.0, 175.0), // Fiji
    ("FK", -51.75, -59.0), // Falkland Islands (Malvinas)
    ("FM", 6.917, 158.2), // Micronesia, Federated States of
    ("FO", 62.0, -7.0), // Faroe Islands
    ("FR", 46.0, 2.0), // France
    ("GA", -1.0, 11.75), // Gabon
    ("GB", 54.0, -2.0), // United Kingdom
    ("GD", 12.12, -61.67), // Grenada
    ("GE", 42.0, 43.5), // Georgia
    ("GF", 4.0, -53.0), // French Guiana
    ("GG", 49.5, -2.56), // Guernsey
    ("GH", 8.0, -2.0), // Ghana
    ("GI", 36.18, -5.367), // Gibraltar
    ("GL", 72.0, -40.0), // Greenland
    ("GM", 13.47, -16.57), // Gambia
    ("GN", 11.0, -10.0), // Guinea
    ("GP", 16.25, -61.58), // Guadeloupe
    ("GQ", 2.0, 10.0), // Equatorial Guinea
    ("GR", 39.0, 22.0), // Greece
    ("GS", -54.5, -37.0), // South Georgia and the South Sandwich Islands
    ("GT", 15.5, -90.25), // Guatemala
    ("GU", 13.47, 144.8), // Guam
    ("GW", 12.0, -15.0), // Guinea-Bissau
    ("GY", 5.0, -59.0), // Guyana
    ("HK", 22.25, 114.2), // Hong Kong
    ("HM", -53.1, 72.52), // Heard Island and McDonald Islands
    ("HN", 15.0, -86.5), // Honduras
    ("HR", 45.17, 15.5), // Croatia
    ("HT", 19.0, -72.42), // Haiti
    ("HU", 47.0, 20.0), // Hungary
    ("ID", -5.0, 120.0), // Indonesia
    ("IE", 53.0, -8.0), // Ireland
    ("IL", 31.5, 34.75), // Israel
    ("IM", 54.23, -4.55), // Isle of Man
    ("IN", 20.0, 77.0), // India
    ("IO", -6.0, 71.5), // British Indian Ocean Territory
    ("IQ", 33.0, 44.0), // Iraq
    ("IR", 32.0, 53.0), // Iran, Islamic Republic of
    ("IS", 65.0, -18.0), // Iceland
    ("IT", 42.83, 12.83), // Italy
    ("JE", 49.21, -2.13), // Jersey
    ("JM", 18.25, -77.5), // Jamaica
    ("JO", 31.0, 36.0), // Jordan
    ("JP", 36.0, 138.0), // Japan
    ("KE", 1.0, 38.0), // Kenya
    ("KG", 41.0, 75.0), // Kyrgyzstan
    ("KH", 13.0, 105.0), // Cambodia
    ("KI", 1.417, 173.0), // Kiribati
    ("KM", -12.17, 44.25), // Comoros
    ("KN", 17.33, -62.75), // Saint Kitts and Nevis
    ("KP", 40.0, 127.0), // Korea, Democratic People's Republic of
    ("KR", 37.0, 127.5), // Korea, Republic of
    ("KW", 29.34, 47.66), // Kuwait
    ("KY", 19.5, -80.5), // Cayman Islands
    ("KZ", 48.0, 68.0), // Kazakhstan
    ("LA", 18.0, 105.0), // Lao People's Democratic Republic
    ("LB", 33.83, 35.83), // Lebanon
    ("LC", 13.88, -61.13), // Saint Lucia
    ("LI", 47.17, 9.533), // Liechtenstein
    ("LK", 7.0, 81.0), // Sri Lanka
    ("LR", 6.5, -9.5), // Liberia
    ("LS", -29.5, 28.5), // Lesotho
    ("LT", 56.0, 24.0), // Lithuania
    ("LU", 49.75, 6.167), // Luxembourg
    ("LV", 57.0, 25.0), // Latvia
    ("LY", 25.0, 17.0), // Libya
    ("MA", 32.0, -5.0), // Morocco
    ("MC", 43.73, 7.4), // Monaco
    ("MD", 47.0, 29.0), // Moldova, Republic of
    ("ME", 42.0, 19.0), // Montenegro
    ("MF", 18.08, -63.06), // Saint Martin (French part)
    ("MG", -20.0, 47.0), // Madagascar
    ("MH", 9.0, 168.0), // Marshall Islands
    ("MK", 41.83, 22.0), // Macedonia, the former Yugoslav Republic of
    ("ML", 17.0, -4.0), // Mali
    ("MM", 22.0, 98.0), // Burma
    ("MN", 46.0, 105.0), // Mongolia
    ("MO", 22.17, 113.5), // Macao
    ("MP", 15.2, 145.8), // Northern Mariana Islands
    ("MQ", 14.67, -61.0), // Martinique
    ("MR", 20.0, -12.0), // Mauritania
    ("MS", 16.75, -62.2), // Montserrat
    ("MT", 35.83, 14.58), // Malta
    ("MU", -20.28, 57.55), // Mauritius
    ("MV", 3.25, 73.0), // Maldives
    ("MW", -13.5, 34.0), // Malawi
    ("MX", 23.0, -102.0), // Mexico
    ("MY", 2.5, 112.5), // Malaysia
    ("MZ", -18.25, 35.0), // Mozambique
    ("NA", -22.0, 17.0), // Namibia
    ("NC", -21.5, 165.5), // New Caledonia
    ("NE", 16.0, 8.0), // Niger
    ("NF", -29.03, 167.9), // Norfolk Island
    ("NG", 10.0, 8.0), // Nigeria
    ("NI", 13.0, -85.0), // Nicaragua
    ("NL", 52.5, 5.75), // Netherlands
    ("NO", 62.0, 10.0), // Norway
    ("NP", 28.0, 84.0), // Nepal
    ("NR", -0.5333, 166.9), // Nauru
    ("NU", -19.03, -169.9), // Niue
    ("NZ", -41.0, 174.0), // New Zealand
    ("OM", 21.0, 57.0), // Oman
    ("PA", 9.0, -80.0), // Panama
    ("PE", -10.0, -76.0), // Peru
    ("PF", -15.0, -140.0), // French Polynesia
    ("PG", -6.0, 147.0), // Papua New Guinea
    ("PH", 13.0, 122.0), // Philippines
    ("PK", 30.0, 70.0), // Pakistan
    ("PL", 52.0, 20.0), // Poland
    ("PM", 46.83, -56.33), // Saint Pierre and Miquelon
    ("PN", -24.7, -127.4), // Pitcairn
    ("PR", 18.25, -66.5), // Puerto Rico
    ("PS", 32.0, 35.25), // Palestinian Territory, Occupied
    ("PT", 39.5, -8.0), // Portugal
    ("PW", 7.5, 134.5), // Palau
    ("PY", -23.0, -58.0), // Paraguay
    ("QA", 25.5, 51.25), // Qatar
    ("RE", -21.1, 55.6), // Réunion
    ("RO", 46.0, 25.0), // Romania
    ("RS", 44.0, 21.0), // Serbia
    ("RU", 60.0, 100.0), // Russia
    ("RW", -2.0, 30.0), // Rwanda
    ("SA", 25.0, 45.0), // Saudi Arabia
    ("SB", -8.0, 159.0), // Solomon Islands
    ("SC", -4.583, 55.67), // Seychelles
    ("SD", 15.0, 30.0), // Sudan
    ("SE", 62.0, 15.0), // Sweden
    ("SG", 1.367, 103.8), // Singapore
    ("SH", -15.93, -5.7), // Saint Helena, Ascension and Tristan da Cunha
    ("SI", 46.0, 15.0), // Slovenia
    ("SJ", 78.0, 20.0), // Svalbard and Jan Mayen
    ("SK", 48.67, 19.5), // Slovakia
    ("SL", 8.5, -11.5), // Sierra Leone
    ("SM", 43.77, 12.42), // San Marino
    ("SN", 14.0, -14.0), // Senegal
    ("SO", 10.0, 49.0), // Somalia
    ("SR", 4.0, -56.0), // Suriname
    ("SS", 8.0, 30.0), // South Sudan
    ("ST", 1.0, 7.0), // Sao Tome and Principe
    ("SV", 13.83, -88.92), // El Salvador
    ("SX", 18.03, -63.05), // Sint Maarten (Dutch part)
    ("SY", 35.0, 38.0), // Syrian Arab Republic
    ("SZ", -26.5, 31.5), // Swaziland
    ("TC", 21.75, -71.58), // Turks and Caicos Islands
    ("TD", 15.0, 19.0), // Chad
    ("TF", -43.0, 67.0), // French Southern Territories
    ("TG", 8.0, 1.167), // Togo
    ("TH", 15.0, 100.0), // Thailand
    ("TJ", 39.0, 71.0), // Tajikistan
    ("TK", -9.0, -172.0), // Tokelau
    ("TL", -8.55, 125.5), // Timor-Leste
    ("TM", 40.0, 60.0), // Turkmenistan
    ("TN", 34.0, 9.0), // Tunisia
    ("TO", -20.0, -175.0), // Tonga
    ("TR", 39.0, 35.0), // Turkey
    ("TT", 11.0, -61.0), // Trinidad and Tobago
    ("TV", -8.0, 178.0), // Tuvalu
    ("TW", 23.5, 121.0), // Taiwan
    ("TZ", -6.0, 35.0), // Tanzania, United Republic of
    ("UA", 49.0, 32.0), // Ukraine
    ("UG", 1.0, 32.0), // Uganda
    ("UM", 19.28, 166.6), // United States Minor Outlying Islands
    ("US", 38.0, -97.0), // United States
    ("UY", -33.0, -56.0), // Uruguay
    ("UZ", 41.0, 64.0), // Uzbekistan
    ("VA", 41.9, 12.45), // Holy See (Vatican City State)
    ("VC", 13.25, -61.2), // Saint Vincent & the Grenadines
    ("VE", 8.0, -66.0), // Venezuela, Bolivarian Republic of
    ("VG", 18.5, -64.5), // Virgin Islands, British
    ("VI", 18.33, -64.83), // Virgin Islands, U.S.
    ("VN", 16.0, 106.0), // Viet Nam
    ("VU", -16.0, 167.0), // Vanuatu
    ("WF", -13.3, -176.2), // Wallis and Futuna
    ("WS", -13.58, -172.3), // Samoa
    ("XK", 42.58, 21.0), // Kosovo
    ("YE", 15.0, 48.0), // Yemen
    ("YT", -12.83, 45.17), // Mayotte
    ("ZA", -29.0, 24.0), // South Africa
    ("ZM", -15.0, 30.0), // Zambia
    ("ZW", -20.0, 30.0), // Zimbabwe
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Baut eine minimale, aber gültige Country-Edition-Datenbank mit
    /// genau einem Baumknoten: die MSB-Bit-Verzweigung der Test-IP zeigt
    /// direkt auf einen Blattwert, sodass `lookup_v4` schon bei `depth ==
    /// 31` zurückkehrt (Regel 25: keine Abhängigkeit von der echten,
    /// auf dem System installierten `/usr/share/GeoIP/GeoIP.dat`).
    fn fake_db_for(ip: Ipv4Addr, country_code: &str) -> CountryDb {
        let index = COUNTRY_CODES
            .iter()
            .position(|c| *c == country_code)
            .expect("Testcode muss in COUNTRY_CODES vorkommen");
        let leaf = COUNTRY_BEGIN + index as u32;
        let leaf_bytes = [
            (leaf & 0xFF) as u8,
            ((leaf >> 8) & 0xFF) as u8,
            ((leaf >> 16) & 0xFF) as u8,
        ];

        let ipnum = u32::from(ip);
        let take_right = (ipnum >> 31) & 1 == 1;

        let mut data = vec![0u8; RECORD_LENGTH * 2];
        let dest = if take_right { RECORD_LENGTH } else { 0 };
        data[dest..dest + RECORD_LENGTH].copy_from_slice(&leaf_bytes);

        data.extend_from_slice(&[0xFF, 0xFF, 0xFF, GEOIP_COUNTRY_EDITION]);
        CountryDb { data }
    }

    #[test]
    fn lookup_findet_land_ueber_ersten_verzweigungsschritt() {
        let ip: Ipv4Addr = "1.2.3.4".parse().unwrap();
        let db = fake_db_for(ip, "DE");
        assert_eq!(db.lookup_v4(ip), Some("DE"));
    }

    #[test]
    fn lookup_liefert_none_bei_zu_kurzer_datenbank() {
        let db = CountryDb {
            data: vec![0u8; 4],
        };
        let ip: Ipv4Addr = "8.8.8.8".parse().unwrap();
        assert_eq!(db.lookup_v4(ip), None);
    }

    #[test]
    fn verify_country_edition_erkennt_gueltigen_trailer() {
        let db = fake_db_for("1.2.3.4".parse().unwrap(), "US");
        assert!(db.verify_country_edition().is_ok());
    }

    #[test]
    fn verify_country_edition_lehnt_zu_kurze_datei_ab() {
        let db = CountryDb { data: vec![0u8; 2] };
        assert!(matches!(
            db.verify_country_edition(),
            Err(GeoIpError::UnrecognizedFormat)
        ));
    }

    #[test]
    fn verify_country_edition_lehnt_andere_edition_ab() {
        let mut data = vec![0u8; RECORD_LENGTH * 2];
        data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 2]); // GEOIP_REGION_EDITION_REV0
        let db = CountryDb { data };
        assert!(matches!(
            db.verify_country_edition(),
            Err(GeoIpError::UnsupportedEdition(2))
        ));
    }

    #[test]
    fn country_centroid_bekannter_code() {
        let (lat, lon) = country_centroid("DE").expect("DE muss bekannt sein");
        assert!((lat - 51.0).abs() < 1.0);
        assert!((lon - 9.0).abs() < 1.0);
    }

    #[test]
    fn country_centroid_unbekannter_code_ist_none() {
        assert_eq!(country_centroid("ZZ"), None);
        assert_eq!(country_centroid("EU"), None);
    }
}
