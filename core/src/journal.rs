//! Deserialisierung einzelner `journalctl -o json`-Zeilen.
//!
//! Das Journal liefert numerische Felder als JSON-Strings, nicht als
//! JSON-Zahlen. `MESSAGE` ist bei nicht-UTF-8-Inhalt ein Byte-Array statt
//! eines Strings (Regel 19) – beide Fälle werden hier behandelt, ohne dass
//! eine fehlerhafte Zeile zum Absturz führt (Regel 16).

use serde::Deserialize;
use thiserror::Error;

/// Ein normalisiertes Journal-Ereignis, wie es intern weiterverarbeitet wird.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalEvent {
    /// Zeitstempel in Mikrosekunden seit der Unix-Epoche.
    pub realtime_timestamp_us: u64,
    /// Name der systemd-Unit, sofern vom Journal gemeldet.
    pub systemd_unit: Option<String>,
    /// Prozess-ID des loggenden Prozesses.
    pub pid: Option<i32>,
    /// Syslog-Priorität (0 = emerg … 7 = debug).
    pub priority: Option<u8>,
    /// Die eigentliche Log-Nachricht. Nicht-UTF-8-Inhalte werden verlustbehaftet
    /// (`from_utf8_lossy`) in einen String überführt, damit die Analyse-Pipeline
    /// ausschließlich mit `String` arbeiten kann.
    pub message: String,
    /// War die Nachricht ursprünglich kein gültiges UTF-8 (Byte-Array-Fall)?
    pub message_was_binary: bool,
    /// Hostname, sofern vom Journal gemeldet.
    pub hostname: Option<String>,
}

/// Fehler beim Parsen einer einzelnen Journal-Zeile.
///
/// Wird vom Aufrufer gezählt und verworfen (Regel 16), nie zu einem Panic.
#[derive(Debug, Error, PartialEq)]
pub enum JournalParseError {
    /// Die Zeile war kein gültiges JSON.
    #[error("Zeile ist kein gültiges JSON: {0}")]
    InvalidJson(String),
    /// Das Pflichtfeld `__REALTIME_TIMESTAMP` fehlte oder war nicht parsbar.
    #[error("__REALTIME_TIMESTAMP fehlt oder ist nicht parsbar")]
    MissingTimestamp,
}

/// Roh-Repräsentation der von journalctl gelieferten JSON-Zeile.
/// Unbekannte Felder werden von serde stillschweigend ignoriert.
#[derive(Debug, Deserialize)]
struct RawJournalEntry {
    #[serde(rename = "__REALTIME_TIMESTAMP")]
    realtime_timestamp: Option<String>,
    #[serde(rename = "_SYSTEMD_UNIT")]
    systemd_unit: Option<String>,
    #[serde(rename = "_PID")]
    pid: Option<String>,
    #[serde(rename = "PRIORITY")]
    priority: Option<String>,
    #[serde(rename = "MESSAGE")]
    message: Option<RawMessage>,
    #[serde(rename = "_HOSTNAME")]
    hostname: Option<String>,
}

/// `MESSAGE` ist entweder ein UTF-8-String oder – bei nicht-UTF-8-Inhalt im
/// Log – ein Array von Bytes (Regel 19).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawMessage {
    Text(String),
    Bytes(Vec<u8>),
}

/// Parst eine einzelne Zeile aus `journalctl -o json` zu einem [`JournalEvent`].
///
/// Fehlerhafte Zeilen (kaputtes JSON, fehlender Zeitstempel) liefern einen
/// [`JournalParseError`] statt zu paniken; der Aufrufer entscheidet, ob und
/// wie ein Drop-Zähler erhöht wird.
pub fn parse_journal_line(line: &str) -> Result<JournalEvent, JournalParseError> {
    let raw: RawJournalEntry = serde_json::from_str(line)
        .map_err(|err| JournalParseError::InvalidJson(err.to_string()))?;

    let realtime_timestamp_us = raw
        .realtime_timestamp
        .as_deref()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or(JournalParseError::MissingTimestamp)?;

    let pid = raw
        .pid
        .as_deref()
        .and_then(|value| value.parse::<i32>().ok());
    let priority = raw
        .priority
        .as_deref()
        .and_then(|value| value.parse::<u8>().ok());

    let (message, message_was_binary) = match raw.message {
        Some(RawMessage::Text(text)) => (text, false),
        Some(RawMessage::Bytes(bytes)) => (String::from_utf8_lossy(&bytes).into_owned(), true),
        None => (String::new(), false),
    };

    Ok(JournalEvent {
        realtime_timestamp_us,
        systemd_unit: raw.systemd_unit,
        pid,
        priority,
        message,
        message_was_binary,
        hostname: raw.hostname,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn fixture_lines(name: &str) -> Vec<String> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("Fixture {path:?} konnte nicht gelesen werden: {err}"))
            .lines()
            .map(|line| line.to_string())
            .collect()
    }

    #[test]
    fn parst_normale_textzeile() {
        let lines = fixture_lines("journal_text_lines.ndjson");
        let event = parse_journal_line(&lines[0]).expect("muss parsbar sein");
        assert_eq!(event.realtime_timestamp_us, 1_735_689_600_000_000);
        assert_eq!(event.systemd_unit.as_deref(), Some("sshd.service"));
        assert_eq!(event.pid, Some(1234));
        assert_eq!(event.priority, Some(6));
        assert_eq!(event.message, "Accepted publickey for admin from 10.0.0.5");
        assert!(!event.message_was_binary);
    }

    #[test]
    fn parst_alle_fixture_zeilen_ohne_fehler() {
        let lines = fixture_lines("journal_text_lines.ndjson");
        for line in lines {
            parse_journal_line(&line).expect("alle Fixture-Zeilen müssen gültig sein");
        }
    }

    #[test]
    fn behandelt_message_als_byte_array() {
        let lines = fixture_lines("journal_binary_message.ndjson");
        let event = parse_journal_line(&lines[0]).expect("muss parsbar sein");
        assert!(event.message_was_binary);
        // 0xFF ist kein gültiges UTF-8 -> from_utf8_lossy ersetzt es durch U+FFFD.
        assert!(event.message.contains('\u{FFFD}'));
    }

    #[test]
    fn kaputtes_json_liefert_fehler_statt_panic() {
        let result = parse_journal_line("das ist kein json {{{");
        assert!(matches!(result, Err(JournalParseError::InvalidJson(_))));
    }

    #[test]
    fn fehlender_zeitstempel_liefert_fehler() {
        let result = parse_journal_line(r#"{"MESSAGE": "ohne Zeitstempel"}"#);
        assert_eq!(result, Err(JournalParseError::MissingTimestamp));
    }

    #[test]
    fn fehlende_optionale_felder_sind_kein_fehler() {
        let result = parse_journal_line(r#"{"__REALTIME_TIMESTAMP": "1000000"}"#);
        let event = result.expect("nur Pflichtfeld gesetzt, muss parsbar sein");
        assert_eq!(event.realtime_timestamp_us, 1_000_000);
        assert_eq!(event.systemd_unit, None);
        assert_eq!(event.message, "");
    }
}
