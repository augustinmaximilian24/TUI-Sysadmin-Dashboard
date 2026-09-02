//! `logsentry-core`: gemeinsame Typen und Konfiguration für Daemon und GUI.
//!
//! Diese Bibliothek enthält bewusst keine Ein-/Ausgabe im Hot Path (kein
//! Netzwerk, kein D-Bus) und keine `tokio`-Runtime-Abhängigkeit – sie stellt
//! reine Datentypen und Logik bereit, die aus `daemon` und `gui` genutzt
//! werden.

pub mod config;

pub use config::Config;
