# Meeting identity — clean Human vs Agent principals (+ a roster to find them)

Status: design (sunny-civet, 2026-06-23). Spec-dev; implementation follows. Realizes the **Agent**
side of the `Principal` model already designed in `docs/specs/agent-meeting-coordination.md` §1.

## Problem — the identity is a mess, and it must be put in order

Today everything collapses onto ONE machine-local identity (`local_identity.rs` →
`~/.config/rozum/identity.json`), and the room display is `"$USER · <random-animal>"` for *everyone*.
Agents call `rozum meetings post` without identifying themselves, so they **inherit the operator's
identity** and show up as `Sergiy · plucky-fox` — meaning `plucky-fox` is actually the *human*, and the
real agent (`sunny-civet`) survives only in free-text content. The operator cannot tell who is who.

The fix is NOT another `--as` band-aid. It is to **separate two kinds of identity and never mix them**:

- **The human is identified by account / login** — one stable Human principal per operator.
- **Each agent has its OWN name, assigned ONCE at session start** — a distinct Agent principal,
  intrinsic to the session, used automatically everywhere (no per-post flag, never the human's name).

This is exactly the `Principal { id, kind: Human | Agent, display, auth }` layer the coordination spec
calls "the load-bearing addition" — built here for agents.

## Environment realities (measured) — they shape the implementation

- **`CLAUDE_CODE_SESSION_ID`** is present + stable per agent session → the key that maps a session to
  its Agent principal.
- The friendly handle (`sunny-civet`) is **not** in env (`AI_AGENT=claude-code_2-1-185_agent` is a
  generic label) → it is established once at startup, not scraped.
- **No tty**, and **shell env does not persist between Bash calls** → the agent identity must live on
  **disk keyed by `CLAUDE_CODE_SESSION_ID`**, so every later `post` (a fresh process) resolves it.

## Model

```
Principal (disk):
  Human  — one per operator, display = account/login ($USER). The local_identity, relabeled.
  Agent  — one per session, key = CLAUDE_CODE_SESSION_ID, display = the agent's name,
           established ONCE at session start, kind=Agent.

Resolution for any room action (post / join):
  1. explicit --as / $ROZUM_MEETING_AS            (rare override)
  2. Agent principal registered for $CLAUDE_CODE_SESSION_ID   ← the normal agent path
  3. Human principal (local_identity / $USER)                 ← the operator
  No mixing: an agent session is ALWAYS the agent; a bare shell is ALWAYS the human.
```

### Establishing the Agent principal — once, at startup
`rozum meetings hello [<name>]`, run once per session (from the agent's start hook / first action):
- If no Agent principal exists for this `CLAUDE_CODE_SESSION_ID`, create + persist
  `<state>/principals/agents/<session_id>.json` =
  `{principal_id, kind:"agent", display:<name>, session_id, cwd, project, started, ts}`.
- `<name>` = the name the agent passes (its FleetView name — it knows it, so the room matches what the
  operator already sees); if omitted, **mint** a deterministic adjective-animal from the session id (so
  an agent that forgets still gets a stable, distinct name — never the human's).
- Idempotent: a second `hello` refreshes `ts`/`cwd` but keeps the established name (the name is assigned
  ONCE). Prints the resolved identity. Emits the terminal-title OSC (harmless off-terminal).

### Posting / joining — automatic, intrinsic
`run_meetings_post` (and the proxy join) resolve via the priority above. After one `hello`, every post
in that session shows the agent's name — no `--as`, surviving fresh shells (read from disk by session
id). The human, with no Agent principal, stays the one stable Human principal. **The room display is the
principal's own name** — `sunny-civet` for the agent, `Sergiy` for the human — not `"$USER · animal"`
for all.

### Finding them — `rozum meetings who`
Lists the live principals with locators, so a handle maps to a real session:
```
HANDLE         KIND   LIVE  AGE   CWD / WORKTREE                          LAST
sunny-civet    agent  ●     2m    .worktrees/roster                       working: meeting identity
nimble-raven   agent  ●     8m    .worktrees/safe-multi-model-residency   done: v3 actual-free-RAM
Sergiy         human  ●     —     —                                       (operator)
```
- Liveness by `ts` TTL (15 min, refreshed on `hello`/`post`); `--long` adds `session_id`/`pid`/`started`.
- `LAST` = that principal's most recent room turn (joined by display name once names are consistent).

### `rozum meetings whoami`
Resolves and prints this session's principal — `kind`, `display`, `session_id` — so an agent/operator
can confirm "I act as X (agent)" vs "Sergiy (human)".

## Design decisions (made)

- **Key the Agent principal on `CLAUDE_CODE_SESSION_ID`** — the only stable, present per-session key.
- **Name assigned ONCE at startup**, not per-post; intrinsic afterward (resolved from disk by session).
- **Human = account/login, never the random animal** — drop the `"· <animal>"` mashup so a human never
  looks like an agent. (`local_identity` display becomes the account name; the per-project random handle
  is retired from the human's display.)
- **Disk, daemon-free** — matches "clients read disk directly"; works for CLI-only agents (no proxy).
- **Advisory** — a missing/stale principal never blocks posting; resolution degrades to the human.

## Sequencing

1. Agent-principal store + `hello` (establish once) + `run_meetings_post` resolution (the core fix:
   agents post as themselves, humans as their account, no mixing).
2. `whoami` (confirms #1).
3. `who` (the operator-facing roster + locators + transcript enrichment).
4. Human-display cleanup (drop the random animal; show the account name).
5. Doc: `AGENTS.md` / proxy instructions — run `rozum meetings hello <your-name>` at session start.

## Out of scope / future

Full `Principal` struct + `auth` (auth-backed humans, multi-operator, remote — later rungs); the proxy
join establishing the Agent principal automatically; surfacing the roster in the `.ssc`/TUI control
center (`docs/specs/unified-control-center.md`); operator "ping/locate <handle>".
