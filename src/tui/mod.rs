use std::sync::Arc;

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use tokio::sync::{Mutex, broadcast};
use tui_textarea::TextArea;

use crate::meeting::participant::ParticipantId;
use crate::meeting::state::{Meeting, MeetingEvent, Phase};

// ── Colors ────────────────────────────────────────────────────────────────────

const HUMAN_COLOR: Color = Color::Rgb(237, 135, 57);
const AI_COLOR: Color = Color::Rgb(130, 207, 255);
const SYSTEM_COLOR: Color = Color::DarkGray;
const MUTED_COLOR: Color = Color::Rgb(90, 90, 90);
const BORDER_COLOR: Color = Color::Rgb(70, 70, 70);
const ACTIVE_COLOR: Color = Color::Rgb(80, 200, 120);

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Clone)]
enum TranscriptEntry {
    Turn { display_name: String, content: String, is_human: bool },
    System(String),
}

#[derive(Default, PartialEq)]
enum InputStyle {
    #[default]
    Normal,
    Interject,
}

struct AppState {
    transcript: Vec<TranscriptEntry>,
    participants: Vec<ParticipantView>,
    moderator_mode: String,
    budget_total: usize,
    budget_max: usize,
    waiting_for: Option<String>,
    waiting_remaining_secs: Option<u64>,
    waiting_timer_stopped: bool,
    transcript_scroll_from_bottom: u16,
    ended: bool,
    room_name: String,
    topic: String,
    tick: u64,
}

#[derive(Clone)]
struct ParticipantView {
    name: String,
    is_human: bool,
    is_active: bool,
    last_poll_age_secs: Option<u64>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run_tui(
    meeting: Arc<Mutex<Meeting>>,
    mut events_rx: broadcast::Receiver<MeetingEvent>,
    _events_tx: broadcast::Sender<MeetingEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = {
        let m = meeting.lock().await;
        AppState {
            transcript: vec![],
            participants: m
                .participants
                .iter()
                .map(|p| ParticipantView {
                    name: p.display_name().to_owned(),
                    is_human: p.is_human(),
                    is_active: m
                        .active_turn
                        .as_ref()
                        .map_or(false, |t| t.participant_id == p.id),
                    last_poll_age_secs: m
                        .participant_liveness
                        .get(&p.id)
                        .and_then(|l| l.last_poll_at)
                        .map(|ts| crate::meeting::state::unix_ts().saturating_sub(ts)),
                })
                .collect(),
            moderator_mode: m.moderator_name.clone(),
            budget_total: 0,
            budget_max: m.budget.max_total_chars,
            waiting_for: None,
            waiting_remaining_secs: None,
            waiting_timer_stopped: false,
            transcript_scroll_from_bottom: 0,
            ended: false,
            room_name: m.name.clone(),
            topic: m.topic.clone(),
            tick: 0,
        }
    };

    let mut input_style = InputStyle::Normal;
    let mut typing_started_turn: Option<u64> = None;
    let mut textarea = TextArea::default();
    apply_textarea_style(&mut textarea, &input_style);

    loop {
        terminal.draw(|f| {
            draw_ui(f, &app, &textarea, &input_style);
        })?;

        while let Ok(evt) = events_rx.try_recv() {
            apply_event(&mut app, evt);
        }

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);

                match key.code {
                    // ── Submit ────────────────────────────────────────────────
                    KeyCode::Enter if !shift => {
                        let text = textarea.lines().join("\n");
                        let text = text.trim().to_owned();
                        let started_turn = typing_started_turn.take();
                        if !text.is_empty() {
                            let mut m = meeting.lock().await;
                            let submitted = if input_style == InputStyle::Interject {
                                m.interject(text);
                                input_style = InputStyle::Normal;
                                true
                            } else {
                                handle_input_text(&text, &mut app, &mut m)
                            };
                            if !submitted {
                                if let Some(turn_id) = started_turn {
                                    if let Some(human_id) = m.human_participant_id() {
                                        let _ = m.resume_active_turn_timeout(
                                            &human_id,
                                            Some(turn_id),
                                        );
                                    }
                                }
                            }
                        } else if let Some(turn_id) = started_turn {
                            let mut m = meeting.lock().await;
                            if let Some(human_id) = m.human_participant_id() {
                                let _ = m.resume_active_turn_timeout(&human_id, Some(turn_id));
                            }
                        }
                        textarea = TextArea::default();
                        apply_textarea_style(&mut textarea, &input_style);
                    }

                    // ── Clear input / exit interject ──────────────────────────
                    KeyCode::Esc => {
                        let empty = textarea.lines().iter().all(|l| l.is_empty());
                        if input_style == InputStyle::Interject {
                            input_style = InputStyle::Normal;
                        }
                        if empty {
                            if let Some(turn_id) = typing_started_turn.take() {
                                let mut m = meeting.lock().await;
                                if let Some(human_id) = m.human_participant_id() {
                                    let _ = m.resume_active_turn_timeout(&human_id, Some(turn_id));
                                }
                            }
                        }
                        textarea = TextArea::default();
                        apply_textarea_style(&mut textarea, &input_style);
                    }

                    // ── Ctrl shortcuts ────────────────────────────────────────
                    KeyCode::Char('c') if ctrl => {
                        meeting.lock().await.end_with_reason("user-quit");
                        break;
                    }
                    KeyCode::Char('n') if ctrl => {
                        meeting.lock().await.skip_turn();
                    }
                    KeyCode::Char('p') if ctrl => {
                        let mut m = meeting.lock().await;
                        if m.phase == Phase::Paused {
                            m.resume();
                        } else {
                            m.pause();
                        }
                    }

                    // ── Transcript scroll ─────────────────────────────────────
                    KeyCode::PageUp => {
                        app.transcript_scroll_from_bottom =
                            app.transcript_scroll_from_bottom.saturating_add(10);
                    }
                    KeyCode::PageDown => {
                        app.transcript_scroll_from_bottom =
                            app.transcript_scroll_from_bottom.saturating_sub(10);
                    }
                    KeyCode::Home if ctrl => {
                        app.transcript_scroll_from_bottom = u16::MAX;
                    }
                    KeyCode::End if ctrl => {
                        app.transcript_scroll_from_bottom = 0;
                    }

                    // ── Forward everything else to the textarea ───────────────
                    _ => {
                        if typing_started_turn.is_none() && is_composing_key(key.code, ctrl) {
                            typing_started_turn = start_human_response(&meeting).await;
                        }
                        textarea.input(key);
                        // Re-apply style (interject toggled via typing '!' prefix)
                        let first_char = textarea.lines().first().and_then(|l| l.chars().next());
                        let want_interject = first_char == Some('!');
                        if want_interject != (input_style == InputStyle::Interject) {
                            input_style = if want_interject {
                                InputStyle::Interject
                            } else {
                                InputStyle::Normal
                            };
                            apply_textarea_style(&mut textarea, &input_style);
                        }
                    }
                }
            }
        }

        app.tick = app.tick.wrapping_add(1);

        {
            let m = meeting.lock().await;
            app.participants = m
                .participants
                .iter()
                .map(|p| ParticipantView {
                    name: p.display_name().to_owned(),
                    is_human: p.is_human(),
                    is_active: m
                        .active_turn
                        .as_ref()
                        .map_or(false, |t| t.participant_id == p.id),
                    last_poll_age_secs: m
                        .participant_liveness
                        .get(&p.id)
                        .and_then(|l| l.last_poll_at)
                        .map(|ts| crate::meeting::state::unix_ts().saturating_sub(ts)),
                })
                .collect();
            app.budget_total = m.budget.total_chars();
            app.budget_max = m.budget.max_total_chars;
            app.moderator_mode = m.moderator_name.clone();
            app.room_name = m.name.clone();
            if let Some(active) = &m.active_turn {
                app.waiting_for = Some(m.display_name_for(&active.participant_id));
                app.waiting_remaining_secs = active
                    .deadline_at
                    .map(|d| d.saturating_sub(crate::meeting::state::unix_ts()));
                app.waiting_timer_stopped =
                    active.deadline_at.is_none() && active.response_started_at.is_some();
            } else if !m.waiting_for_operator {
                app.waiting_for = None;
                app.waiting_remaining_secs = None;
                app.waiting_timer_stopped = false;
            }
            if m.phase == Phase::Ended && !app.ended {
                app.ended = true;
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

// Returns true if the text was consumed as a command (not a regular turn).
fn handle_input_text(text: &str, app: &mut AppState, m: &mut Meeting) -> bool {
    // Strip interject prefix
    let text = if let Some(rest) = text.strip_prefix('!') {
        m.interject(rest.trim().to_owned());
        return true;
    } else {
        text
    };

    if let Some(rest) = text.strip_prefix("/name ") {
        let new_name = rest.trim().to_owned();
        m.rename(new_name.clone());
        app.room_name = new_name;
        true
    } else if let Some(rest) = text.strip_prefix("/mode ") {
        match rest.trim() {
            "round-robin" => m.switch_mode(Box::new(
                crate::meeting::moderator::round_robin::RoundRobin::new(),
            )),
            "manual" => m.switch_mode(Box::new(
                crate::meeting::moderator::manual::Manual::new(),
            )),
            other => app
                .transcript
                .push(TranscriptEntry::System(format!("unknown mode: {other}"))),
        }
        true
    } else if let Some(rest) = text.strip_prefix("/next ") {
        if let Err(e) = m.choose_next_speaker(rest) {
            app.transcript.push(TranscriptEntry::System(e.to_string()));
        }
        true
    } else if let Some(rest) = text.strip_prefix("/kick ") {
        m.kick(&ParticipantId::new(rest.trim()));
        true
    } else if let Some(rest) = text.strip_prefix("/interject ") {
        m.interject(rest.trim().to_owned());
        true
    } else if text == "/skip" {
        m.skip_turn();
        true
    } else if text == "/pause" {
        m.pause();
        true
    } else if text == "/resume" {
        m.resume();
        true
    } else if text == "/stop" {
        m.end();
        true
    } else {
        m.submit_human(text.to_owned());
        false
    }
}

// ── Event handling ────────────────────────────────────────────────────────────

fn apply_event(app: &mut AppState, evt: MeetingEvent) {
    match evt {
        MeetingEvent::TurnAdded { display_name, content, .. } => {
            let is_human = app
                .participants
                .iter()
                .any(|p| p.name == display_name && p.is_human);
            app.transcript.push(TranscriptEntry::Turn { display_name, content, is_human });
            app.waiting_for = None;
            app.waiting_remaining_secs = None;
            app.waiting_timer_stopped = false;
        }
        MeetingEvent::ParticipantJoined { display_name, is_human } => {
            app.transcript
                .push(TranscriptEntry::System(format!("{display_name} joined")));
            app.participants.push(ParticipantView {
                name: display_name,
                is_human,
                is_active: false,
                last_poll_age_secs: None,
            });
        }
        MeetingEvent::ParticipantLeft { display_name } => {
            app.transcript
                .push(TranscriptEntry::System(format!("{display_name} left")));
            app.participants.retain(|p| p.name != display_name);
        }
        MeetingEvent::WaitingFor { display_name, timeout_ms, .. } => {
            app.waiting_for = Some(display_name);
            app.waiting_remaining_secs = Some(timeout_ms / 1000);
            app.waiting_timer_stopped = false;
        }
        MeetingEvent::ModeChanged { new_mode } => {
            app.moderator_mode = new_mode;
        }
        MeetingEvent::BudgetWarning { chars_used } => {
            app.transcript.push(TranscriptEntry::System(format!(
                "budget warning: {chars_used} chars used"
            )));
        }
        MeetingEvent::MeetingEnded { reason } => {
            app.transcript
                .push(TranscriptEntry::System(format!("meeting ended · {reason}")));
            app.ended = true;
            app.waiting_timer_stopped = false;
        }
        MeetingEvent::Renamed { old_name, new_name } => {
            app.room_name = new_name.clone();
            app.transcript.push(TranscriptEntry::System(format!(
                "room renamed  {old_name} → {new_name}"
            )));
        }
    }
}

// ── Drawing ───────────────────────────────────────────────────────────────────

fn draw_ui(
    f: &mut ratatui::Frame,
    app: &AppState,
    textarea: &TextArea,
    input_style: &InputStyle,
) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // status bar
            Constraint::Length(1), // separator
            Constraint::Min(3),    // transcript
            Constraint::Length(1), // separator
            Constraint::Length(3), // input (always visible)
            Constraint::Length(1), // hints
        ])
        .split(area);

    draw_status_bar(f, app, chunks[0]);
    draw_separator(f, chunks[1]);
    draw_transcript(f, app, chunks[2]);
    draw_separator(f, chunks[3]);
    f.render_widget(textarea, chunks[4]);
    draw_hints(f, app, input_style, chunks[5]);
}

fn draw_status_bar(f: &mut ratatui::Frame, app: &AppState, area: Rect) {
    let mut spans: Vec<Span> = vec![
        Span::styled(
            " rozum",
            Style::default().fg(HUMAN_COLOR).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  ", Style::default().fg(MUTED_COLOR)),
        Span::styled(
            app.room_name.clone(),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
    ];

    if !app.topic.is_empty() {
        spans.push(Span::styled("  ·  ", Style::default().fg(MUTED_COLOR)));
        spans.push(Span::styled(
            format!("\"{}\"", app.topic),
            Style::default().fg(Color::Gray),
        ));
    }

    for p in &app.participants {
        spans.push(Span::raw("  "));
        let (color, icon) = participant_chip(p);
        spans.push(Span::styled(
            format!("{icon} {}", p.name),
            Style::default().fg(color),
        ));
    }

    let right = {
        let budget = if app.budget_max == usize::MAX {
            fmt_chars(app.budget_total)
        } else {
            let pct = (app.budget_total * 100)
                .checked_div(app.budget_max)
                .unwrap_or(0);
            format!("{}  {}%", fmt_chars(app.budget_total), pct)
        };
        format!("{}  ·  {} ", app.moderator_mode, budget)
    };
    let left_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let right_width = right.chars().count();
    let pad = (area.width as usize)
        .saturating_sub(left_width)
        .saturating_sub(right_width);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(right, Style::default().fg(MUTED_COLOR)));

    f.render_widget(
        Paragraph::new(Line::from(spans))
            .style(Style::default().bg(Color::Rgb(20, 20, 20))),
        area,
    );
}

fn draw_separator(f: &mut ratatui::Frame, area: Rect) {
    let line = "─".repeat(area.width as usize);
    f.render_widget(
        Paragraph::new(line).style(Style::default().fg(BORDER_COLOR)),
        area,
    );
}

fn draw_transcript(f: &mut ratatui::Frame, app: &AppState, area: Rect) {
    let width = area.width.saturating_sub(1) as usize;
    let mut lines: Vec<Line<'static>> = vec![];

    for entry in &app.transcript {
        match entry {
            TranscriptEntry::Turn { display_name, content, is_human } => {
                let name_color = if *is_human { HUMAN_COLOR } else { AI_COLOR };
                lines.push(Line::from(Span::styled(
                    format!(" {display_name}"),
                    Style::default().fg(name_color).add_modifier(Modifier::BOLD),
                )));
                for wrapped in wrap_words(content, width.saturating_sub(1)) {
                    lines.push(Line::from(format!(" {wrapped}")));
                }
                lines.push(Line::from(""));
            }
            TranscriptEntry::System(msg) => {
                lines.push(Line::from(Span::styled(
                    format!(" ── {msg} ──"),
                    Style::default().fg(SYSTEM_COLOR),
                )));
                lines.push(Line::from(""));
            }
        }
    }

    if !app.ended {
        if let Some(waiting) = &app.waiting_for {
            let spinner = SPINNERS[(app.tick / 2) as usize % SPINNERS.len()];
            let detail = if app.waiting_timer_stopped {
                "  responding".to_owned()
            } else {
                app.waiting_remaining_secs
                    .map(|s| format!("  {s}s"))
                    .unwrap_or_default()
            };
            lines.push(Line::from(Span::styled(
                format!(" {spinner} {waiting}{detail}"),
                Style::default().fg(ACTIVE_COLOR),
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            " ── ended ──",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }

    let height = area.height as usize;
    let max_scroll = lines.len().saturating_sub(height) as u16;
    let scroll =
        max_scroll.saturating_sub(app.transcript_scroll_from_bottom.min(max_scroll));

    f.render_widget(
        Paragraph::new(Text::from(lines)).scroll((scroll, 0)),
        area,
    );
}

fn draw_hints(
    f: &mut ratatui::Frame,
    app: &AppState,
    input_style: &InputStyle,
    area: Rect,
) {
    let text = if app.ended {
        " Ctrl+C  quit".to_owned()
    } else if *input_style == InputStyle::Interject {
        " Enter  interject now  ·  Esc  cancel  ·  (message prefixed with ! bypasses turn order)".to_owned()
    } else {
        " Enter  send  ·  Ctrl+N  skip  ·  Ctrl+P  pause  ·  Ctrl+C  quit  ·  PgUp/Dn  scroll  ·  !msg  interject  ·  /help".to_owned()
    };
    f.render_widget(
        Paragraph::new(text).style(Style::default().fg(MUTED_COLOR)),
        area,
    );
}

// ── TextArea styling ──────────────────────────────────────────────────────────

fn apply_textarea_style(textarea: &mut TextArea, style: &InputStyle) {
    let (border_color, cursor_bg, title) = match style {
        InputStyle::Normal => (
            BORDER_COLOR,
            HUMAN_COLOR,
            Span::styled(" message ", Style::default().fg(MUTED_COLOR)),
        ),
        InputStyle::Interject => (
            Color::Yellow,
            Color::Yellow,
            Span::styled(" ! interject ", Style::default().fg(Color::Yellow)),
        ),
    };
    textarea.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(title)
            .border_style(Style::default().fg(border_color)),
    );
    textarea.set_style(Style::default().fg(Color::White));
    textarea.set_cursor_style(
        Style::default().fg(Color::Black).bg(cursor_bg),
    );
    textarea.set_cursor_line_style(Style::default());
}

// ── Helpers ───────────────────────────────────────────────────────────────────

const SPINNERS: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn participant_chip(p: &ParticipantView) -> (Color, &'static str) {
    if p.is_human {
        (HUMAN_COLOR, "◈")
    } else if p.is_active {
        (ACTIVE_COLOR, "●")
    } else if p.last_poll_age_secs.map_or(false, |a| a <= 30) {
        (AI_COLOR, "○")
    } else {
        (MUTED_COLOR, "○")
    }
}

fn fmt_chars(n: usize) -> String {
    if n >= 1000 {
        format!("{:.1}k chars", n as f64 / 1000.0)
    } else {
        format!("{n} chars")
    }
}

fn wrap_words(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_owned()];
    }
    let mut result: Vec<String> = vec![];
    for source_line in text.lines() {
        let before = result.len();
        let mut current = String::new();
        for word in source_line.split_whitespace() {
            let wlen = word.chars().count();
            let clen = current.chars().count();
            if current.is_empty() {
                if wlen <= max_width {
                    current.push_str(word);
                } else {
                    let mut seg = String::new();
                    for ch in word.chars() {
                        if seg.chars().count() == max_width {
                            result.push(std::mem::take(&mut seg));
                        }
                        seg.push(ch);
                    }
                    current = seg;
                }
            } else if clen + 1 + wlen <= max_width {
                current.push(' ');
                current.push_str(word);
            } else {
                result.push(std::mem::take(&mut current));
                current.push_str(word);
            }
        }
        if !current.is_empty() {
            result.push(current);
        }
        if result.len() == before {
            result.push(String::new());
        }
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

async fn start_human_response(meeting: &Arc<Mutex<Meeting>>) -> Option<u64> {
    let mut m = meeting.lock().await;
    let human_id = m.human_participant_id()?;
    m.start_active_turn_response(&human_id, None).ok()
}

fn is_composing_key(code: KeyCode, ctrl: bool) -> bool {
    if ctrl {
        return false;
    }
    matches!(
        code,
        KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete | KeyCode::Tab
    )
}
