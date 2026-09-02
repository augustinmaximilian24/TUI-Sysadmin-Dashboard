//! `logsentry-core`: gemeinsame Typen und Konfiguration für Daemon und GUI.
//!
//! Diese Bibliothek enthält bewusst keine Ein-/Ausgabe im Hot Path (kein
//! Netzwerk, kein D-Bus) und keine `tokio`-Runtime-Abhängigkeit – sie stellt
//! reine Datentypen und Logik bereit, die aus `daemon` und `gui` genutzt
//! werden.

pub mod channel;
pub mod config;
pub mod journal;
pub mod mask;
pub mod template;

pub use channel::{ring_channel, RingReceiver, RingSender};
pub use config::Config;
pub use journal::{JournalEvent, JournalParseError};
pub use template::{TemplateEngine, TemplateId, TemplateMatch};
