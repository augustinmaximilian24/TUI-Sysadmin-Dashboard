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
    /// Persistenz der Baselines über Neustarts hinweg (Phase 4).
    pub persistence: PersistenceConfig,
    /// Erfassung des Systemzustands (Phase 5).
    pub system: SystemConfig,
    /// Pfad und Rechte des Unix-Sockets (Phase 6).
    pub socket: SocketConfig,
    /// Allow-List, Rate-Limit und Ausführungsparameter des
    /// Aktions-Subsystems (Phase 8).
    pub actions: ActionsConfig,
    /// Prometheus-Textfile-Export (Phase 10, optional).
    pub prometheus: PrometheusConfig,
    /// Anomalie-Masken für wiederkehrende, ungefährliche Wartungsmeldungen
    /// (siehe [`crate::quiet`]).
    pub quiet: QuietConfig,
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

/// Einstellungen für die Erfassung des Systemzustands (Phase 5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SystemConfig {
    /// Abstand zwischen zwei Messungen in Sekunden.
    pub poll_interval_seconds: u64,
    /// Namen der systemd-Units, deren Status verfolgt wird (Allow-Liste
    /// statt aller Units: Regel 22, keine Magic-Werte im Code, und ein
    /// Heimserver hat typischerweise nur eine Handvoll Units, die den
    /// Blick in der GUI wert sind).
    pub watched_units: Vec<String>,
    /// Einhängepunkte, deren Belegung gemessen wird. Leer bedeutet: alle
    /// von `sysinfo` gefundenen Dateisysteme.
    pub watched_mount_points: Vec<String>,
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            poll_interval_seconds: 5,
            watched_units: vec![
                "sshd.service".to_string(),
                "cron.service".to_string(),
                "docker.service".to_string(),
                "systemd-journald.service".to_string(),
                "logsentry-daemon.service".to_string(),
            ],
            watched_mount_points: Vec::new(),
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

/// Einstellungen für die Persistenz der Baselines (Phase 4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PersistenceConfig {
    /// Pfad der redb-Datei. Der Daemon ist einziger Schreiber; Modus 0600.
    pub path: String,
    /// Abstand zwischen zwei Snapshots in Minuten. Zusätzlich wird bei
    /// sauberem Beenden gesichert.
    pub snapshot_interval_minutes: u64,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            path: "/var/lib/logsentry/baselines.redb".to_string(),
            snapshot_interval_minutes: 5,
        }
    }
}

/// Einstellungen für den Unix-Socket, über den GUI und Daemon kommunizieren.
///
/// Siehe `docs/phase6-protokoll.md` Abschnitt 5. `path` blieb beim
/// bestehenden Default aus Phase 0 (`collector.sock`), um bestehendes
/// Verhalten nicht stillschweigend zu ändern; der Entwurf schlägt
/// `logsentry.sock` vor -- beides ist nur ein Dateiname, funktional
/// gleichwertig, per Konfiguration änderbar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SocketConfig {
    /// Pfad zur Socket-Datei (Regel 11: unter `/run/logsentry/`).
    pub path: String,
    /// Gruppe, der die Socket-Datei gehört (Regel 11). Leer = kein
    /// `chown` (Dev-Betrieb ohne Root, z. B. unter `$XDG_RUNTIME_DIR`).
    pub group: String,
    /// Dateimodus der Socket-Datei, oktal (Regel 11: 0660).
    pub mode: u32,
    /// Maximale Anzahl gleichzeitiger Client-Verbindungen.
    pub max_clients: u32,
    /// Vom Client wünschbares Snapshot-Intervall, oberer Rand der Klemme.
    pub snapshot_interval_ms: u32,
    /// Unterer Rand der Klemme für das clientseitig gewünschte Intervall.
    pub min_snapshot_interval_ms: u32,
    /// Zeit, die ein Client nach dem Verbinden für `Hello` hat, bevor der
    /// Daemon trennt.
    pub hello_timeout_ms: u32,
    /// Größe des Rings für `RecentAnomalies` (an neu verbundene Clients
    /// gesendet).
    pub recent_anomalies: usize,
    /// Größe des Rohzeilen-Rings für `GetContext`.
    pub context_lines: usize,
    /// Obergrenze für `before`/`after` in einer `GetContext`-Anfrage.
    pub context_max_lines: u16,
}

impl Default for SocketConfig {
    fn default() -> Self {
        Self {
            path: "/run/logsentry/collector.sock".to_string(),
            group: "logsentry".to_string(),
            mode: 0o660,
            max_clients: 8,
            snapshot_interval_ms: 1000,
            min_snapshot_interval_ms: 250,
            hello_timeout_ms: 5000,
            recent_anomalies: 200,
            context_lines: 2000,
            context_max_lines: 200,
        }
    }
}

/// Konfiguration des Aktions-Subsystems (Phase 8).
///
/// Siehe `docs/phase8-aktionen.md` Abschnitt 3. `allowed_kinds` ist bewusst
/// `Vec<String>` statt `Vec<ActionKind>`: ein unbekannter Wert in der
/// TOML-Datei soll die Konfiguration nicht scheitern lassen, sondern beim
/// Allow-List-Abgleich einfach nie matchen (Mapping in `daemon::actions`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ActionsConfig {
    /// Globaler Schalter (Regel 26): `true` protokolliert jede Aktion nur,
    /// ohne sie auszuführen. Fail-safe-Default `true` -- ein frisch
    /// installierter Daemon führt nichts aus, bis der Betreiber das
    /// bewusst per Konfiguration ändert.
    pub dry_run: bool,
    /// Erlaubte Aktionsarten (`"restart_unit"`, `"stop_unit"`,
    /// `"terminate_process"`, `"block_ip"`, `"mute_anomaly"`). Was hier
    /// fehlt, wird abgelehnt, unabhängig von allen anderen Feldern
    /// (Regel 9).
    pub allowed_kinds: Vec<String>,
    /// Units, die per `RestartUnit`/`StopUnit` angefasst werden dürfen.
    /// Exakter Namensvergleich, keine Muster.
    pub allowed_units: Vec<String>,
    /// Maximale Anzahl Versuche je Aktionsart und Ziel innerhalb von
    /// `rate_limit_window_minutes` (Regel 14).
    pub rate_limit_max_actions: u32,
    pub rate_limit_window_minutes: u64,
    /// Wie lange nach einer `RestartUnit`/`StopUnit`/`TerminateProcess`-
    /// Aktion deren eigene Journal-Zeilen von der Anomalie-Erkennung
    /// ausgenommen werden (Regel 13).
    pub self_filter_window_secs: u64,
    pub terminate_default_grace_secs: u16,
    pub terminate_max_grace_secs: u16,
    pub block_ip_min_duration_secs: u32,
    pub block_ip_max_duration_secs: u32,
    pub nftables_family: String,
    pub nftables_table: String,
    pub nftables_set_v4: String,
    pub nftables_set_v6: String,
    /// Pfad der JSON-Lines-Audit-Datei (ein Eintrag pro Versuch, auch
    /// abgelehnte/fehlgeschlagene).
    pub audit_log_path: String,
}

impl Default for ActionsConfig {
    fn default() -> Self {
        Self {
            dry_run: true,
            allowed_kinds: Vec::new(),
            allowed_units: Vec::new(),
            rate_limit_max_actions: 3,
            rate_limit_window_minutes: 10,
            self_filter_window_secs: 30,
            terminate_default_grace_secs: 5,
            terminate_max_grace_secs: 60,
            block_ip_min_duration_secs: 60,
            block_ip_max_duration_secs: 604_800,
            nftables_family: "inet".to_string(),
            nftables_table: "filter".to_string(),
            nftables_set_v4: "logsentry_blocked_v4".to_string(),
            nftables_set_v6: "logsentry_blocked_v6".to_string(),
            audit_log_path: "/var/lib/logsentry/audit.jsonl".to_string(),
        }
    }
}

/// Prometheus-Textfile-Export (Phase 10, optional): schreibt bei jedem
/// Socket-Snapshot eine `.prom`-Datei im node_exporter-Textfile-Format.
/// Deaktiviert per Default -- reines Opt-in für Installationen, die
/// bereits Prometheus/node_exporter betreiben.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PrometheusConfig {
    pub enabled: bool,
    /// Zielpfad, typischerweise das Textfile-Collector-Verzeichnis von
    /// node_exporter (z. B. `/var/lib/node_exporter/textfile_collector/`).
    /// Wird atomar geschrieben (temporäre Datei + `rename`), damit
    /// node_exporter nie eine unvollständige Datei liest.
    pub textfile_path: String,
}

impl Default for PrometheusConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            textfile_path: "/var/lib/logsentry/logsentry.prom".to_string(),
        }
    }
}

/// Eine Anomalie-Maske: Ereignisse, deren Nachricht auf `pattern` passt
/// (und, sofern gesetzt, deren Unit exakt `unit` entspricht), werden vor
/// der statistischen Analyse ausgefiltert und laufen nie durch
/// [`AnalysisEngine`](crate::AnalysisEngine).
///
/// Anders als das laufzeit-getriggerte `MuteAnomaly` (Phase 8, GUI-Aktion
/// je Template-ID) ist dies eine statische, in der Konfiguration gepflegte
/// Allow-List für bekannte, harmlose Wartungsmeldungen (tägliche/stündliche
/// Cron-Jobs, Backup-Mounts) -- diese Ereignisse erreichen wegen ihrer
/// niedrigen Frequenz nie das Vertrauensgewicht der Zeitprofil-Baseline
/// (Phase 4) und würden sonst bei jedem Auftreten erneut als Surprisal
/// gemeldet.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct QuietTemplateRule {
    /// Exakter `_SYSTEMD_UNIT`-Name, auf den die Regel eingeschränkt wird.
    /// `None` (Default) heißt: unabhängig von der Unit anwenden.
    pub unit: Option<String>,
    /// Regex gegen die rohe Journal-Nachricht (`MESSAGE`-Feld). Ungültige
    /// Muster führen nicht zum Absturz (Regel 16): [`crate::quiet::QuietFilter`]
    /// verwirft sie mit einer Warnung.
    pub pattern: String,
}

/// Konfiguration der Anomalie-Masken für wiederkehrende Wartungsmeldungen.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct QuietConfig {
    /// Regeln in Prüfreihenfolge; die erste passende Regel entscheidet.
    pub templates: Vec<QuietTemplateRule>,
}

impl QuietConfig {
    /// Default-Masken für die auf HauptPc identifizierten Wartungs-
    /// Templates (siehe Anomalie-Review vom 2026-09-21): Timeshift-Backup-
    /// Mounts, dpkg-db-backup, logrotate, man-db, flatpak-system-helper
    /// sowie die cron-internen `CMD`/`LIST`-Protokollzeilen für
    /// `run-parts`. Alle sind tages-/stundenperiodisch, treten dadurch nie
    /// oft genug für eine vertrauenswürdige Zeitprofil-Baseline auf und
    /// lösten deshalb bei jedem Lauf erneut Surprisal-Alarme aus (bis zu
    /// `critical`).
    fn default_maintenance_rules() -> Vec<QuietTemplateRule> {
        let rule = |pattern: &str| QuietTemplateRule {
            unit: None,
            pattern: pattern.to_string(),
        };
        vec![
            rule(r"^run-timeshift-\d+-backup\.mount: Deactivated successfully\.$"),
            rule(r"\bdpkg-db-backup\.service\b"),
            rule(r"\blogrotate\.service\b"),
            rule(r"\bman-db\.service\b"),
            rule(
                r"^flatpak-system-helper\.service: (Deactivated successfully\.|Consumed [0-9.]+s CPU time\.)$",
            ),
            rule(r"^\(root\) CMD \(cd / && run-parts --report /etc/cron\.(hourly|daily|weekly|monthly)\)$"),
            rule(r"^\(root\) LIST \(root\)$"),
        ]
    }
}

impl Default for QuietConfig {
    fn default() -> Self {
        Self {
            templates: Self::default_maintenance_rules(),
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
    fn default_socket_config_hat_erwartete_werte() {
        let config = Config::default();
        assert_eq!(config.socket.group, "logsentry");
        assert_eq!(config.socket.mode, 0o660);
        assert_eq!(config.socket.max_clients, 8);
        assert_eq!(config.socket.snapshot_interval_ms, 1000);
        assert_eq!(config.socket.min_snapshot_interval_ms, 250);
        assert_eq!(config.socket.hello_timeout_ms, 5000);
        assert_eq!(config.socket.recent_anomalies, 200);
        assert_eq!(config.socket.context_lines, 2000);
        assert_eq!(config.socket.context_max_lines, 200);
    }

    #[test]
    fn default_actions_config_ist_fail_safe() {
        let config = Config::default();
        assert!(config.actions.dry_run, "Default muss dry_run=true sein");
        assert!(config.actions.allowed_kinds.is_empty());
        assert!(config.actions.allowed_units.is_empty());
        assert_eq!(config.actions.rate_limit_max_actions, 3);
        assert_eq!(config.actions.rate_limit_window_minutes, 10);
        assert_eq!(config.actions.terminate_max_grace_secs, 60);
        assert_eq!(config.actions.block_ip_max_duration_secs, 604_800);
    }

    #[test]
    fn actions_config_teiluerberschreibung_laesst_restliche_defaults_stehen() {
        let raw = r#"
            [actions]
            dry_run = false
            allowed_kinds = ["restart_unit"]
            allowed_units = ["sshd.service"]
        "#;
        let config = Config::load_from_str(raw).expect("gueltiges TOML");
        assert!(!config.actions.dry_run);
        assert_eq!(config.actions.allowed_kinds, vec!["restart_unit"]);
        assert_eq!(config.actions.allowed_units, vec!["sshd.service"]);
        // Nicht gesetzte Felder bleiben beim Default.
        assert_eq!(config.actions.rate_limit_max_actions, 3);
        assert_eq!(config.actions.self_filter_window_secs, 30);
    }

    #[test]
    fn default_prometheus_config_ist_deaktiviert() {
        let config = Config::default();
        assert!(!config.prometheus.enabled);
    }

    #[test]
    fn prometheus_config_kann_aktiviert_werden() {
        let raw = r#"
            [prometheus]
            enabled = true
            textfile_path = "/tmp/logsentry.prom"
        "#;
        let config = Config::load_from_str(raw).expect("gueltiges TOML");
        assert!(config.prometheus.enabled);
        assert_eq!(config.prometheus.textfile_path, "/tmp/logsentry.prom");
    }

    #[test]
    fn socket_config_teiluerberschreibung_laesst_restliche_defaults_stehen() {
        let raw = r#"
            [socket]
            path = "/run/logsentry/dev.sock"
            max_clients = 2
        "#;
        let config = Config::load_from_str(raw).expect("gueltiges TOML");
        assert_eq!(config.socket.path, "/run/logsentry/dev.sock");
        assert_eq!(config.socket.max_clients, 2);
        // Nicht gesetzte Felder bleiben beim Default.
        assert_eq!(config.socket.group, "logsentry");
        assert_eq!(config.socket.mode, 0o660);
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
