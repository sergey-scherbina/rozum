//! `rozum meetings attach` — a minimal TUI that attaches to a room in the daemon.
//!
//! This is the thin ratatui shell over [`MeetingClient`] (the tested model). It
//! is additive: bare `rozum` (the legacy in-process room) is untouched; the
//! daemon-client TUI lives behind `meetings attach` until a later cutover. Polish
//! and full picker UX are interactive follow-ups. See
//! `docs/specs/agent-meetings-daemon.md`.

use std::io::Stdout;

use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::daemon::daemon_alive;
use super::room_path::meeting_sock;
use super::tui_client::MeetingClient;

type Term = Terminal<CrosstermBackend<Stdout>>;
type Boxed<U> = Result<U, Box<dyn std::error::Error + Send + Sync>>;

/// Attach a TUI to a room. `room = None` → the cwd project's room.
pub async fn run_attach(room: Option<String>) -> Boxed<()> {
    let sock = meeting_sock();
    if !daemon_alive(&sock).await {
        // `meetings start` spawns the detached daemon and waits for the socket.
        if let Ok(exe) = std::env::current_exe() {
            let _ = tokio::process::Command::new(exe)
                .args(["meetings", "start"])
                .status()
                .await;
        }
    }

    let display_name = std::env::var("USER").unwrap_or_else(|_| "human".into());
    let mut client = MeetingClient::connect(&sock, &display_name).await?;

    // Enter a room: an explicit name, or the cwd project's room.
    match room {
        Some(name) => {
            let rooms = client.list_rooms().await?;
            match rooms.into_iter().find(|r| r.name == name) {
                Some(info) => client.enter_named(&info).await?,
                None => return Err(format!("no such room: {name}").into()),
            }
        }
        None => {
            let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
            client.enter_project(&cwd).await?;
        }
    }

    let mut term = setup_terminal()?;
    let res = event_loop(&mut term, &mut client, &display_name).await;
    restore_terminal(&mut term)?;
    res
}

async fn event_loop(term: &mut Term, client: &mut MeetingClient, me: &str) -> Boxed<()> {
    let mut input = String::new();
    let mut events = EventStream::new();
    draw(term, client, &input)?;

    loop {
        tokio::select! {
            // New messages land in the transcript; redraw.
            polled = client.poll() => {
                if polled.is_ok() {
                    draw(term, client, &input)?;
                }
            }
            // Keyboard.
            maybe_ev = events.next() => {
                let Some(Ok(Event::Key(key))) = maybe_ev else { continue };
                if key.kind != crossterm::event::KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Esc => break,
                    KeyCode::Enter => {
                        let text = std::mem::take(&mut input);
                        if !text.trim().is_empty() {
                            let _ = client.submit(&text).await;
                        }
                    }
                    KeyCode::Backspace => { input.pop(); }
                    KeyCode::PageUp => { client.load_prev_day(); }
                    KeyCode::Char(c) => input.push(c),
                    _ => {}
                }
                draw(term, client, &input)?;
            }
        }
    }
    let _ = me;
    Ok(())
}

fn draw(term: &mut Term, client: &MeetingClient, input: &str) -> Boxed<()> {
    term.draw(|f| {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(3),
            ])
            .split(f.area());

        let header = Paragraph::new(Line::from(vec![Span::styled(
            format!(" rozum · {} ", client.room_name().unwrap_or("(no room)")),
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )]));
        f.render_widget(header, chunks[0]);

        // Transcript: render the tail that fits, with day separators.
        let height = chunks[1].height.saturating_sub(2) as usize;
        let turns = client.transcript();
        let start = turns.len().saturating_sub(height.max(1));
        let mut lines: Vec<Line> = vec![];
        let mut last_date: Option<&str> = None;
        for t in &turns[start..] {
            if last_date != Some(t.date.as_str()) {
                lines.push(Line::from(Span::styled(
                    format!("── {} ──", t.date),
                    Style::default().add_modifier(Modifier::DIM),
                )));
                last_date = Some(t.date.as_str());
            }
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{}: ", t.display_name),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(t.content.clone()),
            ]));
        }
        let transcript = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title("transcript (PgUp: older · Esc: quit)"),
        );
        f.render_widget(transcript, chunks[1]);

        let input_w = Paragraph::new(format!("> {input}")).block(
            Block::default()
                .borders(Borders::ALL)
                .title("message (Enter to send)"),
        );
        f.render_widget(input_w, chunks[2]);
    })?;
    Ok(())
}

fn setup_terminal() -> Boxed<Term> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(term: &mut Term) -> Boxed<()> {
    disable_raw_mode()?;
    crossterm::execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}
