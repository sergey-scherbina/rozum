//! `rozum meetings attach` — the human TUI: a thin ratatui shell over
//! [`MeetingClient`] (the tested model). Messages arrive on a **dedicated poll
//! connection** (`spawn_poll`), so keypresses never cancel an in-flight
//! long-poll. A room picker (`Ctrl-O` / `/rooms`) lists rooms and switches; `/new`
//! creates an ad-hoc room. Rendering is verified interactively. See
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
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::daemon::daemon_alive;
use super::room_path::meeting_sock;
use super::store::StoredTurn;
use super::tui_client::{MeetingClient, RoomInfo};

type Term = Terminal<CrosstermBackend<Stdout>>;
type Boxed<U> = Result<U, Box<dyn std::error::Error + Send + Sync>>;

enum Screen {
    Room,
    Picker { rooms: Vec<RoomInfo>, sel: usize },
}

/// Attach a TUI to a room. `room = None` → the cwd project's room.
pub async fn run_attach(room: Option<String>) -> Boxed<()> {
    let sock = meeting_sock();
    if !daemon_alive(&sock).await {
        if let Ok(exe) = std::env::current_exe() {
            let _ = tokio::process::Command::new(exe)
                .args(["meetings", "start"])
                .status()
                .await;
        }
    }

    // The human's stable local identity → one participant (handle) across launches, not a
    // fresh random one each time. See `super::local_identity`.
    let id = super::local_identity::load_or_create();
    let mut client = MeetingClient::connect_as(&sock, &id.display, &id.token).await?;

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
    let res = event_loop(&mut term, client).await;
    restore_terminal(&mut term)?;
    res
}

async fn event_loop(term: &mut Term, mut client: MeetingClient) -> Boxed<()> {
    let mut transcript: Vec<StoredTurn> = client.transcript().to_vec();
    let mut input = String::new();
    let mut screen = Screen::Room;
    let mut events = EventStream::new();
    let (mut rx, mut poll) = client.spawn_poll();

    loop {
        draw(term, &client, &transcript, &input, &screen)?;
        tokio::select! {
            batch = rx.recv() => {
                if let Some(b) = batch { transcript.extend(b); }
            }
            maybe_ev = events.next() => {
                let Some(Ok(Event::Key(key))) = maybe_ev else { continue };
                if key.kind != crossterm::event::KeyEventKind::Press { continue; }
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match &mut screen {
                    Screen::Room => match key.code {
                        KeyCode::Char('c') if ctrl => break,
                        KeyCode::Char('o') if ctrl => {
                            let rooms = client.list_rooms().await.unwrap_or_default();
                            screen = Screen::Picker { rooms, sel: 0 };
                        }
                        KeyCode::Esc => break,
                        KeyCode::PageUp => {
                            let older = client.prev_day_turns();
                            transcript.splice(0..0, older);
                        }
                        KeyCode::Enter => {
                            let text = std::mem::take(&mut input);
                            match command(&text) {
                                Some(Cmd::Quit) => break,
                                Some(Cmd::Rooms) => {
                                    let rooms = client.list_rooms().await.unwrap_or_default();
                                    screen = Screen::Picker { rooms, sel: 0 };
                                }
                                Some(Cmd::New(topic)) => {
                                    if client.new_room(topic.as_deref()).await.is_ok() {
                                        (transcript, rx, poll) =
                                            reseed(&client, &mut poll).await;
                                    }
                                }
                                None if !text.trim().is_empty() => {
                                    let _ = client.submit(&text).await;
                                }
                                None => {}
                            }
                        }
                        KeyCode::Backspace => { input.pop(); }
                        KeyCode::Char(c) => input.push(c),
                        _ => {}
                    },
                    Screen::Picker { rooms, sel } => match key.code {
                        KeyCode::Char('c') if ctrl => break,
                        KeyCode::Esc => screen = Screen::Room,
                        KeyCode::Up => { *sel = sel.saturating_sub(1); }
                        KeyCode::Down => {
                            if *sel + 1 <= rooms.len() { *sel += 1; }
                        }
                        KeyCode::Enter => {
                            let chosen = rooms.get(*sel).cloned();
                            let ok = match chosen {
                                Some(info) => client.enter_named(&info).await.is_ok(),
                                None => client.new_room(None).await.is_ok(), // last row = new room
                            };
                            if ok {
                                (transcript, rx, poll) = reseed(&client, &mut poll).await;
                            }
                            screen = Screen::Room;
                        }
                        _ => {}
                    },
                }
            }
        }
    }
    poll.abort();
    Ok(())
}

enum Cmd {
    Quit,
    Rooms,
    New(Option<String>),
}

fn command(text: &str) -> Option<Cmd> {
    let t = text.trim();
    if t == "/quit" || t == "/q" {
        Some(Cmd::Quit)
    } else if t == "/rooms" {
        Some(Cmd::Rooms)
    } else if let Some(rest) = t.strip_prefix("/new") {
        let topic = rest.trim();
        Some(Cmd::New(if topic.is_empty() {
            None
        } else {
            Some(topic.to_owned())
        }))
    } else {
        None
    }
}

/// After switching rooms, reseed the transcript from the new room's current day
/// and restart the poll stream on it.
async fn reseed(
    client: &MeetingClient,
    poll: &mut JoinHandle<()>,
) -> (
    Vec<StoredTurn>,
    mpsc::Receiver<Vec<StoredTurn>>,
    JoinHandle<()>,
) {
    poll.abort();
    let transcript = client.transcript().to_vec();
    let (rx, new_poll) = client.spawn_poll();
    (transcript, rx, new_poll)
}

fn draw(
    term: &mut Term,
    client: &MeetingClient,
    transcript: &[StoredTurn],
    input: &str,
    screen: &Screen,
) -> Boxed<()> {
    term.draw(|f| {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(3),
            ])
            .split(f.area());

        let header = Paragraph::new(Line::from(Span::styled(
            format!(" rozum · {} ", client.room_name().unwrap_or("(no room)")),
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
        )));
        f.render_widget(header, chunks[0]);

        match screen {
            Screen::Room => {
                let height = chunks[1].height.saturating_sub(2) as usize;
                let start = transcript.len().saturating_sub(height.max(1));
                let mut lines: Vec<Line> = vec![];
                let mut last_date: Option<&str> = None;
                for t in &transcript[start..] {
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
                let body = Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("transcript (PgUp older · ^O rooms · Esc quit)"),
                );
                f.render_widget(body, chunks[1]);

                let input_w = Paragraph::new(format!("> {input}")).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("message (Enter send · /rooms /new /quit)"),
                );
                f.render_widget(input_w, chunks[2]);
            }
            Screen::Picker { rooms, sel } => {
                let mut items: Vec<ListItem> = rooms
                    .iter()
                    .map(|r| {
                        let proj = r.project.as_deref().unwrap_or("-");
                        ListItem::new(format!(
                            "{}  ·  {}  ·  {} ppl  ·  {}",
                            r.name,
                            if r.topic.is_empty() { proj } else { &r.topic },
                            r.participants,
                            r.last_date.as_deref().unwrap_or("—"),
                        ))
                    })
                    .collect();
                items.push(ListItem::new("[ + new room ]"));
                let mut state = ratatui::widgets::ListState::default();
                state.select(Some((*sel).min(items.len().saturating_sub(1))));
                let list = List::new(items)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("rooms (↑↓ select · Enter open · Esc cancel)"),
                    )
                    .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
                f.render_stateful_widget(list, chunks[1], &mut state);
                f.render_widget(Paragraph::new(""), chunks[2]);
            }
        }
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
