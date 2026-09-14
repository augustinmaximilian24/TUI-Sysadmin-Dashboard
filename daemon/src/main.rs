//! `logsentry-daemon`: privilegierter Collector-Prozess.
//!
//! Läuft als systemd-Unit, liest den Journal-Stream, führt Analyse und
//! (nach Bestätigung durch die GUI) Aktionen aus. Die GUI verbindet sich
//! als reiner Client über einen Unix-Socket (Phase 6).
//!
//! Stand Phase 4: Ingestion (Live/Replay), Normalisierung, Anomalie-Analyse
//! mit persistierten Zeitprofil-Baselines. Anomalien werden protokolliert;
//! die Socket-Übergabe folgt in Phase 6.
//!
//! Kommandozeile:
//! - `--since <zeit> [--until <zeit>]`: Replay statt Live
//! - `--dump-baselines`: Bestand als JSON auf stdout ausgeben und beenden
//! - `--reset-baselines`: Baseline-Datei sichern und leer neu beginnen
//! - `--config <pfad>`: abweichender Konfigurationspfad

mod ingestion;
mod pipeline;
mod state;
mod sysmon;
mod units;
mod wire;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Context;
use ingestion::IngestionMode;
use logsentry_core::baseline::{move_aside, BaselineDb, PersistError, PersistedState};
use logsentry_core::{ring_channel, Config, JournalEvent};
use pipeline::Pipeline;

const DEFAULT_CONFIG_PATH: &str = "/etc/logsentry/logsentry.toml";

/// Ausgewertete Kommandozeile.
#[derive(Debug, Clone, PartialEq)]
struct CliArgs {
    mode: IngestionMode,
    config_path: PathBuf,
    dump_baselines: bool,
    reset_baselines: bool,
}

/// Liest die Kommandozeile. Ohne `--since` läuft der Daemon im Live-Modus.
fn parse_args(args: &[String]) -> CliArgs {
    let since = find_flag_value(args, "--since");
    let until = find_flag_value(args, "--until");
    let mode = match since {
        Some(since) => IngestionMode::Replay { since, until },
        None => IngestionMode::Live,
    };
    CliArgs {
        mode,
        config_path: find_flag_value(args, "--config")
            .map_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH), PathBuf::from),
        dump_baselines: args.iter().any(|a| a == "--dump-baselines"),
        reset_baselines: args.iter().any(|a| a == "--reset-baselines"),
    }
}

fn find_flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|idx| args.get(idx + 1))
        .cloned()
}

/// Hostname für die Plausibilitätsprüfung der Baseline-Datei. Kein Panic
/// und keine zusätzliche Crate: `/etc/hostname`, dann `HOSTNAME`, sonst
/// ein Platzhalter.
fn read_hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Öffnet die Baseline-Datei. Eine Datei neuerer Version verhindert den
/// Start (Datenverlust wäre die Alternative); alle anderen Fehler führen zum
/// Betrieb ohne Persistenz, denn Baselines sind Beschleunigung, keine
/// Voraussetzung.
fn open_baseline_db(path: &Path, hostname: &str) -> anyhow::Result<Option<BaselineDb>> {
    match BaselineDb::open(path, hostname) {
        Ok(db) => Ok(Some(db)),
        Err(err @ PersistError::NewerSchema { .. }) => Err(anyhow::Error::from(err)),
        Err(err) => {
            tracing::warn!(
                pfad = %path.display(),
                fehler = %err,
                "Baseline-Datei nicht nutzbar, Betrieb ohne Persistenz"
            );
            Ok(None)
        }
    }
}

/// Wartet auf SIGINT oder SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(term) => term,
                Err(err) => {
                    tracing::warn!(fehler = %err, "SIGTERM-Handler nicht verfügbar, nur Ctrl-C");
                    let _ = ctrl_c.await;
                    return;
                }
            };
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

/// Läuft im Hintergrund und protokolliert den Systemzustand periodisch.
///
/// Phase 5 liefert nur Protokoll-Zeilen; die Übergabe an den Socket-Client
/// folgt in Phase 6, wenn ein Empfänger existiert. Ein fehlender D-Bus
/// (Test-/Container-Umgebungen, siehe `daemon::units`) wird einmalig
/// gewarnt; danach laufen CPU/RAM/Load/Disk/Temperaturen unverändert
/// weiter, nur ohne Unit-Status -- ein einzelner ausgefallener Sensor darf
/// die übrigen Metriken nicht mitreißen.
async fn run_system_monitor(config: logsentry_core::config::SystemConfig) {
    let mut sysmon = sysmon::SysMonitor::new(&config);
    let units = match units::UnitMonitor::connect().await {
        Ok(monitor) => Some(monitor),
        Err(err) => {
            tracing::warn!(fehler = %err, "kein Zugriff auf System-D-Bus, Unit-Status bleibt leer");
            None
        }
    };

    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
        config.poll_interval_seconds.max(1),
    ));
    loop {
        ticker.tick().await;
        let timestamp_us = now_us();
        let mut snapshot = sysmon.poll(timestamp_us);

        if let Some(monitor) = &units {
            match monitor.poll(&config.watched_units).await {
                Ok(unit_status) => snapshot.units = unit_status,
                Err(err) => tracing::warn!(fehler = %err, "Unit-Status nicht abrufbar"),
            }
        }

        tracing::debug!(
            cpu_prozent = format!("{:.1}", snapshot.cpu.global_usage_percent),
            ram_prozent = format!("{:.1}", snapshot.memory.used_percent),
            last_1min = format!("{:.2}", snapshot.load.one),
            temperaturen = snapshot.temperatures.len(),
            units = snapshot.units.len(),
            "Systemzustand"
        );
    }
}

/// Mikrosekunden seit Unix-Epoch. `0` im (praktisch nie eintretenden) Fall
/// einer Systemuhr vor 1970, statt zu paniken.
fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();
    let cli = parse_args(&args);

    let config = match Config::load_from_file(&cli.config_path) {
        Ok(config) => config,
        Err(err) => {
            tracing::warn!(
                pfad = %cli.config_path.display(),
                fehler = %err,
                "Konfiguration konnte nicht geladen werden, verwende Default-Konfiguration"
            );
            Config::default()
        }
    };

    let baseline_path = PathBuf::from(&config.persistence.path);
    let hostname = read_hostname();

    if cli.reset_baselines && baseline_path.exists() {
        let moved = move_aside(&baseline_path, "bak-manual")
            .with_context(|| format!("Baseline-Datei {} sichern", baseline_path.display()))?;
        tracing::info!(gesichert_nach = %moved.display(), "Baselines zurückgesetzt");
    }

    let db = open_baseline_db(&baseline_path, &hostname)?;

    if cli.dump_baselines {
        let Some(db) = db else {
            anyhow::bail!("keine Baseline-Datei geöffnet, nichts auszugeben");
        };
        println!("{}", db.dump_json().context("Baselines exportieren")?);
        return Ok(());
    }

    let restored: Option<PersistedState> = match &db {
        Some(db) => match db.load() {
            Ok(state) => state,
            Err(err) => {
                tracing::warn!(fehler = %err, "Bestand nicht ladbar, beginne leer");
                None
            }
        },
        None => None,
    };

    tracing::info!(
        socket_pfad = %config.socket.path,
        baseline_pfad = %baseline_path.display(),
        persistenz = db.is_some(),
        modus = ?cli.mode,
        "logsentry-daemon gestartet"
    );

    let (sender, receiver) = ring_channel::<JournalEvent>(config.ingestion.channel_capacity);
    let parse_error_counter = Arc::new(AtomicU64::new(0));

    let pipeline = Pipeline::new(&config, db, restored);
    let consumer = tokio::spawn(pipeline.run(receiver, Arc::clone(&parse_error_counter)));

    // Der Systemzustands-Task hat keinen Zustand, der beim Beenden
    // gesichert werden müsste (anders als die Pipeline) -- er wird beim
    // Herunterfahren einfach abgebrochen, statt auf sein Ende zu warten.
    let system_monitor = tokio::spawn(run_system_monitor(config.system.clone()));

    // Ingestion läuft, bis journalctl endet (Replay) oder ein Signal kommt.
    // Die Future lebt nur in diesem Block: Beim Verlassen fällt sie und mit
    // ihr der Sender, der Kanal schließt, und die Pipeline sichert ein
    // letztes Mal. Ein `drop(pinned)` täte das NICHT -- es dropte nur die
    // Pin-Referenz, die Future selbst lebte bis zum Funktionsende weiter,
    // und `consumer.await` unten würde auf einen nie schließenden Kanal
    // warten.
    let ingestion_result = {
        let ingestion = ingestion::run_ingestion(
            cli.mode,
            &config.ingestion,
            sender,
            Arc::clone(&parse_error_counter),
        );
        tokio::pin!(ingestion);

        tokio::select! {
            result = &mut ingestion => result,
            _ = shutdown_signal() => {
                tracing::info!("Beendigungssignal empfangen");
                Ok(())
            }
        }
    };

    system_monitor.abort();

    if let Err(err) = &ingestion_result {
        tracing::error!(fehler = %err, "Ingestion beendet mit Fehler");
    }

    match consumer.await {
        Ok(summary) => tracing::info!(
            ereignisse = summary.events,
            anomalien = summary.anomalies,
            templates = summary.templates,
            verworfen_ueberlauf = summary.dropped_overflow,
            parse_fehler = summary.parse_errors,
            unterdrueckt = summary.suppressed,
            unterdrueckt_lernphase = summary.suppressed_learning,
            "logsentry-daemon beendet"
        ),
        Err(err) => tracing::error!(fehler = %err, "Pipeline-Task abgebrochen"),
    }

    tracing::debug!(
        parse_fehler = parse_error_counter.load(Ordering::Relaxed),
        "Zähler nach Beenden"
    );

    ingestion_result.map_err(anyhow::Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        std::iter::once("logsentry-daemon")
            .chain(list.iter().copied())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn ohne_flags_ist_modus_live_und_nichts_gesetzt() {
        let cli = parse_args(&args(&[]));
        assert_eq!(cli.mode, IngestionMode::Live);
        assert!(!cli.dump_baselines);
        assert!(!cli.reset_baselines);
        assert_eq!(cli.config_path, PathBuf::from(DEFAULT_CONFIG_PATH));
    }

    #[test]
    fn since_flag_ergibt_replay_ohne_until() {
        let cli = parse_args(&args(&["--since", "2026-01-01 00:00:00"]));
        assert_eq!(
            cli.mode,
            IngestionMode::Replay {
                since: "2026-01-01 00:00:00".to_string(),
                until: None,
            }
        );
    }

    #[test]
    fn since_und_until_flag_ergeben_replay_mit_beiden() {
        let cli = parse_args(&args(&[
            "--since",
            "2026-01-01 00:00:00",
            "--until",
            "2026-01-02 00:00:00",
        ]));
        assert_eq!(
            cli.mode,
            IngestionMode::Replay {
                since: "2026-01-01 00:00:00".to_string(),
                until: Some("2026-01-02 00:00:00".to_string()),
            }
        );
    }

    #[test]
    fn dump_und_reset_flags_werden_erkannt() {
        let cli = parse_args(&args(&["--dump-baselines", "--reset-baselines"]));
        assert!(cli.dump_baselines);
        assert!(cli.reset_baselines);
    }

    #[test]
    fn config_flag_ueberschreibt_den_pfad() {
        let cli = parse_args(&args(&["--config", "/tmp/x.toml"]));
        assert_eq!(cli.config_path, PathBuf::from("/tmp/x.toml"));
    }

    #[test]
    fn hostname_ist_nie_leer() {
        assert!(!read_hostname().is_empty());
    }
}
