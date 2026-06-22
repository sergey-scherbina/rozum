# clients/

Client frontends for the rozum **meeting daemon** (`src/meeting/`). The daemon is the
server (Rust, part of the rozum binary); these are separately-built clients that talk to it.

- **`meeting/`** — the meeting web client: a Progressive Web App authored in ScalaScript
  (`meeting.ssc`) and compiled to a standalone Rust binary via the `scalascript` toolchain
  (not part of the rozum cargo crate). It is the only meeting web (the old hand-written
  `src/meeting/web.rs` was removed). Build + run: see `meeting/README.md`.
