//! Fehlertypen des `logsentry-proto`-Crates.

use thiserror::Error;

/// Fehler beim Verarbeiten von Protokoll-Daten (z. B. Validierung von
/// Wire-Typen). Framing-Fehler (Zeilenlimit, I/O) kommen in Schritt 2 als
/// eigener Typ dazu, da sie an einen konkreten Reader gebunden sind.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtoError {
    /// Ein `UnitName` entsprach nicht dem erwarteten systemd-Namensmuster.
    #[error("ungültiger systemd-Unit-Name {name:?}: {reason}")]
    InvalidUnitName { name: String, reason: &'static str },
}
