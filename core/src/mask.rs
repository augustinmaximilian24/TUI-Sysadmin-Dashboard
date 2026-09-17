//! Regex-Masken zur Normalisierung von Log-Nachrichten.
//!
//! Jede Nachricht wird bei Leerzeichen in Tokens zerlegt; jedes Token wird
//! einzeln gegen eine feste Reihenfolge von Mustern geprüft (Zeitstempel,
//! UUID, MAC, PID-in-Klammern, IPv6, IPv4, Hex-Adresse, Pfad, Zahl) und bei
//! Treffer durch einen Platzhalter ersetzt. Die Token-basierte Prüfung (statt
//! Ersetzung im Fließtext) vermeidet die in der `regex`-Crate fehlende
//! Lookaround-Unterstützung und liefert nebenbei bereits die Tokenisierung,
//! die das Drain-artige Clustering ([`crate::template`]) braucht.

use std::sync::OnceLock;

use regex::Regex;

/// Zeichen, die am Rand eines Tokens (z. B. Satzzeichen) abgetrennt werden,
/// bevor der Kern gegen die Muster geprüft wird, und danach wieder angehängt
/// werden. So bleibt `"10.0.0.5,"` maskierbar, ohne dass das Komma stört.
/// `[`/`]` sind bewusst NICHT enthalten: sie sind Teil des PID-Klammer-Musters
/// (`sshd[1234]`) und dürfen nicht vor dessen Prüfung abgeschnitten werden.
const TRIM_CHARS: &[char] = &[',', '.', ':', ';', '"', '\'', '(', ')'];

fn timestamp_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(?:\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?|\d{2}:\d{2}:\d{2})$",
        )
        .expect("statischer Timestamp-Regex muss kompilieren")
    })
}

fn uuid_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
            .expect("statischer UUID-Regex muss kompilieren")
    })
}

fn mac_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:[0-9a-fA-F]{2}[:-]){5}[0-9a-fA-F]{2}$")
            .expect("statischer MAC-Regex muss kompilieren")
    })
}

/// Erfasst das verbreitete Syslog-Format `prozessname[1234]`. Der Prozessname
/// bleibt erhalten, nur die PID wird maskiert.
fn pid_bracket_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^([A-Za-z_][\w.-]*)\[(\d+)\]$").expect("statischer PID-Regex muss kompilieren")
    })
}

/// Angenäherte IPv6-Erkennung (keine vollständige RFC-4291-Validierung,
/// deckt aber übliche Formen inkl. `::`-Kompression ab). Wird bewusst nach
/// der MAC-Prüfung aufgerufen, da eine MAC-Adresse sonst fälschlich als
/// IPv6 durchgehen könnte (beides kolon-getrennte Hex-Gruppen).
fn ipv6_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:[0-9a-fA-F]{0,4}:){2,7}[0-9a-fA-F]{0,4}$")
            .expect("statischer IPv6-Regex muss kompilieren")
    })
}

fn ipv4_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:\d{1,3}\.){3}\d{1,3}$").expect("statischer IPv4-Regex muss kompilieren")
    })
}

fn hex_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^0[xX][0-9a-fA-F]+$").expect("statischer Hex-Regex muss kompilieren")
    })
}

fn path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^/[\w.-]+(?:/[\w.-]*)*$").expect("statischer Pfad-Regex muss kompilieren")
    })
}

fn number_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d+$").expect("statischer Zahlen-Regex muss kompilieren"))
}

/// Trennt führende/abschließende Satzzeichen von einem Token ab.
/// Da alle Zeichen in [`TRIM_CHARS`] ASCII (1 Byte) sind, entspricht die
/// Zeichenanzahl am Rand direkt der Byte-Anzahl, was einfache Byte-Slicing
/// erlaubt.
fn split_punctuation(token: &str) -> (&str, &str, &str) {
    let lead_len: usize = token
        .chars()
        .take_while(|c| TRIM_CHARS.contains(c))
        .map(char::len_utf8)
        .sum();
    let (lead, rest) = token.split_at(lead_len);

    let trail_len: usize = rest
        .chars()
        .rev()
        .take_while(|c| TRIM_CHARS.contains(c))
        .map(char::len_utf8)
        .sum();
    let (core, trail) = rest.split_at(rest.len() - trail_len);

    (lead, core, trail)
}

/// Prüft den (bereits von Satzzeichen befreiten) Kern eines Tokens gegen
/// alle Muster in fester Reihenfolge und liefert den Platzhalter oder den
/// unveränderten Kern, falls nichts passt.
fn mask_core(core: &str) -> String {
    if core.is_empty() {
        return String::new();
    }
    if timestamp_re().is_match(core) {
        return "<TIMESTAMP>".to_string();
    }
    if uuid_re().is_match(core) {
        return "<UUID>".to_string();
    }
    if mac_re().is_match(core) {
        return "<MAC>".to_string();
    }
    if let Some(caps) = pid_bracket_re().captures(core) {
        return format!("{}[<PID>]", &caps[1]);
    }
    if ipv6_re().is_match(core) && core.matches(':').count() >= 2 {
        return "<IPV6>".to_string();
    }
    if ipv4_re().is_match(core) {
        return "<IPV4>".to_string();
    }
    if hex_re().is_match(core) {
        return "<HEX>".to_string();
    }
    if path_re().is_match(core) {
        return "<PATH>".to_string();
    }
    if number_re().is_match(core) {
        return "<NUM>".to_string();
    }
    core.to_string()
}

/// Maskiert ein einzelnes Token (inklusive Satzzeichen-Behandlung).
fn mask_token(token: &str) -> String {
    let (lead, core, trail) = split_punctuation(token);
    format!("{lead}{}{trail}", mask_core(core))
}

/// Zerlegt eine Log-Nachricht bei Leerzeichen und maskiert jedes Token.
///
/// Liefert die maskierten Tokens als Vektor (Grundlage für das Drain-artige
/// Clustering in [`crate::template`]).
pub fn mask_message(message: &str) -> Vec<String> {
    message.split_whitespace().map(mask_token).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maskiert_ipv4() {
        assert_eq!(mask_token("10.0.0.5"), "<IPV4>");
        assert_eq!(mask_token("203.0.113.9,"), "<IPV4>,");
    }

    #[test]
    fn maskiert_ipv6() {
        assert_eq!(mask_token("fe80::1"), "<IPV6>");
        assert_eq!(
            mask_token("2001:0db8:85a3:0000:0000:8a2e:0370:7334"),
            "<IPV6>"
        );
    }

    #[test]
    fn maskiert_mac_nicht_als_ipv6() {
        assert_eq!(mask_token("AA:BB:CC:DD:EE:FF"), "<MAC>");
        assert_eq!(mask_token("aa:bb:cc:dd:ee:ff"), "<MAC>");
    }

    #[test]
    fn maskiert_uuid() {
        assert_eq!(mask_token("550e8400-e29b-41d4-a716-446655440000"), "<UUID>");
    }

    #[test]
    fn maskiert_pid_in_klammern_und_behaelt_prozessnamen() {
        assert_eq!(mask_token("sshd[1234]:"), "sshd[<PID>]:");
    }

    #[test]
    fn mehrfache_trennzeichen_am_rand_werden_korrekt_getrennt() {
        // Regressionstest: frühere Implementierung schnitt hier fälschlich
        // die schließende Klammer als Teil des Kerns ab.
        assert_eq!(mask_token("cron[998]:"), "cron[<PID>]:");
        assert_eq!(mask_token("(10.0.0.5),"), "(<IPV4>),");
    }

    #[test]
    fn maskiert_hex_adresse() {
        assert_eq!(mask_token("0xdeadbeef"), "<HEX>");
    }

    #[test]
    fn maskiert_pfad() {
        assert_eq!(mask_token("/usr/bin/backup.sh"), "<PATH>");
        assert_eq!(mask_token("/var/lib/docker/234"), "<PATH>");
    }

    #[test]
    fn maskiert_reine_zahl() {
        assert_eq!(mask_token("51422"), "<NUM>");
    }

    #[test]
    fn maskiert_zeitstempel() {
        assert_eq!(mask_token("2026-01-01T00:00:00Z"), "<TIMESTAMP>");
        assert_eq!(mask_token("03:04:05"), "<TIMESTAMP>");
    }

    #[test]
    fn laesst_geraetenamen_mit_eingebetteten_ziffern_unveraendert() {
        // "eth0", "sda1", "nvme0n1" sind keine reinen Zahlen -> unverändert.
        assert_eq!(mask_token("eth0"), "eth0");
        assert_eq!(mask_token("nvme0n1"), "nvme0n1");
    }

    #[test]
    fn laesst_normale_woerter_unveraendert() {
        assert_eq!(mask_token("Accepted"), "Accepted");
        assert_eq!(mask_token("publickey"), "publickey");
        assert_eq!(mask_token("alice"), "alice");
    }

    #[test]
    fn mask_message_zerlegt_und_maskiert_ganze_zeile() {
        let tokens = mask_message("Accepted publickey for admin from 10.0.0.5");
        assert_eq!(
            tokens,
            vec!["Accepted", "publickey", "for", "admin", "from", "<IPV4>"]
        );
    }
}
