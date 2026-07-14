//! ratatui 视图与键盘交互；不执行协议，只派发状态机动作。

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::state::{ConsoleAction, ConsoleState, platform_variant_name};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    Platforms,
    Sensors,
}

#[derive(Debug)]
pub(crate) struct UiState {
    pub(crate) focus: Focus,
    pub(crate) show_help: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            focus: Focus::Platforms,
            show_help: false,
        }
    }
}

/// 返回 `true` 表示用户请求退出。
pub(crate) fn handle_key(key: KeyEvent, state: &mut ConsoleState, ui: &mut UiState) -> bool {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return false;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc if !ui.show_help => return true,
        KeyCode::Char('?') => ui.show_help = !ui.show_help,
        KeyCode::Esc => ui.show_help = false,
        KeyCode::Tab => {
            ui.focus = match ui.focus {
                Focus::Platforms => Focus::Sensors,
                Focus::Sensors => Focus::Platforms,
            };
        }
        KeyCode::Up | KeyCode::Char('k') if !ui.show_help => move_selection(state, ui.focus, -1),
        KeyCode::Down | KeyCode::Char('j') if !ui.show_help => move_selection(state, ui.focus, 1),
        KeyCode::Char('d') if !ui.show_help => state.perform(ConsoleAction::Dump),
        KeyCode::Char('r') if !ui.show_help => state.perform(ConsoleAction::StreamRecord),
        KeyCode::Char('c') if !ui.show_help => state.perform(ConsoleAction::SshCapture),
        KeyCode::Char('f') if !ui.show_help => state.perform(ConsoleAction::RemoteFetch),
        KeyCode::Char('w') if !ui.show_help => state.perform(ConsoleAction::RemoteWatch),
        KeyCode::Char('x') if !ui.show_help => state.perform(ConsoleAction::CancelAll),
        _ => {}
    }
    false
}

fn move_selection(state: &mut ConsoleState, focus: Focus, delta: isize) {
    match focus {
        Focus::Platforms => {
            let count = state.profile_count();
            if count == 0 {
                return;
            }
            let current = state.selected_profile_index.unwrap_or(0);
            let next = offset_index(current, count, delta);
            state.select_platform_index(next);
        }
        Focus::Sensors => {
            let count = state.sensor_choices.len();
            if count == 0 {
                return;
            }
            let next = offset_index(state.selected_sensor_index, count, delta);
            state.select_sensor_index(next);
        }
    }
}

fn offset_index(current: usize, count: usize, delta: isize) -> usize {
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize).min(count - 1)
    }
}

pub(crate) fn render(frame: &mut Frame<'_>, state: &ConsoleState, ui: &UiState) {
    let root = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(14),
            Constraint::Length(4),
        ])
        .split(root);
    render_header(frame, state, rows[0]);
    render_body(frame, state, ui, rows[1]);
    render_footer(frame, state, rows[2]);
    if ui.show_help {
        render_help(frame, state, centered_rect(78, 76, root));
    }
}

fn render_header(frame: &mut Frame<'_>, state: &ConsoleState, area: Rect) {
    let title = state.selected_profile().map_or_else(
        || "Camera Toolbox Platform Console — no platform".to_owned(),
        |profile| {
            format!(
                "Camera Toolbox Platform Console — {} ({})",
                profile.display_name,
                platform_variant_name(&profile.config)
            )
        },
    );
    frame.render_widget(
        Paragraph::new(title)
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn render_body(frame: &mut Frame<'_>, state: &ConsoleState, ui: &UiState, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(32), Constraint::Percentage(68)])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(columns[0]);
    render_profiles(frame, state, ui, left[0]);
    render_sensors(frame, state, ui, left[1]);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(38),
            Constraint::Percentage(30),
            Constraint::Percentage(32),
        ])
        .split(columns[1]);
    render_capabilities(frame, state, right[0]);
    render_jobs_assets(frame, state, right[1]);
    render_event_log(frame, state, right[2]);
}

fn render_profiles(frame: &mut Frame<'_>, state: &ConsoleState, ui: &UiState, area: Rect) {
    let items = state
        .profiles
        .platforms()
        .map(|profile| {
            ListItem::new(format!(
                "{}  [{}]",
                profile.display_name,
                platform_variant_name(&profile.config)
            ))
        })
        .collect::<Vec<_>>();
    let mut list_state = ListState::default().with_selected(state.selected_profile_index);
    let border = focus_color(ui.focus == Focus::Platforms);
    let list = List::new(items)
        .block(
            Block::default()
                .title(" Platform profiles [Tab] ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border)),
        )
        .highlight_symbol("▶ ")
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn render_sensors(frame: &mut Frame<'_>, state: &ConsoleState, ui: &UiState, area: Rect) {
    let items = state
        .sensor_choices
        .iter()
        .map(|choice| ListItem::new(choice.label.clone()))
        .collect::<Vec<_>>();
    let mut list_state = ListState::default().with_selected(Some(state.selected_sensor_index));
    let border = focus_color(ui.focus == Focus::Sensors);
    let list = List::new(items)
        .block(
            Block::default()
                .title(" Sensor / Mode [Tab] ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border)),
        )
        .highlight_symbol("▶ ")
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn focus_color(focused: bool) -> Color {
    if focused { Color::Yellow } else { Color::Gray }
}

fn render_capabilities(frame: &mut Frame<'_>, state: &ConsoleState, area: Rect) {
    let mut lines = Vec::new();
    if let Some(snapshot) = state.snapshot.as_deref() {
        lines.push(Line::from(vec![
            Span::styled("Resolved: ", Style::default().fg(Color::Green)),
            Span::raw(format!(
                "platform={} sensor={:?}",
                snapshot.key.platform_id, snapshot.key.sensor
            )),
        ]));
        lines.push(Line::raw(format!(
            "aggregate={}…",
            &snapshot.aggregate_hash.to_hex()[..16]
        )));
    }
    lines.extend(state.capability_lines().into_iter().map(Line::raw));
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(" Resolved capabilities / evidence ")
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_jobs_assets(frame: &mut Frame<'_>, state: &ConsoleState, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(area);
    let jobs = state
        .jobs
        .iter()
        .rev()
        .map(|(id, status)| ListItem::new(format!("{id}: {status}")))
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(jobs).block(
            Block::default()
                .title(format!(" Jobs ({}) ", state.jobs.len()))
                .borders(Borders::ALL),
        ),
        columns[0],
    );
    let assets = state
        .assets
        .iter()
        .rev()
        .map(|id| ListItem::new(id.as_str().to_owned()))
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(assets).block(
            Block::default()
                .title(format!(" Assets ({}) ", state.assets.len()))
                .borders(Borders::ALL),
        ),
        columns[1],
    );
}

fn render_event_log(frame: &mut Frame<'_>, state: &ConsoleState, area: Rect) {
    let events = state
        .event_log
        .iter()
        .rev()
        .map(|line| Line::raw(line.clone()))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(events).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(" Typed controller event log ")
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, state: &ConsoleState, area: Rect) {
    let status = state
        .error
        .as_deref()
        .or(state.startup_message.as_deref())
        .unwrap_or("Ready; actions are gated by resolved typed handles and explicit arguments.");
    let color = if state.error.is_some() {
        Color::Red
    } else if state.startup_message.is_some() {
        Color::Yellow
    } else {
        Color::Gray
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(
                "↑/↓ select  Tab pane  d Dump  r Record  c Capture  f Fetch  w Watch  x Cancel",
            ),
            Line::from(vec![
                Span::raw("? Help  q Quit  | "),
                Span::styled(status, Style::default().fg(color)),
            ]),
        ])
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn render_help(frame: &mut Frame<'_>, state: &ConsoleState, area: Rect) {
    let mut lines = vec![
        Line::styled(
            "Actionable help",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw("Tab changes focus; ↑/↓ or j/k changes the selected Platform or Sensor/Mode."),
        Line::raw(
            "Sensor changes re-resolve the existing candidate. Platform changes bind a new candidate.",
        ),
        Line::raw("q/Ctrl-C restores the terminal after requesting cancellation and stream close."),
        Line::raw(""),
    ];
    for action in [
        ConsoleAction::Dump,
        ConsoleAction::StreamRecord,
        ConsoleAction::SshCapture,
        ConsoleAction::RemoteFetch,
        ConsoleAction::RemoteWatch,
        ConsoleAction::CancelAll,
    ] {
        let status = state.action_status(action).map_or_else(
            |reason| format!("disabled — {reason}"),
            |()| "enabled".to_owned(),
        );
        lines.push(Line::raw(format!(
            "[{}] {:24} {status}",
            action.key(),
            action.label()
        )));
    }
    lines.extend([
        Line::raw(""),
        Line::raw("No action accepts shell text. SSH capture uses the registered typed recipe."),
        Line::raw(
            "Stream recording never starts without explicit duration, quota and destinations.",
        ),
        Line::raw(
            "Fetch requires an explicit remote path; watch is rooted by the selected profile.",
        ),
        Line::raw("Press ? or Esc to close help."),
    ]);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(" Help ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        area,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}
