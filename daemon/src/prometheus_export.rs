//! Prometheus-Textfile-Export (Phase 10, optional).
//!
//! Reine Formatierung (`format_prometheus_textfile`) getrennt vom
//! Schreiben (`write_prometheus_textfile`), damit die Formatierung ohne
//! Dateisystemzugriff testbar ist. Geschrieben wird atomar (temporäre
//! Datei im selben Verzeichnis, dann `rename`), damit node_exporters
//! Textfile-Collector nie eine unvollständige Datei liest.

use std::io::Write;
use std::path::Path;

use logsentry_proto::Snapshot;

/// Baut den Inhalt der `.prom`-Datei aus einem Snapshot. Format: siehe
/// <https://github.com/prometheus/node_exporter#textfile-collector>.
pub fn format_prometheus_textfile(snapshot: &Snapshot) -> String {
    let mut out = String::new();

    write_metric(
        &mut out,
        "logsentry_events_total",
        "counter",
        "Verarbeitete Journal-Ereignisse seit Start.",
        snapshot.stats.events as f64,
    );
    write_metric(
        &mut out,
        "logsentry_anomalies_total",
        "counter",
        "Gemeldete Anomalien seit Start.",
        snapshot.stats.anomalies_emitted as f64,
    );
    write_metric(
        &mut out,
        "logsentry_dropped_overflow_total",
        "counter",
        "Wegen Kanal-Überlauf verworfene Ereignisse seit Start.",
        snapshot.stats.dropped_overflow as f64,
    );
    write_metric(
        &mut out,
        "logsentry_parse_errors_total",
        "counter",
        "Nicht parsbare Journal-Zeilen seit Start.",
        snapshot.stats.parse_errors as f64,
    );
    write_metric(
        &mut out,
        "logsentry_events_per_second",
        "gauge",
        "Aktuelle Ereignisrate.",
        snapshot.stats.events_per_sec,
    );
    write_metric(
        &mut out,
        "logsentry_clients",
        "gauge",
        "Aktuell verbundene Clients.",
        f64::from(snapshot.stats.clients),
    );
    write_metric(
        &mut out,
        "logsentry_entropy_bits",
        "gauge",
        "Aktuelle Fenster-Entropie in Bit.",
        snapshot.window.entropy_bits,
    );
    write_metric(
        &mut out,
        "logsentry_entropy_z",
        "gauge",
        "Robuster Z-Score der Fenster-Entropie.",
        snapshot.window.entropy_z,
    );
    write_metric(
        &mut out,
        "logsentry_distinct_templates",
        "gauge",
        "Unterschiedliche Templates im aktuellen Zeitfenster.",
        snapshot.window.distinct_templates as f64,
    );
    write_metric(
        &mut out,
        "logsentry_learning_active",
        "gauge",
        "Ob die globale Lernphase noch läuft (1) oder nicht (0).",
        if snapshot.learning.active { 1.0 } else { 0.0 },
    );
    write_metric(
        &mut out,
        "logsentry_daemon_uptime_seconds",
        "gauge",
        "Laufzeit des Daemons seit dem letzten Start.",
        snapshot.daemon_uptime_secs as f64,
    );

    if let Some(system) = &snapshot.system {
        write_metric(
            &mut out,
            "logsentry_cpu_usage_percent",
            "gauge",
            "Gesamte CPU-Auslastung des Systems.",
            f64::from(system.cpu.global_usage_percent),
        );
        write_metric(
            &mut out,
            "logsentry_memory_used_percent",
            "gauge",
            "Belegter Anteil des Arbeitsspeichers.",
            f64::from(system.memory.used_percent),
        );
        write_metric(
            &mut out,
            "logsentry_load1",
            "gauge",
            "Systemlast, Mittel der letzten Minute.",
            system.load.one,
        );
    }

    out
}

fn write_metric(out: &mut String, name: &str, kind: &str, help: &str, value: f64) {
    out.push_str(&format!("# HELP {name} {help}\n"));
    out.push_str(&format!("# TYPE {name} {kind}\n"));
    out.push_str(&format!("{name} {value}\n"));
}

/// Schreibt den Textfile-Export atomar: temporäre Datei im selben
/// Verzeichnis wie das Ziel, dann `rename` (garantiert atomar innerhalb
/// desselben Dateisystems, Regel: keine für node_exporter sichtbare
/// Zwischenversion).
pub fn write_prometheus_textfile(path: &Path, snapshot: &Snapshot) -> std::io::Result<()> {
    let content = format_prometheus_textfile(snapshot);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp_path = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("logsentry.prom"),
        std::process::id()
    ));

    let mut file = std::fs::File::create(&tmp_path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp_path, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use logsentry_proto::{LearningState, PipelineStats, WindowStats};

    fn sample_snapshot() -> Snapshot {
        Snapshot {
            timestamp_us: 1,
            daemon_uptime_secs: 42,
            learning: LearningState {
                active: true,
                remaining_secs: Some(60),
            },
            stats: PipelineStats {
                events: 100,
                parse_errors: 1,
                dropped_overflow: 2,
                templates: 10,
                anomalies_emitted: 3,
                suppressed: 0,
                suppressed_learning: 0,
                events_per_sec: 1.5,
                clients: 2,
            },
            window: WindowStats {
                window_secs: 60,
                entropy_bits: 2.5,
                entropy_z: 0.3,
                events_in_window: 10,
                distinct_templates: 4,
            },
            system: None,
            replay: false,
        }
    }

    #[test]
    fn enthaelt_help_type_und_wert_je_metrik() {
        let text = format_prometheus_textfile(&sample_snapshot());
        assert!(text.contains("# HELP logsentry_events_total"));
        assert!(text.contains("# TYPE logsentry_events_total counter"));
        assert!(text.contains("logsentry_events_total 100"));
        assert!(text.contains("logsentry_learning_active 1"));
        assert!(text.contains("logsentry_entropy_bits 2.5"));
    }

    #[test]
    fn ohne_systemzustand_fehlen_dessen_metriken_statt_falscher_nullen() {
        let text = format_prometheus_textfile(&sample_snapshot());
        assert!(!text.contains("logsentry_cpu_usage_percent"));
    }

    #[test]
    fn write_prometheus_textfile_erzeugt_lesbare_datei_ohne_temp_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logsentry.prom");
        write_prometheus_textfile(&path, &sample_snapshot()).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("logsentry_events_total 100"));

        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftover.is_empty(), "temporäre Datei wurde nicht umbenannt");
    }
}
