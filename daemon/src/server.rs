//! Unix-Socket-Server des Daemons: Socket-Anlage, Accept-Schleife,
//! Client-Zähler, Shutdown-Pfad.
//!
//! Normativ: `docs/phase6-protokoll.md` Abschnitt 5 (Socket-Anlage,
//! Rechte) und Abschnitt 7 (Daemon-Struktur). Wird ab Schritt 6 aus
//! `main.rs` heraus verwendet; bis dahin ist dieses Modul über eigene
//! Integrationstests abgedeckt.
//!
//! `#![allow(dead_code)]`: siehe Begründung in `wire.rs`.
#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::stat::{umask, Mode};
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
/// 3. `umask` kurzzeitig setzen, binden, `umask` zurücksetzen -- so
///    entsteht der gewünschte Modus ohne Race zwischen `bind` und `chmod`.
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

/// Bindet `path` so, dass die entstehende Socket-Datei exakt `mode` als
/// Zugriffsrechte bekommt, unabhängig vom Prozess-`umask`.
fn bind_with_mode(path: &Path, mode: u32) -> std::io::Result<UnixListener> {
    let restrictive = Mode::from_bits_truncate((!mode & 0o777) as libc::mode_t);
    let previous = umask(restrictive);
    let result = UnixListener::bind(path);
    umask(previous);
    result
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
    config: &SocketConfig,
    hostname: Arc<str>,
    state: Arc<SharedState>,
    mut shutdown: watch::Receiver<bool>,
) {
    let client_count = Arc::new(AtomicU32::new(0));
    let max_clients = config.max_clients;
    let task_config = Arc::new(ClientTaskConfig {
        hostname,
        hello_timeout: Duration::from_millis(u64::from(config.hello_timeout_ms)),
        min_snapshot_interval_ms: config.min_snapshot_interval_ms,
        context_max_lines: config.context_max_lines,
    });

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let current = client_count.fetch_add(1, Ordering::SeqCst) + 1;
                        if current > max_clients {
                            client_count.fetch_sub(1, Ordering::SeqCst);
                            tokio::spawn(client_task::reject_too_many(stream, max_clients));
                            continue;
                        }
                        let state = Arc::clone(&state);
                        let task_config = Arc::clone(&task_config);
                        let client_shutdown = shutdown.clone();
                        let client_count = Arc::clone(&client_count);
                        tokio::spawn(async move {
                            client_task::run(stream, state, client_shutdown, task_config).await;
                            client_count.fetch_sub(1, Ordering::SeqCst);
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
