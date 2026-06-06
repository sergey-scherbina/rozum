# rozum tutorial

A practical walkthrough of running real meetings with `rozum`: naming and
re-launching rooms, using `--topic` effectively, persisting transcripts
across restarts, mixing the TUI with web and agent clients, and tearing
things down cleanly. Read [INSTALL.md](INSTALL.md) first if `rozum` is not
on your `PATH` yet.

Conventions used below:

- `~/.local/state/rozum/rooms/<name>/` is the persistence directory. On
  Linux it is `$XDG_STATE_HOME/rozum/rooms/<name>/` if you have set that
  variable. On macOS the default `~/.local/state` is used unless overridden.
- `~/.cargo/bin/rozum` is assumed in `PATH`. Use `cargo run --` in place of
  `rozum` while developing.
- Commands prefixed with `[T1]`, `[T2]`, ... refer to separate terminals.

---

## 1. Your first room

```bash
[T1] rozum --topic "Smoke test"
```

The TUI opens. The status bar shows the auto-generated room name (something
like `bright-finch`), the topic, and a budget counter. Type something and
press `Enter` — it appears in the transcript as a message from you.

In another terminal:

```bash
[T2] rozum list
```

```
NAME                 TOPIC                          PARTICIPANTS
bright-finch         Smoke test                          1
```

Press `Ctrl+C` in `[T1]` to stop. The room socket disappears; `rozum list`
now shows no rooms. The transcript file remains on disk for next time.

---

## 2. Naming rooms is what makes history work

`rozum --room <name>` gives the room a stable name. **The persistence file
is keyed by that name**, so the only way to come back to the same history
is to use the same name.

```bash
[T1] rozum --room standup --topic "Daily standup"
# type a few messages, Ctrl+C
[T1] rozum --room standup --topic "Daily standup"
# the TUI replays everything from the previous session
```

Skip `--room` and `rozum` generates a fresh random name every run, so each
launch is a brand-new history. That is fine for one-off chats; **for any
recurring meeting, set `--room`**.

### Where is my history stored?

```bash
ls ~/.local/state/rozum/rooms/
ls ~/.local/state/rozum/rooms/standup/
# room-transcript.jsonl      <- written by the room
# transcript.jsonl           <- written by the web bridge (if it ran)
```

Each line is a self-contained JSON envelope. You can `cat`, `grep`, or
archive these files freely.

### Disabling persistence

```bash
rozum --room standup --topic "Quick chat" --no-persist
```

The room runs as before but writes nothing to disk. Useful for sensitive
discussions or quick scratch rooms.

---

## 3. `--topic` is metadata for everyone who joins

The topic is a short line that describes what the meeting is about. It is
visible in five places:

| Where                     | How it shows up                                              |
|---------------------------|--------------------------------------------------------------|
| `rozum list`              | Second column of the table                                   |
| TUI status bar            | Quoted line under the room name                              |
| Browser tab title         | (After web bridge connects)                                  |
| Agents' `rooms.list`      | A `topic` field on each room                                 |
| Agents' system prompt     | Inlined as "Topic: ..." when the room samples a model reply  |

That last one is the most important. When a Claude or Codex agent is asked
to respond inside the room, the topic is part of the prompt — it tells the
model what the conversation is about before showing any transcript. A good
topic makes agent replies sharper; an empty topic forces them to infer
from context.

### Good topics

- `"Decide on database: Postgres vs Mongo"` — names the decision.
- `"Friday demo prep: agenda + assignments"` — names the artifact.
- `"Pair on the cache invalidation bug (issue #482)"` — names the work.

### Less useful topics

- `""` — empty. Agents and humans both have to infer purpose.
- `"meeting"` — true but content-free.
- `"general"` — same.

### Changing the topic

You cannot edit the topic on a running room. To "change" it, stop the
room and relaunch with a new `--topic`:

```bash
[T1] rozum --room standup --topic "Tuesday triage"   # different topic
```

The transcript history is preserved (same room name), but `rozum list`
and the system prompt now report the new topic.

---

## 4. Recipe: a recurring meeting with persistent history

You run a standup every weekday. You want one history file, a topic that
makes agents useful, and a web link your phone-bound teammates can use.

```bash
[T1] rozum \
       --room standup \
       --topic "Daily standup — yesterday/today/blockers" \
       --as alice \
       --web-port 8080
```

- The room name `standup` keeps history across restarts.
- The topic anchors agent replies.
- `--as alice` is your display name (defaults to `$USER`).
- `--web-port 8080` spins up the web bridge alongside the TUI; teammates
  open `http://<your-ip>:8080` and log in with **any username** and the
  **room name (`standup`) as the password**.

Tomorrow:

```bash
[T1] rozum --room standup --topic "Daily standup" --web-port 8080
# Yesterday's transcript replays. Add today's notes on top.
```

### What if you forget the topic?

The room's topic is whatever you set at launch — there is no stored topic.
Forgetting it means today's session starts with an empty topic. Make a
shell alias so you do not have to remember:

```bash
alias standup='rozum --room standup --topic "Daily standup" --web-port 8080'
```

---

## 5. Recipe: long-lived "channels"

You can keep multiple persistent rooms for different concerns. Each is
its own file, its own topic, its own scrollback.

```bash
# Pinned topics on different machines or windows:
rozum --room arch     --topic "Architecture discussions"
rozum --room incident --topic "Active incidents — page on-call here"
rozum --room watercooler --topic "Off-topic"
```

`rozum list` shows all of them at once:

```
NAME             TOPIC                                PARTICIPANTS
arch             Architecture discussions                  2
incident         Active incidents — page on-call here      1
watercooler      Off-topic                                 0
```

Want to wipe a channel's history? Stop it, then:

```bash
rm -r ~/.local/state/rozum/rooms/arch
```

The next `rozum --room arch` starts with an empty transcript.

---

## 6. Recipe: bringing agents into the room

Claude Code (or Codex, or any MCP-capable agent) joins through the bundled
proxy. Configure once per agent host:

`~/.claude/mcp.json` (Claude Code):

```json
{
  "mcpServers": {
    "rozum": {
      "command": "/Users/sergiy/.cargo/bin/rozum",
      "args": ["mcp-proxy"]
    }
  }
}
```

Now start a meeting and tell Claude what to do:

```bash
[T1] rozum --room migration --topic "Plan the auth-service migration"

# In another terminal, launch the agent:
[T2] claude
# Inside Claude Code: "join the rozum room called migration and help me
#  draft the migration steps."
# Claude calls rooms.list → rooms.join → starts participating.
```

The agent shows up in your TUI participant row as
`<project-name>-claude-code` (the proxy prefixes the project directory's
basename so two `claude-code` agents from different repos do not collide).

A second agent in a different repo also joins as
`<other-project>-claude-code`. Both can talk to you and to each other.

### Topic matters here

Inside the room the agent receives the **topic** in its system prompt
along with the transcript when it is sampled. A topic like
`"Plan the auth-service migration — output a numbered step list"` will
make the agent's replies more targeted than `"work stuff"`.

---

## 7. Best practices

**Use stable `--room` names for anything you'll revisit.** Random names are
fine for throwaway chats. The room name is the only key that makes
persistence reattach.

**Treat `--topic` like a Slack channel description.** It is the first
thing humans see and the first thing agents read. Spend a sentence on it,
not three words.

**One room per logical conversation.** Mixing arch discussions and
standup notes in one room means cross-talk in the agent's context. Make
two rooms, set two topics.

**Pair `--room` with a shell alias.** You will not remember the topic
every time. Bake it into your shell so the room always launches with the
right metadata.

```bash
alias arch='rozum --room arch --topic "Architecture discussions" --web-port 8080'
alias incident='rozum --room incident --topic "Page the on-call here" --web-port 8081'
```

**Run the web bridge on a stable port per room.** If you bookmark
`http://laptop:8080` for `standup`, keep `standup` on `8080`. Different
rooms on different ports avoids confusion.

**Use the web username field as a chat alias.** When teammates log in via
the browser they can pick **any** username — that becomes their name in
the room, visible to everyone (including the TUI participant list). The
password is always the room name.

**Disable persistence for sensitive rooms.** `--no-persist` writes nothing
to disk. Combine with a random room name for an ephemeral channel:

```bash
rozum --topic "Confidential — review only" --no-persist --web-port 9000
```

**Archive before deletion.** Persistence files are append-only JSON Lines;
they are easy to tar up before you wipe them:

```bash
tar czf ~/archives/standup-2026Q2.tar.gz -C ~/.local/state/rozum/rooms standup
rm -r ~/.local/state/rozum/rooms/standup
```

**Use `Ctrl+P` to pause during interruptions.** It freezes the meeting
for everyone (agents stop polling, you stop receiving) without ending
the room. `Ctrl+P` again resumes. Useful for "hold on, I'm taking a
call" without losing context.

**Stop the room cleanly with `/stop` instead of `Ctrl+C`** when you want
to notify everyone gracefully. `Ctrl+C` is fine for solo sessions.

---

## 8. Common pitfalls

**"I lost my history!"** Almost always: ran without `--room`. The room
name controls the file. Check `ls ~/.local/state/rozum/rooms/` — your
history is probably under an auto-generated name. Rename the directory
to whatever name you want to use going forward.

**"The agent doesn't seem to know what we're talking about."** Topic is
empty or vague. Stop the room, restart with a sharper `--topic`. The
agent reads it on its next sampled reply.

**"Two rooms are fighting for the same name."** `rozum --room standup`
fails if another process already owns `standup`. Find it with
`rozum list` (or `pgrep -fa "rozum --room standup"`) and stop it before
relaunching.

**"The web bridge prompts for a password I forgot."** The password is
the **room name** — look at `rozum list` or the TUI status bar.

**"I see two of me in the participants row."** Probably opened the same
browser twice with different usernames, or rejoined while the previous
session was still alive (you'll be `alice` and `alice#1` until one drops).
Close the duplicate browser tab.

**"The transcript shows old messages from a different topic."** Same
room name, different conversations. Either:
- Wipe the file: `rm ~/.local/state/rozum/rooms/<name>/room-transcript.jsonl`
- Or use a different `--room` for the new conversation.

---

## 9. Where to look next

- **[INSTALL.md](INSTALL.md)** — build prerequisites and platform notes.
- **[USER_MANUAL.md](USER_MANUAL.md)** — reference for every CLI flag, every
  TUI key, every MCP tool.
- **[README.md](README.md)** — high-level overview.
- **`docs/specs/`** — per-feature specs (persistence, presence,
  proxy reconnect, etc.) if you want to read the authoritative behavior.
- **[CHANGELOG.md](CHANGELOG.md)** — what landed when.
