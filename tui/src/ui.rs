//! Rendering für den TUI-Client: reine Darstellungslogik, liest nur aus
//! `App`, verändert nichts (Eingaben verarbeitet `main.rs`).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

use logsentry_proto::{AnomalyLevel, ConnectionState};

use crate::app::App;

pub fn draw(frame: &mut Frame, app: &App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_header(frame, app, root[0]);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(root[1]);

    draw_anomaly_table(frame, app, main[0]);
    draw_detail_and_system(frame, app, main[1]);

    draw_footer(frame, app, root[2]);
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let host = app.hostname.as_deref().unwrap_or("–");
    let connection = connection_label(app.connection.as_ref());
    let mut spans = vec![
        Span::styled(format!(" logsentry · {host} "), Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" │ "),
        Span::raw(connection),
    ];

    if let Some(snapshot) = &app.snapshot {
        spans.push(Span::raw(" │ "));
        spans.push(Span::raw(format!("Uptime {}s", snapshot.daemon_uptime_secs)));
        spans.push(Span::raw(" │ "));
        spans.push(Span::raw(format!("Entropie {:.2} bit", snapshot.window.entropy_bits)));
        spans.push(Span::raw(" │ "));
        spans.push(Span::raw(format!("Verworfen {}", snapshot.stats.dropped_overflow)));
        spans.push(Span::raw(" │ "));
        if snapshot.learning.active {
            spans.push(Span::styled("Lernphase", Style::default().fg(Color::Yellow)));
        } else {
            spans.push(Span::raw("Lernphase beendet"));
        }
        if snapshot.replay {
            spans.push(Span::raw(" │ "));
            spans.push(Span::styled("Replay", Style::default().fg(Color::Cyan)));
        }
    }

    let paragraph = Paragraph::new(Line::from(spans)).block(Block::default().borders(Borders::ALL));
    frame.render_widget(paragraph, area);
}

fn draw_anomaly_table(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.anomalies.iter().enumerate().map(|(i, a)| {
        let color = match a.level {
            AnomalyLevel::Info => Color::LightBlue,
            AnomalyLevel::Warn => Color::Yellow,
            AnomalyLevel::Critical => Color::LightRed,
        };
        let style = if i == app.selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        Row::new(vec![
            Cell::from(format!("{:?}", a.level)).style(Style::default().fg(color)),
            Cell::from(a.unit.clone().unwrap_or_else(|| "–".to_string())),
            Cell::from(format!("{:.2}", a.breakdown.combined)),
            Cell::from(a.template_text.clone()),
        ])
        .style(style)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(20),
            Constraint::Length(6),
            Constraint::Min(10),
        ],
    )
    .header(Row::new(vec!["Level", "Unit", "Score", "Template"]).style(Style::default().add_modifier(Modifier::BOLD)))
    .block(Block::default().borders(Borders::ALL).title(format!(" Anomalien ({}) ", app.anomalies.len())));

    frame.render_widget(table, area);
}

fn draw_detail_and_system(frame: &mut Frame, app: &App, area: Rect) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    let detail_text = match app.selected_anomaly() {
        None => "Keine Anomalie ausgewählt.".to_string(),
        Some(a) => format!(
            "ID {}\nUnit: {}\nPID: {}\nLevel: {:?}\n\nTemplate:\n{}\n\nBeispielzeile:\n{}\n\nScore-Aufschlüsselung:\nGesamt: {:.3}\nRate-Z: {:.2} ({:?})\nSurprisal: {:.2} bit\nEntropie-Z: {:.2}",
            a.id,
            a.unit.as_deref().unwrap_or("–"),
            a.pid.map_or("–".to_string(), |p| p.to_string()),
            a.level,
            a.template_text,
            a.sample_message,
            a.breakdown.combined,
            a.breakdown.rate_z,
            a.breakdown.rate_source,
            a.breakdown.surprisal_bits,
            a.breakdown.entropy_z,
        ),
    };
    let detail = Paragraph::new(detail_text)
        .block(Block::default().borders(Borders::ALL).title(" Detail "))
        .wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(detail, parts[0]);

    let system_text = match app.snapshot.as_ref().and_then(|s| s.system.as_ref()) {
        None => "noch keine Messung".to_string(),
        Some(system) => {
            let mut text = format!(
                "CPU {:.1} %\nRAM {:.1} %\nLoad {:.2} / {:.2} / {:.2}",
                system.cpu.global_usage_percent,
                system.memory.used_percent,
                system.load.one,
                system.load.five,
                system.load.fifteen,
            );
            for unit in &system.units {
                text.push_str(&format!("\n{} — {}/{}", unit.name, unit.active_state, unit.sub_state));
            }
            text
        }
    };
    let system = Paragraph::new(system_text)
        .block(Block::default().borders(Borders::ALL).title(" Systemzustand "));
    frame.render_widget(system, parts[1]);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let text = app
        .status
        .clone()
        .unwrap_or_else(|| "↑/k ↓/j navigieren · q/Esc beenden".to_string());
    frame.render_widget(Paragraph::new(text), area);
}

fn connection_label(state: Option<&ConnectionState>) -> String {
    match state {
        None => "verbinde …".to_string(),
        Some(ConnectionState::Connecting { attempt }) if *attempt == 0 => "verbinde …".to_string(),
        Some(ConnectionState::Connecting { attempt }) => format!("verbinde … (Versuch {attempt})"),
        Some(ConnectionState::Connected { session_id }) => format!("verbunden (Sitzung {session_id})"),
        Some(ConnectionState::Denied(err)) => format!("verweigert: {err}"),
        Some(ConnectionState::Disconnected {
            retry_in,
            last_error,
        }) => {
            format!(
                "getrennt ({last_error}), neuer Versuch in {:.0}s",
                retry_in.as_secs_f64()
            )
        }
    }
}
