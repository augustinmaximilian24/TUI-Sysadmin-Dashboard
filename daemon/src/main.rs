//! `logsentry-daemon`: privilegierter Collector-Prozess.
//!
//! Läuft als systemd-Unit, liest den Journal-Stream, führt Analyse und
//! (nach Bestätigung durch die GUI) Aktionen aus. Die GUI verbindet sich
//! als reiner Client über einen Unix-Socket (Phase 6).
//!
//! Phase 0: nur Konfiguration laden und Logging initialisieren.

use logsentry_core::Config;

const DEFAULT_CONFIG_PATH: &str = "/etc/logsentry/logsentry.toml";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let config = match Config::load_from_file(DEFAULT_CONFIG_PATH) {
        Ok(config) => config,
        Err(err) => {
            tracing::warn!(
                pfad = DEFAULT_CONFIG_PATH,
                fehler = %err,
                "Konfiguration konnte nicht geladen werden, verwende Default-Konfiguration"
            );
            Config::default()
        }
    };

    tracing::info!(
        socket_pfad = %config.socket.path,
        fenster_sekunden = config.analysis.window_seconds,
        "logsentry-daemon gestartet (Phase 0: nur Gerüst, keine Ingestion aktiv)"
    );

    Ok(())
}
