# Model sandbox — structural confinement for agentic model runs

## Purpose

Both models rozum hosts run **agentic loops** that execute file operations and
shell commands on the operator's machine:

- the **main** model (Qwen3.6) — reliable, large;
- the **experimental** model (gpt-oss) — smaller, less stable; we study its
  failure modes and harden the stack around it.

Either can emit a wrong or unsafe action (gpt-oss already does: malformed shell,
`cargo new <subdir>`, the occasional destructive command). They must be confined
so they **cannot do anything harmful — without a stream of approval prompts on
every action**. The safety is **structural** (an OS-enforced jail over a set of
paths), not **interactive** (per-action confirmation). Inside the allowed area
the model has full freedom and zero prompts; outside it, actions are denied by
the kernel, not by asking the human.

This is the confinement layer for `rozum launch <agent>` (Claude Code / Codex /
opencode driven by the local gateway) and for rozum's own `agent.rs` runtime.
Spec dependency: `launch-wrapper.md` (where the child process is spawned),
`portability-and-the-backend-spi.md` (the durable layer this lives in).

## The core model: a sandbox is a SET of `(path, mode)` rules — not one directory

A single dedicated directory is **not enough**: a coding agent must *build*, and
the toolchain (`cargo`/`rustc`/`git`/linker) reads `~/.cargo`, `~/.rustup`, system
libraries, and writes `target/` + `$TMPDIR`. "Free inside one dir" with everything
else denied would fail to compile. So the sandbox is a **set of paths, each with a
mode** — `rw` (read/write/exec), `ro` (read-only), or `deny` — with
**most-specific-path-wins** and a **default of deny** for write/exec.

| Class | Mode | Examples | Why |
|---|---|---|---|
| **Workspace(s)** | rw + exec | the task working dir(s); a scratch dir | full freedom — where the model does its work (may be **several** paths) |
| **Toolchain caches** | rw | `~/.cargo`, `~/.rustup`, the build `target/`, `$TMPDIR` / `/tmp` | builds must work; these are caches/artifacts, not the operator's data |
| **Read-only** | ro | system libs (`/usr`, `/bin`, `/System`, `/Library`), the repo source (optional), cached model snapshots | run tools / reference code without mutating it |
| **Network** | loopback-only | `127.0.0.1:<gateway-port>` | reach the local model gateway; nothing else |
| **Everything else** | deny | `~/.ssh`, `~/.aws`, `~/.config/*` creds, keychains, `/etc`, other projects, the rest of `$HOME` | secrets + the rest of the machine |

A named **profile** is a concrete path set. The v1 profile — **`rust-coding`** —
is the table above rooted at the launch cwd. The multi-path set is also the
**portable abstraction**: the same rules map onto every enforcement backend
(below), so the policy is written once and enforced per-OS.

## No-noise principle: `approval = never`, safety from the jail

The agent child runs with escalation/approval **disabled** (no asking). This is
safe *precisely because* the jail makes harmful actions impossible — there is
nothing to ask about. Bonus: this removes the Codex "rejected escalation → retry
loop" stall (`matrix-failure-analysis.md` Finding 1a), where `approval=never`
*without* a jail made the model loop on denied permission requests. The jail lets
us say "yes to everything in-bounds" safely → fewer stalls, better gpt-oss/codex
reliability. (Synergy, not the primary goal.)

## Enforcement backends — the path-set maps onto each

| Backend | How the `(path, mode)` set is enforced | Status |
|---|---|---|
| **macOS Seatbelt** (`sandbox-exec` + generated SBPL) | `(deny default)` + `(allow file-read* (subpath …))` / `(allow file-write* (subpath …))` / `(allow process-exec* (subpath …))` / `(allow network-outbound (remote ip "localhost:<port>"))` | **primary (M4)** |
| **Linux Landlock / bubblewrap** | a Landlock ruleset per path, or `bwrap` bind-mounts (`--bind` rw, `--ro-bind` ro) | portability (later) |
| **Container** (Docker / Apple `container` / Lima) | each path = a volume mount (`:rw` / `:ro`); strongest isolation; gateway reached via host loopback | max-isolation option (later) |

Seatbelt is a deprecated-but-functional API (Codex itself uses it on macOS); it is
acceptable for v1. The container backend is the bulletproof, cross-platform option
and aligns with the North Star "any hardware" goal — but it is heavier (the agent
runs in the container while the gateway/MLX runs on the host GPU, reached over
loopback), so it is a later phase, not the MVP.

## Config surface

`rozum launch --sandbox[=<profile>] <agent>` / `rozum.toml`:

```toml
[sandbox]
profile     = "rust-coding"        # preset path set (default when --sandbox given bare)
workspace   = [".", "/tmp/scratch"] # rw paths — MAY BE SEVERAL
read_only   = ["../shared-lib"]     # extra ro paths
allow_toolchain = true              # auto-add ~/.cargo, ~/.rustup, target/, $TMPDIR (rw)
network     = "gateway-only"        # "gateway-only" | "none" | "full"
```

- `--sandbox` with no value → the `rust-coding` profile rooted at the launch cwd.
- **ON by default on macOS** (2026-06-19): every `rozum launch` jails the agent to
  its cwd with the `rust-coding` profile, no env needed. `ROZUM_SANDBOX=0` (or empty)
  disables it; `=1` forces the cwd; `=<dir>` jails to <dir>. Off-macOS there is no
  Seatbelt, so it stays OFF until the Linux/container backend (P2/P3) — `rozum launch`
  is never broken by an unavailable jail.
- `rozum launch --no-sandbox` (2026-06-20): CLI sugar for `ROZUM_SANDBOX=0` — the
  flag sets the env so `sandbox_workspace()` stays the single decision point. Works
  after the program name too (`reorder_launch_args` hoists it like the other launch
  flags), but a `--no-sandbox` placed after a `--` separator is passed through to the
  child program unchanged.
- Because the default workspace is the **cwd**, secret dirs (`~/.ssh`, cloud creds,
  keychains) are denied for **both read and write** even when the cwd encompasses
  `$HOME` — the secret denies are emitted last and last-match-wins.

## Threat model — what "wrong" means (and what is out of scope)

In scope (v1): writing/deleting outside the workspace (clobbering the repo, `$HOME`,
other projects); destructive commands (contained — the FS outside is ro/deny);
reading secrets (`~/.ssh`, cloud creds, keychains, token env); network exfiltration
/ fetching untrusted code; privilege escalation (`sudo`, system mutation).

Out of scope (v1): CPU/RAM/disk DoS, kernel exploits, side-channels — these need a
VM/container with resource limits (the container backend, a later phase).

## Plan (spec-dev: this spec first)

- **P0 — this spec.** The path-set model, threat model, the `rust-coding` profile,
  config surface, and the Seatbelt-first/container-later decision.
- **P1 — macOS Seatbelt MVP.** Generate an SBPL profile from the `(path, mode)`
  set; wrap the `rozum launch` child in `sandbox-exec`; run the agent with
  approval=never. *Done when:* inside the workspace the agent creates/edits/runs
  with **no prompts**; a write or exec **outside** is denied; non-loopback network
  is denied; and `cargo build` **succeeds** (the toolchain paths are correct).
- **P2 — harden + Linux.** Pin the exact toolchain path discovery (`cargo` home,
  `rustup` home, `$TMPDIR`, git config) and add the Landlock/bubblewrap backend.
- **P3 — container backend + reliability synergy.** Add the container backend for
  max isolation/portability, and drop the approval-reject path now that the jail
  makes it unnecessary (ties into gpt-oss/codex reliability).

## Decisions (v1 defaults — other variants can be tried later)

Approved 2026-06-19:
- **Workspace = the launch cwd (`rw`), may be several paths.** For the
  experimental/unstable model (gpt-oss) prefer an **ephemeral scratch** dir (clean,
  disposable — as the matrix already does with `/tmp/rozum-agentic-*`); for trusted
  runs the project dir may be the workspace in place.
- **Reference repos = `ro`.** The task workspace is the only `rw` source; any other
  repo the model should *read but not edit* (a shared lib, the rozum source itself)
  is added as a `read_only` path — not `deny` (no forced copy-in).
- **Network = `gateway-only`** (loopback to the model gateway; nothing else).

These are v1 defaults; the config surface above lets the operator pick other
combinations, and we revisit the defaults once P1 is exercised.

## Implementation notes (resolve during P1/P2)

- **Toolchain path discovery** must be robust across machines — derive `~/.cargo`,
  `~/.rustup`, `$TMPDIR`, and the git config from the environment + `cargo`/`rustup`
  introspection, not a hardcoded `$HOME`.
- **Seatbelt is a deprecated API** — fine for v1 (Codex relies on it); if Apple
  removes it, the container backend (P3) is the fallback.
