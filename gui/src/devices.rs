//! Geräte-Tab (Phase-12-Erweiterung, Option "nur Anwesenheit"): Liste der
//! per ARP im Heimnetz erkannten Geräte (siehe `daemon/src/lan_devices.rs`).
//! Reine Anwesenheitsanzeige -- keine Ziel-Verbindungen. Ein Fritz!Box-
//! Paketmitschnitt dafür wurde ausprobiert und wieder verworfen (siehe
//! Commit-Historie 2026-09-29/30: die dauerhafte Vollspiegelung des
//! LAN-Verkehrs drückte auf einer echten Box den Durchsatz von ~100 auf
//! ~22 Mbit/s).

use std::sync::{Arc, Mutex, PoisonError};

use eframe::egui;
use logsentry_proto::LanDeviceInfo;

use crate::client::GuiState;

/// Ab wann "vor N Minuten/Stunden" statt "gerade eben" angezeigt wird.
const JUST_SEEN_THRESHOLD_SECS: u64 = 60;

/// Formatiert eine Mikrosekunden-Zeitstempeldifferenz relativ zu jetzt.
/// Reine Funktion (kein `SystemTime::now()`-Aufruf hier), daher ohne
/// Zeit-Mocking testbar.
fn format_relative(last_seen_us: u64, now_us: u64) -> String {
    let elapsed_secs = now_us.saturating_sub(last_seen_us) / 1_000_000;
    if elapsed_secs < JUST_SEEN_THRESHOLD_SECS {
        "gerade eben".to_string()
    } else if elapsed_secs < 3600 {
        format!("vor {} Minute(n)", elapsed_secs / 60)
    } else if elapsed_secs < 86_400 {
        format!("vor {} Stunde(n)", elapsed_secs / 3600)
    } else {
        format!("vor {} Tag(en)", elapsed_secs / 86_400)
    }
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Zustand des Geräte-Tabs: hält nur einen Klon des geteilten GUI-Zustands,
/// derselbe wie die Haupt-Daemon-Verbindung (`gui/src/client.rs`) --
/// keine eigene zweite Verbindung zum Daemon nötig.
pub struct DevicesTab {
    state: Arc<Mutex<GuiState>>,
}

impl DevicesTab {
    pub fn new(state: Arc<Mutex<GuiState>>) -> Self {
        Self { state }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        let mut devices: Vec<LanDeviceInfo> = {
            let guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            guard.lan_devices.clone()
        };
        devices.sort_by(|a, b| a.display_name.cmp(&b.display_name));

        ui.heading("Geräte im Heimnetz");
        ui.label(
            egui::RichText::new(format!("{} bekannte(s) Gerät(e)", devices.len()))
                .color(crate::theme::TEXT_MUTED),
        );
        ui.add_space(8.0);

        if devices.is_empty() {
            ui.label(
                egui::RichText::new(
                    "Noch keine Geräte erkannt (warte auf den nächsten ARP-Scan) …",
                )
                .color(crate::theme::TEXT_MUTED),
            );
            return;
        }

        let now = now_us();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for device in &devices {
                crate::theme::card(ui, |ui| {
                    crate::theme::section_heading(ui, &device.display_name);
                    ui.label(format!("IP: {}", device.ip));
                    ui.label(
                        egui::RichText::new(format!("MAC: {}", device.mac))
                            .color(crate::theme::TEXT_MUTED)
                            .size(11.0),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "zuletzt gesehen: {}",
                            format_relative(device.last_seen_us, now)
                        ))
                        .color(crate::theme::TEXT_MUTED)
                        .size(11.0),
                    );
                });
                ui.add_space(6.0);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ein plausibler Mikrosekunden-Unix-Zeitstempel, komfortabel größer als
    /// jeder in diesen Tests abgezogene Versatz.
    const NOW_US: u64 = 2_000_000_000_000_000;

    #[test]
    fn format_relative_zeigt_gerade_eben_innerhalb_der_schwelle() {
        assert_eq!(
            format_relative(NOW_US - 10_000_000, NOW_US),
            "gerade eben"
        );
    }

    #[test]
    fn format_relative_zeigt_minuten() {
        let five_minutes_us = 5 * 60 * 1_000_000;
        assert_eq!(
            format_relative(NOW_US - five_minutes_us, NOW_US),
            "vor 5 Minute(n)"
        );
    }

    #[test]
    fn format_relative_zeigt_stunden() {
        let two_hours_us = 2 * 3600 * 1_000_000;
        assert_eq!(
            format_relative(NOW_US - two_hours_us, NOW_US),
            "vor 2 Stunde(n)"
        );
    }

    #[test]
    fn format_relative_zeigt_tage() {
        let three_days_us = 3 * 86_400 * 1_000_000;
        assert_eq!(
            format_relative(NOW_US - three_days_us, NOW_US),
            "vor 3 Tag(en)"
        );
    }
}
