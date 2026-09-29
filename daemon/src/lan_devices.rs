//! LAN-Geräte-Erkennung (Phase-12-Erweiterung, optional): periodischer
//! `ip neigh show`-Scan der ARP-Tabelle, optional vorher per `nmap -sn`
//! aufgefrischt (weckt schlafende Geräte, die sonst nicht in der
//! ARP-Tabelle auftauchen), dazu best-effort Namensauflösung per mDNS
//! (`avahi-resolve-address`). Reine Anwesenheitserkennung -- wohin ein
//! Gerät sich verbindet, liefert erst [`crate::fritzbox_capture`].

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use logsentry_core::config::LanDevicesConfig;
use logsentry_proto::LanDeviceInfo;
use tokio::process::Command;

use crate::now_us;
use crate::state::SharedState;

/// Obergrenze gleichzeitig verfolgter Geräte (Regel 18: kein unbeschränktes
/// Wachstum). Ein Heimnetz hat üblicherweise ein paar Dutzend Geräte, das
/// hier ist eine Sicherheitsgrenze, kein realistischer Normalwert.
const MAX_TRACKED_DEVICES: usize = 256;

/// Wie lange ein nicht mehr gesehenes Gerät im Bestand bleibt, bevor es
/// verworfen wird -- lang genug, dass ein übers Wochenende ausgeschaltetes
/// Gerät nicht sofort als "neu" gilt, wenn es wiederkommt.
const RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);

/// Parst die Ausgabe von `ip neigh show` (je Zeile z. B.
/// `192.168.178.20 dev eth0 lladdr aa:bb:cc:dd:ee:ff REACHABLE`) zu
/// (IP, MAC)-Paaren. Reine Funktion, kein Prozessaufruf, daher direkt
/// testbar. Zeilen ohne `lladdr` (z. B. `FAILED`-Einträge ohne aufgelöste
/// MAC) werden übersprungen -- die MAC hält die Geräte-Identität über
/// DHCP-Lease-Wechsel stabil, ohne sie gibt es keine sinnvolle Identität.
fn parse_ip_neigh(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            let ip = (*tokens.first()?).to_string();
            let mac_idx = tokens.iter().position(|&t| t == "lladdr")? + 1;
            let mac = (*tokens.get(mac_idx)?).to_lowercase();
            Some((ip, mac))
        })
        .collect()
}

async fn run_ip_neigh() -> std::io::Result<String> {
    let output = Command::new("ip").args(["neigh", "show"]).output().await?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Frischt die ARP-Tabelle per Ping-Sweep auf, bevor `ip neigh show`
/// gelesen wird -- ohne das sehen wir nur Geräte, mit denen der Kernel
/// gerade ohnehin Kontakt hatte. Ergebnis wird nicht ausgewertet, ein
/// fehlendes `nmap` ist kein Fehler (`config.ping_sweep_enabled` ist
/// eigens abschaltbar).
async fn run_ping_sweep(subnet: &str) {
    if subnet.is_empty() {
        return;
    }
    let _ = Command::new("nmap")
        .args(["-sn", subnet])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

/// Best-effort mDNS-Namensauflösung. `.ok()`-degradiert wie `SO_PEERCRED`
/// bzw. das GeoIP-Modul an anderer Stelle im Projekt -- fehlendes
/// `avahi-utils` oder ein Gerät, das nicht per mDNS antwortet, ist kein
/// Fehler, nur ein fehlender Anzeigename (Fallback: die IP selbst).
async fn resolve_display_name(ip: &str) -> Option<String> {
    let output = Command::new("avahi-resolve-address")
        .arg(ip)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // Ausgabeformat von avahi-resolve-address: "<ip>\t<hostname>".
    let text = String::from_utf8_lossy(&output.stdout);
    let name = text.split_whitespace().nth(1)?;
    Some(name.trim_end_matches('.').to_string())
}

/// Bestand bekannter Geräte über mehrere Scans hinweg, damit `first_seen_us`
/// stabil bleibt statt bei jedem Scan neu gesetzt zu werden.
struct DeviceRegistry {
    known: HashMap<String, LanDeviceInfo>,
}

impl DeviceRegistry {
    fn new() -> Self {
        Self {
            known: HashMap::new(),
        }
    }

    fn observe(&mut self, mac: String, ip: String, display_name: Option<String>, now_us: u64) {
        match self.known.get_mut(&mac) {
            Some(existing) => {
                existing.ip = ip;
                existing.last_seen_us = now_us;
                if let Some(name) = display_name {
                    existing.display_name = name;
                }
            }
            None => {
                let display_name = display_name.unwrap_or_else(|| ip.clone());
                self.known.insert(
                    mac.clone(),
                    LanDeviceInfo {
                        mac,
                        ip,
                        display_name,
                        first_seen_us: now_us,
                        last_seen_us: now_us,
                    },
                );
            }
        }
    }

    /// Verwirft Geräte, die länger als [`RETENTION`] nicht mehr gesehen
    /// wurden, und deckelt den Bestand zusätzlich hart auf
    /// [`MAX_TRACKED_DEVICES`] (Regel 18) -- bei Überschreitung fliegen die
    /// am längsten nicht gesehenen Geräte zuerst.
    fn prune(&mut self, now_us: u64) {
        let cutoff = now_us.saturating_sub(RETENTION.as_micros() as u64);
        self.known.retain(|_, device| device.last_seen_us >= cutoff);

        if self.known.len() > MAX_TRACKED_DEVICES {
            let mut by_last_seen: Vec<(String, u64)> = self
                .known
                .iter()
                .map(|(mac, device)| (mac.clone(), device.last_seen_us))
                .collect();
            by_last_seen.sort_by_key(|(_, last_seen)| *last_seen);
            let overflow = self.known.len() - MAX_TRACKED_DEVICES;
            for (mac, _) in by_last_seen.into_iter().take(overflow) {
                self.known.remove(&mac);
            }
        }
    }

    fn snapshot(&self) -> Vec<LanDeviceInfo> {
        self.known.values().cloned().collect()
    }
}

/// Haupt-Schleife: scannt periodisch, aktualisiert den Bestand und
/// veröffentlicht ihn im geteilten Zustand. Läuft nicht an, wenn
/// `config.enabled = false` (Aufrufer in `main.rs` prüft das bereits, hier
/// nochmal als Bollwerk gegen versehentliches Verdrahten ohne die Prüfung).
pub async fn run_lan_devices(config: LanDevicesConfig, state: Arc<SharedState>) {
    if !config.enabled {
        return;
    }
    let mut registry = DeviceRegistry::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(config.scan_interval_secs.max(1)));

    loop {
        ticker.tick().await;

        if config.ping_sweep_enabled {
            run_ping_sweep(&config.ping_sweep_subnet).await;
        }

        let output = match run_ip_neigh().await {
            Ok(output) => output,
            Err(err) => {
                tracing::warn!(
                    fehler = %err,
                    "`ip neigh` fehlgeschlagen, LAN-Geräte-Scan übersprungen"
                );
                continue;
            }
        };

        let now = now_us();
        for (ip, mac) in parse_ip_neigh(&output) {
            let display_name = resolve_display_name(&ip).await;
            registry.observe(mac, ip, display_name, now);
        }
        registry.prune(now);
        state.publish_lan_devices(registry.snapshot());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ip_neigh_liest_ip_und_mac_aus_typischer_zeile() {
        let output = "192.168.178.20 dev eth0 lladdr aa:bb:cc:dd:ee:ff REACHABLE\n\
                       192.168.178.21 dev eth0 lladdr 11:22:33:44:55:66 STALE\n";
        let parsed = parse_ip_neigh(output);
        assert_eq!(
            parsed,
            vec![
                ("192.168.178.20".to_string(), "aa:bb:cc:dd:ee:ff".to_string()),
                ("192.168.178.21".to_string(), "11:22:33:44:55:66".to_string()),
            ]
        );
    }

    #[test]
    fn parse_ip_neigh_ueberspringt_zeilen_ohne_lladdr() {
        let output = "192.168.178.30 dev eth0  FAILED\n";
        assert!(parse_ip_neigh(output).is_empty());
    }

    #[test]
    fn parse_ip_neigh_normalisiert_mac_auf_kleinbuchstaben() {
        let output = "192.168.178.20 dev eth0 lladdr AA:BB:CC:DD:EE:FF REACHABLE\n";
        assert_eq!(
            parse_ip_neigh(output),
            vec![("192.168.178.20".to_string(), "aa:bb:cc:dd:ee:ff".to_string())]
        );
    }

    #[test]
    fn registry_haelt_first_seen_stabil_ueber_mehrere_beobachtungen() {
        let mut registry = DeviceRegistry::new();
        registry.observe(
            "aa:bb".to_string(),
            "10.0.0.5".to_string(),
            Some("Handy".to_string()),
            1_000,
        );
        registry.observe(
            "aa:bb".to_string(),
            "10.0.0.6".to_string(), // DHCP-Lease gewechselt
            None,
            2_000,
        );
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].ip, "10.0.0.6");
        assert_eq!(snapshot[0].first_seen_us, 1_000);
        assert_eq!(snapshot[0].last_seen_us, 2_000);
        // Name bleibt erhalten, wenn die zweite Beobachtung keinen liefert.
        assert_eq!(snapshot[0].display_name, "Handy");
    }

    #[test]
    fn prune_entfernt_lange_nicht_gesehene_geraete() {
        let mut registry = DeviceRegistry::new();
        registry.observe("aa:bb".to_string(), "10.0.0.5".to_string(), None, 1_000);
        let far_future = 1_000 + RETENTION.as_micros() as u64 + 1;
        registry.prune(far_future);
        assert!(registry.snapshot().is_empty());
    }

    #[test]
    fn prune_deckelt_auf_max_tracked_devices() {
        let mut registry = DeviceRegistry::new();
        for i in 0..(MAX_TRACKED_DEVICES + 5) {
            registry.observe(format!("mac-{i}"), format!("10.0.0.{i}"), None, i as u64);
        }
        registry.prune((MAX_TRACKED_DEVICES + 5) as u64);
        assert_eq!(registry.snapshot().len(), MAX_TRACKED_DEVICES);
    }
}
