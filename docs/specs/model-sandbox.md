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
| **Container** (Docker) | each writable path = a `-v <p>:<p>:rw` bind; the rest of the host FS is **absent** (no mount ⇒ unreachable, stronger than a deny rule); secrets under a mount masked with `--tmpfs`; gateway reached via `host.docker.internal` | **available** (`ROZUM_SANDBOX_BACKEND=docker`) |

Seatbelt is a deprecated-but-functional API (Codex itself uses it on macOS); it is
acceptable for v1. The container backend is the bulletproof, cross-platform option
and aligns with the North Star "any hardware" goal — but it is heavier (the agent
runs in the container while the gateway/MLX runs on the host GPU, reached over
loopback), so it is opt-in, not the default.

### Docker backend (`ROZUM_SANDBOX_BACKEND=docker`)

Select it per-launch with `ROZUM_SANDBOX_BACKEND=docker` (`container` is an alias).
The same `rust-coding` `(path, mode)` set is rendered to a `docker run` invocation
(`SandboxPolicy::to_docker_run_args`):

- **writable paths** → `-v <host>:<host>:rw` bind mounts; **host path == container
  path** so the workspace/cwd in the agent's args and `-w` line up. The toolchain
  caches (`~/.cargo`, `~/.rustup`, `$TMPDIR`) mount too, so builds reuse the registry
  cache and work offline (if the image lacks a toolchain, the host's is mounted in).
- **everything else** is simply not mounted ⇒ **absent** in the container — stronger
  than Seatbelt's deny (there is no path to reach the operator's data).
- **secrets under a writable mount** (e.g. `~/.ssh` when the workspace is `$HOME`) are
  shadowed by an empty `--tmpfs` so the real secret is unreadable; secrets not under a
  mount need nothing (already absent).
- **gateway** — the agent reaches the host gateway/MLX over loopback, but inside a
  container `127.0.0.1` is the container itself, so rozum points every gateway URL at
  `host.docker.internal` (Docker's host alias, added via `--add-host …:host-gateway`).
  This is the single choke point: `exec_agent`'s `base` URL uses it, so the
  Anthropic/OpenAI base URLs and codex `-c base_url` are all container-correct.
- **env** — only an allowlist (`SANDBOX_FORWARD_ENV`) is forwarded into the container
  via `-e <NAME>`; arbitrary host env does not leak.
- **network caveat** — `gateway-only` under Docker still permits general egress over
  the default bridge (it is best-effort: host reachable + bridge). For strict no-egress
  use `network = "none"`; a firewalled custom network is future hardening.
- **image** — the agent runs in `ROZUM_SANDBOX_DOCKER_IMAGE` (default
  `rozum-agent:latest`); it MUST carry the agent CLI (`claude`/`codex`/`opencode`) on
  `PATH` plus a build toolchain. rozum ships one: **`docker/rozum-agent.Dockerfile`**
  (Rust + git + Node 22 + the three CLIs), built with **`scripts/build-agent-image.sh`**.
  If the image is missing locally, `rozum launch` prints a build hint (it does not
  silently try to pull the unpublished default). Note: the `rust` base exposes cargo via
  `ENV PATH`, but agents often build through a *login* shell (`bash -lc`) that resets
  PATH — the image adds `/etc/profile.d/rust.sh` so `cargo` is found either way.
  `opencode` additionally reads a config file written to a host temp path
  (`OPENCODE_CONFIG`) that is not mounted, so opencode-under-Docker needs that file
  mounted in (future); `claude`/`codex` are env/flag-driven and work as-is.

Validated 2026-06-20 on M4 (Docker 29.6): unit tests on the rendered argv, a real
`docker run busybox` e2e (in-workspace write round-trips to the host; an out-of-mount
write does not; a secret under the mount reads back empty), a container→host
`host.docker.internal` reachability probe, and a full `rozum launch --no-model … docker`
run (the container's stdout surfaced; the env allowlist forwarded `CLAUDE_CODE_*` while a
non-listed host var stayed empty). The `rozum-agent` image was then built and a real
coding workload validated end-to-end: `rozum launch … docker` ran `cargo new` + `cargo
build` + executed the binary **inside the container**, and the build output round-tripped
to the host workspace — the Docker analog of the Seatbelt P1 "cargo build succeeds" gate.

## Config surface

`rozum launch --sandbox[=<profile>] <agent>` / `rozum.toml`:

```toml
[sandbox]
profile     = "rust-coding"        # preset path set (default when --sandbox given bare)
workspace   = [".", "/tmp/scratch"] # rw paths — MAY BE SEVERAL
read_only   = ["../shared-lib"]     # extra ro paths
allow_toolchain = true              # auto-add ~/.cargo, ~/.rustup, target/, $TMPDIR (rw)
network     = "gateway-only"        # "gateway-only" | "none" | "full"
backend     = "seatbelt"            # "seatbelt" (macOS) | "docker" — env: ROZUM_SANDBOX_BACKEND
```

- `--sandbox` with no value → the `rust-coding` profile rooted at the launch cwd.
- **ON by default on macOS** (2026-06-19): every `rozum launch` jails the agent to
  its cwd with the `rust-coding` profile, no env needed. `ROZUM_SANDBOX=0` (or empty)
  disables it; `=1` forces the cwd; `=<dir>` jails to <dir>. The default backend is
  Seatbelt (macOS-only); off macOS the jail stays OFF **unless** the Docker backend is
  selected (`ROZUM_SANDBOX_BACKEND=docker`), which runs on any OS with a docker daemon —
  so `rozum launch` is never broken by an unavailable jail.
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
- **P3 — container backend + reliability synergy.** Docker backend **DONE
  2026-06-20** (`ROZUM_SANDBOX_BACKEND=docker`): the path-set renders to `docker run`
  (bind mounts + tmpfs-masked secrets + `host.docker.internal` gateway + env
  allowlist), validated by unit tests + a real `docker run` e2e + a full `rozum launch`
  run. The **`rozum-agent` image** (`docker/rozum-agent.Dockerfile` +
  `scripts/build-agent-image.sh`) is **DONE 2026-06-20** — a real `cargo build` runs in
  the container jail. Remaining: a firewalled custom network for strict `gateway-only`
  egress; an `opencode` config mount; resource limits (DoS/kernel threats); and dropping
  the approval-reject path now that the jail makes it unnecessary (gpt-oss/codex reliability).

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
