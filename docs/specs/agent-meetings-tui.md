# Agent Meetings — TUI

> **⚠️ HISTORICAL — do NOT read this as a description of the TUI, and never as a parity list.**
> Superseded 2026-06-17: the TUI became a client of the dedicated meeting daemon
> (`agent-meetings-daemon.md`) with a room picker (`Ctrl-O` / `/rooms`) and day-scoped rendering.
> **Everything below about moderator modes, turn timeouts, interject, the participants panel and the
> budget panel was REMOVED** — the current client does free-form submit only.
>
> The live behaviour is defined by `crates/rozum-meeting/src/tui/attach.rs` (~312 lines); read that.
> This warning is this strong because the file already misled once: `ucc-meetings-in-tk` was scoped
> against it in 2026-08 and would have rebuilt controls that no longer exist
> (`docs/specs/ucc-meetings-in-tk.md` records the correction).

## Layout

```
┌─ rapid-finch · "X"  (3 participants) ──────┬─ Participants ──────┐
│                                            │ ● sergey    (you)  │
│  sergey:  Давайте сузим до варианта B      │ ● codex     active │
│  codex:   Вот мой первый тезис...         │ ○ claude    wait   │
│  claude:  Согласен с (1), но (2)...       ├─ Moderator ─────────┤
│  sergey:  Уточни пункт (1)                │ round-robin        │
│  (waiting on codex...)                    │ [r]r [m]manual     │
│                                            ├─ Budget ────────────┤
│                                            │ last:  450 chars   │
│                                            │ total: 4.2k / 20k  │
├─ You (sergey) ─────────────────────────────┴────────────────────┤
│ > _                                                             │
└─────────────────────────────────────────────────────────────────┘
 [t]ype [i]nterject [n]skip [space]pause [s]top [r/m]mode [q]quit
```

## Panels

### Transcript (left, scrollable)
- Shows all turns in order: `<display_name>: <content>`
- Human turns shown with `(you)` annotation in Participants; no special markup in transcript (equal participant)
- Status line below last turn: `(waiting on <name>...)` while moderator has chosen next speaker but they haven't submitted yet; `(paused)` when paused; nothing when meeting is ended
- Long messages and long words wrap inside the transcript panel; no horizontal truncation is required to read a turn
- Scroll: `↑↓` / `PgUp PgDn`; auto-scrolls to bottom on new turn unless user has manually scrolled up
- Current implementation must at minimum keep the newest transcript content
  visible by scrolling the transcript viewport to the bottom when content
  exceeds the panel height.

### Participants (top-right)
- One line per participant: `●/○ <name> <status>`
- `●` = active (currently speaking); `○` = waiting
- `(you)` suffix for the human participant
- MCP participants show recent poll age (`poll Ns`) or stale age (`stale Ns`)

### Moderator (mid-right)
- Shows active mode name
- `[r]` and `[m]` hotkeys change mode inline

### Budget (bottom-right)
- `last: N chars` — size of most recent turn
- `total: N / MAX` — chars used / `max_total_chars`; turns red when > 80%

### Input (bottom, full-width)
- Activated by `[t]` (normal turn) or `[i]` (interject)
- Multi-line supported (Enter sends, Shift+Enter newline)
- Shows current mode: `> ` (normal turn) or `! ` (interject)
- `Esc` cancels without sending
- Slash commands parsed here: `/name`, `/mode`, `/kick`, `/pause`, `/resume`, `/stop`

## Hotkeys

| Key | Action |
|---|---|
| `t` | Open input for a normal turn |
| `i` | Open input for an interject turn |
| `space` | Toggle pause/resume |
| `s` | End the meeting (process stays alive) |
| `n` | Skip the current active turn |
| `r` | Switch moderator to round-robin |
| `m` | Switch moderator to manual |
| `q` | End meeting and exit process |
| `↑` / `↓` | Scroll transcript |
| `PgUp` / `PgDn` | Fast-scroll transcript |
| `Esc` (in input) | Cancel input |
| `Enter` (in input) | Send turn |
| `Shift+Enter` (in input) | Insert newline |

## Slash Commands

All parsed in the input panel. `/help` lists them.

| Command | Effect |
|---|---|
| `/name <new>` | Rename the room |
| `/mode round-robin\|manual` | Switch moderator mode |
| `/next <participant>` | Choose the next speaker in manual mode |
| `/kick <name>` | Remove participant |
| `/skip` | Skip the current active turn |
| `/pause` | Pause the meeting |
| `/resume` | Resume the meeting |
| `/stop` | End the meeting |
| `/budget <max_chars>` | Update `max_total_chars` limit |

## Events → TUI Updates

| `MeetingEvent` | Visual effect |
|---|---|
| `TurnAdded { participant, content }` | Append turn to transcript; flash participant row in Participants panel |
| `ParticipantJoined { kind, display_name }` | Add row to Participants; show system line in transcript: `── <name> joined ──` |
| `ParticipantLeft { display_name }` | Grey out row; show `── <name> left ──` |
| `WaitingFor { participant, timeout_ms }` | Update status line at bottom of transcript; mark participant row as active |
| `ModeChanged { new_mode }` | Update Moderator panel |
| `BudgetWarning { chars_used }` | Flash Budget panel; show inline warning in transcript |
| `MeetingEnded { reason }` | Show `── meeting ended: <reason> ──`; disable input |

## Interject Behaviour

When the human sends an interject (`[i]` + text + Enter):
1. Input panel closes.
2. `Meeting::interject(content)` called directly (same process, no IPC).
3. Meeting inserts a human turn with a synthetic `injected` flag into the transcript at the *next* slot (after the current speaker finishes if one is in progress, or immediately if none is active).
4. Moderator resumes normal order after the interject turn.
5. All pending `wait_my_turn` calls that haven't received the interject yet include it in their next `transcript_delta`.

## Turn Timeout Behaviour

For a normal human turn, the countdown measures time until the operator starts
typing a reply. The first composing key in the normal reply box stops the
round-robin timeout. `Esc`, empty submit, or a slash command that leaves the
same turn active resumes the timeout for that turn. Interject input does not
acknowledge the active turn.
