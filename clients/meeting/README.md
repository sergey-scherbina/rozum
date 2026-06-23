# rozum meeting — pure .ssc → Rust

`meeting.ssc` is the meeting web server written entirely in ScalaScript and
compiled to a self-contained Rust binary via `bin/ssc build-rust`. It reads and
posts to rozum rooms by shelling out to `rozum meetings post` and reading the
daemon's current room registry from `rozum meetings status`.

Features: dynamic multi-room selector from the daemon registry/disk rooms, role
colors, local-operator `you` highlighting, per-message timestamps, newest-80
history trimming, live JS polling of a `/m/<room>` message fragment (no
full-page reload — typing is never interrupted), fetch-based posting, PWA
manifest + iOS standalone meta + safe-area mobile layout, plus a `/manage`
panel for rooms, installed models, gateway state/model switching, and
model-participant start/stop.

Room transcript lookup:

- project rooms: `<project>/.rozum/room/`, using the `project:` path from
  `rozum meetings status`;
- ad-hoc/global rooms: `$XDG_STATE_HOME/rozum/rooms/<room>/`, falling back to
  `~/.local/state/rozum/rooms/<room>/`.

Room create/delete/clean-empty and model/gateway controls are local operator
actions. Project rooms are protected from deletion in the web UI.

## Build & run
    ./build.sh                      # -> ~/.local/bin/rozum-meeting-ssc
    rozum-meeting-ssc               # serves on http://127.0.0.1:8405

## Persistent service (launchd)
    cp com.rozum.meeting-ssc.plist ~/Library/LaunchAgents/
    launchctl load ~/Library/LaunchAgents/com.rozum.meeting-ssc.plist

## Expose over Tailscale (HTTPS, secure-context for PWA)
    tailscale --socket=<busi-sock> serve --bg --https=8445 http://127.0.0.1:8405

## Routes
    GET  /            -> page("demo")
    GET  /r/<room>    -> full page for a room
    GET  /m/<room>    -> #msgs fragment (polled by inline JS)
    GET  /mp/<room>   -> model-participant control page for a room
    POST /p           -> post message (text/plain body `room=<room>\ncontent=...`)
    POST /do          -> local management actions
    GET  /manage      -> management panel
    GET  /manifest.webmanifest
    GET  /sw.js
    GET  /icon.svg

Room routes are prefix routes, so newly created/discovered rooms do not need a
server restart. The room selector is rebuilt from the latest `rozum meetings
status` output on render.
