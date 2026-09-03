//! Datentypen für den Systemzustand: CPU, RAM, Load, Disk, Temperaturen und
//! Status ausgewählter systemd-Units (Phase 5).
//!
//! Reine Datentypen ohne I/O, damit sie unabhängig vom tatsächlichen
//! `sysinfo`/`zbus`-Zugriff (im `daemon`-Crate) testbar sind und später
//! unverändert über das Socket-Protokoll (Phase 6) an die GUI gehen können.

use serde::{Deserialize, Serialize};

/// CPU-Auslastung.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CpuSnapshot {
    /// Gesamtauslastung über alle Kerne, in Prozent (0..100).
    pub global_usage_percent: f32,
    /// Auslastung je Kern, in Prozent (0..100).
    pub per_core_usage_percent: Vec<f32>,
}

/// Arbeitsspeicher-Belegung.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemorySnapshot {
    /// Gesamter physischer Speicher in Bytes.
    pub total_bytes: u64,
    /// Belegter Speicher in Bytes.
    pub used_bytes: u64,
    /// Belegter Anteil in Prozent (0..100), aus `used`/`total` abgeleitet.
    pub used_percent: f32,
}

/// Systemlast (Unix Load Average).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LoadSnapshot {
    /// Mittlere Last der letzten Minute.
    pub one: f64,
    /// Mittlere Last der letzten 5 Minuten.
    pub five: f64,
    /// Mittlere Last der letzten 15 Minuten.
    pub fifteen: f64,
}

/// Belegung eines einzelnen Dateisystems.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiskSnapshot {
    /// Einhängepunkt (z. B. `/`, `/home`).
    pub mount_point: String,
    /// Gesamtkapazität in Bytes.
    pub total_bytes: u64,
    /// Verfügbarer Platz in Bytes.
    pub available_bytes: u64,
    /// Belegter Anteil in Prozent (0..100).
    pub used_percent: f32,
}

/// Eine einzelne Temperaturmessung (CPU, GPU, NVMe, Chipsatz, …).
///
/// Es gibt bewusst keine eigene GPU-Kategorie: Alle Temperaturen kommen
/// gleichförmig aus `sysinfo::Components`, das unter Linux sämtliche
/// `hwmon`-Sensoren einliest -- inklusive GPU-Sensoren, sofern der
/// Treiber (z. B. `amdgpu`, `nouveau`, oder proprietäre NVIDIA-Treiber
/// neuerer Versionen) sie dort registriert. Eine separate NVML-Anbindung
/// wurde bewusst nicht hinzugefügt: Sie wäre eine zusätzliche, nur für
/// NVIDIA nutzbare Abhängigkeit für einen Fall, den `hwmon` auf den meisten
/// Systemen bereits abdeckt. Ist keine Quelle vorhanden, bleibt die Liste
/// schlicht leer -- die GUI (Phase 7) blendet das Feld dann aus, statt
/// einen Wert zu raten oder 0 °C anzuzeigen.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TemperatureSnapshot {
    /// Vom Treiber vergebene Bezeichnung (z. B. `Package id 0`, `edge`).
    pub label: String,
    /// Temperatur in Grad Celsius.
    pub celsius: f32,
}

/// Status einer überwachten systemd-Unit (`ActiveState`/`SubState` aus
/// `org.freedesktop.systemd1.Manager.ListUnits`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnitStatus {
    /// Name der Unit (z. B. `sshd.service`).
    pub name: String,
    /// `ActiveState` (z. B. `active`, `inactive`, `failed`).
    pub active_state: String,
    /// `SubState` (z. B. `running`, `dead`, `exited`).
    pub sub_state: String,
}

/// Vollständiger Systemzustand zu einem Zeitpunkt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SystemSnapshot {
    /// Zeitpunkt der Messung (Mikrosekunden seit Epoch).
    pub timestamp_us: u64,
    /// CPU-Auslastung.
    pub cpu: CpuSnapshot,
    /// Arbeitsspeicher-Belegung.
    pub memory: MemorySnapshot,
    /// Systemlast.
    pub load: LoadSnapshot,
    /// Belegung der überwachten Dateisysteme.
    pub disks: Vec<DiskSnapshot>,
    /// Verfügbare Temperaturmessungen (leer, wenn keine Quelle vorhanden ist).
    pub temperatures: Vec<TemperatureSnapshot>,
    /// Status der in der Konfiguration ausgewählten systemd-Units.
    pub units: Vec<UnitStatus>,
}

/// Berechnet einen Belegungsanteil in Prozent aus `used`/`total`.
///
/// `0.0` bei `total == 0` (z. B. ein nicht auslesbares Dateisystem), statt
/// durch 0 zu teilen. Das Ergebnis wird auf 0..100 begrenzt, da `used`
/// durch Zeitpunkt-Ungenauigkeiten beim Messen geringfügig über `total`
/// liegen kann.
pub fn used_percent(total: u64, used: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    ((used as f64 / total as f64) * 100.0).clamp(0.0, 100.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_percent_bei_halber_belegung() {
        assert!((used_percent(1000, 500) - 50.0).abs() < 1e-6);
    }

    #[test]
    fn used_percent_bei_voller_belegung() {
        assert!((used_percent(1000, 1000) - 100.0).abs() < 1e-6);
    }

    #[test]
    fn used_percent_bei_leerem_gesamtwert_ist_null_statt_division_durch_null() {
        assert_eq!(used_percent(0, 0), 0.0);
        assert_eq!(used_percent(0, 500), 0.0);
    }

    #[test]
    fn used_percent_wird_auf_hundert_gedeckelt() {
        // used > total kann durch Zeitpunkt-Ungenauigkeiten beim Messen
        // zweier separater Werte auftreten.
        assert!((used_percent(1000, 1500) - 100.0).abs() < 1e-6);
    }
}
