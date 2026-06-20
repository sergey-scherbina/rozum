# Demo doctor

## Overview

`rozum doctor` is a lightweight readiness report for the live demo path: meeting
daemon, shared gateway, sandbox configuration, Docker jail image, web/PWA
endpoint, Tailscale CLI, and the demo launcher. It is intentionally advisory and
non-destructive: it tells the operator what is ready, what is missing, and which
command usually fixes it, without starting models, changing services, downloading
assets, or modifying the workspace.

## Interface

- CLI: `rozum doctor [--web-url <url>] [--strict]`
  - `--web-url <url>` probes the already-running meeting web/PWA endpoint. The
    root page and PWA routes are checked as reachability signals.
  - `--strict` exits non-zero when any warning is present. Without `--strict`,
    only hard failures make the command fail.
- Output: a compact line-oriented report with `ok`, `warn`, `fail`, or `skip`
  status per check, followed by a summary.
- Exit status:
  - `0` when no check failed.
  - `1` when any check failed.
  - `1` under `--strict` when any check warned or failed.

## Behavior

- [x] Reports whether the meeting daemon is reachable and, when reachable, how
      many rooms it can list.
- [x] Reports whether a shared gateway registry exists and whether the registered
      gateway answers its health check.
- [x] Reports the selected sandbox backend/network from env/config precedence and
      warns when the jail is disabled or ineffective on the current platform.
- [x] When Docker is the selected sandbox backend, checks that the Docker CLI is
      available and that the configured `rozum-agent` image exists locally.
- [x] Checks that `scripts/demo-conference.sh` exists and is executable.
- [x] Checks Tailscale CLI availability as an optional phone-demo dependency.
- [x] With `--web-url`, probes the already-running web/PWA endpoint without
      requiring credentials or posting data.
- [x] Never starts/stops daemons, launches models, pulls Docker images, mutates
      service config, writes room messages, or changes files.

## Out of scope

- A full interactive repair command.
- Starting the gateway, meeting daemon, web server, Tailscale serve, or model
  participants.
- Validating model answer quality or running long model loads.
- Replacing the deeper sandbox e2e regression harness.

## Design

The command is implemented as a small Rust module that returns a structured
`DoctorReport`. The CLI renderer is text-only so it is easy to use from a
terminal, a meeting-room paste, or a future CI/preflight script. Checks should
prefer existing project APIs (`meeting::daemon::daemon_alive`, `share::read_active`
and gateway health helpers, sandbox config parsers) over shelling out; external
commands are only used where the dependency itself is a CLI (`docker`,
`tailscale`).

## Results

Implemented 2026-06-20 as `src/doctor.rs` + `rozum doctor [--web-url <url>] [--strict]`.
The first pass is intentionally read-only and advisory: default mode exits non-zero only on
hard failures, while `--strict` treats warnings as a failing preflight.

Verification:
- `cargo test doctor --lib --no-default-features` — 5 passed.
- `cargo test doctor --lib` — 5 passed.
- `cargo build --bin rozum --no-default-features` — passed.
- Live `target/debug/rozum doctor` on the dev machine reported: demo launcher ok,
  Tailscale CLI ok, meeting daemon running with 18 rooms, no active shared gateway
  registry (warn), Seatbelt sandbox enabled with `GatewayOnly`, Docker image skipped
  because Docker backend is not selected, web/PWA skipped because no `--web-url` was supplied.
