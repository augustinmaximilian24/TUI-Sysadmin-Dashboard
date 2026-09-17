//! Ingestion-Task: liest `journalctl -o json` zeilenweise, parst jede Zeile
//! zu einem [`JournalEvent`] und speist sie in den bounded Ring-Kanal ein.
//!
//! Zwei Modi: `Live` (`journalctl -f`, folgt dem Stream und startet bei
//! unerwartetem Prozessende mit exponentiellem Backoff neu) und `Replay`
//! (`--since`/`--until`, läuft einmal durch und kehrt danach zurück).

use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use logsentry_core::config::IngestionConfig;
use logsentry_core::journal::{parse_journal_line, JournalEvent};
use logsentry_core::RingSender;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Obergrenze für den exponentiellen Backoff beim Neustart von `journalctl`.
const MAX_BACKOFF_MS: u64 = 30_000;

/// Mindestlaufzeit eines Durchlaufs, ab der der Backoff nach seinem Ende
/// wieder auf den Basiswert zurückgesetzt wird, statt weiter zu wachsen.
/// Ohne diese Schwelle blieb `backoff_ms` nach einer einzigen Störung für
/// die gesamte Prozesslaufzeit auf dem zuletzt erreichten Wert stehen, auch
/// wenn `journalctl` danach stunden- oder tagelang störungsfrei lief.
const MIN_STABLE_RUN: Duration = Duration::from_secs(60);

/// Betriebsmodus der Ingestion.
#[derive(Debug, Clone, PartialEq)]
pub enum IngestionMode {
    /// Folgt dem Live-Stream (`journalctl -f`) und startet bei Absturz neu.
    Live,
    /// Liest einen historischen Ausschnitt (`--since`/optional `--until`)
    /// und kehrt danach zurück, ohne neu zu starten.
    Replay {
        /// Startzeitpunkt, im von `journalctl --since` akzeptierten Format.
        since: String,
        /// Optionales Ende, im von `journalctl --until` akzeptierten Format.
        until: Option<String>,
    },
}

/// Fehler der Ingestion-Pipeline.
#[derive(Debug, Error)]
pub enum IngestionError {
    /// Die Rechteprüfung selbst konnte nicht durchgeführt werden (z. B. `id` fehlt).
    #[error("Rechteprüfung konnte nicht durchgeführt werden: {0}")]
    PermissionCheckFailed(#[source] std::io::Error),

    /// Der Benutzer ist in keiner Journal-lesenden Gruppe.
    #[error(
        "Benutzer ist nicht in einer Journal-lesenden Gruppe (aktuelle Gruppen: {groups:?}). \
         Lösung: `sudo usermod -aG systemd-journal $USER` ausführen, danach neu anmelden \
         (oder die Gruppe `adm`, je nach Distribution)."
    )]
    MissingJournalPermission {
        /// Die tatsächlich ermittelten Gruppen des aktuellen Benutzers.
        groups: Vec<String>,
    },

    /// `journalctl` konnte nicht gestartet werden (z. B. nicht im PATH).
    #[error("journalctl-Prozess konnte nicht gestartet werden: {0}")]
    SpawnFailed(#[source] std::io::Error),

    /// Der gestartete Prozess lieferte kein stdout-Handle.
    #[error("journalctl-Prozess lieferte kein stdout-Handle")]
    StdoutMissing,

    /// Ein I/O-Fehler beim Lesen aus `journalctl`s stdout.
    #[error("Fehler beim Lesen von journalctl-stdout: {0}")]
    ReadFailed(#[source] std::io::Error),
}

/// Prüft rein anhand von UID und Gruppenliste, ob Journal-Lesezugriff besteht.
/// Reine Funktion, unabhängig vom tatsächlichen `id`-Aufruf, daher gut testbar.
fn has_journal_access(uid: u32, groups: &[String]) -> bool {
    uid == 0 || groups.iter().any(|g| g == "systemd-journal" || g == "adm")
}

/// Prüft beim Start, ob der aktuelle Benutzer das Journal lesen darf
/// (Mitgliedschaft in `systemd-journal` oder `adm`, oder root).
///
/// Liefert bei fehlender Berechtigung eine klare Fehlermeldung mit
/// Lösungsvorschlag statt eines kryptischen `journalctl`-Fehlers.
pub async fn check_journal_permissions() -> Result<(), IngestionError> {
    let uid_output = Command::new("id")
        .arg("-u")
        .output()
        .await
        .map_err(IngestionError::PermissionCheckFailed)?;
    let uid: u32 = String::from_utf8_lossy(&uid_output.stdout)
        .trim()
        .parse()
        .unwrap_or(u32::MAX);

    let groups_output = Command::new("id")
        .arg("-nG")
        .output()
        .await
        .map_err(IngestionError::PermissionCheckFailed)?;
    let groups: Vec<String> = String::from_utf8_lossy(&groups_output.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect();

    if has_journal_access(uid, &groups) {
        Ok(())
    } else {
        Err(IngestionError::MissingJournalPermission { groups })
    }
}

/// Baut die Argumentliste für `journalctl` aus dem gewählten Modus.
/// Reine Funktion (kein Prozessstart), daher direkt testbar.
fn journalctl_args(mode: &IngestionMode) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        "json".to_string(),
        "--no-pager".to_string(),
    ];
    match mode {
        IngestionMode::Live => {
            args.push("-f".to_string());
            args.push("-n".to_string());
            args.push("0".to_string());
        }
        IngestionMode::Replay { since, until } => {
            args.push("--since".to_string());
            args.push(since.clone());
            if let Some(until) = until {
                args.push("--until".to_string());
                args.push(until.clone());
            }
        }
    }
    args
}

/// Startet `journalctl` als Kindprozess mit an den Modus angepassten Argumenten.
fn spawn_journalctl(mode: &IngestionMode) -> Result<tokio::process::Child, IngestionError> {
    Command::new("journalctl")
        .args(journalctl_args(mode))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(IngestionError::SpawnFailed)
}

/// Verarbeitet einen einzelnen Durchlauf: Prozess starten, Zeilen lesen und
/// parsen, bis der Prozess endet oder ein Lesefehler auftritt.
async fn read_one_pass(
    mode: &IngestionMode,
    sender: &RingSender<JournalEvent>,
    parse_error_counter: &Arc<AtomicU64>,
) -> Result<std::process::ExitStatus, IngestionError> {
    let mut child = spawn_journalctl(mode)?;
    let stdout = child.stdout.take().ok_or(IngestionError::StdoutMissing)?;
    let mut lines = BufReader::new(stdout).lines();

    loop {
        let next = lines
            .next_line()
            .await
            .map_err(IngestionError::ReadFailed)?;
        let Some(line) = next else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        match parse_journal_line(&line) {
            Ok(event) => sender.send(event).await,
            Err(err) => {
                // Regel 16: fehlerhafte Zeilen zählen und verwerfen, nie paniken.
                parse_error_counter.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(fehler = %err, "Journal-Zeile verworfen");
            }
        }
    }

    child.wait().await.map_err(IngestionError::ReadFailed)
}

/// Haupt-Schleife der Ingestion. Prüft zunächst die Berechtigungen, dann:
/// - im `Replay`-Modus: ein Durchlauf, danach `Ok(())`.
/// - im `Live`-Modus: endloser Durchlauf mit exponentiellem Backoff bei
///   unerwartetem Prozessende (Obergrenze [`MAX_BACKOFF_MS`]).
pub async fn run_ingestion(
    mode: IngestionMode,
    config: &IngestionConfig,
    sender: RingSender<JournalEvent>,
    parse_error_counter: Arc<AtomicU64>,
) -> Result<(), IngestionError> {
    check_journal_permissions().await?;

    if let IngestionMode::Replay { .. } = &mode {
        let status = read_one_pass(&mode, &sender, &parse_error_counter).await?;
        tracing::info!(status = ?status, "Replay-Durchlauf abgeschlossen");
        return Ok(());
    }

    let base_backoff_ms = config.restart_backoff_ms.max(1);
    let mut backoff_ms = base_backoff_ms;
    loop {
        // Sowohl ein sauber beendeter Durchlauf (Ok) als auch ein
        // Fehlschlag beim Starten/Lesen (Err, z. B. ein transienter
        // fork()-Fehler unter MemoryMax oder ein EIO auf der Pipe) führen
        // im Live-Modus zum selben Backoff-und-Neustart -- vorher ließ der
        // `?`-Operator jeden Err-Fall direkt aus der Ingestion und damit den
        // ganzen Daemon-Prozess enden, statt es erneut zu versuchen.
        let started_at = tokio::time::Instant::now();
        let outcome = read_one_pass(&mode, &sender, &parse_error_counter).await;

        if started_at.elapsed() >= MIN_STABLE_RUN {
            backoff_ms = base_backoff_ms;
        }

        match outcome {
            Ok(status) => {
                tracing::warn!(
                    status = ?status,
                    backoff_ms,
                    "journalctl-Prozess unerwartet beendet, starte nach Backoff neu"
                );
            }
            Err(err) => {
                tracing::warn!(
                    fehler = %err,
                    backoff_ms,
                    "Ingestion-Durchlauf fehlgeschlagen, starte nach Backoff neu"
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms.saturating_mul(2)).min(MAX_BACKOFF_MS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_hat_immer_zugriff() {
        assert!(has_journal_access(0, &[]));
    }

    #[test]
    fn systemd_journal_gruppe_gewaehrt_zugriff() {
        let groups = vec!["users".to_string(), "systemd-journal".to_string()];
        assert!(has_journal_access(1000, &groups));
    }

    #[test]
    fn adm_gruppe_gewaehrt_zugriff() {
        let groups = vec!["adm".to_string()];
        assert!(has_journal_access(1000, &groups));
    }

    #[test]
    fn ohne_passende_gruppe_kein_zugriff() {
        let groups = vec!["users".to_string(), "docker".to_string()];
        assert!(!has_journal_access(1000, &groups));
    }

    #[test]
    fn live_args_enthalten_follow_und_no_backlog() {
        let args = journalctl_args(&IngestionMode::Live);
        assert!(args.contains(&"-f".to_string()));
        assert!(args
            .windows(2)
            .any(|w| w == ["-n".to_string(), "0".to_string()]));
        assert!(args.contains(&"json".to_string()));
    }

    #[test]
    fn replay_args_enthalten_since_und_optional_until() {
        let mode = IngestionMode::Replay {
            since: "2026-01-01 00:00:00".to_string(),
            until: Some("2026-01-02 00:00:00".to_string()),
        };
        let args = journalctl_args(&mode);
        assert!(args.contains(&"--since".to_string()));
        assert!(args.contains(&"2026-01-01 00:00:00".to_string()));
        assert!(args.contains(&"--until".to_string()));
        assert!(args.contains(&"2026-01-02 00:00:00".to_string()));
    }

    #[test]
    fn replay_ohne_until_laesst_argument_weg() {
        let mode = IngestionMode::Replay {
            since: "2026-01-01 00:00:00".to_string(),
            until: None,
        };
        let args = journalctl_args(&mode);
        assert!(!args.contains(&"--until".to_string()));
    }

    /// Integrationstest gegen den echten `journalctl`-Binary im Replay-Modus
    /// (Regel 28: jedes Feature muss im Replay-Modus gelaufen sein). Läuft
    /// gegen einen Zeitraum ohne erwartete Treffer, da dieser Container
    /// keinen persistenten Journal-Bestand hat – geprüft wird der volle
    /// Codepfad (Spawn, Lesen, Beenden), nicht der Inhalt.
    #[tokio::test]
    async fn replay_modus_laeuft_gegen_echtes_journalctl_durch() {
        if which_journalctl().is_none() {
            eprintln!("journalctl nicht gefunden, Test übersprungen");
            return;
        }

        let mode = IngestionMode::Replay {
            since: "2000-01-01 00:00:00".to_string(),
            until: Some("2000-01-01 00:00:01".to_string()),
        };
        let config = IngestionConfig::default();
        let (tx, _rx) = logsentry_core::ring_channel::<JournalEvent>(16);
        let counter = Arc::new(AtomicU64::new(0));

        let result = run_ingestion(mode, &config, tx, counter).await;
        assert!(
            result.is_ok(),
            "Replay-Durchlauf sollte fehlerfrei enden: {result:?}"
        );
    }

    fn which_journalctl() -> Option<()> {
        std::process::Command::new("journalctl")
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|_| ())
    }
}
