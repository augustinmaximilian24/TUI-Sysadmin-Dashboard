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
const AIRPORT_HINTS: &[(&str, f32, f32, &str, &str)] = &[
    ("fra", 50.03, 8.57, "Frankfurt", "DE"),
    ("lhr", 51.47, -0.45, "London", "GB"),
    ("ams", 52.31, 4.76, "Amsterdam", "NL"),
    ("cdg", 49.01, 2.55, "Paris", "FR"),
    ("mad", 40.47, -3.56, "Madrid", "ES"),
    ("fco", 41.80, 12.25, "Rom", "IT"),
    ("mxp", 45.63, 8.72, "Mailand", "IT"),
    ("zrh", 47.46, 8.55, "Zürich", "CH"),
    ("vie", 48.11, 16.57, "Wien", "AT"),
    ("waw", 52.17, 20.97, "Warschau", "PL"),
    ("arn", 59.65, 17.92, "Stockholm", "SE"),
    ("hel", 60.32, 24.96, "Helsinki", "FI"),
    ("cph", 55.62, 12.65, "Kopenhagen", "DK"),
    ("dub", 53.43, -6.24, "Dublin", "IE"),
    ("bru", 50.90, 4.48, "Brüssel", "BE"),
    ("prg", 50.10, 14.26, "Prag", "CZ"),
    ("bud", 47.44, 19.26, "Budapest", "HU"),
    ("iad", 38.95, -77.45, "Washington D.C.", "US"),
    ("jfk", 40.64, -73.78, "New York", "US"),
    ("ewr", 40.69, -74.17, "New York (Newark)", "US"),
    ("ord", 41.98, -87.90, "Chicago", "US"),
    ("atl", 33.64, -84.43, "Atlanta", "US"),
    ("dfw", 32.90, -97.04, "Dallas", "US"),
    ("lax", 33.94, -118.41, "Los Angeles", "US"),
    ("sfo", 37.62, -122.38, "San Francisco", "US"),
    ("sea", 47.45, -122.31, "Seattle", "US"),
    ("den", 39.86, -104.67, "Denver", "US"),
    ("mia", 25.80, -80.29, "Miami", "US"),
    ("bos", 42.36, -71.01, "Boston", "US"),
    ("yyz", 43.68, -79.63, "Toronto", "CA"),
    ("yul", 45.47, -73.74, "Montreal", "CA"),
    ("gru", -23.43, -46.47, "São Paulo", "BR"),
    ("sin", 1.36, 103.99, "Singapur", "SG"),
    ("hkg", 22.31, 113.91, "Hongkong", "HK"),
    ("nrt", 35.76, 140.39, "Tokio", "JP"),
    ("icn", 37.46, 126.44, "Seoul", "KR"),
    ("syd", -33.95, 151.18, "Sydney", "AU"),
    ("bom", 19.09, 72.87, "Mumbai", "IN"),
    ("del", 28.56, 77.10, "Delhi", "IN"),
    ("dxb", 25.25, 55.36, "Dubai", "AE"),
    ("jnb", -26.13, 28.24, "Johannesburg", "ZA"),
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
fn airport_hint(hostname: &str) -> Option<(f32, f32, &'static str, &'static str)> {
    hostname.split('.').find_map(|label| {
        let label = label.to_ascii_lowercase();
        AIRPORT_HINTS.iter().find_map(|&(code, lat, lon, city, country)| {
            let rest = label.strip_prefix(code)?;
            let boundary_ok = rest.chars().next().is_none_or(|c| !c.is_ascii_alphabetic());
            boundary_ok.then_some((lat, lon, city, country))
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
const DTAG_CITY_HINTS: &[(&str, f32, f32, &str, &str)] = &[
    ("f", 50.11, 8.68, "Frankfurt", "DE"),
    ("m", 48.14, 11.58, "München", "DE"),
    ("b", 52.52, 13.40, "Berlin", "DE"),
    ("d", 51.23, 6.78, "Düsseldorf", "DE"),
    ("hh", 53.55, 9.99, "Hamburg", "DE"),
    ("k", 50.94, 6.96, "Köln", "DE"),
    ("s", 48.78, 9.18, "Stuttgart", "DE"),
    ("h", 52.37, 9.73, "Hannover", "DE"),
    ("n", 49.45, 11.08, "Nürnberg", "DE"),
    ("l", 51.34, 12.37, "Leipzig", "DE"),
    ("bn", 50.74, 7.10, "Bonn", "DE"),
    ("ma", 49.49, 8.47, "Mannheim", "DE"),
    ("do", 51.51, 7.47, "Dortmund", "DE"),
    ("e", 51.46, 7.01, "Essen", "DE"),
    ("br", 53.08, 8.80, "Bremen", "DE"),
];

/// Erkennt das Telekom-Backbone-Schema: endet der Hostname auf
/// `.DE.NET.DTAG.DE`, steht direkt davor das Standortkürzel. Bewusst über
/// die festen Endlabels geprüft statt per Teilstring-Suche -- ein bloßes
/// `"f"` irgendwo im Namen wäre sonst ein Dauer-Fehltreffer.
fn dtag_city_hint(hostname: &str) -> Option<(f32, f32, &'static str, &'static str)> {
    let labels: Vec<String> = hostname.split('.').map(|l| l.to_ascii_lowercase()).collect();
    let suffix_start = labels.len().checked_sub(4)?;
    if labels[suffix_start..] != ["de", "net", "dtag", "de"] {
        return None;
    }
    let code = labels.get(suffix_start.checked_sub(1)?)?;
    DTAG_CITY_HINTS
        .iter()
        .find(|(c, ..)| c == code)
        .map(|&(_, lat, lon, city, country)| (lat, lon, city, country))
}

/// Versucht, `ip` über Reverse-DNS genauer zu verorten als der
/// Länder-Mittelpunkt: erst das Telekom-Backbone-Schema (trifft auf die
/// ersten Zwischenstationen jeder Route dieses Anschlusses zu), sonst die
/// Flughafencode-Tabelle (trifft auf die Rechenzentren am Ende zu).
/// `None`, wenn Reverse-DNS fehlschlägt oder der Hostname keinem bekannten
/// Muster entspricht -- der Aufrufer fällt dann auf den Länder-Mittelpunkt
/// zurück.
pub fn resolve_precise(ip: Ipv4Addr) -> Option<(f32, f32, String, &'static str)> {
    let hostname = reverse_dns(ip)?;
    resolve_with(ip, &hostname, forward_dns)
}

/// Auswahl der Hinweise, ohne Netzwerk testbar: `forward` löst einen
/// Hostnamen auf seine IPv4-Adressen auf.
///
/// Der Flughafencode gilt nur, wenn der Hostname **vorwärts** wieder auf `ip`
/// zeigt (forward-confirmed reverse DNS). Den PTR-Eintrag setzt, wer den
/// Adressblock verwaltet, und könnte dort einen beliebigen Ort behaupten;
/// den Vorwärts-Eintrag der Anbieterdomain (z. B. `1e100.net`) kann er nicht
/// setzen. Das Telekom-Schema bleibt ungeprüft: Router haben oft keinen
/// Vorwärts-Eintrag, und der Hinweis kommt dort nur über die feste Endung.
fn resolve_with(
    ip: Ipv4Addr,
    hostname: &str,
    forward: impl FnOnce(&str) -> Vec<Ipv4Addr>,
) -> Option<(f32, f32, String, &'static str)> {
    let (lat, lon, city, country) = match dtag_city_hint(hostname) {
        Some(hint) => hint,
        None => {
            let hint = airport_hint(hostname)?;
            forward(hostname).contains(&ip).then_some(hint)?
        }
    };
    Some((lat, lon, city.to_string(), country))
}

/// Vorwärts-Auflösung wie [`reverse_dns`] über `getent` (`ahostsv4` liefert
/// je Adresse eine Zeile `<ip> STREAM <name>`). Fehler ergeben eine leere
/// Liste, der Hinweis gilt dann als unbestätigt (Regel 16).
fn forward_dns(hostname: &str) -> Vec<Ipv4Addr> {
    let Ok(output) = Command::new("getent").arg("ahostsv4").arg(hostname).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erkennt_google_stil_hostnamen() {
        assert_eq!(
            airport_hint("fra16s48-in-f14.1e100.net"),
            Some((50.03, 8.57, "Frankfurt", "DE"))
        );
        assert_eq!(
            airport_hint("lhr25s16-in-f14.1e100.net"),
            Some((51.47, -0.45, "London", "GB"))
        );
    }

    #[test]
    fn hinweise_tragen_das_land_des_ortes() {
        // Die Google-IP, die DB-IP fälschlich unter AE führt.
        assert_eq!(
            airport_hint("zrh04s07-in-f170.1e100.net"),
            Some((47.46, 8.55, "Zürich", "CH"))
        );
        assert_eq!(
            dtag_city_hint("f-eh1-i.F.DE.NET.DTAG.DE").map(|(.., country)| country),
            Some("DE")
        );
    }

    const ZRH_HOST: &str = "zrh04s07-in-f170.1e100.net";
    const ZRH_IP: Ipv4Addr = Ipv4Addr::new(172, 217, 19, 170);

    #[test]
    fn flughafen_hinweis_gilt_nur_bei_bestaetigtem_hostnamen() {
        let bestaetigt = resolve_with(ZRH_IP, ZRH_HOST, |_| vec![ZRH_IP]);
        assert_eq!(bestaetigt.map(|(.., city, country)| (city, country)), Some(("Zürich".to_string(), "CH")));
        // Der Hostname löst auf eine andere IP auf: gefälschter PTR-Eintrag.
        let andere_ip = Ipv4Addr::new(203, 0, 113, 7);
        assert_eq!(resolve_with(andere_ip, ZRH_HOST, |_| vec![ZRH_IP]), None);
        // Keine Vorwärts-Auflösung: nicht bestätigt.
        assert_eq!(resolve_with(ZRH_IP, ZRH_HOST, |_| vec![]), None);
    }

    #[test]
    fn telekom_hinweis_braucht_keine_vorwaerts_pruefung() {
        let hit = resolve_with(ZRH_IP, "f-eh1-i.F.DE.NET.DTAG.DE", |_| vec![]);
        assert_eq!(hit.map(|(.., city, country)| (city, country)), Some(("Frankfurt".to_string(), "DE")));
    }

    #[test]
    fn erkennt_cdn_stil_hostnamen_mit_bindestrich() {
        assert_eq!(
            airport_hint("server-1-2-3-4.fra-1.r.cloudfront.net").map(|(_, _, city, _)| city),
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
            dtag_city_hint("f-eh1-i.F.DE.NET.DTAG.DE").map(|(_, _, city, _)| city),
            Some("Frankfurt")
        );
        assert_eq!(
            dtag_city_hint("m-ef1-i.M.DE.NET.DTAG.DE").map(|(_, _, city, _)| city),
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
