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
mod devices;
mod export;
mod fleet;
mod knowledge_graph;
mod network_map;
mod services;
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
    // Siehe Kommentar in daemon/src/main.rs: ohne diesen Fallback
    // unterdrückt `fmt::init()` mit aktiviertem "env-filter"-Feature bei
    // fehlendem `RUST_LOG` ausnahmslos jede Ausgabe.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let config = load_config(&args);
    let socket_path = resolve_socket_path(&args, &config);
    let knowledge_graph_config = config.knowledge_graph.clone();
    let home_overview_config = logsentry_core::config::KnowledgeGraphConfig {
        enabled: config.home_overview.enabled,
        graph_json_path: config.home_overview.graph_json_path.clone(),
        links_overlay_path: config.home_overview.links_overlay_path.clone(),
        overlay_adds_nodes: config.home_overview.overlay_adds_nodes,
        ..config.knowledge_graph.clone()
    };
    let network_map_config = config.network_map.clone();
    let services_config = config.services.clone();
    let fleet_config = config.fleet.clone();
    let lan_devices_enabled = config.lan_devices.enabled;

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
            let tabs = app::TabConfigs {
                knowledge_graph: knowledge_graph_config,
                home_overview: home_overview_config,
                network_map: network_map_config,
                services: services_config,
                fleet: fleet_config,
                lan_devices_enabled,
            };
            Ok(Box::new(app::LogsentryApp::new(
                state,
                outbound,
                tabs,
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
