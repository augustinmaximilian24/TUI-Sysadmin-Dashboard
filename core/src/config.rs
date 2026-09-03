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
    /// Einstellungen für Zeitprofil-Baselines pro Unit (Phase 4).
    pub baseline: BaselineConfig,
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
///
/// Die Schwellwerte `score_*` beziehen sich auf den **kombinierten** Score,
/// der per Konstruktion in 0..1 liegt (siehe `analysis::engine`). Die
/// `*_reference`-Werte legen fest, ab welchem Rohwert ein Einzelsignal als
/// voll ausgeschlagen (= 1.0) gilt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AnalysisConfig {
    /// Länge des Gleitfensters in Sekunden für Entropie-Berechnung.
    pub window_seconds: u64,
    /// Länge eines Zeit-Buckets in Sekunden für die Raten-Historie.
    pub bucket_seconds: u64,
    /// Anzahl abgeschlossener Buckets, die je Template vorgehalten werden.
    pub rate_history_buckets: usize,
    /// Harte Obergrenze an Ereignissen im Gleitfenster (Regel 18).
    pub max_window_events: usize,
    /// Nach wie vielen inaktiven Buckets ein Template vergessen wird.
    pub idle_eviction_buckets: u64,
    /// Dauer der Lernphase in Minuten, in der nur beobachtet, nicht alarmiert wird.
    pub learning_phase_minutes: u64,
    /// Glättungsparameter α der Surprisal-Berechnung (Lidstone/Jeffreys).
    pub surprisal_smoothing_alpha: f64,
    /// Gewicht des Raten-Signals in der Score-Kombination.
    pub weight_rate: f64,
    /// Gewicht des Surprisal-Signals in der Score-Kombination.
    pub weight_surprisal: f64,
    /// Gewicht des Entropie-Signals in der Score-Kombination.
    pub weight_entropy: f64,
    /// Z-Score, ab dem das Raten-Signal als voll ausgeschlagen gilt.
    pub rate_z_reference: f64,
    /// Surprisal in Bit, ab dem dieses Signal als voll ausgeschlagen gilt.
    pub surprisal_reference_bits: f64,
    /// Entropie-Z-Score, ab dem dieses Signal als voll ausgeschlagen gilt.
    pub entropy_z_reference: f64,
    /// Kombinierter Score, ab dem eine Anomalie als `info` gilt.
    pub score_info_threshold: f64,
    /// Kombinierter Score, ab dem eine Anomalie als `warn` gilt.
    pub score_warn_threshold: f64,
    /// Kombinierter Score, ab dem eine Anomalie als `critical` gilt.
    pub score_critical_threshold: f64,
    /// Faktor für die Hysterese: ein Level wird erst unterhalb von
    /// `schwelle * faktor` wieder verlassen.
    pub hysteresis_exit_factor: f64,
    /// Sperrzeit in Sekunden, bevor dasselbe Template erneut meldet.
    pub cooldown_seconds: u64,
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
            bucket_seconds: 5,
            rate_history_buckets: 60,
            max_window_events: 50_000,
            idle_eviction_buckets: 720,
            learning_phase_minutes: 10,
            surprisal_smoothing_alpha: 0.5,
            weight_rate: 1.0,
            weight_surprisal: 0.8,
            weight_entropy: 0.4,
            rate_z_reference: 8.0,
            surprisal_reference_bits: 12.0,
            entropy_z_reference: 6.0,
            // Empirisch am Replay-Korpus bestimmt: gewöhnliches Poisson-
            // Rauschen des Normalbetriebs erreicht dort höchstens 0.48,
            // echte Ereignisse (OOM-Kill, Bruteforce) beginnen bei 0.53.
            score_info_threshold: 0.5,
            score_warn_threshold: 0.6,
            score_critical_threshold: 0.8,
            hysteresis_exit_factor: 0.8,
            cooldown_seconds: 300,
            template_similarity_threshold: 0.7,
            max_templates: 5000,
        }
    }
}

/// Einstellungen für Zeitprofil-Baselines pro Unit (Phase 4).
/// Siehe `docs/phase4-baselines.md` Abschnitt 8 für die Bedeutung jedes
/// Feldes im Gesamtentwurf.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BaselineConfig {
    /// Halbwertszeit des exponentiellen Zerfalls in Stunden.
    pub half_life_hours: f64,
    /// Vertrauensschwelle (Beobachtungsgewicht) je Slot-Baseline.
    ///
    /// Abweichung vom Entwurfsdokument (dort: 24.0, "~2 Minuten an
    /// Buckets"): Der Replay-Test in Phase 4 Schritt 7 deckte auf, dass 24
    /// zu niedrig ist -- eine einzelne durchgängige Sitzung erreicht diesen
    /// Wert, bevor überhaupt Tag-zu-Tag-Streuung beobachtet werden konnte,
    /// wodurch die Baseline sich selbst auf Basis einer untypisch glatten
    /// Kurzzeit-Stichprobe für vertrauenswürdig erklärt (ein Median/MAD aus
    /// z. B. 30 Buckets *derselben* Sitzung ist keine verlässliche
    /// Schätzung der echten Streuung). 720.0 entspricht bei 5-Sekunden-
    /// Buckets höchstens einer Stunde durchgängiger Aktivität im selben
    /// Slot -- immer noch nur ein grober Näherungswert für "über mehrere
    /// Tage beobachtet", da das Gewicht nicht zwischen einer einzelnen
    /// langen Sitzung und mehreren kurzen Besuchen an verschiedenen Tagen
    /// unterscheidet. Eine genauere Lösung (Verfolgung unterschiedlicher
    /// Kalendertage je Slot) ist als Weiterentwicklung offen.
    pub min_weight: f64,
    /// Vertrauensschwelle (Beobachtungsgewicht) je Unit-Profil.
    pub profile_min_weight: f64,
    /// Harte Obergrenze unterschiedlicher Templates je Unit-Profil.
    pub profile_max_templates: usize,
    /// Harte Obergrenze unterschiedlicher Zählwerte je Histogramm.
    pub max_bins: usize,
    /// Harte Obergrenze der Anzahl gleichzeitig gehaltener Baselines
    /// (Regel 18).
    pub max_baselines: usize,
}

impl Default for BaselineConfig {
    fn default() -> Self {
        Self {
            half_life_hours: 168.0,
            min_weight: 720.0,
            profile_min_weight: 200.0,
            profile_max_templates: 512,
            max_bins: 32,
            max_baselines: 20_000,
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
        assert_eq!(config.analysis.score_warn_threshold, 0.6);
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
