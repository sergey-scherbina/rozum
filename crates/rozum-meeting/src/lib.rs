// rozum-meeting — the meeting room: the disk-backed daemon plus its client
// frontends. Extracted from the `rozum` monolith as Phase 1 of the workspace
// split (docs/specs/workspace-split.md); module names are preserved so the
// internal `crate::meeting::…` / `crate::tui::…` paths resolve unchanged and
// the `rozum` crate re-exports these modules under their original paths.
pub mod discord;
pub mod meeting;
pub mod messenger;
pub mod messenger_acl;
pub mod telegram;
pub mod tui;
pub mod web;
