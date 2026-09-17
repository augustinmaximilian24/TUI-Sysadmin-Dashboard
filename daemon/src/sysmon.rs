//! Erfassung des Systemzustands über `sysinfo` (Phase 5): CPU, RAM, Load,
//! Dateisystem-Belegung und Temperaturen.
//!
//! Einziger I/O-Ort für diese Metriken; `logsentry_core::system` enthält
//! nur die reinen, testbaren Datentypen.

use logsentry_core::config::SystemConfig;
use logsentry_core::system::{
    used_percent, CpuSnapshot, DiskSnapshot, LoadSnapshot, MemorySnapshot, SystemSnapshot,
    TemperatureSnapshot,
};
use sysinfo::{Components, Disks, System};

/// Hält den `sysinfo`-Zustand zwischen zwei Messungen. `sysinfo` braucht
/// diesen Zustand, um z. B. CPU-Auslastung als Differenz zweier Messungen
/// zu berechnen.
pub struct SysMonitor {
    system: System,
    disks: Disks,
    components: Components,
    watched_mount_points: Vec<String>,
}

impl SysMonitor {
    /// Erstellt den Monitor und nimmt eine erste, "leere" Messung vor.
    ///
    /// Wichtig: `sysinfo`s CPU-Auslastung ist die Differenz zweier
    /// Messungen. Die *erste* [`SysMonitor::poll`]-Ausgabe nach `new()`
    /// liefert daher einen wenig aussagekräftigen CPU-Wert (typischerweise
    /// nahe 0), weil die Basis dafür gerade erst gelegt wurde. Da der
    /// Daemon ohnehin periodisch pollt (`SystemConfig::poll_interval_seconds`),
    /// ist der zweite und jeder weitere Wert korrekt; ein künstliches
    /// Warten beim Start wurde bewusst nicht eingebaut, um den Start nicht
    /// zu blockieren.
    pub fn new(config: &SystemConfig) -> Self {
        let mut system = System::new_all();
        system.refresh_all();
        Self {
            system,
            disks: Disks::new_with_refreshed_list(),
            components: Components::new_with_refreshed_list(),
            watched_mount_points: config.watched_mount_points.clone(),
        }
    }

    /// Nimmt eine neue Messung vor und liefert einen Schnappschuss ohne
    /// Unit-Status (der kommt separat über `zbus`, siehe
    /// [`super::units::UnitMonitor`]).
    pub fn poll(&mut self, timestamp_us: u64) -> SystemSnapshot {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.disks.refresh(true);
        self.components.refresh(true);

        let cpu = CpuSnapshot {
            global_usage_percent: self.system.global_cpu_usage(),
            per_core_usage_percent: self
                .system
                .cpus()
                .iter()
                .map(sysinfo::Cpu::cpu_usage)
                .collect(),
        };

        let total = self.system.total_memory();
        let used = self.system.used_memory();
        let memory = MemorySnapshot {
            total_bytes: total,
            used_bytes: used,
            used_percent: used_percent(total, used),
        };

        let load_avg = System::load_average();
        let load = LoadSnapshot {
            one: load_avg.one,
            five: load_avg.five,
            fifteen: load_avg.fifteen,
        };

        let disks = self
            .disks
            .list()
            .iter()
            .filter(|disk| {
                self.watched_mount_points.is_empty()
                    || self
                        .watched_mount_points
                        .iter()
                        .any(|m| m.as_str() == disk.mount_point().to_string_lossy())
            })
            .map(|disk| {
                let total = disk.total_space();
                let available = disk.available_space();
                let used = total.saturating_sub(available);
                DiskSnapshot {
                    mount_point: disk.mount_point().to_string_lossy().into_owned(),
                    total_bytes: total,
                    available_bytes: available,
                    used_percent: used_percent(total, used),
                }
            })
            .collect();

        // Leere Liste, wenn keine Quelle vorhanden ist (Regel: nicht raten).
        let temperatures = self
            .components
            .list()
            .iter()
            .filter_map(|component| {
                component.temperature().map(|celsius| TemperatureSnapshot {
                    label: component.label().to_string(),
                    celsius,
                })
            })
            .collect();

        SystemSnapshot {
            timestamp_us,
            cpu,
            memory,
            load,
            disks,
            temperatures,
            units: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kein Fixture-Test möglich (Regel 25 gilt für Analysefunktionen mit
    /// deterministischer Eingabe; `sysinfo` liest die echte Maschine).
    /// Stattdessen: Rauchtest, dass zwei Messungen ohne Panic laufen und
    /// plausible Werte liefern -- läuft überall, auch in Containern ohne
    /// hwmon (dann bleibt `temperatures` schlicht leer, kein Fehler).
    #[test]
    fn zwei_messungen_liefern_plausible_werte_ohne_panic() {
        let config = SystemConfig::default();
        let mut monitor = SysMonitor::new(&config);
        let _erste = monitor.poll(1000);
        let zweite = monitor.poll(2000);

        assert!(zweite.cpu.global_usage_percent >= 0.0);
        assert!(
            zweite.memory.total_bytes > 0,
            "ein Testsystem ohne RAM ist unplausibel"
        );
        assert!((0.0..=100.0).contains(&zweite.memory.used_percent));
        for disk in &zweite.disks {
            assert!((0.0..=100.0).contains(&disk.used_percent));
        }
        // temperatures kann leer sein (kein hwmon im Container) -- das ist
        // der Punkt dieses Designs, kein Fehler.
    }

    #[test]
    fn watched_mount_points_filtert_wenn_gesetzt() {
        let config = SystemConfig {
            watched_mount_points: vec!["/dieser/pfad/existiert/nicht".to_string()],
            ..SystemConfig::default()
        };
        let mut monitor = SysMonitor::new(&config);
        let snapshot = monitor.poll(1000);
        assert!(
            snapshot.disks.is_empty(),
            "ein nicht existierender Einhängepunkt darf keine Disks liefern"
        );
    }
}
