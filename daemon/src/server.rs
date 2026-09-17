//! Unix-Socket-Server des Daemons: Socket-Anlage, Accept-Schleife,
//! Client-Zähler, Shutdown-Pfad.
//!
//! Normativ: `docs/phase6-protokoll.md` Abschnitt 5 (Socket-Anlage,
//! Rechte) und Abschnitt 7 (Daemon-Struktur). Wird aus `main.rs` heraus
//! verwendet.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nix::errno::Errno;
use nix::unistd::{chown, Group};
use thiserror::Error;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

use logsentry_core::config::SocketConfig;

use crate::client_task::{self, ClientTaskConfig};
use crate::state::SharedState;

/// Zeit, die Clients nach `Goodbye(Shutdown)` zum Flushen bekommen, bevor
/// die Socket-Datei entfernt wird (Abschnitt 4, Regel 6).
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// Fehler beim Anlegen des Sockets. Jede Variante trägt genug Kontext für
/// eine verständliche Fehlermeldung an den Betreiber (Abschnitt 5).
#[derive(Debug, Error)]
pub enum ServerError {
    #[error("Verzeichnis {0} konnte nicht angelegt werden: {1}")]
    CreateDir(String, std::io::Error),
    #[error("bind auf {0} fehlgeschlagen: {1}")]
    Bind(String, std::io::Error),
    #[error("verwaiste Socket-Datei {0} konnte nicht entfernt werden: {1}")]
    RemoveStale(String, std::io::Error),
    #[error("eine weitere Instanz läuft bereits (Socket {0} antwortet)")]
    AlreadyRunning(String),
    #[error("unbekannte Gruppe {0:?} -- siehe `groupadd --system logsentry`")]
    UnknownGroup(String),
    #[error("chown auf {0} fehlgeschlagen: {1}")]
    Chown(String, Errno),
}

/// Legt den Socket nach den Regeln aus Abschnitt 5 an:
/// 1. Übergeordnetes Verzeichnis anlegen (Modus 0750), falls es fehlt.
/// 2. Existierende Socket-Datei: Verbindungsversuch. `ECONNREFUSED` heißt
///    verwaist -> entfernen. Erfolgreiche Verbindung heißt eine zweite
///    Instanz läuft bereits -> Abbruch.
/// 3. Auf einen temporären Namen im selben Verzeichnis binden, `chmod`,
///    dann atomar auf den Zielpfad umbenennen -- so ist der Zielpfad nie
///    unter falschen Rechten sichtbar, ohne die *prozessweite* `umask` zu
///    verändern (die hätte parallele Dateizugriffe anderer Threads im
///    selben Prozess mitgetroffen, siehe Regressionstest unten).
/// 4. Gruppe aus der Konfiguration auflösen und die Socket-Datei ihr
///    zuordnen, sofern eine Gruppe konfiguriert ist (leer = kein `chown`,
///    für Dev-Betrieb ohne Root).
pub async fn bind_socket(config: &SocketConfig) -> Result<UnixListener, ServerError> {
    let path = Path::new(&config.path);

    if let Some(parent) = path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|err| ServerError::CreateDir(parent.display().to_string(), err))?;
            // Bestes Bemühen: Im Produktivbetrieb legt `RuntimeDirectory=`
            // (Phase 9) das Verzeichnis mit den richtigen Rechten an; ein
            // fehlgeschlagenes `chmod` hier ist kein Grund zum Abbruch.
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o750));
        }
    }

    if path.exists() {
        match UnixStream::connect(path).await {
            Ok(_) => return Err(ServerError::AlreadyRunning(config.path.clone())),
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {
                std::fs::remove_file(path)
                    .map_err(|err| ServerError::RemoveStale(config.path.clone(), err))?;
            }
            // Andere Fehler (z. B. Rechteproblem) sind kein eindeutiges
            // Zeichen für eine verwaiste Datei -- `bind` wird gleich mit
            // einer aussagekräftigeren Fehlermeldung scheitern.
            Err(_) => {}
        }
    }

    let listener = bind_with_mode(path, config.mode)
        .map_err(|err| ServerError::Bind(config.path.clone(), err))?;

    if !config.group.is_empty() {
        let group = Group::from_name(&config.group)
            .map_err(|_| ServerError::UnknownGroup(config.group.clone()))?
            .ok_or_else(|| ServerError::UnknownGroup(config.group.clone()))?;
        chown(path, None, Some(group.gid))
            .map_err(|err| ServerError::Chown(config.path.clone(), err))?;
    }

    Ok(listener)
}

/// Für den temporären Bind-Namen: eindeutig auch bei mehreren Aufrufen
/// innerhalb derselben Sekunde im selben Prozess (z. B. der
/// Regressionstest unten, der viele Sockets parallel anlegt).
fn unique_suffix() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Bindet `path` so, dass die entstehende Socket-Datei exakt `mode` als
/// Zugriffsrechte bekommt und unter diesem Namen nie mit anderen Rechten
/// sichtbar ist: Binden auf einen temporären Namen im selben Verzeichnis
/// (damit `rename` garantiert atomar und ohne Dateisystemgrenze bleibt),
/// `chmod`, dann `rename` auf `path`. Räumt die temporäre Datei bei jedem
/// Fehlerpfad auf.
fn bind_with_mode(path: &Path, mode: u32) -> std::io::Result<UnixListener> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("collector.sock");
    let tmp_path: PathBuf = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        unique_suffix()
    ));

    let listener = UnixListener::bind(&tmp_path)?;

    if let Err(err) = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(mode)) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }

    Ok(listener)
}

/// Nimmt Verbindungen entgegen, bis `shutdown` auf `true` wechselt.
///
/// Ein Client über dem konfigurierten `max_clients`-Limit bekommt sofort
/// `Goodbye(TooManyClients)` und wird nicht gezählt. Beim Herunterfahren
/// bekommen laufende Client-Tasks über ihre eigene Kopie von `shutdown`
/// die Chance, `Goodbye(Shutdown)` zu senden (Abschnitt 4, Regel 6),
/// bevor die Socket-Datei entfernt wird.
pub async fn run(
    listener: UnixListener,
    config: SocketConfig,
    hostname: Arc<str>,
    state: Arc<SharedState>,
    actions: Arc<crate::actions::ActionExecutor>,
    mut shutdown: watch::Receiver<bool>,
) {
    let max_clients = config.max_clients;
    let task_config = Arc::new(ClientTaskConfig {
        hostname,
        hello_timeout: Duration::from_millis(u64::from(config.hello_timeout_ms)),
        min_snapshot_interval_ms: config.min_snapshot_interval_ms,
        context_max_lines: config.context_max_lines,
        actions,
    });

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let current = state.client_connected();
                        if current > max_clients {
                            state.client_disconnected();
                            tokio::spawn(client_task::reject_too_many(stream, max_clients));
                            continue;
                        }
                        // SO_PEERCRED der Verbindung (Regel 13: das
                        // Audit-Log muss den Benutzer erfassen, nicht nur
                        // eine pro-Prozess-`session_id`, die für alle
                        // gleichzeitig verbundenen Clients gleich ist).
                        // `Err` ist auf Linux für einen echten
                        // Unix-Socket praktisch ausgeschlossen; `None`
                        // degradiert dann nur zu einem unbekannten Benutzer
                        // im Audit-Log statt die Verbindung abzulehnen.
                        let peer_uid = stream.peer_cred().map(|cred| cred.uid()).ok();
                        let state = Arc::clone(&state);
                        let task_config = Arc::clone(&task_config);
                        let client_shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            client_task::run(stream, Arc::clone(&state), client_shutdown, task_config, peer_uid).await;
                            state.client_disconnected();
                        });
                    }
                    Err(err) => {
                        tracing::warn!(fehler = %err, "accept() auf dem Collector-Socket fehlgeschlagen");
                    }
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }

    tracing::info!("Socket-Server beendet sich, gebe Clients Zeit zum Flushen");
    tokio::time::sleep(SHUTDOWN_GRACE).await;
    if let Err(err) = std::fs::remove_file(&config.path) {
        if err.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(fehler = %err, pfad = %config.path, "Socket-Datei konnte beim Beenden nicht entfernt werden");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(dir: &tempfile::TempDir, name: &str) -> SocketConfig {
        SocketConfig {
            path: dir.path().join(name).to_string_lossy().to_string(),
            group: String::new(),
            mode: 0o660,
            ..Default::default()
        }
    }

    /// Regressionstest für die `umask`-Race: `bind_with_mode` griff früher
    /// ohne Sperre auf die prozessweite `umask` zu, wodurch ein
    /// gleichzeitiger Aufruf mit anderem Modus die Rechte einer fremden
    /// Socket-Datei verfälschen konnte -- reproduzierbar erst unter echter
    /// Parallelität vieler `#[tokio::test]`s im selben Binary, deshalb hier
    /// explizit mit vielen Threads statt nur zwei Tasks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn paralleles_binden_mit_unterschiedlichem_modus_verfaelscht_keine_rechte() {
        let dir = tempfile::tempdir().unwrap();
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let path = dir.path().join(format!("s{i}.sock"));
                let mode: u32 = if i % 2 == 0 { 0o660 } else { 0o600 };
                tokio::spawn(async move {
                    let listener = bind_with_mode(&path, mode).unwrap();
                    let got = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                    drop(listener);
                    (mode, got)
                })
            })
            .collect();

        for handle in handles {
            let (expected, got) = handle.await.unwrap();
            assert_eq!(got, expected);
        }
    }

    #[tokio::test]
    async fn bind_erzeugt_datei_mit_korrektem_modus() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(&dir, "collector.sock");
        let listener = bind_socket(&config).await.unwrap();
        let meta = std::fs::metadata(&config.path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o660);
        drop(listener);
    }

    #[tokio::test]
    async fn zweiter_bind_auf_laufendem_socket_scheitert_als_already_running() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(&dir, "collector.sock");
        let _listener = bind_socket(&config).await.unwrap();

        let err = bind_socket(&config).await.unwrap_err();
        assert!(matches!(err, ServerError::AlreadyRunning(_)));
    }

    #[tokio::test]
    async fn verwaiste_socket_datei_wird_ersetzt() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(&dir, "collector.sock");

        // Eine Socket-Datei anlegen und sofort wieder schließen (ohne sie
        // zu entfernen), damit sie existiert, aber niemand mehr zuhört --
        // wie nach einem Absturz des Daemons.
        {
            let listener = std::os::unix::net::UnixListener::bind(&config.path).unwrap();
            drop(listener);
        }
        assert!(Path::new(&config.path).exists());

        let listener = bind_socket(&config).await;
        assert!(
            listener.is_ok(),
            "verwaiste Datei hätte ersetzt werden müssen"
        );
    }

    #[tokio::test]
    async fn unbekannte_gruppe_wird_als_fehler_gemeldet() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(&dir, "collector.sock");
        config.group = "diese-gruppe-gibt-es-ganz-sicher-nicht-12345".to_string();

        let err = bind_socket(&config).await.unwrap_err();
        assert!(matches!(err, ServerError::UnknownGroup(_)));
    }

    #[tokio::test]
    async fn fehlendes_elternverzeichnis_wird_angelegt() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config(&dir, "sock");
        config.path = dir
            .path()
            .join("neu/verschachtelt/collector.sock")
            .to_string_lossy()
            .to_string();

        let listener = bind_socket(&config).await;
        assert!(listener.is_ok());
        assert!(Path::new(&config.path).parent().unwrap().is_dir());
    }
}
