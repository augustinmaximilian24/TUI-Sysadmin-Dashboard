//! Status ausgewählter systemd-Units über D-Bus (Phase 5).
//!
//! Nutzt `org.freedesktop.systemd1.Manager.ListUnits`, nicht
//! `Command::new("systemctl")` -- konsistent mit Regel 10 (Aktionen in
//! Phase 8 müssen ohnehin über D-Bus/polkit laufen; Statusabfragen folgen
//! demselben Weg statt zwei Zugriffsarten zu pflegen).
//!
//! `ListUnits` liefert `ActiveState` und `SubState` bereits direkt im
//! zurückgegebenen Tupel; eine zusätzliche Property-Abfrage je Unit über
//! einen eigenen `Unit`-Proxy ist für diesen Zweck nicht nötig.

use logsentry_core::system::UnitStatus;
use zbus::zvariant::OwnedObjectPath;
use zbus::{proxy, Connection};

/// Ein Eintrag aus `ListUnits`: `(name, description, load_state,
/// active_state, sub_state, followed, unit_path, job_id, job_type,
/// job_path)` -- Signatur `a(ssssssouso)`.
type RawUnitEntry = (
    String,
    String,
    String,
    String,
    String,
    String,
    OwnedObjectPath,
    u32,
    String,
    OwnedObjectPath,
);

#[proxy(
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1",
    interface = "org.freedesktop.systemd1.Manager"
)]
pub(crate) trait SystemdManager {
    #[zbus(name = "ListUnits")]
    fn list_units(&self) -> zbus::Result<Vec<RawUnitEntry>>;

    /// Startet eine Unit neu (`mode = "replace"`, wie `systemctl restart`).
    /// Genutzt von `daemon::actions` (Phase 8) statt `Command::new("systemctl")`
    /// (Regel 10); die Autorisierung läuft über die D-Bus/polkit-Policy des
    /// System-Bus.
    #[zbus(name = "RestartUnit")]
    fn restart_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;

    /// Stoppt eine Unit (`mode = "replace"`, wie `systemctl stop`).
    #[zbus(name = "StopUnit")]
    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
}

/// Fehler der Unit-Statusabfrage.
#[derive(Debug, thiserror::Error)]
pub enum UnitMonitorError {
    /// Verbindung zum System-Bus fehlgeschlagen (kein D-Bus, keine Rechte).
    #[error("Verbindung zum System-D-Bus fehlgeschlagen: {0}")]
    Connect(#[source] zbus::Error),
    /// Der `ListUnits`-Aufruf selbst schlug fehl.
    #[error("ListUnits fehlgeschlagen: {0}")]
    ListUnits(#[source] zbus::Error),
}

/// Verbindung zum System-D-Bus für Unit-Statusabfragen.
pub struct UnitMonitor {
    connection: Connection,
}

impl UnitMonitor {
    /// Baut die Verbindung zum System-Bus auf.
    pub async fn connect() -> Result<Self, UnitMonitorError> {
        let connection = Connection::system()
            .await
            .map_err(UnitMonitorError::Connect)?;
        Ok(Self { connection })
    }

    /// Fragt den Status der in `watched` genannten Units ab. Units, die
    /// aktuell nicht existieren oder nie geladen wurden, tauchen einfach
    /// nicht im Ergebnis auf (kein Fehler).
    pub async fn poll(&self, watched: &[String]) -> Result<Vec<UnitStatus>, UnitMonitorError> {
        let manager = SystemdManagerProxy::new(&self.connection)
            .await
            .map_err(UnitMonitorError::ListUnits)?;
        let raw = manager
            .list_units()
            .await
            .map_err(UnitMonitorError::ListUnits)?;
        Ok(filter_watched_units(raw, watched))
    }
}

/// Reine Filter-/Umwandlungslogik, getrennt vom D-Bus-Aufruf und damit
/// ohne laufenden System-Bus testbar.
fn filter_watched_units(raw: Vec<RawUnitEntry>, watched: &[String]) -> Vec<UnitStatus> {
    raw.into_iter()
        .filter(|(name, ..)| watched.iter().any(|w| w == name))
        .map(
            |(name, _description, _load_state, active_state, sub_state, ..)| UnitStatus {
                name,
                active_state,
                sub_state,
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, active: &str, sub: &str) -> RawUnitEntry {
        (
            name.to_string(),
            format!("{name} description"),
            "loaded".to_string(),
            active.to_string(),
            sub.to_string(),
            String::new(),
            OwnedObjectPath::try_from("/org/freedesktop/systemd1/unit/x").expect("gültiger Pfad"),
            0,
            String::new(),
            OwnedObjectPath::try_from("/").expect("gültiger Pfad"),
        )
    }

    #[test]
    fn nur_beobachtete_units_landen_im_ergebnis() {
        let raw = vec![
            entry("sshd.service", "active", "running"),
            entry("unwichtig.service", "active", "running"),
            entry("cron.service", "inactive", "dead"),
        ];
        let watched = vec!["sshd.service".to_string(), "cron.service".to_string()];
        let result = filter_watched_units(raw, &watched);

        assert_eq!(result.len(), 2);
        assert!(result
            .iter()
            .any(|u| u.name == "sshd.service" && u.active_state == "active"));
        assert!(result
            .iter()
            .any(|u| u.name == "cron.service" && u.sub_state == "dead"));
    }

    #[test]
    fn nicht_existierende_beobachtete_unit_erzeugt_keinen_fehler_nur_eine_luecke() {
        let raw = vec![entry("sshd.service", "active", "running")];
        let watched = vec![
            "sshd.service".to_string(),
            "gibt-es-nicht.service".to_string(),
        ];
        let result = filter_watched_units(raw, &watched);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn leere_watch_liste_ergibt_leeres_ergebnis() {
        let raw = vec![entry("sshd.service", "active", "running")];
        let result = filter_watched_units(raw, &[]);
        assert!(result.is_empty());
    }

    /// Live-Rauchtest gegen den echten System-Bus, sofern vorhanden.
    /// Übersprungen statt fehlzuschlagen, wenn kein D-Bus läuft (wie beim
    /// journalctl-Test in Phase 1) -- dieser Container hat typischerweise
    /// keinen laufenden System-Bus.
    #[tokio::test]
    async fn live_verbindung_zum_system_bus_falls_vorhanden() {
        match UnitMonitor::connect().await {
            Ok(monitor) => {
                let result = monitor.poll(&["dbus.service".to_string()]).await;
                assert!(
                    result.is_ok(),
                    "ListUnits sollte bei bestehender Verbindung funktionieren"
                );
            }
            Err(err) => {
                eprintln!("kein System-Bus verfügbar, Test übersprungen: {err}");
            }
        }
    }
}
