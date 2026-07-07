# UCC Action JSON Bodies

## Overview

The UCC browser app uses ScalaScript `formBody(...)` for write actions. It emits a flat JSON
object body at click time, but does not set `Content-Type: application/json`. Control-serve
must accept that browser-native body shape for launch/stop/project actions instead of relying
on Axum's `Json<T>` extractor, which rejects such requests before route handlers run.

## Interface

The following existing endpoints keep their paths and logical JSON fields:

- `POST /control/agent/launch`: `{ "model", "room", "policy"?, "persona"?, "handle"? }`
- `POST /control/agent/stop`: `{ "id" }` or legacy plain id text
- `POST /control/coder/launch`: `{ "agent", "model", "workdir", "prompt" }`
- `POST /control/coder/stop`: `{ "id" }` or legacy plain id text
- `POST /control/session/launch`: `{ "agent", "model", "workdir", "prompt"? }`
- `POST /control/session/stop`: `{ "id" }` or legacy plain id text
- `POST /control/project/add`: `{ "name" }` or legacy form/plain-compatible body

Request parsing is content-type tolerant: a valid JSON object body must be accepted even when
the HTTP `Content-Type` header is absent or generic.

## Behavior

- [x] UCC `formBody(...)` POST bodies reach the control handlers and are parsed as JSON objects
      without requiring `Content-Type: application/json`.
- [x] Interactive session launch accepts `agent`, `model`, `workdir`, and optional `prompt` from
      the browser body, then follows the existing admission gate and tmux launch path.
- [x] Stop endpoints accept browser JSON `{ "id": "..." }` and retain backwards-compatible plain
      id text for scripts.
- [x] Project creation accepts browser JSON `{ "name": "..." }` and retains the existing validation.
- [x] Malformed or missing required fields still return structured 400 errors instead of launching
      anything.

## Results

- Implemented in `crates/rozum-gateway/src/control.rs` (`e451e6a`, `0a537df`).
- Regression coverage: `cargo test -p rozum-gateway ucc_` and
  `cargo test -p rozum-gateway control::tests::`.
- Deployed with `clients/control/deploy-ucc-web.sh` on 2026-07-07; SPA/runtime checks passed and
  control-serve reloaded on port 8411.
- Smoke: unauthenticated `POST /control/session/launch` with the ScalaScript-style JSON body now returns
  the expected auth 401, showing the request reaches middleware instead of being rejected by Axum's
  content-type-sensitive JSON extractor.

## Out of scope

- Changing ScalaScript's global `fetchActionWith` runtime.
- Adding a visible frontend error toast for failed control actions.
- Changing model admission, tmux terminal attach, or `rozum launch` behavior.
