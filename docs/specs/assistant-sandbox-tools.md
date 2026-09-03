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
rozum meetings participant --model <m> --room <r> [--sandbox <dir>] [--shell] [--acl <file>] …
```

`<dir>` is created if absent and canonicalized at startup. If it cannot be
opened, the participant logs the error and runs chat-only (the room stays live).
`--shell` additionally offers `run_command`; without it the shell tool is never
advertised (file access does not imply shell access). `--acl <file>` gates the
tools per messenger user (see `messenger-access-control.md`).

### Tools advertised to the model

| Tool | Capability | Arguments | Effect |
|---|---|---|---|
| `list_files` | read | `path` (optional, default `.`) | List entries (name, kind, size) under a sandbox-relative directory. |
| `read_file` | read | `path` | Return the file's text (output capped, truncation noted). |
| `write_file` | write | `path`, `content` | Create/overwrite a file; missing parent dirs are created inside the sandbox. |
| `run_command` | shell | `command` | Run a shell command confined to the sandbox (see below); combined stdout/stderr returned. |

Which tools are advertised for a given reply is `read`/`write`/`shell` filtered
by (a) the `--shell` flag for the shell tool and (b) the triggering user's ACL
capabilities. When none apply the model runs as plain chat (no tools).

The reply loop runs at most `MAX_TOOL_ROUNDS` (6) tool rounds per message, then
returns the model's text. Tool errors are returned to the model as text, never
crashing the room. `max_tokens` is raised in tool mode so the model can emit
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
- `run_command` runs under a macOS **seatbelt** profile (`sandbox-exec`): it may
  read the system (needed to load/run binaries) and read+write inside the root,
  but every write, delete, or rename **outside** the root is denied. `HOME` and
  `TMPDIR` are redirected into the sandbox so tool dotfiles/tempfiles stay inside.
  Reads outside the root are **not** blocked — restricting them aborts dyld's
  shared-cache mapping on modern macOS, so it is deliberately out of scope; the
  shell can read but not modify the wider system. If `sandbox-exec` is missing,
  `run_command` refuses rather than running unconfined. Bounded by a timeout and
  an output cap.
- **Network** access from `run_command` is **allowed by default** and can be
  denied per participant with `--shell-no-network`. Write confinement to the
  sandbox holds either way.
- Who may trigger each tool is gated per messenger user by the ACL
  (`messenger-access-control.md`): `shell` is off unless both `--shell` is set
  and the user holds the `shell` capability.

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


## Project retrieval (2026-09-03)

`--rag-project <DIR>` adds a fifth tool, `rag_search`, over that project's RAG index — the same
retrieval the MCP surfaces serve, through the same ranking policy. It is deliberately NOT part
of the sandbox: the sandbox *confines* a directory, this *reads* a different tree, and folding
one into the other would let a sandbox grant widen quietly into "can search the source".

It is gated twice and needs both — the operator's flag, and the same per-user `read` capability
that governs file tools — and it requires no sandbox at all. Full reasoning, including why the
advertised name is `rag_search` and why the system prompt had to change, is in
`rag-expose-to-agents.md` § "The third surface".
