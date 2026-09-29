//! SSH-Tunnel-Subprozess pro Fleet-Host: `ssh -L <port>:<remote_socket_path>
//! <ssh_target> -N` hält den Tunnel offen. Backoff-Überwachung nach
//! demselben Muster wie der journalctl-Supervisor in
//! `daemon/src/ingestion.rs` -- eigenständig hier in `gui/`, da beide
//! Crates unabhängig bleiben sollen (kein Cross-Crate-Sharing für dieses
//! Muster nötig).

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use eframe::egui;
use logsentry_core::config::{RemoteHost, FLEET_BASE_LOCAL_PORT};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// Wie viele der letzten stderr-Zeilen von `ssh` behalten werden, um einen
/// aussagekräftigen Fehlertext zu zeigen (Regel 18: kein unbeschränktes
/// Wachstum).
const STDERR_TAIL_LINES: usize = 10;

/// Mindestlaufzeit, ab der der Backoff nach Tunnel-Ende zurückgesetzt wird
/// (dieselbe Idee wie `MIN_STABLE_RUN` in `daemon/src/ingestion.rs` bzw.
/// `STABLE_CONNECTION` in `proto::client`).
const STABLE_TUNNEL: Duration = Duration::from_secs(10);

/// Zustand des Tunnel-Subprozesses selbst (unabhängig vom
/// Daemon-Verbindungsstatus dahinter, siehe `gui/src/fleet/mod.rs`).
#[derive(Debug, Clone, PartialEq)]
pub enum TunnelState {
    Starting,
    Up,
    Failed { message: String, retry_in: Duration },
}

/// Lokaler TCP-Port für den Tunnel eines Hosts: `host.local_port`, falls
/// gesetzt, sonst deterministisch aus dem Index in der `hosts`-Liste
/// abgeleitet -- `ssh -L` kann einen mit Port 0 gewählten Port nicht
/// zurückmelden, ein fester Port ist deshalb einfacher als eine spätere
/// Rückfrage beim Betriebssystem.
pub fn resolve_local_port(host: &RemoteHost, index: usize) -> u16 {
    host.local_port
        .unwrap_or(FLEET_BASE_LOCAL_PORT.saturating_add(index as u16))
}

fn spawn_ssh(host: &RemoteHost, local_port: u16) -> std::io::Result<Child> {
    Command::new("ssh")
        .args([
            "-N",
            "-o",
            "BatchMode=yes",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "-L",
            &format!("127.0.0.1:{local_port}:{}", host.remote_socket_path),
        ])
        .arg(&host.ssh_target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
}

/// Liest die letzten [`STDERR_TAIL_LINES`] Zeilen von `ssh`s stderr, bis der
/// Kindprozess sein Ende der Pipe schließt (üblicherweise beim Beenden).
async fn read_stderr_tail(child: &mut Child) -> VecDeque<String> {
    let mut tail = VecDeque::with_capacity(STDERR_TAIL_LINES);
    let Some(stderr) = child.stderr.take() else {
        return tail;
    };
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if tail.len() >= STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    tail
}

/// Baut aus Exit-Status und den letzten stderr-Zeilen einen verständlichen
/// Fehlertext (z. B. "Permission denied (publickey)" statt nur einem rohen
/// Exit-Code).
fn describe_exit(
    status: std::io::Result<std::process::ExitStatus>,
    stderr_tail: &VecDeque<String>,
) -> String {
    let last_line = stderr_tail.back();
    match (status, last_line) {
        (Ok(status), Some(line)) => format!("ssh beendet ({status}): {line}"),
        (Ok(status), None) => format!("ssh beendet ({status})"),
        (Err(err), Some(line)) => format!("ssh-Prozess-Fehler: {err} ({line})"),
        (Err(err), None) => format!("ssh-Prozess-Fehler: {err}"),
    }
}

fn set_state(state: &Arc<Mutex<TunnelState>>, ctx: &egui::Context, new_state: TunnelState) {
    *state.lock().unwrap_or_else(PoisonError::into_inner) = new_state;
    ctx.request_repaint();
}

/// Hält den Tunnel für einen Host dauerhaft offen: startet `ssh`, wartet auf
/// sein Ende, meldet den Grund und startet nach Backoff neu. Kein
/// Readiness-Probe vor dem eigentlichen Verbindungsaufbau nötig -- der
/// Proto-Client (`gui/src/fleet/mod.rs`) behandelt "Port horcht noch nicht"
/// über seinen eigenen Reconnect-Mechanismus bereits identisch zu "Daemon
/// läuft noch nicht".
///
/// # ponytail
/// Kein Tunnel-Readiness-Probe vor dem Verbindungsaufbau -- der Proto-Client
/// behandelt einen noch nicht horchenden Port ohnehin wie einen fehlenden
/// Daemon (Backoff-Retry). Eigene Probe-Logik nachziehen, falls sich das in
/// der Praxis als zu langsam/unruhig erweist.
pub async fn supervise_tunnel(
    host: RemoteHost,
    local_port: u16,
    state: Arc<Mutex<TunnelState>>,
    ctx: egui::Context,
) {
    let mut attempt: u32 = 0;
    loop {
        set_state(&state, &ctx, TunnelState::Starting);
        let started = Instant::now();

        let message = match spawn_ssh(&host, local_port) {
            Ok(mut child) => {
                set_state(&state, &ctx, TunnelState::Up);
                let stderr_tail = read_stderr_tail(&mut child).await;
                let status = child.wait().await;
                describe_exit(status, &stderr_tail)
            }
            Err(err) => format!("ssh konnte nicht gestartet werden: {err}"),
        };

        if started.elapsed() >= STABLE_TUNNEL {
            attempt = 0;
        }
        let delay = logsentry_proto::backoff_delay(attempt);
        set_state(
            &state,
            &ctx,
            TunnelState::Failed {
                message,
                retry_in: delay,
            },
        );
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(local_port: Option<u16>) -> RemoteHost {
        RemoteHost {
            name: "Test".to_string(),
            ssh_target: "localhost".to_string(),
            remote_socket_path: "/run/logsentry/collector.sock".to_string(),
            local_port,
        }
    }

    #[test]
    fn resolve_local_port_nutzt_expliziten_override() {
        assert_eq!(resolve_local_port(&host(Some(12345)), 0), 12345);
        assert_eq!(resolve_local_port(&host(Some(12345)), 3), 12345);
    }

    #[test]
    fn resolve_local_port_leitet_aus_index_ab_ohne_override() {
        assert_eq!(resolve_local_port(&host(None), 0), FLEET_BASE_LOCAL_PORT);
        assert_eq!(
            resolve_local_port(&host(None), 2),
            FLEET_BASE_LOCAL_PORT + 2
        );
    }

    #[test]
    fn describe_exit_bevorzugt_die_letzte_stderr_zeile() {
        let mut tail = VecDeque::new();
        tail.push_back("Permission denied (publickey).".to_string());
        let status = std::process::Command::new("false")
            .status()
            .expect("`false` ist auf jedem Unix-System vorhanden");
        let text = describe_exit(Ok(status), &tail);
        assert!(text.contains("Permission denied"));
    }

    #[test]
    fn describe_exit_faellt_ohne_stderr_auf_den_status_zurueck() {
        let status = std::process::Command::new("true")
            .status()
            .expect("`true` ist auf jedem Unix-System vorhanden");
        let text = describe_exit(Ok(status), &VecDeque::new());
        assert!(text.contains("ssh beendet"));
    }
}
