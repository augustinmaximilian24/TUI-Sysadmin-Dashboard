//! `logsentry-tui`: zweiter Konsument desselben Protokolls wie die GUI
//! (Phase 10, optional) -- für headless-Betrieb bzw. über SSH, z. B. auf
//! dem Heimserver ohne grafische Oberfläche. Reiner Client wie die GUI,
//! läuft nie mit erhöhten Rechten.
//!
//! Anders als die GUI (eigenständige Tokio-Runtime neben `eframe`s
//! synchroner Event-Loop) läuft hier alles in einem einzigen `#[tokio::main]`:
//! ratatui zeichnet synchron, Terminal-Eingaben werden nicht-blockierend
//! per `crossterm::event::poll` abgefragt, dazwischen `tokio::select!` auf
//! eingehende Server-Nachrichten und Verbindungsstatus. Kein Mutex/keine
//! geteilte Struktur nötig, da nur ein Task den Zustand hält.
//!
//! Kommandozeile: wie bei der GUI `--config <pfad>` / `--socket <pfad>`.

mod app;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};

use logsentry_core::Config;
use logsentry_proto::{spawn, ClientConfig, ConnectionState, Endpoint, Subscription};

use app::App;

const DEFAULT_CONFIG_PATH: &str = "/etc/logsentry/logsentry.toml";
/// Wie oft neu gezeichnet und auf Terminal-Eingaben geprüft wird, wenn
/// gerade keine Server-Nachricht ansteht.
const TICK: Duration = Duration::from_millis(150);

fn find_flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|idx| args.get(idx + 1))
        .cloned()
}

fn resolve_socket_path(args: &[String]) -> PathBuf {
    if let Some(socket) = find_flag_value(args, "--socket") {
        return PathBuf::from(socket);
    }
    let config_path = find_flag_value(args, "--config")
        .map_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH), PathBuf::from);
    let config = Config::load_from_file(&config_path).unwrap_or_default();
    PathBuf::from(config.socket.path)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let socket_path = resolve_socket_path(&args);

    let (_outbound, mut inbound, mut connection_state) = spawn(ClientConfig {
        endpoint: Endpoint::Unix(socket_path),
        client_name: "logsentry-tui".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        subscription: Subscription::default(),
    });

    let mut terminal = ratatui::init();
    let mut app = App::default();
    let result = run(&mut terminal, &mut app, &mut inbound, &mut connection_state).await;
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    inbound: &mut logsentry_proto::InboundReceiver,
    connection_state: &mut tokio::sync::watch::Receiver<ConnectionState>,
) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(TICK);

    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;

        tokio::select! {
            _ = tick.tick() => {
                poll_terminal_events(app)?;
            }
            changed = connection_state.changed() => {
                if changed.is_err() {
                    break;
                }
                app.connection = Some(connection_state.borrow().clone());
            }
            message = inbound.recv() => {
                match message {
                    Some(message) => app.apply_message(message),
                    None => break,
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

/// Fragt anstehende Terminal-Eingaben nicht-blockierend ab
/// (`poll(Duration::ZERO)`), statt eine eigene Blocking-Task/Thread für
/// `crossterm::event::read()` zu betreiben -- für ein einzelnes Tastatur-
/// Eingabegerät ohne spürbare Latenz bei einem 150-ms-Tick.
fn poll_terminal_events(app: &mut App) -> anyhow::Result<()> {
    while event::poll(Duration::ZERO)? {
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                handle_key(app, key.code);
            }
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Up | KeyCode::Char('k') => app.select_previous(),
        KeyCode::Down | KeyCode::Char('j') => app.select_next(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        std::iter::once("logsentry-tui")
            .chain(list.iter().copied())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn socket_flag_hat_vorrang_vor_konfiguration() {
        let path = resolve_socket_path(&args(&[
            "--config",
            "/pfad/den/es/nicht/gibt.toml",
            "--socket",
            "/tmp/mein.sock",
        ]));
        assert_eq!(path, PathBuf::from("/tmp/mein.sock"));
    }

    #[test]
    fn fehlende_konfiguration_ergibt_eingebauten_default() {
        let path = resolve_socket_path(&args(&["--config", "/pfad/den/es/nicht/gibt.toml"]));
        assert_eq!(
            path,
            PathBuf::from(logsentry_core::config::SocketConfig::default().path)
        );
    }
}
