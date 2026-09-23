//! Bestes-verfügbares Nachschärfen einer Verbindungsposition über den
//! Länder-Mittelpunkt aus [`super::geoip`] hinaus.
//!
//! Es gibt keine offline installierte Stadt-Datenbank auf diesem System
//! (`geoip-database` liefert nur Länder, ein City-Paket ist in den
//! verfügbaren Paketquellen nicht vorhanden) -- ein Kauf/Download einer
//! MaxMind-GeoLite-City-Datenbank ist hier bewusst kein Ziel (Lizenz,
//! kein automatischer Download aus dieser Sitzung heraus).
//!
//! Stattdessen ein pragmatischer, häufig zutreffender Zwischenweg: viele
//! große Anbieter (Google, Cloud-CDNs, Hyperscaler-PoPs) kodieren einen
//! dreibuchstabigen IATA-Flughafencode für das jeweilige Rechenzentrum in
//! ihren Reverse-DNS-Hostnamen -- z. B. `fra16s48-in-f14.1e100.net` für
//! ein Google-System in Frankfurt. Reverse-DNS (`getent hosts`, dieselbe
//! `/etc/nsswitch.conf`-Auflösung wie `host`/`dig`) plus ein kleiner,
//! auswendig sicherer Flughafen-Code-Tisch (keine geratenen Koordinaten
//! -- nur Städte, deren Lage eindeutiges Allgemeinwissen ist) ersetzen
//! den Länder-Mittelpunkt durch einen echten Ort, wann immer der Hostname
//! passt. Trifft nichts zu, bleibt es beim Länder-Mittelpunkt -- nie
//! schlechter als vorher (Regel 16: kein Fehler, nur eine ausbleibende
//! Verbesserung).

use std::net::Ipv4Addr;
use std::process::Command;

/// Wortgrenzen-sicherer Mindestabstand: ein Code darf nicht Teil eines
/// längeren Wortes sein (`"fra"` in `"frankfurt-mail"` wäre falsch), aber
/// direkt vor Ziffern/Trennzeichen/Ende stehen (`"fra16"`, `"fra-1"`,
/// `"fra.example.com"`).
const AIRPORT_HINTS: &[(&str, f32, f32, &str)] = &[
    ("fra", 50.03, 8.57, "Frankfurt"),
    ("lhr", 51.47, -0.45, "London"),
    ("ams", 52.31, 4.76, "Amsterdam"),
    ("cdg", 49.01, 2.55, "Paris"),
    ("mad", 40.47, -3.56, "Madrid"),
    ("fco", 41.80, 12.25, "Rom"),
    ("mxp", 45.63, 8.72, "Mailand"),
    ("zrh", 47.46, 8.55, "Zürich"),
    ("vie", 48.11, 16.57, "Wien"),
    ("waw", 52.17, 20.97, "Warschau"),
    ("arn", 59.65, 17.92, "Stockholm"),
    ("hel", 60.32, 24.96, "Helsinki"),
    ("cph", 55.62, 12.65, "Kopenhagen"),
    ("dub", 53.43, -6.24, "Dublin"),
    ("bru", 50.90, 4.48, "Brüssel"),
    ("prg", 50.10, 14.26, "Prag"),
    ("bud", 47.44, 19.26, "Budapest"),
    ("iad", 38.95, -77.45, "Washington D.C."),
    ("jfk", 40.64, -73.78, "New York"),
    ("ewr", 40.69, -74.17, "New York (Newark)"),
    ("ord", 41.98, -87.90, "Chicago"),
    ("atl", 33.64, -84.43, "Atlanta"),
    ("dfw", 32.90, -97.04, "Dallas"),
    ("lax", 33.94, -118.41, "Los Angeles"),
    ("sfo", 37.62, -122.38, "San Francisco"),
    ("sea", 47.45, -122.31, "Seattle"),
    ("den", 39.86, -104.67, "Denver"),
    ("mia", 25.80, -80.29, "Miami"),
    ("bos", 42.36, -71.01, "Boston"),
    ("yyz", 43.68, -79.63, "Toronto"),
    ("yul", 45.47, -73.74, "Montreal"),
    ("gru", -23.43, -46.47, "São Paulo"),
    ("sin", 1.36, 103.99, "Singapur"),
    ("hkg", 22.31, 113.91, "Hongkong"),
    ("nrt", 35.76, 140.39, "Tokio"),
    ("icn", 37.46, 126.44, "Seoul"),
    ("syd", -33.95, 151.18, "Sydney"),
    ("bom", 19.09, 72.87, "Mumbai"),
    ("del", 28.56, 77.10, "Delhi"),
    ("dxb", 25.25, 55.36, "Dubai"),
    ("jnb", -26.13, 28.24, "Johannesburg"),
];

/// Reverse-DNS über die System-NSS-Auflösung (`getent hosts`, dasselbe
/// wie `host`/`dig -x` nutzen) -- kein eigener DNS-Client, kein neuer
/// Crate, nur ein bereits auf jedem Linux vorhandenes Werkzeug (wie
/// `journalctl` beim Ingestion-Subprozess). Schlägt die Auflösung fehl
/// oder timet sie aus, ist das Ergebnis einfach `None` (Regel 16).
fn reverse_dns(ip: Ipv4Addr) -> Option<String> {
    let output = Command::new("getent").arg("hosts").arg(ip.to_string()).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    // Format: "<ip>[ \t]+<hostname>[ Aliase...]"
    text.lines().next()?.split_whitespace().nth(1).map(str::to_string)
}

/// Prüft jedes Punkt-getrennte Label von `hostname` (der Flughafencode
/// steht je nach Anbieter im ersten Label wie bei Google
/// `fra16s48-in-f14.1e100.net`, oder in einem mittleren Label wie bei
/// vielen CDN-Konventionen `server-1.fra-1.r.example.net`) auf einen
/// bekannten Flughafencode als Präfix, gefolgt von einer Ziffer, `-`
/// oder dem Ende des Labels -- verhindert Fehltreffer wie `"fra"` in
/// `"frankfurt-relay.example.com"`.
fn airport_hint(hostname: &str) -> Option<(f32, f32, &'static str)> {
    hostname.split('.').find_map(|label| {
        let label = label.to_ascii_lowercase();
        AIRPORT_HINTS.iter().find_map(|&(code, lat, lon, city)| {
            let rest = label.strip_prefix(code)?;
            let boundary_ok = rest.chars().next().is_none_or(|c| !c.is_ascii_alphabetic());
            boundary_ok.then_some((lat, lon, city))
        })
    })
}

/// Standortkürzel im Backbone-Namensschema der Deutschen Telekom
/// (`<router>.<KÜRZEL>.DE.NET.DTAG.DE`, z. B. `f-eh1-i.F.DE.NET.DTAG.DE`).
/// Die Kürzel sind die üblichen deutschen Städtekennzeichen -- erst durch
/// sie unterscheidet die Routenanzeige überhaupt zwischen zwei deutschen
/// Zwischenstationen, statt beide auf denselben Länder-Mittelpunkt zu
/// legen. Wie bei [`AIRPORT_HINTS`] nur Städte, deren Lage eindeutiges
/// Allgemeinwissen ist.
const DTAG_CITY_HINTS: &[(&str, f32, f32, &str)] = &[
    ("f", 50.11, 8.68, "Frankfurt"),
    ("m", 48.14, 11.58, "München"),
    ("b", 52.52, 13.40, "Berlin"),
    ("d", 51.23, 6.78, "Düsseldorf"),
    ("hh", 53.55, 9.99, "Hamburg"),
    ("k", 50.94, 6.96, "Köln"),
    ("s", 48.78, 9.18, "Stuttgart"),
    ("h", 52.37, 9.73, "Hannover"),
    ("n", 49.45, 11.08, "Nürnberg"),
    ("l", 51.34, 12.37, "Leipzig"),
    ("bn", 50.74, 7.10, "Bonn"),
    ("ma", 49.49, 8.47, "Mannheim"),
    ("do", 51.51, 7.47, "Dortmund"),
    ("e", 51.46, 7.01, "Essen"),
    ("br", 53.08, 8.80, "Bremen"),
];

/// Erkennt das Telekom-Backbone-Schema: endet der Hostname auf
/// `.DE.NET.DTAG.DE`, steht direkt davor das Standortkürzel. Bewusst über
/// die festen Endlabels geprüft statt per Teilstring-Suche -- ein bloßes
/// `"f"` irgendwo im Namen wäre sonst ein Dauer-Fehltreffer.
fn dtag_city_hint(hostname: &str) -> Option<(f32, f32, &'static str)> {
    let labels: Vec<String> = hostname.split('.').map(|l| l.to_ascii_lowercase()).collect();
    let suffix_start = labels.len().checked_sub(4)?;
    if labels[suffix_start..] != ["de", "net", "dtag", "de"] {
        return None;
    }
    let code = labels.get(suffix_start.checked_sub(1)?)?;
    DTAG_CITY_HINTS
        .iter()
        .find(|(c, ..)| c == code)
        .map(|&(_, lat, lon, city)| (lat, lon, city))
}

/// Versucht, `ip` über Reverse-DNS genauer zu verorten als der
/// Länder-Mittelpunkt: erst das Telekom-Backbone-Schema (trifft auf die
/// ersten Zwischenstationen jeder Route dieses Anschlusses zu), sonst die
/// Flughafencode-Tabelle (trifft auf die Rechenzentren am Ende zu).
/// `None`, wenn Reverse-DNS fehlschlägt oder der Hostname keinem bekannten
/// Muster entspricht -- der Aufrufer fällt dann auf den Länder-Mittelpunkt
/// zurück.
pub fn resolve_precise(ip: Ipv4Addr) -> Option<(f32, f32, String)> {
    let hostname = reverse_dns(ip)?;
    let (lat, lon, city) = dtag_city_hint(&hostname).or_else(|| airport_hint(&hostname))?;
    Some((lat, lon, city.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erkennt_google_stil_hostnamen() {
        assert_eq!(
            airport_hint("fra16s48-in-f14.1e100.net"),
            Some((50.03, 8.57, "Frankfurt"))
        );
        assert_eq!(
            airport_hint("lhr25s16-in-f14.1e100.net"),
            Some((51.47, -0.45, "London"))
        );
    }

    #[test]
    fn erkennt_cdn_stil_hostnamen_mit_bindestrich() {
        assert_eq!(
            airport_hint("server-1-2-3-4.fra-1.r.cloudfront.net").map(|(_, _, city)| city),
            Some("Frankfurt")
        );
    }

    #[test]
    fn lehnt_zufaellige_teilstring_treffer_ab() {
        assert_eq!(airport_hint("frankfurt-relay.example.com"), None);
        assert_eq!(airport_hint("mail.example.com"), None);
    }

    #[test]
    fn leerer_oder_unbekannter_hostname_liefert_none() {
        assert_eq!(airport_hint(""), None);
        assert_eq!(airport_hint("random-host.example.org"), None);
    }

    #[test]
    fn erkennt_telekom_backbone_standorte() {
        // Beide Hostnamen stammen aus echten Messläufen dieses Anschlusses.
        assert_eq!(
            dtag_city_hint("f-eh1-i.F.DE.NET.DTAG.DE").map(|(_, _, city)| city),
            Some("Frankfurt")
        );
        assert_eq!(
            dtag_city_hint("m-ef1-i.M.DE.NET.DTAG.DE").map(|(_, _, city)| city),
            Some("München")
        );
    }

    #[test]
    fn dtag_muster_greift_nur_bei_passendem_suffix() {
        assert_eq!(dtag_city_hint("f-eh1-i.F.DE.NET.EXAMPLE.DE"), None);
        assert_eq!(dtag_city_hint("p3e9bf27a.dip0.t-ipconnect.de"), None);
        assert_eq!(dtag_city_hint("DE.NET.DTAG.DE"), None, "kein Kürzel davor");
        assert_eq!(dtag_city_hint("x-1.ZZ.DE.NET.DTAG.DE"), None, "unbekanntes Kürzel");
    }
}
