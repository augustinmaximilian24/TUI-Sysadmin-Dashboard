//! `logsentry-daemon`: privilegierter Collector-Prozess.
//!
//! Läuft als systemd-Unit, liest den Journal-Stream, führt Analyse und
//! (nach Bestätigung durch die GUI) Aktionen aus. Die GUI verbindet sich
//! als reiner Client über einen Unix-Socket (Phase 6).
//!
//! Phase 1: Ingestion ist aktiv (Live- oder Replay-Modus über `--since`),
//! Normalisierung/Analyse folgen in Phase 2/3 – bis dahin werden Events nur
//! gezählt.

mod ingestion;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ingestion::IngestionMode;
use logsentry_core::{ring_channel, Config, JournalEvent};

const DEFAULT_CONFIG_PATH: &str = "/etc/logsentry/logsentry.toml";

/// Liest `--since`/`--until` aus den Kommandozeilenargumenten, falls vorhanden.
/// Ohne `--since` läuft der Daemon im Live-Modus.
fn parse_mode_from_args(args: &[String]) -> IngestionMode {
    let since = find_flag_value(args, "--since");
    let until = find_flag_value(args, "--until");

    match since {
        Some(since) => IngestionMode::Replay { since, until },
        None => IngestionMode::Live,
    }
}

fn find_flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|idx| args.get(idx + 1))
        .cloned()
}

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

    let args: Vec<String> = std::env::args().collect();
    let mode = parse_mode_from_args(&args);

    tracing::info!(
        socket_pfad = %config.socket.path,
        fenster_sekunden = config.analysis.window_seconds,
        modus = ?mode,
        "logsentry-daemon gestartet"
    );

    let (sender, mut receiver) = ring_channel::<JournalEvent>(config.ingestion.channel_capacity);
    let parse_error_counter = Arc::new(AtomicU64::new(0));

    // Konsument-Platzhalter: zählt nur, bis Normalizer/Analysis in Phase 2/3
    // an diese Stelle treten. Läuft als eigener Task, damit die Ingestion
    // (blockierendes Lesen von journalctl) nicht auf den Konsum wartet.
    let consumer = tokio::spawn(async move {
        let mut received: u64 = 0;
        while let Some(_event) = receiver.recv().await {
            received += 1;
            if received.is_multiple_of(100) {
                tracing::debug!(
                    empfangen = received,
                    verworfen_ueberlauf = receiver.dropped_count(),
                    "Zwischenstand Ingestion"
                );
            }
        }
        tracing::info!(empfangen_gesamt = received, "Konsument beendet");
    });

    let ingestion_result =
        ingestion::run_ingestion(mode, &config.ingestion, sender, Arc::clone(&parse_error_counter))
            .await;

    if let Err(err) = &ingestion_result {
        tracing::error!(fehler = %err, "Ingestion beendet mit Fehler");
    }

    // Konsument beenden lassen: `sender` wurde in `run_ingestion` verschoben
    // und wird beim Rückkehren aus der Funktion gedroppt. Der Empfänger
    // erhält danach `None`, sobald der Puffer leer ist -- kein explizites
    // close() nötig.
    let _ = consumer.await;

    tracing::info!(
        parse_fehler = parse_error_counter.load(Ordering::Relaxed),
        "logsentry-daemon beendet"
    );

    ingestion_result.map_err(anyhow::Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ohne_flags_ist_modus_live() {
        let args = vec!["logsentry-daemon".to_string()];
        assert_eq!(parse_mode_from_args(&args), IngestionMode::Live);
    }

    #[test]
    fn since_flag_ergibt_replay_ohne_until() {
        let args = vec![
            "logsentry-daemon".to_string(),
            "--since".to_string(),
            "2026-01-01 00:00:00".to_string(),
        ];
        assert_eq!(
            parse_mode_from_args(&args),
            IngestionMode::Replay {
                since: "2026-01-01 00:00:00".to_string(),
                until: None,
            }
        );
    }

    #[test]
    fn since_und_until_flag_ergeben_replay_mit_beiden() {
        let args = vec![
            "logsentry-daemon".to_string(),
            "--since".to_string(),
            "2026-01-01 00:00:00".to_string(),
            "--until".to_string(),
            "2026-01-02 00:00:00".to_string(),
        ];
        assert_eq!(
            parse_mode_from_args(&args),
            IngestionMode::Replay {
                since: "2026-01-01 00:00:00".to_string(),
                until: Some("2026-01-02 00:00:00".to_string()),
            }
        );
    }
}
