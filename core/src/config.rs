//! Zentrale Konfiguration für Collector und GUI.
//!
//! Die Konfiguration wird aus einer TOML-Datei geladen. Fehlt die Datei
//! oder einzelne Felder, greifen sinnvolle Default-Werte (Regel 22:
//! Analyseparameter gehören in die Konfiguration, nicht als Magic Numbers
//! in den Code).

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Fehler beim Laden oder Parsen der Konfiguration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Die Konfigurationsdatei konnte nicht gelesen werden (z. B. fehlende Datei).
    #[error("Konfigurationsdatei konnte nicht gelesen werden: {0}")]
    Read(#[from] std::io::Error),

    /// Der Inhalt der Datei ist kein gültiges TOML gemäß dem Config-Schema.
    #[error("Konfiguration konnte nicht geparst werden: {0}")]
    Parse(#[from] toml::de::Error),
}

/// Wurzel-Konfiguration von `logsentry`.
///
/// Wird aus `logsentry.toml` geladen. Jedes Feld hat einen Default,
/// sodass eine leere oder teilweise Datei nicht zum Absturz führt.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Einstellungen zur Journal-Ingestion (Phase 1).
    pub ingestion: IngestionConfig,
    /// Einstellungen für die statistische Analyse (Phase 3).
    pub analysis: AnalysisConfig,
    /// Pfad und Rechte des Unix-Sockets (Phase 6).
    pub socket: SocketConfig,
}

/// Einstellungen für die Journal-Ingestion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct IngestionConfig {
    /// Größe des bounded mpsc-Kanals zwischen Ingestion-Task und Normalizer (Regel 17/18).
    pub channel_capacity: usize,
    /// Backoff-Basiswert in Millisekunden, wenn `journalctl` unerwartet endet.
    pub restart_backoff_ms: u64,
}

impl Default for IngestionConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 1024,
            restart_backoff_ms: 500,
        }
    }
}

/// Einstellungen für die Anomalie-Analyse (Ringpuffer, Schwellwerte).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AnalysisConfig {
    /// Länge des Gleitfensters in Sekunden für Entropie-Berechnung.
    pub window_seconds: u64,
    /// Schwellwert für den robusten Z-Score, ab dem eine Warnung ausgelöst wird.
    pub z_score_warn_threshold: f64,
    /// Schwellwert für den robusten Z-Score, ab dem ein kritischer Alarm ausgelöst wird.
    pub z_score_critical_threshold: f64,
    /// Dauer der Lernphase in Minuten, in der nur beobachtet, nicht alarmiert wird.
    pub learning_phase_minutes: u64,
    /// Ähnlichkeitsschwelle (0.0–1.0) für das Drain-artige Template-Clustering:
    /// Anteil übereinstimmender Tokens, ab dem eine Zeile einem bestehenden
    /// Cluster statt einem neuen zugeordnet wird.
    pub template_similarity_threshold: f64,
    /// Harte Obergrenze der Anzahl gleichzeitig verwalteter Templates
    /// (Regel 18: kein unbeschränktes Wachstum der Template-Registry).
    pub max_templates: usize,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        Self {
            window_seconds: 60,
            z_score_warn_threshold: 3.5,
            z_score_critical_threshold: 6.0,
            learning_phase_minutes: 10,
            template_similarity_threshold: 0.7,
            max_templates: 5000,
        }
    }
}

/// Einstellungen für den Unix-Socket, über den GUI und Daemon kommunizieren.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SocketConfig {
    /// Pfad zur Socket-Datei (Regel 11: unter `/run/logsentry/`).
    pub path: String,
}

impl Default for SocketConfig {
    fn default() -> Self {
        Self {
            path: "/run/logsentry/collector.sock".to_string(),
        }
    }
}

impl Config {
    /// Lädt die Konfiguration aus einer TOML-Datei am gegebenen Pfad.
    ///
    /// Fehlende Felder in der Datei werden mit Defaults aufgefüllt.
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&raw)?;
        Ok(config)
    }

    /// Lädt die Konfiguration aus einem TOML-String (z. B. für Tests).
    pub fn load_from_str(raw: &str) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(raw)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_hat_erwartete_werte() {
        let config = Config::default();
        assert_eq!(config.ingestion.channel_capacity, 1024);
        assert_eq!(config.analysis.window_seconds, 60);
        assert_eq!(config.socket.path, "/run/logsentry/collector.sock");
    }

    #[test]
    fn leere_toml_ergibt_default_config() {
        let config = Config::load_from_str("").expect("leeres TOML muss parsbar sein");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn teilweise_toml_ueberschreibt_nur_gesetzte_felder() {
        let raw = r#"
            [analysis]
            window_seconds = 120
        "#;
        let config = Config::load_from_str(raw).expect("gueltiges TOML");
        assert_eq!(config.analysis.window_seconds, 120);
        // Nicht gesetzte Felder bleiben beim Default.
        assert_eq!(config.analysis.z_score_warn_threshold, 3.5);
        assert_eq!(config.ingestion.channel_capacity, 1024);
    }

    #[test]
    fn datei_laden_funktioniert() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("logsentry.toml");
        std::fs::write(
            &file_path,
            r#"
                [socket]
                path = "/run/logsentry/test.sock"
            "#,
        )
        .expect("schreiben");

        let config = Config::load_from_file(&file_path).expect("laden");
        assert_eq!(config.socket.path, "/run/logsentry/test.sock");
    }

    #[test]
    fn fehlende_datei_liefert_fehler_statt_panic() {
        let result = Config::load_from_file("/pfad/der/nicht/existiert.toml");
        assert!(result.is_err());
    }
}
