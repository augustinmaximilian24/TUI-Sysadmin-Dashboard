//! Einheitliches Farb- und Stilschema für die GUI: „Holographic HUD".
//!
//! Glas-Karten mit leuchtenden Eck-Klammern (wie ein Sci-Fi-HUD), runde
//! Ring-Gauges für Auslastungswerte statt reiner Balken, elektro-blaue
//! Akzentfarbe. Hintergrund bewusst neutral dunkelgrau/schwarz statt
//! blaustichig -- nur Akzente (Rahmen, Klammern, Gauges, Text) sind blau.
//! Fokus liegt auf dem Dark-Mode (Standard); Light-Mode bleibt nah an
//! `egui::Visuals::light()`, übernimmt aber dieselbe Akzentfarbe.

use eframe::egui::{self, Color32, FontId, Margin, Rounding, Shadow, Stroke};

use logsentry_proto::{AnomalyLevel, ConnectionState};

pub const BG_ROOT: Color32 = Color32::from_rgb(9, 9, 10);
pub const BG_PANEL: Color32 = Color32::from_rgb(14, 14, 16);
pub const BG_CARD: Color32 = Color32::from_rgb(20, 21, 24);
pub const BORDER: Color32 = Color32::from_rgb(40, 41, 46);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(148, 154, 163);

pub const ACCENT: Color32 = Color32::from_rgb(61, 220, 255);
pub const OK: Color32 = Color32::from_rgb(77, 255, 176);
pub const LEVEL_INFO: Color32 = Color32::from_rgb(61, 220, 255);
pub const LEVEL_WARN: Color32 = Color32::from_rgb(255, 194, 77);
pub const LEVEL_CRITICAL: Color32 = Color32::from_rgb(255, 93, 122);

const PILL_ROUNDING: Rounding = Rounding::same(999.0);
const CARD_ROUNDING: Rounding = Rounding::same(6.0);

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
    v.panel_fill = BG_ROOT;
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
    v.widgets.inactive.rounding = Rounding::same(4.0);

    v.widgets.hovered.bg_fill = Color32::from_rgb(28, 30, 34);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    v.widgets.hovered.rounding = Rounding::same(4.0);

    v.widgets.active.bg_fill = ACCENT.linear_multiply(0.28);
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    v.widgets.active.rounding = Rounding::same(4.0);

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
        .fill(color.linear_multiply(0.12))
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
        .fill(color.linear_multiply(0.16))
        .stroke(Stroke::new(1.0_f32, color))
        .rounding(Rounding::same(4.0))
        .inner_margin(Margin::symmetric(7.0, 2.0))
        .show(ui, |ui| {
            ui.colored_label(color, level_text(level));
        });
}

/// Farbverlauf grün -> gelb -> rot für Auslastungsanzeigen, unabhängig davon
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
/// Zeilen (Temperaturen, Disks, Score) sauber untereinander ausgerichtet
/// bleiben. Für CPU/RAM/Load im Systemzustand siehe stattdessen
/// [`radial_gauge`].
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

/// Runder Ring-Gauge im HUD-Stil (Hintergrundring + farbiger Bogen +
/// zentrierter Wert + Beschriftung darunter), z. B. für CPU/RAM/Load.
pub fn radial_gauge(ui: &mut egui::Ui, label: &str, fraction: f32, value_text: &str) {
    let size = 76.0;
    let stroke_width = 6.0;
    ui.vertical_centered(|ui| {
        let (rect, _response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        let center = rect.center();
        let radius = size / 2.0 - stroke_width;

        painter.circle_stroke(center, radius, Stroke::new(stroke_width, BORDER));

        let frac = fraction.clamp(0.0, 1.0);
        if frac > 0.0 {
            let color = usage_color(frac);
            let start = -std::f32::consts::FRAC_PI_2;
            let end = start + frac * std::f32::consts::TAU;
            let steps = ((frac * 48.0).ceil() as usize).max(1);
            let points: Vec<egui::Pos2> = (0..=steps)
                .map(|i| {
                    let t = start + (end - start) * (i as f32 / steps as f32);
                    center + egui::vec2(radius * t.cos(), radius * t.sin())
                })
                .collect();
            painter.add(egui::Shape::line(points, Stroke::new(stroke_width, color)));
        }

        painter.text(
            center,
            egui::Align2::CENTER_CENTER,
            value_text,
            FontId::monospace(14.0),
            egui::Color32::from_rgb(225, 228, 232),
        );

        ui.add_space(2.0);
        ui.label(egui::RichText::new(label).color(TEXT_MUTED).size(11.0));
    });
}

/// Zeichnet vier kurze, leuchtende Eck-Klammern über `rect` -- die
/// namensgebende HUD-Anmutung der Karten, statt eines durchgezogenen
/// Rahmens an jeder Kante.
fn corner_brackets(ui: &egui::Ui, rect: egui::Rect, color: Color32) {
    let len = 12.0;
    let stroke = Stroke::new(1.5_f32, color);
    let painter = ui.painter();
    for (corner, dx, dy) in [
        (rect.left_top(), 1.0, 1.0),
        (rect.right_top(), -1.0, 1.0),
        (rect.left_bottom(), 1.0, -1.0),
        (rect.right_bottom(), -1.0, -1.0),
    ] {
        painter.line_segment([corner, corner + egui::vec2(len * dx, 0.0)], stroke);
        painter.line_segment([corner, corner + egui::vec2(0.0, len * dy)], stroke);
    }
}

/// Rahmt einen Abschnitt als Glas-Karte mit leuchtenden Eck-Klammern und
/// einem dezenten Akzent-Glow, damit Systemzustand/Detail-Panel nicht als
/// eine durchgehende Textwand wirken.
pub fn card(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    let response = egui::Frame::none()
        .fill(BG_CARD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .rounding(CARD_ROUNDING)
        .inner_margin(Margin::same(12.0))
        .shadow(Shadow {
            offset: egui::vec2(0.0, 0.0),
            blur: 20.0,
            spread: 0.0,
            color: ACCENT.linear_multiply(0.10),
        })
        .show(ui, add_contents)
        .response;
    corner_brackets(ui, response.rect, ACCENT);
}

/// Kleine Überschrift für einen Abschnitt innerhalb einer Karte oder eines
/// Panels: etwas kleiner als `ui.heading()`, aber deutlich abgesetzt vom
/// Fließtext.
pub fn section_heading(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).strong().size(13.0).color(ACCENT));
}
