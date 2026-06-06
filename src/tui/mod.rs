use std::collections::HashSet;
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

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Clone)]
enum TranscriptEntry {
    Turn {
        display_name: String,
        content: String,
        is_human: bool,
    },
    System(String),
}

struct AppState {
    transcript: Vec<TranscriptEntry>,
    participants: Vec<ParticipantView>,
    budget_total: usize,
    budget_max: usize,
    transcript_scroll_from_bottom: u16,
    ended: bool,
    room_name: String,
    topic: String,
    web_url: Option<String>,
    responding: HashSet<String>,
    polling: HashSet<String>,
}

#[derive(Clone)]
struct ParticipantView {
    name: String,
    is_human: bool,
    last_poll_age_secs: Option<u64>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run_tui(
    meeting: Arc<Mutex<Meeting>>,
    mut events_rx: broadcast::Receiver<MeetingEvent>,
    _events_tx: broadcast::Sender<MeetingEvent>,
    web_url: Option<String>,
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
                    last_poll_age_secs: m
                        .participant_liveness
                        .get(&p.id)
                        .and_then(|l| l.last_poll_at)
                        .map(|ts| crate::meeting::state::unix_ts().saturating_sub(ts)),
                })
                .collect(),
            budget_total: 0,
            budget_max: m.budget.max_total_chars,
            transcript_scroll_from_bottom: 0,
            ended: false,
            room_name: m.name.clone(),
            topic: m.topic.clone(),
            web_url,
            responding: m
                .active_responding()
                .into_iter()
                .map(|(_, name, _, _)| name)
                .collect(),
            polling: m
                .active_polling()
                .into_iter()
                .map(|(_, name, _, _)| name)
                .collect(),
        }
    };

    let mut textarea = TextArea::default();
    apply_textarea_style(&mut textarea);

    loop {
        terminal.draw(|f| {
            draw_ui(f, &app, &textarea);
        })?;

        while let Ok(evt) = events_rx.try_recv() {
            apply_event(&mut app, evt);
        }

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);

                let alt = key.modifiers.contains(KeyModifiers::ALT);
                match key.code {
                    KeyCode::Enter if !shift && !alt => {
                        let text = textarea.lines().join("\n");
                        let text = text.trim().to_owned();
                        if !text.is_empty() {
                            let mut m = meeting.lock().await;
                            handle_input_text(&text, &mut app, &mut m);
                        }
                        textarea = TextArea::default();
                        apply_textarea_style(&mut textarea);
                    }

                    KeyCode::Esc => {
                        textarea = TextArea::default();
                        apply_textarea_style(&mut textarea);
                    }

                    KeyCode::Char('c') if ctrl => {
                        meeting.lock().await.end_with_reason("user-quit");
                        break;
                    }
                    KeyCode::Char('p') if ctrl => {
                        let mut m = meeting.lock().await;
                        if m.phase == Phase::Paused {
                            m.resume();
                        } else {
                            m.pause();
                        }
                    }

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

                    _ => {
                        textarea.input(key);
                    }
                }
            }
        }

        {
            let m = meeting.lock().await;
            app.participants = m
                .participants
                .iter()
                .map(|p| ParticipantView {
                    name: p.display_name().to_owned(),
                    is_human: p.is_human(),
                    last_poll_age_secs: m
                        .participant_liveness
                        .get(&p.id)
                        .and_then(|l| l.last_poll_at)
                        .map(|ts| crate::meeting::state::unix_ts().saturating_sub(ts)),
                })
                .collect();
            app.budget_total = m.budget.total_chars();
            app.budget_max = m.budget.max_total_chars;
            app.room_name = m.name.clone();
            app.responding = m
                .active_responding()
                .into_iter()
                .map(|(_, name, _, _)| name)
                .collect();
            app.polling = m
                .active_polling()
                .into_iter()
                .map(|(_, name, _, _)| name)
                .collect();
            if m.phase == Phase::Ended && !app.ended {
                app.ended = true;
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

fn handle_input_text(text: &str, app: &mut AppState, m: &mut Meeting) {
    if let Some(rest) = text.strip_prefix("/name ") {
        let new_name = rest.trim().to_owned();
        m.rename(new_name.clone());
        app.room_name = new_name;
    } else if let Some(rest) = text.strip_prefix("/kick ") {
        m.kick(&ParticipantId::new(rest.trim()));
    } else if text == "/pause" {
        m.pause();
    } else if text == "/resume" {
        m.resume();
    } else if text == "/stop" {
        m.end();
    } else {
        m.submit_human(text.to_owned());
    }
}

// ── Event handling ────────────────────────────────────────────────────────────

fn apply_event(app: &mut AppState, evt: MeetingEvent) {
    match evt {
        MeetingEvent::TurnAdded {
            display_name,
            content,
            ..
        } => {
            let is_human = app
                .participants
                .iter()
                .any(|p| p.name == display_name && p.is_human);
            app.transcript.push(TranscriptEntry::Turn {
                display_name,
                content,
                is_human,
            });
        }
        MeetingEvent::ParticipantJoined {
            display_name,
            is_human,
        } => {
            app.transcript
                .push(TranscriptEntry::System(format!("{display_name} joined")));
            app.participants.push(ParticipantView {
                name: display_name,
                is_human,
                last_poll_age_secs: None,
            });
        }
        MeetingEvent::ParticipantLeft { display_name } => {
            app.transcript
                .push(TranscriptEntry::System(format!("{display_name} left")));
            app.participants.retain(|p| p.name != display_name);
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
        }
        MeetingEvent::Renamed { old_name, new_name } => {
            app.room_name = new_name.clone();
            app.transcript.push(TranscriptEntry::System(format!(
                "room renamed  {old_name} → {new_name}"
            )));
        }
        MeetingEvent::RespondingChanged {
            display_name,
            started,
            ..
        } => {
            if started {
                app.responding.insert(display_name);
            } else {
                app.responding.remove(&display_name);
            }
        }
        MeetingEvent::PollingChanged {
            display_name,
            started,
            ..
        } => {
            if started {
                app.polling.insert(display_name);
            } else {
                app.polling.remove(&display_name);
            }
        }
    }
}

// ── Drawing ───────────────────────────────────────────────────────────────────

fn draw_ui(f: &mut ratatui::Frame, app: &AppState, textarea: &TextArea) {
    let area = f.area();

    let input_lines = textarea.lines().len().max(1) as u16;
    let max_input_h = (area.height / 3).max(3);
    // +2 for the rounded border around the textarea block (top + bottom row).
    let input_h = (input_lines + 2).clamp(3, max_input_h);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),       // status bar
            Constraint::Length(1),       // separator
            Constraint::Min(3),          // transcript
            Constraint::Length(1),       // separator
            Constraint::Length(input_h), // input
            Constraint::Length(1),       // hints
        ])
        .split(area);

    draw_status_bar(f, app, chunks[0]);
    draw_separator(f, chunks[1]);
    draw_transcript(f, app, chunks[2]);
    draw_typing_separator(f, app, chunks[3]);
    f.render_widget(textarea, chunks[4]);
    draw_hints(f, app, chunks[5]);
}

fn draw_typing_separator(f: &mut ratatui::Frame, app: &AppState, area: Rect) {
    // For each participant we show at most one state: typing wins over
    // waiting (an agent that is composing a reply is no longer just polling).
    let mut statuses: Vec<(String, &'static str, Color)> = vec![];
    for name in &app.responding {
        statuses.push((name.clone(), "typing", AI_COLOR));
    }
    for name in &app.polling {
        if !app.responding.contains(name) {
            statuses.push((name.clone(), "waiting", MUTED_COLOR));
        }
    }
    if statuses.is_empty() {
        draw_separator(f, area);
        return;
    }
    statuses.sort_by(|a, b| a.0.cmp(&b.0));

    let mut spans: Vec<Span> = vec![Span::styled("─", Style::default().fg(BORDER_COLOR))];
    let mut text_w: usize = 1;
    for (i, (name, state, color)) in statuses.iter().enumerate() {
        let chunk = format!(" {name} is {state}… ");
        text_w += chunk.chars().count();
        spans.push(Span::styled(chunk, Style::default().fg(*color)));
        if i + 1 < statuses.len() {
            spans.push(Span::styled("·", Style::default().fg(BORDER_COLOR)));
            text_w += 1;
        }
    }
    let trailing = (area.width as usize).saturating_sub(text_w);
    spans.push(Span::styled(
        "─".repeat(trailing),
        Style::default().fg(BORDER_COLOR),
    ));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_status_bar(f: &mut ratatui::Frame, app: &AppState, area: Rect) {
    let mut spans: Vec<Span> = vec![
        Span::styled(
            " rozum",
            Style::default()
                .fg(HUMAN_COLOR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  ", Style::default().fg(MUTED_COLOR)),
        Span::styled(
            app.room_name.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
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

    let budget = if app.budget_max == usize::MAX {
        fmt_chars(app.budget_total)
    } else {
        let pct = (app.budget_total * 100)
            .checked_div(app.budget_max)
            .unwrap_or(0);
        format!("{}  {}%", fmt_chars(app.budget_total), pct)
    };

    let right = if let Some(url) = &app.web_url {
        format!("{}  ·  {} ", url, budget)
    } else {
        format!("{} ", budget)
    };

    let left_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let right_width = right.chars().count();
    let pad = (area.width as usize)
        .saturating_sub(left_width)
        .saturating_sub(right_width);
    spans.push(Span::raw(" ".repeat(pad)));

    let right_color = if app.web_url.is_some() {
        Color::Cyan
    } else {
        MUTED_COLOR
    };
    spans.push(Span::styled(right, Style::default().fg(right_color)));

    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Rgb(20, 20, 20))),
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
            TranscriptEntry::Turn {
                display_name,
                content,
                is_human,
            } => {
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

    if app.ended {
        lines.push(Line::from(Span::styled(
            " ── ended ──",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }

    let height = area.height as usize;
    let max_scroll = lines.len().saturating_sub(height) as u16;
    let scroll = max_scroll.saturating_sub(app.transcript_scroll_from_bottom.min(max_scroll));

    f.render_widget(Paragraph::new(Text::from(lines)).scroll((scroll, 0)), area);
}

fn draw_hints(f: &mut ratatui::Frame, app: &AppState, area: Rect) {
    let text = if app.ended {
        " Ctrl+C  quit".to_owned()
    } else {
        " Enter  send  ·  Ctrl+P  pause  ·  Ctrl+C  quit  ·  PgUp/Dn  scroll  ·  /name /kick /pause /resume /stop".to_owned()
    };
    f.render_widget(
        Paragraph::new(text).style(Style::default().fg(MUTED_COLOR)),
        area,
    );
}

// ── TextArea styling ──────────────────────────────────────────────────────────

fn apply_textarea_style(textarea: &mut TextArea) {
    textarea.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(" message ", Style::default().fg(MUTED_COLOR)))
            .border_style(Style::default().fg(BORDER_COLOR)),
    );
    textarea.set_style(Style::default().fg(Color::White));
    textarea.set_cursor_style(Style::default().fg(Color::Black).bg(HUMAN_COLOR));
    textarea.set_cursor_line_style(Style::default());
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn participant_chip(p: &ParticipantView) -> (Color, &'static str) {
    if p.is_human {
        (HUMAN_COLOR, "◈")
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
