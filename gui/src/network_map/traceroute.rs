//! Ermittelt den tatsächlichen Weg zu einer Gegenstelle (Zwischen-Hops)
//! statt nur Start und Ziel zu kennen.
//!
//! Verfahren wie schon bei `journalctl` (Ingestion) und `getent hosts`
//! ([`super::precise_location`]): ein bereits vorhandenes Systemwerkzeug
//! als Subprozess, kein eigener Netzwerk-Stack und vor allem **keine
//! zusätzlichen Rechte für die GUI** (Regel 7). Genutzt wird `mtr`, dessen
//! Hilfsprogramm `mtr-packet` auf diesem System die Capability
//! `cap_net_raw=ep` trägt -- der Rohsocket-Zugriff liegt also im bereits
//! vom Distributionspaket autorisierten Helfer, nicht im logsentry-Prozess.
//! Fehlt `mtr` oder verweigert es den Dienst, gibt es schlicht keine Hops
//! und die Karte zeichnet wie bisher die direkte Verbindung (Regel 16).
//!
//! Bewusst **nicht** `traceroute` (auf diesem System nicht installiert) und
//! nicht `tracepath` (lief im Test ohne Ausgabe in den Timeout).

use std::net::Ipv4Addr;
use std::process::Command;

/// Ein gemessener Zwischenschritt auf dem Weg zum Ziel, in der
/// Reihenfolge der Messung.
///
/// `None` steht für einen Hop, der nicht geantwortet hat (`???` in der
/// mtr-Ausgabe, üblich bei Routern, die ICMP verwerfen). Solche Hops
/// werden bewusst **nicht** weggelassen: sie sind Teil des tatsächlichen
/// Weges, und ein stillschweigend ausgelassener Zwischenschritt würde die
/// Strecke kürzer erscheinen lassen, als sie ist.
pub type Hop = Option<Ipv4Addr>;

/// Ruft `mtr` im Report-Modus auf und liefert **alle** gemessenen
/// Zwischenschritte zum Ziel, in Reihenfolge vom Start weg -- inklusive
/// des eigenen Routers, nicht antwortender Hops und des Ziels selbst als
/// letztem Eintrag.
///
/// `-n` unterdrückt mtrs eigene Namensauflösung: die Hostnamen holt später
/// [`super::precise_location`] gezielt für die Hops, die es auch auf der
/// Karte gibt -- so verzögert kein DNS-Timeout den gesamten Messlauf.
/// Der harte Zeitdeckel läuft über das `timeout`-Standardwerkzeug, da
/// [`std::process::Command`] selbst keinen Timeout kennt und ein eigener
/// Wächter-Thread hier nur Mechanik ohne Mehrwert wäre.
pub fn trace(target: Ipv4Addr, max_hops: u8, timeout_secs: u64) -> Vec<Hop> {
    let output = Command::new("timeout")
        .arg(timeout_secs.to_string())
        .arg("mtr")
        .arg("-n")
        .arg("--report")
        .arg("--report-cycles")
        .arg("1")
        .arg("--max-ttl")
        .arg(max_hops.to_string())
        .arg(target.to_string())
        .output();

    let Ok(output) = output else {
        return Vec::new(); // `mtr`/`timeout` nicht vorhanden -- kein Fehler
    };
    let Ok(text) = String::from_utf8(output.stdout) else {
        return Vec::new();
    };
    parse_report(&text)
}

/// Reine Parse-Funktion über die Report-Ausgabe von `mtr -n --report`,
/// damit sie ohne Netzwerkzugriff mit festen Fixtures testbar ist
/// (Regel 25).
///
/// Format je Hop-Zeile: `  3.|-- 62.153.181.94   0.0%  1  9.4 ...`, wobei
/// ein nicht antwortender Hop als `???` erscheint. Es wird **nichts**
/// gefiltert: welche Hops interessant sind (öffentlich, verortbar),
/// entscheidet der Aufrufer -- hier soll das Messergebnis unverfälscht
/// ankommen.
///
/// Einzige Zusammenfassung: antwortet derselbe Router auf mehrere
/// aufeinanderfolgende TTLs, zählt das als ein Zwischenschritt statt als
/// mehrere -- das ist keine Filterung, sondern eine Eigenheit der Messung.
fn parse_report(text: &str) -> Vec<Hop> {
    let mut hops: Vec<Hop> = Vec::new();
    for line in text.lines() {
        let Some((_, rest)) = line.split_once("|--") else {
            continue; // Kopfzeilen ("Start:", "HOST:")
        };
        let Some(field) = rest.split_whitespace().next() else {
            continue;
        };
        let hop: Hop = field.parse::<Ipv4Addr>().ok();
        if hop.is_some() && hops.last() == Some(&hop) {
            continue;
        }
        hops.push(hop);
    }
    hops
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Echte, gekürzte Ausgabe von `mtr -n --report --report-cycles 1`
    /// dieses Systems (Regel 25: feste Fixture statt Live-Messung im Test).
    const FIXTURE: &str = "\
Start: 2026-09-23T19:12:24+0200
HOST: HauptPc                     Loss%   Snt   Last   Avg  Best  Wrst StDev
  1.|-- 192.168.178.1              0.0%     1    2.3   2.3   2.3   2.3   0.0
  2.|-- 62.155.242.122             0.0%     1    6.6   6.6   6.6   6.6   0.0
  3.|-- ???                       100.0     1    0.0   0.0   0.0   0.0   0.0
  4.|-- 80.157.207.46              0.0%     1    9.4   9.4   9.4   9.4   0.0
  5.|-- 80.157.207.46              0.0%     1    9.5   9.5   9.5   9.5   0.0
  6.|-- 8.8.8.8                    0.0%     1    9.7   9.7   9.7   9.7   0.0
";

    #[test]
    fn liefert_alle_hops_in_reihenfolge_inklusive_router_und_ziel() {
        let hops = parse_report(FIXTURE);
        assert_eq!(
            hops,
            vec![
                Some(Ipv4Addr::new(192, 168, 178, 1)),
                Some(Ipv4Addr::new(62, 155, 242, 122)),
                None, // "???" -- Hop ohne Antwort bleibt als Lücke erhalten
                Some(Ipv4Addr::new(80, 157, 207, 46)),
                Some(Ipv4Addr::new(8, 8, 8, 8)),
            ]
        );
    }

    #[test]
    fn derselbe_router_auf_mehreren_ttls_zaehlt_einmal() {
        // Hop 4 und 5 der Fixture sind dieselbe Adresse.
        let hops = parse_report(FIXTURE);
        assert_eq!(hops.iter().filter(|h| **h == Some(Ipv4Addr::new(80, 157, 207, 46))).count(), 1);
    }

    #[test]
    fn mehrere_stille_hops_bleiben_einzeln_erhalten() {
        let text = "\
  1.|-- ???                       100.0     1    0.0   0.0   0.0   0.0   0.0
  2.|-- ???                       100.0     1    0.0   0.0   0.0   0.0   0.0
";
        assert_eq!(parse_report(text), vec![None, None]);
    }

    #[test]
    fn leere_oder_unbrauchbare_ausgabe_liefert_keine_hops() {
        assert!(parse_report("").is_empty());
        assert!(parse_report("mtr: command not found").is_empty());
    }
}
