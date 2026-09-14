//! Fehlertypen des `logsentry-proto`-Crates.

use thiserror::Error;

/// Fehler beim Verarbeiten von Protokoll-Daten (z. B. Validierung von
/// Wire-Typen).
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtoError {
    /// Ein `UnitName` entsprach nicht dem erwarteten systemd-Namensmuster.
    #[error("ungültiger systemd-Unit-Name {name:?}: {reason}")]
    InvalidUnitName { name: String, reason: &'static str },
}

/// Fehler beim Lesen oder Schreiben einer Protokoll-Zeile
/// ([`crate::framing`]). Getrennt von [`ProtoError`], da er an einen
/// konkreten I/O-Vorgang gebunden ist und nicht `Clone`/`Eq` sein kann
/// (io::Error ist es nicht).
#[derive(Debug, Error)]
pub enum FrameError {
    /// Eine Zeile hat `max_bytes` überschritten, bevor ein `\n` gefunden
    /// wurde. Die Verbindung muss getrennt werden (siehe
    /// `docs/phase6-protokoll.md` Abschnitt 4, Regel 3).
    #[error("Zeile überschreitet das Limit von {max_bytes} Byte")]
    TooLong { max_bytes: usize },
    /// Eine vollständige Zeile war kein gültiges UTF-8.
    #[error("Zeile ist kein gültiges UTF-8: {0}")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),
    /// I/O-Fehler beim Lesen oder Schreiben.
    #[error("I/O-Fehler: {0}")]
    Io(#[from] std::io::Error),
}
