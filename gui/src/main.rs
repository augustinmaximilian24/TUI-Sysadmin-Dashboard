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
mod export;
mod knowledge_graph;
mod theme;

use std::path::PathBuf;

use eframe::egui;
use logsentry_core::Config;

const DEFAULT_CONFIG_PATH: &str = "/etc/logsentry/logsentry.toml";

fn find_flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|idx| args.get(idx + 1))
        .cloned()
}

/// Lädt die Konfiguration aus `--config` (oder dem Default-Pfad). Eine
/// fehlende Datei ist kein Fehler -- dann gelten die eingebauten Defaults
/// aus [`Config`] (u. a. für `socket` und `knowledge_graph`).
fn load_config(args: &[String]) -> Config {
    let config_path = find_flag_value(args, "--config")
        .map_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH), PathBuf::from);
    Config::load_from_file(&config_path).unwrap_or_default()
}

/// Ermittelt den Socket-Pfad: `--socket` hat Vorrang, sonst `[socket].path`
/// aus der geladenen Konfiguration.
fn resolve_socket_path(args: &[String], config: &Config) -> PathBuf {
    if let Some(socket) = find_flag_value(args, "--socket") {
        return PathBuf::from(socket);
    }
    PathBuf::from(config.socket.path.clone())
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();
    let config = load_config(&args);
    let socket_path = resolve_socket_path(&args, &config);
    let knowledge_graph_config = config.knowledge_graph.clone();

    // Eigenständige Runtime statt `#[tokio::main]`: `eframe::run_native`
    // übernimmt den aufrufenden Thread mit seiner eigenen Event-Loop, die
    // Socket-Kommunikation läuft komplett auf dieser separaten Runtime im
    // Hintergrund.
    let runtime = tokio::runtime::Runtime::new()?;
    let _enter = runtime.enter();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "logsentry",
        native_options,
        Box::new(move |cc| {
            let (state, outbound) = client::spawn_bridge(socket_path, cc.egui_ctx.clone());
            Ok(Box::new(app::LogsentryApp::new(
                state,
                outbound,
                knowledge_graph_config,
                &cc.egui_ctx,
            )))
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
        let call_args = args(&[
            "--config",
            "/pfad/den/es/nicht/gibt.toml",
            "--socket",
            "/tmp/mein.sock",
        ]);
        let config = load_config(&call_args);
        let path = resolve_socket_path(&call_args, &config);
        assert_eq!(path, PathBuf::from("/tmp/mein.sock"));
    }

    #[test]
    fn fehlende_konfiguration_ergibt_eingebauten_default() {
        let call_args = args(&["--config", "/pfad/den/es/nicht/gibt.toml"]);
        let config = load_config(&call_args);
        let path = resolve_socket_path(&call_args, &config);
        assert_eq!(
            path,
            PathBuf::from(logsentry_core::config::SocketConfig::default().path)
        );
    }
}
