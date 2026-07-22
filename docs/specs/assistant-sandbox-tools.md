# Sandboxed file tools for the model participant

## Overview

The `assistant` room's local model (`rozum meetings participant`, driving the
Telegram/Discord bridges) can be given real file access confined to one
directory. When launched with `--sandbox <dir>`, the participant advertises four
OpenAI tools to the model on every reply and runs the model's tool-calls against
that directory, feeding results back until the model produces a text answer.
Without `--sandbox`, behavior is unchanged: plain chat, no tools.

Spec of the surrounding bridge and its trust model:
`docs/specs/messenger-bridges-daemon.md`.

## Interface

### CLI

```text
rozum meetings participant --model <m> --room <r> [--sandbox <dir>] …
```

`<dir>` is created if absent and canonicalized at startup. If it cannot be
opened, the participant logs the error and runs chat-only (the room stays live).

### Tools advertised to the model

| Tool | Arguments | Effect |
|---|---|---|
| `list_files` | `path` (optional, default `.`) | List entries (name, kind, size) under a sandbox-relative directory. |
| `read_file` | `path` | Return the file's text (output capped, truncation noted). |
| `write_file` | `path`, `content` | Create/overwrite a file; missing parent dirs are created inside the sandbox. |
| `run_command` | `command` | Run `sh -c <command>` with cwd = sandbox root; combined stdout/stderr returned. |

The reply loop runs at most `MAX_TOOL_ROUNDS` (6) tool rounds per message, then
returns the model's text. Tool errors are returned to the model as text, never
crashing the room. `max_tokens` is raised in sandbox mode so the model can emit
file contents.

### System prompt

In sandbox mode the participant appends a note telling the model it has a
working directory and the four tools, so it uses them instead of replying "I
have no filesystem access".

## Confinement and trust

This path is driven by an **untrusted messenger** — the bridge is an explicit
trust boundary (`messenger-bridges-daemon.md`), and the sender allowlist
(`TELEGRAM_ALLOWED_USER_IDS` / `DISCORD_ALLOWED_USER_IDS`) is the only gate on
who can reach these tools.

- `list_files` / `read_file` / `write_file` are **hard-confined** to the sandbox
  root: caller paths must be relative, may not contain `..`, and the resolved
  path (deepest existing ancestor canonicalized) must stay under the root — a
  symlink inside the sandbox pointing outward is rejected.
- `run_command` runs with cwd = root but **cannot be confined to it**: a shell
  inherits the daemon user's full rights. It is bounded only by a timeout and an
  output cap; its real safety boundary is the sender allowlist. Enable
  `--sandbox` on the `assistant` participant only when that allowlist is trusted.

## Decisions

- **Opt-in flag, not always-on.** No `--sandbox` → no tools → unchanged chat.
  The operator turns file access on per deployment (e.g. in the launchd plist).
- **Confine read/write lexically + symlink guard, not shell.** Path tools are
  cheaply and reliably confinable; a shell is not. Rather than pretend
  `run_command` is jailed, the spec states it is only as safe as the allowlist.
- **Errors as tool text, not room crashes.** A bad path or failing command
  returns an error string the model can read and recover from; a gateway hiccup
  still stays silent per the existing bridge contract.

## Results

`cargo test -p rozum-meeting --lib --no-default-features` covers path
confinement (`..`, absolute, symlink escape), read/write round-trip, list, and
`run_command` output capture. Live Telegram E2E is operator-driven (needs a bot
token + allowlist) and validates the model actually invoking the tools.
