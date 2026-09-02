//! `logsentry-gui`: reiner Client unter dem Benutzerkonto (niemals root, Regel 7).
//!
//! Verbindet sich in Phase 6/7 über den Unix-Socket mit dem Daemon. Phase 0
//! liefert nur das leere Fenster im reaktiven Repaint-Modus (Regel 20).

use eframe::egui;

/// Minimaler Anwendungszustand für das Phase-0-Gerüst.
struct LogsentryApp {
    /// Menschlich lesbarer Verbindungsstatus, Platzhalter bis Phase 6.
    status_text: String,
}

impl Default for LogsentryApp {
    fn default() -> Self {
        Self {
            status_text: "nicht verbunden (Socket-Client folgt in Phase 6)".to_string(),
        }
    }
}

impl eframe::App for LogsentryApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Reaktiver Modus: kein Continuous-Repaint. Ein niedrigfrequentes
        // request_repaint_after wird erst in Phase 7 zusammen mit dem
        // Live-Snapshot vom Daemon eingeführt (Zielrate 4-10 Hz, Regel 20).
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("logsentry");
            ui.label(&self.status_text);
        });
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "logsentry",
        native_options,
        Box::new(|_cc| Ok(Box::new(LogsentryApp::default()))),
    )
    .map_err(|err| anyhow::anyhow!("eframe konnte nicht gestartet werden: {err}"))?;

    Ok(())
}
