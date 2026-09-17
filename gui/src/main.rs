//! `logsentry-gui`: reiner Client unter dem Benutzerkonto (niemals root, Regel 7).
//!
//! Verbindet sich über den Unix-Socket mit dem Daemon (Phase 6/7). Die
//! Socket-Kommunikation läuft auf einem eigenständigen Tokio-Runtime im
//! Hintergrund (`client.rs`); `eframe` selbst bleibt synchron und blockiert
//! nie auf I/O (Regel 21).
//!
//! Kommandozeile:
//! - `--config <pfad>`: Konfigurationsdatei, aus der `[socket].path` gelesen wird
//!   (Default: derselbe Pfad wie beim Daemon, `/etc/logsentry/logsentry.toml`).
//! - `--socket <pfad>`: Socket-Pfad direkt angeben, überschreibt die Konfiguration
//!   (praktisch für Entwicklung ohne installierte Konfigurationsdatei).

mod app;
mod client;

use std::path::PathBuf;

use logsentry_core::Config;

const DEFAULT_CONFIG_PATH: &str = "/etc/logsentry/logsentry.toml";

fn find_flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|idx| args.get(idx + 1))
        .cloned()
}

/// Ermittelt den Socket-Pfad: `--socket` hat Vorrang, sonst `[socket].path`
/// aus der Konfigurationsdatei (`--config` oder Default-Pfad). Eine
/// fehlende Konfigurationsdatei ist kein Fehler -- dann gilt schlicht der
/// eingebaute Default aus `SocketConfig`.
fn resolve_socket_path(args: &[String]) -> PathBuf {
    if let Some(socket) = find_flag_value(args, "--socket") {
        return PathBuf::from(socket);
    }
    let config_path = find_flag_value(args, "--config")
        .map_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH), PathBuf::from);
    let config = Config::load_from_file(&config_path).unwrap_or_default();
    PathBuf::from(config.socket.path)
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();
    let socket_path = resolve_socket_path(&args);

    // Eigenständige Runtime statt `#[tokio::main]`: `eframe::run_native`
    // übernimmt den aufrufenden Thread mit seiner eigenen Event-Loop, die
    // Socket-Kommunikation läuft komplett auf dieser separaten Runtime im
    // Hintergrund.
    let runtime = tokio::runtime::Runtime::new()?;
    let _enter = runtime.enter();

    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "logsentry",
        native_options,
        Box::new(move |cc| {
            let (state, outbound) = client::spawn_bridge(socket_path, cc.egui_ctx.clone());
            Ok(Box::new(app::LogsentryApp::new(state, outbound)))
        }),
    )
    .map_err(|err| anyhow::anyhow!("eframe konnte nicht gestartet werden: {err}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        std::iter::once("logsentry-gui")
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
