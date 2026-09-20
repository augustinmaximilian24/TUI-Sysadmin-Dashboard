//! Einheitliches Farb- und Stilschema für die GUI.
//!
//! Bündelt Akzent- und Statusfarben sowie kleine wiederverwendbare Widgets
//! (Chips, Level-Badges, Auslastungsbalken, Karten-Rahmen) an einer Stelle,
//! statt Farbliterale über `app.rs` zu verstreuen. Fokus liegt auf dem
//! Dark-Mode (Standard); Light-Mode bleibt nah an `egui::Visuals::light()`,
//! übernimmt aber dieselben Akzentfarben für Wiedererkennbarkeit.

use eframe::egui::{self, Color32, Margin, Rounding, Stroke};

use logsentry_proto::{AnomalyLevel, ConnectionState};

pub const BG_ROOT: Color32 = Color32::from_rgb(16, 17, 21);
pub const BG_PANEL: Color32 = Color32::from_rgb(22, 24, 29);
pub const BG_CARD: Color32 = Color32::from_rgb(28, 30, 36);
pub const BORDER: Color32 = Color32::from_rgb(45, 48, 58);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(150, 156, 168);

pub const ACCENT: Color32 = Color32::from_rgb(96, 165, 250);
pub const OK: Color32 = Color32::from_rgb(74, 222, 128);
pub const LEVEL_INFO: Color32 = Color32::from_rgb(96, 165, 250);
pub const LEVEL_WARN: Color32 = Color32::from_rgb(245, 176, 65);
pub const LEVEL_CRITICAL: Color32 = Color32::from_rgb(239, 83, 80);

const PILL_ROUNDING: Rounding = Rounding::same(999.0);
const CARD_ROUNDING: Rounding = Rounding::same(8.0);

/// Setzt Visuals und Grundabstände für den gegebenen Modus. Wird einmal pro
/// Frame aufgerufen (billig: reines Setzen von Structs, kein I/O).
pub fn apply(ctx: &egui::Context, dark: bool) {
    let mut visuals = if dark {
        dark_visuals()
    } else {
        egui::Visuals::light()
    };
    visuals.hyperlink_color = ACCENT;
    visuals.selection.bg_fill = ACCENT.linear_multiply(0.35);
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.warn_fg_color = LEVEL_WARN;
    visuals.error_fg_color = LEVEL_CRITICAL;
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.window_margin = Margin::same(12.0);
    ctx.set_style(style);
}

fn dark_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG_PANEL;
    v.window_fill = BG_PANEL;
    v.extreme_bg_color = BG_ROOT;
    v.faint_bg_color = BG_CARD;
    v.code_bg_color = BG_CARD;
    v.window_rounding = CARD_ROUNDING;
    v.menu_rounding = CARD_ROUNDING;

    v.widgets.noninteractive.bg_fill = BG_PANEL;
    v.widgets.noninteractive.weak_bg_fill = BG_CARD;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    v.widgets.noninteractive.rounding = CARD_ROUNDING;

    v.widgets.inactive.bg_fill = BG_CARD;
    v.widgets.inactive.weak_bg_fill = BG_CARD;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    v.widgets.inactive.rounding = Rounding::same(6.0);

    v.widgets.hovered.bg_fill = Color32::from_rgb(36, 39, 47);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    v.widgets.hovered.rounding = Rounding::same(6.0);

    v.widgets.active.bg_fill = ACCENT.linear_multiply(0.28);
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    v.widgets.active.rounding = Rounding::same(6.0);

    v
}

/// Farbe für den Verbindungsstatus (Kopfzeile).
pub fn connection_color(state: Option<&ConnectionState>) -> Color32 {
    match state {
        Some(ConnectionState::Connected { .. }) => OK,
        Some(ConnectionState::Connecting { .. }) => LEVEL_WARN,
        Some(ConnectionState::Denied(_)) => LEVEL_CRITICAL,
        Some(ConnectionState::Disconnected { .. }) | None => TEXT_MUTED,
    }
}

pub fn level_color(level: AnomalyLevel) -> Color32 {
    match level {
        AnomalyLevel::Info => LEVEL_INFO,
        AnomalyLevel::Warn => LEVEL_WARN,
        AnomalyLevel::Critical => LEVEL_CRITICAL,
    }
}

fn level_text(level: AnomalyLevel) -> &'static str {
    match level {
        AnomalyLevel::Info => "INFO",
        AnomalyLevel::Warn => "WARN",
        AnomalyLevel::Critical => "CRIT",
    }
}

/// Ein farbiges Status-Pille (z. B. Verbindungsstatus, Lernphase, Replay).
pub fn chip(ui: &mut egui::Ui, text: impl Into<String>, color: Color32) {
    egui::Frame::none()
        .fill(color.linear_multiply(0.14))
        .stroke(Stroke::new(1.0_f32, color))
        .rounding(PILL_ROUNDING)
        .inner_margin(Margin::symmetric(9.0, 3.0))
        .show(ui, |ui| {
            ui.colored_label(color, text.into());
        });
}

/// Eine neutrale, unauffällige Pille für rein informative Kennzahlen
/// (Uptime, Entropie, …) ohne Statusaussage.
pub fn neutral_chip(ui: &mut egui::Ui, text: impl Into<String>) {
    egui::Frame::none()
        .fill(BG_CARD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .rounding(PILL_ROUNDING)
        .inner_margin(Margin::symmetric(9.0, 3.0))
        .show(ui, |ui| {
            ui.label(text.into());
        });
}

/// Kompaktes Level-Badge (z. B. für die Anomalie-Tabelle und das
/// Detail-Panel): farbiger Rahmen mit Kurztext statt reinem Farbtext, damit
/// der Schweregrad auch bei einem schnellen Überflug der Liste auffällt.
pub fn level_badge(ui: &mut egui::Ui, level: AnomalyLevel) {
    let color = level_color(level);
    egui::Frame::none()
        .fill(color.linear_multiply(0.18))
        .stroke(Stroke::new(1.0_f32, color))
        .rounding(Rounding::same(4.0))
        .inner_margin(Margin::symmetric(7.0, 2.0))
        .show(ui, |ui| {
            ui.colored_label(color, level_text(level));
        });
}

/// Farbverlauf grün -> gelb -> rot für Auslastungsbalken, unabhängig davon
/// ob es sich um Prozent (0..1) oder eine andere auf 0..1 normierte Größe
/// handelt (z. B. Temperatur / Referenzwert).
fn usage_color(fraction: f32) -> Color32 {
    if fraction >= 0.9 {
        LEVEL_CRITICAL
    } else if fraction >= 0.75 {
        LEVEL_WARN
    } else {
        OK
    }
}

/// Beschrifteter Auslastungsbalken mit fester Label-Breite, damit mehrere
/// Zeilen (CPU/RAM/Load, Disks, Temperaturen) sauber untereinander
/// ausgerichtet bleiben.
pub fn usage_bar(ui: &mut egui::Ui, label: &str, fraction: f32, value_text: String) {
    ui.horizontal(|ui| {
        ui.add_sized([64.0, 0.0], egui::Label::new(label));
        ui.add(
            egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                .desired_width(150.0)
                .fill(usage_color(fraction))
                .text(value_text),
        );
    });
}

/// Rahmt einen Abschnitt als leicht abgesetzte Karte (Hintergrund + Rand +
/// Innenabstand), damit Systemzustand/Detail-Panel nicht als eine
/// durchgehende Textwand wirken.
pub fn card(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(BG_CARD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .rounding(CARD_ROUNDING)
        .inner_margin(Margin::same(10.0))
        .show(ui, add_contents);
}

/// Kleine Überschrift für einen Abschnitt innerhalb einer Karte oder eines
/// Panels: etwas kleiner als `ui.heading()`, aber deutlich abgesetzt vom
/// Fließtext.
pub fn section_heading(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).strong().size(14.0));
}
