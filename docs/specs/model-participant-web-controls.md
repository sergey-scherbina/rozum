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

- [ ] Starting from the web UI launches the existing participant CLI with the
      current room name and the selected model/gateway/policy/persona options.
- [ ] The web server supervises at most one participant child and reports
      running/stopped/exited status without claiming participants started
      outside this web process.
- [ ] Stopping from the web UI terminates only the managed child.
- [ ] Invalid start requests are rejected before spawning a process; a second
      start while running returns `409`.
- [ ] Status includes the visible model/gateway configuration and a best-effort
      gateway probe.
- [ ] Existing chat history, submit, and stream behavior remain unchanged.

## Out of scope

- Managing multiple simultaneous model participants from one web process.
- Discovering or stopping participant processes launched elsewhere.
- Changing `rozum meetings participant` reply logic or gateway protocol.
- Loading model weights or calling the gateway directly from the web server
  except for a lightweight status probe.
