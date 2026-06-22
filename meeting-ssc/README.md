# rozum meeting — pure .ssc → Rust

`meeting.ssc` is the meeting web server written entirely in ScalaScript and
compiled to a self-contained Rust binary via `bin/ssc build-rust`. It reads and
posts to rozum rooms by shelling out to `rozum meetings post` and reading the
daemon's room transcripts under `~/.local/state/rozum/rooms/<room>/`.

Features: dynamic multi-room tabs from the daemon registry/disk rooms, role
colors, local-operator `you` highlighting, per-message timestamps, newest-80
history trimming, live JS polling of a `/m/<room>` message fragment (no
full-page reload — typing is never interrupted), fetch-based posting, PWA
manifest + iOS standalone meta + safe-area mobile layout.

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
    GET  /r/<room>    -> full page for a room discovered at server startup
    GET  /m/<room>    -> #msgs fragment (polled by inline JS)
    POST /p/<room>    -> post message (text/plain body `content=...`)
    GET  /manifest.webmanifest

The current Rust HTTP target registers concrete room routes at process startup
rather than exposing path params to handlers. Restart `rozum-meeting-ssc` after
creating a new room so it appears as a routable tab.
