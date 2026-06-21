# Model Participant Web Controls

## Overview

Add a small operator control surface to `rozum meetings web` so a demo can start,
stop, and inspect one live model participant from the browser instead of juggling
terminal commands. The control surface is a web-process supervisor for the
existing `rozum meetings participant` CLI; it does not embed model inference in
the web server and does not change daemon room semantics.

## Interface

The existing Basic-auth gate for `rozum meetings web` covers the new endpoints.

- `GET /api/model/status`
  - Returns whether the web process currently manages a participant child,
    the child pid when running, the last configured model, handle, reply policy,
    gateway URL, peers, persona mode, last exit status, and a best-effort gateway
    reachability probe.
- `POST /api/model/start`
  - Body:
    `{ "model": "...", "as_handle": "...?", "reply_policy": "mention|always|manual", "gateway_url": "...", "peers": "...?", "persona": "...?" }`
  - Starts `rozum meetings participant` for the current web room.
  - Defaults: handle derived from the model, policy `mention`, gateway
    `http://127.0.0.1:8080/v1`, empty peers/persona.
  - If a managed participant is already running, returns `409`.
- `POST /api/model/stop`
  - Stops only the child process started by this web process and returns the new
    status.

The browser UI exposes compact controls for model, handle, gateway URL, reply
policy, peers, persona, start, stop, and status.

## Behavior

- [x] Starting from the web UI launches the existing participant CLI with the
      current room name and the selected model/gateway/policy/persona options.
- [x] The web server supervises at most one participant child and reports
      running/stopped/exited status without claiming participants started
      outside this web process.
- [x] Stopping from the web UI terminates only the managed child.
- [x] Invalid start requests are rejected before spawning a process; a second
      start while running returns `409`.
- [x] Status includes the visible model/gateway configuration and a best-effort
      gateway probe.
- [x] Existing chat history, submit, and stream behavior remain unchanged.

## Out of scope

- Managing multiple simultaneous model participants from one web process.
- Discovering or stopping participant processes launched elsewhere.
- Changing `rozum meetings participant` reply logic or gateway protocol.
- Loading model weights or calling the gateway directly from the web server
  except for a lightweight status probe.

## Results

Implemented in `src/meeting/web.rs` and `src/meeting/web_index.html`.
`rozum meetings web` now exposes `GET /api/model/status`,
`POST /api/model/start`, and `POST /api/model/stop`, plus a compact browser
control panel for model spec, handle, gateway URL, reply policy, peers, and
persona.

Verified with:

- `cargo test meeting::web --no-default-features`
- `cargo build --no-default-features`
- live smoke: temporary `rozum meetings web` with isolated XDG dirs, Basic-auth
  `GET /api/model/status`, `POST /api/model/start` with manual policy and a
  dummy gateway, second start returning `409`, and `POST /api/model/stop`
  terminating the managed child.

Browser-level Playwright smoke was skipped because Playwright is not installed
in this workspace.
