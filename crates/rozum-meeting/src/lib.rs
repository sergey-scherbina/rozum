// rozum-meeting — the meeting room: the disk-backed daemon plus its client
// frontends. Extracted from the `rozum` monolith as Phase 1 of the workspace
// split (docs/specs/workspace-split.md); module names are preserved so the
// internal `crate::meeting::…` / `crate::tui::…` paths resolve unchanged and
// the `rozum` crate re-exports these modules under their original paths.
pub mod meeting;
pub mod tui;
pub mod web;
pub mod discord;
pub mod telegram;
