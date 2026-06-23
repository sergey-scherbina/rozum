# Meeting mention-inbox — make "→ handle" a real, durable delivery

Status: design (sunny-civet, 2026-06-23). Spec-dev; implementation follows in the same branch.

## Problem

Addressing a sibling in the room — `-> plucky-fox`, `@nimble-raven` — is **convention, not delivery**.
The room is a broadcast log; there is no targeted notification. An agent learns it was addressed only
when it next *re-reads* the room (pull). The push ladder (`rozum-native-channels`: Tier-1 channel /
Tier-2 `wait_my_turn` / Tier-3 piggyback) is built and correct, but:

- **It is dormant unless the target has a live interactive `claude` session + connected `daemon_proxy`.**
  Verified 2026-06-23: with no proxy connected, my posts landed in **0** piggyback drops — not pushed to
  anyone; siblings in CLI/matrix mode coordinate purely by re-reading the room.
- **A message posted while the target has no proxy is lost to push entirely** — piggyback is written by
  the (ephemeral) proxy, so "no proxy at post time" = "no drop". It survives only in the room transcript.
- **No "this concerns YOU" signal** — even a re-reading agent must scan and notice it was addressed; a
  CLI-only agent (no proxy at all) has nothing targeted to check.

Net: cross-agent handoffs are best-effort and rely entirely on the pull discipline. We want addressing
an agent to be a **first-class, durable, offline-surviving** delivery, without weakening the room as the
source of truth.

## Design decisions (made, not deferred — operator: "do as you think best, you'll use it")

1. **The workhorse is `addresses(content, own_handle)` — each consumer checks its OWN real handle.**
   Patterns: `@<handle>` or `->` + optional space + `<handle>`, with a trailing word boundary (so
   `-> plucky-fox` matches but `-> plucky-foxtrot` does not) and `@` at a word boundary. The CLI takes
   `--as <handle>` (the agent knows its own handle) and the wakeup pusher uses the proxy's own handle;
   both pass a real, distinctive kebab handle, for which the room's technical prose (`-> undeclared`,
   `-> opt-in`) yields no false positives.
   - **Live-data caveat (verified 2026-06-23):** `display_name` is NOT a reliable handle source in this
     room — agents post under a shared local identity (`"Sergiy · plucky-fox"`) and self-identify in the
     *content* (`"working (sunny-civet): …"`, `"nimble-raven -> …"`). So a hard "known-handles" gate
     derived from `handle_of(display_name)` would wrongly reject a legitimate `--as sunny-civet`. We
     therefore do NOT gate the CLI on known handles; `addresses(own_handle)` is sufficient and correct.
   - `known_handles(turns)` / `mentions(content, known)` remain as a secondary helper (for a future
     daemon path that wants every addressee of a turn), filtering to a *supplied* handle set — useful
     only where a trustworthy handle set exists (e.g. the daemon's participant registry, not
     display-name scraping).
2. **The inbox is a cursor-based VIEW over the transcript, not a second message store.** The room
   transcript stays the single durable record. "My inbox" = transcript turns that mention my handle and
   sit past my per-handle *seen cursor*. This means it is durable and offline-surviving for free (cursor
   + transcript are both on disk), with zero duplication and no consistency problem.
3. **Reading advances the cursor (delivery tracking), non-destructively.** `meetings inbox` shows unread
   mentions and advances the seen cursor to the latest shown. `--peek` shows without advancing; `--all`
   ignores the cursor. Because the room is still the durable record, a "read-then-forgot" mention is
   never truly lost — it is always re-findable with `--all` or by re-reading the room. So a cursor, not
   an explicit destructive ack, is the right weight.
4. **Per-room scope** (the project room the agent is in). Cross-room aggregation is a future extension.
5. **Detection lives in a shared `meeting::mention` module** so the CLI uses it now and the
   `daemon_proxy` wakeup pusher uses the same logic for the `mentioned` flag later — one source of truth
   for "what counts as addressing X".

## Architecture

```
room transcript (durable, broadcast)  ── store::read_since ──┐
                                                             ▼
meeting::mention::mentions(content, &known_handles) -> Vec<handle>   (pure, tested)
                                                             │
        ┌────────────────────────────────────────────────────┼─────────────────────────────┐
        ▼                                                    ▼                               ▼
  CLI: `rozum meetings inbox --as <h>`              daemon_proxy wakeup pusher        seen-cursor
  (turns mentioning h, past cursor[h];              (Tier-1 channel + Tier-3          <room>/.inbox/<h>.json
   advances cursor)                                  piggyback): set meta.mentioned    = {date, n}
                                                      when delta addresses the recipient
```

## Deliverables

### A. `meeting::mention` — detection core (do first; pure + tested)
`crates/rozum-meeting/src/meeting/mention.rs`:
- `fn known_handles(turns: &[StoredTurn]) -> BTreeSet<String>` — distinct `handle_of(display_name)`.
- `fn handle_of(display_name: &str) -> &str` — part after `" · "` (else the whole name).
- `fn mentions(content: &str, known: &BTreeSet<String>) -> Vec<String>` — handles addressed by this
  content (`@h` / `-> h`, boundary-checked, only known handles).
- `fn addresses(content: &str, handle: &str) -> bool` — does this content address `handle`?
- Unit tests: real-room positives (`-> sunny-civet`, `@plucky-fox`), the false-positive corpus
  (`-> undeclared`, `-> opt-in`, `-> runtime` → none), boundary (`-> plucky-foxtrot` ≠ `plucky-fox`),
  case-insensitivity, multiple mentions in one turn.

### B. `rozum meetings inbox` — the CLI (gap C; uses A)
`MeetingsAction::Inbox { room, as_handle, peek, all, count }` → `run_meetings_inbox`:
- Resolve room root (reuse `run_meetings_read`'s resolver).
- `store::read_since(root, None, 0)`; filter `addresses(turn.content, handle)`.
- Load cursor `<root>/.inbox/<handle>.json` (`{date,n}`); show mentions strictly after it (unless
  `--all`); format `[HH:MM] <from-handle>: <content>` (reuse the `meetings read` formatter).
- Advance cursor to the latest shown (unless `--peek`). Empty inbox → "no new messages for <handle>".
- `--as <handle>` required (the agent knows its own handle), mirroring `meetings post --as`.

### C. wakeup `mentioned` flag (gap A/B in the push path; follow-up task)
In `daemon_proxy.rs` `ensure_wakeup_task` (the disk-tailing pusher): when a delta turn `addresses` the
proxy's own handle, set `meta.mentioned = true` (and `your_turn`) on the `notifications/claude/channel`
event; have the Tier-3 piggyback append prefix such a line (e.g. `‹for you›`). The `PROXY_INSTRUCTIONS`
already tell the agent to treat a channel event as a wakeup — `mentioned` lets it prioritize. This is
the only piece that touches the delicate proxy path; ship A+B first, then this.

## Sequencing

1. **A** — `meeting::mention` module + tests. Self-contained, pure.
2. **B** — `meetings inbox` CLI + a cursor round-trip test. Directly closes the offline/CLI-only gap
   (an agent with no proxy can still see "addressed to me, unread"). Ship A+B together.
3. **C** — `mentioned` flag in the wakeup pusher. Separate, smaller, touches `daemon_proxy`.

## Out of scope / future

Cross-room inbox aggregation; explicit ack vs cursor; mention of a not-yet-present handle (someone who
never posted — no known handle); `@everyone`/broadcast tags; surfacing the inbox in the `.ssc`/TUI
control center (folds into `docs/specs/unified-control-center.md` once that exists).
