//! Abbildung von `logsentry-core`-Typen auf das Wire-Format
//! (`logsentry-proto`).
//!
//! `proto` hängt bewusst nicht von `core` ab (`docs/phase6-protokoll.md`
//! Abschnitt 1) -- diese Datei ist der einzige Ort im Daemon, an dem beide
//! Richtungen aufeinandertreffen. Reine Datentyp-Konvertierungen: kein I/O,
//! kein `panic!` (Regel 16), jede Funktion ist auf ihrer Eingabe total.
//!
//! `#![allow(dead_code)]`: Dieses Modul ist Schritt 4 der
//! Umsetzungsreihenfolge (`docs/phase6-protokoll.md` Abschnitt 8) und wird
//! erst in Schritt 6 (`pipeline.rs` an `SharedState` angebunden) aus
//! `main.rs`/`pipeline.rs` heraus tatsächlich aufgerufen. Bis dahin ist der
//! Daemon ein Binary-Crate ohne öffentliche API-Oberfläche, gegen die
//! `pub fn` sonst als "erreichbar" zählen würde -- die Warnung ist also
//! kein Hinweis auf einen Bug, sondern auf noch fehlende Verdrahtung. Wird
//! entfernt, sobald Schritt 6 abgeschlossen ist.
#![allow(dead_code)]

use std::sync::Arc;

use logsentry_core::baseline::RateSource as CoreRateSource;
use logsentry_core::system as core_system;
use logsentry_core::{Anomaly as CoreAnomaly, AnomalyLevel as CoreAnomalyLevel};
use logsentry_proto as wire;

/// Obergrenze für `AnomalyEvent::sample_message` in Bytes (Regel: Snapshot-
/// und Anomalie-Nachrichten dürfen nicht durch eine einzelne, potenziell
/// riesige Log-Zeile unbeschränkt wachsen -- Regel 18).
const SAMPLE_MESSAGE_MAX_BYTES: usize = 1024;
const TRUNCATION_MARKER: &str = "[…]";

/// Kürzt `raw` auf höchstens [`SAMPLE_MESSAGE_MAX_BYTES`] Bytes, ohne einen
/// UTF-8-Zeichen mittendrin abzuschneiden, und hängt [`TRUNCATION_MARKER`]
/// an, falls gekürzt wurde.
pub fn truncate_sample_message(raw: &str) -> String {
    if raw.len() <= SAMPLE_MESSAGE_MAX_BYTES {
        return raw.to_string();
    }
    let budget = SAMPLE_MESSAGE_MAX_BYTES.saturating_sub(TRUNCATION_MARKER.len());
    let mut end = budget.min(raw.len());
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + TRUNCATION_MARKER.len());
    out.push_str(&raw[..end]);
    out.push_str(TRUNCATION_MARKER);
    out
}

/// Bildet einen CPU-Systemzustand ab.
pub fn map_cpu_snapshot(c: core_system::CpuSnapshot) -> wire::CpuSnapshot {
    wire::CpuSnapshot {
        global_usage_percent: c.global_usage_percent,
        per_core_usage_percent: c.per_core_usage_percent,
    }
}

/// Bildet einen Arbeitsspeicher-Systemzustand ab.
pub fn map_memory_snapshot(m: core_system::MemorySnapshot) -> wire::MemorySnapshot {
    wire::MemorySnapshot {
        total_bytes: m.total_bytes,
        used_bytes: m.used_bytes,
        used_percent: m.used_percent,
    }
}

/// Bildet die Systemlast ab.
pub fn map_load_snapshot(l: core_system::LoadSnapshot) -> wire::LoadSnapshot {
    wire::LoadSnapshot {
        one: l.one,
        five: l.five,
        fifteen: l.fifteen,
    }
}

/// Bildet die Belegung eines Dateisystems ab.
pub fn map_disk_snapshot(d: core_system::DiskSnapshot) -> wire::DiskSnapshot {
    wire::DiskSnapshot {
        mount_point: d.mount_point,
        total_bytes: d.total_bytes,
        available_bytes: d.available_bytes,
        used_percent: d.used_percent,
    }
}

/// Bildet eine Temperaturmessung ab.
pub fn map_temperature_snapshot(t: core_system::TemperatureSnapshot) -> wire::TemperatureSnapshot {
    wire::TemperatureSnapshot {
        label: t.label,
        celsius: t.celsius,
    }
}

/// Bildet den Status einer systemd-Unit ab.
pub fn map_unit_status(u: core_system::UnitStatus) -> wire::UnitStatus {
    wire::UnitStatus {
        name: u.name,
        active_state: u.active_state,
        sub_state: u.sub_state,
    }
}

/// Bildet den vollständigen Systemzustand ab.
pub fn map_system_snapshot(s: core_system::SystemSnapshot) -> wire::SystemSnapshot {
    wire::SystemSnapshot {
        timestamp_us: s.timestamp_us,
        cpu: map_cpu_snapshot(s.cpu),
        memory: map_memory_snapshot(s.memory),
        load: map_load_snapshot(s.load),
        disks: s.disks.into_iter().map(map_disk_snapshot).collect(),
        temperatures: s
            .temperatures
            .into_iter()
            .map(map_temperature_snapshot)
            .collect(),
        units: s.units.into_iter().map(map_unit_status).collect(),
    }
}

/// Bildet die Herkunft eines Rate-Z-Scores ab (Phase 4: Fallback-Kette
/// Slot-Baseline -> ANY-Slot-Baseline -> Kurzzeit-Historie -> keine).
pub fn map_rate_source(source: CoreRateSource) -> wire::RateSource {
    match source {
        CoreRateSource::SlotBaseline => wire::RateSource::SlotBaseline,
        CoreRateSource::AnySlotBaseline => wire::RateSource::AnySlotBaseline,
        CoreRateSource::ShortTerm => wire::RateSource::ShortTerm,
        CoreRateSource::None => wire::RateSource::None,
    }
}

/// Bildet den Schweregrad ab. `None`, wenn `level` `Normal` ist -- dieser
/// Fall verlässt die Analyse-Engine nie als `Some(Anomaly)` (sie liefert
/// dann `None` statt einer Anomalie), aber diese Funktion bleibt defensiv
/// total statt zu unterstellen, dass das immer so bleibt.
pub fn map_anomaly_level(level: CoreAnomalyLevel) -> Option<wire::AnomalyLevel> {
    match level {
        CoreAnomalyLevel::Normal => None,
        CoreAnomalyLevel::Info => Some(wire::AnomalyLevel::Info),
        CoreAnomalyLevel::Warn => Some(wire::AnomalyLevel::Warn),
        CoreAnomalyLevel::Critical => Some(wire::AnomalyLevel::Critical),
    }
}

/// Bildet die Score-Aufschlüsselung ab.
pub fn map_score_breakdown(b: logsentry_core::ScoreBreakdown) -> wire::ScoreBreakdown {
    wire::ScoreBreakdown {
        rate_z: b.rate_z,
        surprisal_bits: b.surprisal_bits,
        entropy_z: b.entropy_z,
        rate_component: b.rate_component,
        surprisal_component: b.surprisal_component,
        entropy_component: b.entropy_component,
        combined: b.combined,
        rate_source: map_rate_source(b.rate_source),
    }
}

/// Zusätzlicher Kontext aus dem auslösenden Journal-Ereignis, den
/// [`core::Anomaly`] selbst nicht trägt (Template-Text kommt aus der
/// Template-Registry, Rohzeile/PID/Priorität aus dem `JournalEvent`).
pub struct AnomalyEventContext<'a> {
    pub template_text: &'a str,
    pub raw_message: &'a str,
    pub pid: Option<i32>,
    pub priority: Option<u8>,
}

/// Baut ein sendefertiges [`wire::AnomalyEvent`]. `id` muss von
/// [`crate::state::SharedState::next_anomaly_id`] stammen (monoton pro
/// Session). Liefert `None`, wenn `anomaly.level` `Normal` ist (siehe
/// [`map_anomaly_level`]) -- in der Praxis kommt das nicht vor, aber die
/// Funktion soll nie eine unsinnige `AnomalyEvent` mit einem Level
/// erzeugen, das laut Protokoll gar nicht existiert.
pub fn build_anomaly_event(
    id: u64,
    anomaly: &CoreAnomaly,
    ctx: AnomalyEventContext<'_>,
) -> Option<wire::AnomalyEvent> {
    let level = map_anomaly_level(anomaly.level)?;
    Some(wire::AnomalyEvent {
        id,
        timestamp_us: anomaly.timestamp_us,
        template_id: anomaly.template_id.0,
        template_text: ctx.template_text.to_string(),
        sample_message: truncate_sample_message(ctx.raw_message),
        unit: anomaly.unit.clone(),
        pid: ctx.pid.and_then(|p| u32::try_from(p).ok()),
        priority: ctx.priority,
        level,
        breakdown: map_score_breakdown(anomaly.breakdown),
        suppressed_since_last: anomaly.suppressed_since_last,
    })
}

/// Ein Eintrag im Rohzeilen-Ring für `GetContext` (siehe
/// [`crate::state::ContextRing`]). Eigener Typ statt direkt
/// `JournalEvent`, damit der Ring nicht an dessen (potenziell wachsenden)
/// Feldern hängt und die Rohzeile als `Arc<str>` geteilt werden kann statt
/// bei jedem `push` geklont zu werden.
#[derive(Debug, Clone)]
pub struct ContextEntry {
    pub timestamp_us: u64,
    pub unit: Option<Arc<str>>,
    pub pid: Option<i32>,
    pub priority: Option<u8>,
    pub message: Arc<str>,
}

impl From<&ContextEntry> for wire::ContextLine {
    fn from(e: &ContextEntry) -> Self {
        Self {
            timestamp_us: e.timestamp_us,
            unit: e.unit.as_deref().map(str::to_string),
            pid: e.pid.and_then(|p| u32::try_from(p).ok()),
            priority: e.priority,
            message: e.message.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kurze_nachricht_bleibt_unveraendert() {
        assert_eq!(truncate_sample_message("kurz"), "kurz");
    }

    #[test]
    fn nachricht_exakt_an_der_grenze_bleibt_unveraendert() {
        let raw = "a".repeat(SAMPLE_MESSAGE_MAX_BYTES);
        let out = truncate_sample_message(&raw);
        assert_eq!(out, raw);
        assert_eq!(out.len(), SAMPLE_MESSAGE_MAX_BYTES);
    }

    #[test]
    fn zu_lange_nachricht_wird_gekuerzt_und_markiert() {
        let raw = "a".repeat(SAMPLE_MESSAGE_MAX_BYTES + 100);
        let out = truncate_sample_message(&raw);
        assert!(out.len() <= SAMPLE_MESSAGE_MAX_BYTES);
        assert!(out.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn kuerzung_schneidet_nie_mitten_in_einem_utf8_zeichen() {
        // Jedes "€" ist 3 Byte lang; die Kürzung darf keinen ungültigen
        // Rest erzeugen -- String::from() würde sonst gar nicht erst
        // kompilieren/laufen, hier prüfen wir, dass die Grenze respektiert
        // wird (die Funktion arbeitet ohnehin nur mit gültigem &str).
        let raw = "€".repeat(400); // 1200 Byte
        let out = truncate_sample_message(&raw);
        assert!(out.len() <= SAMPLE_MESSAGE_MAX_BYTES);
        assert!(out.ends_with(TRUNCATION_MARKER));
        // Der Teil vor der Markierung muss aus vollständigen "€" bestehen.
        let body = &out[..out.len() - TRUNCATION_MARKER.len()];
        assert!(body.chars().all(|c| c == '€'));
    }

    #[test]
    fn anomaly_level_normal_wird_nicht_abgebildet() {
        assert_eq!(map_anomaly_level(CoreAnomalyLevel::Normal), None);
    }

    #[test]
    fn anomaly_level_info_warn_critical_werden_abgebildet() {
        assert_eq!(
            map_anomaly_level(CoreAnomalyLevel::Info),
            Some(wire::AnomalyLevel::Info)
        );
        assert_eq!(
            map_anomaly_level(CoreAnomalyLevel::Warn),
            Some(wire::AnomalyLevel::Warn)
        );
        assert_eq!(
            map_anomaly_level(CoreAnomalyLevel::Critical),
            Some(wire::AnomalyLevel::Critical)
        );
    }

    #[test]
    fn rate_source_wird_eins_zu_eins_abgebildet() {
        assert_eq!(
            map_rate_source(CoreRateSource::SlotBaseline),
            wire::RateSource::SlotBaseline
        );
        assert_eq!(
            map_rate_source(CoreRateSource::AnySlotBaseline),
            wire::RateSource::AnySlotBaseline
        );
        assert_eq!(
            map_rate_source(CoreRateSource::ShortTerm),
            wire::RateSource::ShortTerm
        );
        assert_eq!(
            map_rate_source(CoreRateSource::None),
            wire::RateSource::None
        );
    }

    fn sample_anomaly(level: CoreAnomalyLevel) -> CoreAnomaly {
        CoreAnomaly {
            timestamp_us: 1_726_000_000_000_000,
            template_id: logsentry_core::TemplateId(42),
            unit: Some("sshd.service".to_string()),
            level,
            breakdown: logsentry_core::ScoreBreakdown {
                rate_z: 4.0,
                surprisal_bits: 10.0,
                entropy_z: 1.0,
                rate_component: 0.5,
                surprisal_component: 0.5,
                entropy_component: 0.1,
                combined: 0.7,
                rate_source: CoreRateSource::SlotBaseline,
            },
            suppressed_since_last: 3,
        }
    }

    #[test]
    fn build_anomaly_event_liefert_none_fuer_normal() {
        let anomaly = sample_anomaly(CoreAnomalyLevel::Normal);
        let ctx = AnomalyEventContext {
            template_text: "Failed password for <USER>",
            raw_message: "Failed password for root",
            pid: Some(1234),
            priority: Some(4),
        };
        assert!(build_anomaly_event(1, &anomaly, ctx).is_none());
    }

    #[test]
    fn build_anomaly_event_uebernimmt_alle_felder() {
        let anomaly = sample_anomaly(CoreAnomalyLevel::Warn);
        let ctx = AnomalyEventContext {
            template_text: "Failed password for <USER>",
            raw_message: "Failed password for root",
            pid: Some(1234),
            priority: Some(4),
        };
        let event = build_anomaly_event(7, &anomaly, ctx).expect("Warn muss abgebildet werden");
        assert_eq!(event.id, 7);
        assert_eq!(event.template_id, 42);
        assert_eq!(event.template_text, "Failed password for <USER>");
        assert_eq!(event.sample_message, "Failed password for root");
        assert_eq!(event.unit.as_deref(), Some("sshd.service"));
        assert_eq!(event.pid, Some(1234));
        assert_eq!(event.priority, Some(4));
        assert_eq!(event.level, wire::AnomalyLevel::Warn);
        assert_eq!(event.suppressed_since_last, 3);
        assert_eq!(event.breakdown.rate_source, wire::RateSource::SlotBaseline);
    }

    #[test]
    fn negative_pid_wird_zu_none_statt_zu_panicken() {
        let anomaly = sample_anomaly(CoreAnomalyLevel::Info);
        let ctx = AnomalyEventContext {
            template_text: "x",
            raw_message: "x",
            pid: Some(-1),
            priority: None,
        };
        let event = build_anomaly_event(1, &anomaly, ctx).unwrap();
        assert_eq!(event.pid, None);
    }

    #[test]
    fn system_snapshot_konvertiert_alle_teile() {
        let core_snap = core_system::SystemSnapshot {
            timestamp_us: 1,
            cpu: core_system::CpuSnapshot {
                global_usage_percent: 12.5,
                per_core_usage_percent: vec![10.0, 15.0],
            },
            memory: core_system::MemorySnapshot {
                total_bytes: 100,
                used_bytes: 50,
                used_percent: 50.0,
            },
            load: core_system::LoadSnapshot {
                one: 0.1,
                five: 0.2,
                fifteen: 0.3,
            },
            disks: vec![core_system::DiskSnapshot {
                mount_point: "/".into(),
                total_bytes: 1000,
                available_bytes: 500,
                used_percent: 50.0,
            }],
            temperatures: vec![core_system::TemperatureSnapshot {
                label: "CPU".into(),
                celsius: 40.0,
            }],
            units: vec![core_system::UnitStatus {
                name: "sshd.service".into(),
                active_state: "active".into(),
                sub_state: "running".into(),
            }],
        };
        let wire_snap: wire::SystemSnapshot = map_system_snapshot(core_snap);
        assert_eq!(wire_snap.cpu.global_usage_percent, 12.5);
        assert_eq!(wire_snap.disks[0].mount_point, "/");
        assert_eq!(wire_snap.temperatures[0].label, "CPU");
        assert_eq!(wire_snap.units[0].name, "sshd.service");
    }
}
