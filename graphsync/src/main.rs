//! Einstiegspunkt von `logsentry-graphsync`.
//!
//! Aufruf:
//! - `--config <pfad>`: Konfigurationsdatei (Default `/etc/logsentry/logsentry.toml`,
//!   Abschnitt `[graph_sync]`)
//! - `--once`: genau einen Durchlauf ausführen und beenden (z. B. zum Testen)
//! - `--no-llm`: KI-Stufe für diesen Prozess abschalten

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use logsentry_core::config::Config;
use logsentry_graphsync::llm::CommandRunner;
use logsentry_graphsync::sync::{LlmTick, Syncer};
use tracing::{info, warn};

const DEFAULT_CONFIG_PATH: &str = "/etc/logsentry/logsentry.toml";

fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn load_config(path: &PathBuf) -> Result<Config> {
    match Config::load_from_file(path) {
        Ok(config) => Ok(config),
        Err(logsentry_core::config::ConfigError::Read(err))
            if err.kind() == std::io::ErrorKind::NotFound =>
        {
            info!(path = %path.display(), "keine Konfigurationsdatei, nutze Defaults");
            Ok(Config::default())
        }
        Err(err) => Err(err).with_context(|| format!("Konfiguration {}", path.display())),
    }
}

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn local_day() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let config_path =
        PathBuf::from(flag_value(&args, "--config").unwrap_or_else(|| DEFAULT_CONFIG_PATH.into()));
    let once = args.iter().any(|a| a == "--once");
    let no_llm = args.iter().any(|a| a == "--no-llm");

    let mut config = load_config(&config_path)?.graph_sync;
    if !config.enabled {
        info!("graph_sync.enabled = false, nichts zu tun");
        return Ok(());
    }
    if config.source_dirs.is_empty() {
        bail!("graph_sync.source_dirs ist leer -- bitte Quellverzeichnisse eintragen");
    }
    if no_llm {
        config.llm.enabled = false;
    }
    let interval = Duration::from_secs(config.scan_interval_secs.max(5));

    let home = std::env::var("HOME").ok();
    let mut syncer = Syncer::new(config.clone(), home.as_deref());
    let runner = CommandRunner {
        config: config.llm.clone(),
        cwd: syncer.llm_cwd(),
    };
    info!(
        roots = ?syncer.roots(),
        output = %syncer.output_path().display(),
        llm = config.llm.enabled,
        "graphsync gestartet"
    );

    let mut first = true;
    loop {
        let report = syncer.scan_once(unix_now());
        if report.truncated {
            warn!(
                max = config.max_files,
                "Dateiobergrenze erreicht, nicht alle Dateien erfasst"
            );
        }
        if report.dirty() {
            info!(
                files = report.files,
                added = report.added,
                changed = report.changed,
                removed = report.removed,
                "Delta erkannt"
            );
        }
        let mut dirty = report.dirty() || first;
        first = false;

        match syncer.llm_tick(&runner, unix_now(), &local_day()).await {
            LlmTick::Done {
                files,
                edges,
                rejected,
                cost_usd,
            } => {
                info!(
                    files,
                    edges,
                    rejected,
                    ?cost_usd,
                    "KI-Analyse abgeschlossen"
                );
                dirty = true;
            }
            LlmTick::Failed(error) => {
                warn!(%error, "KI-Analyse fehlgeschlagen, neuer Versuch nach Wartezeit");
                dirty = true;
            }
            LlmTick::Disabled | LlmTick::NothingToDo | LlmTick::Blocked(_) => {}
        }

        if dirty {
            if let Err(err) = syncer.write_output() {
                warn!(%err, "Ausgabe konnte nicht geschrieben werden");
            }
            if let Err(err) = syncer.save_state() {
                warn!(%err, "Zustand konnte nicht gespeichert werden");
            }
        }

        if once {
            return Ok(());
        }
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            _ = shutdown_signal() => {
                info!("beende graphsync");
                return Ok(());
            }
        }
    }
}

/// Wartet auf SIGTERM (systemd) oder Strg+C.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = ctrl_c => {}
                _ = term.recv() => {}
            }
        }
        Err(_) => {
            let _ = ctrl_c.await;
        }
    }
}
