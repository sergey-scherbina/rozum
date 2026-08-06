//! `#[serde(default = "...")]` values for the control API's request shapes.
//!
//! Extracted from `control.rs` (`gw-monolith-decompose`). Four one-line functions is a small module
//! and it earns its place: `default_tail` was the last thing `matrix.rs` still imported from its
//! parent, and the alternative — letting each module define its own — is how the coder log and the
//! matrix log quietly end up tailing different numbers of lines.
//!
//! `default_roles` deliberately stayed behind: it returns `Vec<UccRole>`, so it belongs to the RBAC
//! types, not here.

pub(crate) fn default_policy() -> String { "mention".into() }
pub(crate) fn default_tail() -> usize { 120 }
pub(crate) fn default_true() -> bool { true }
pub(crate) fn default_scrollback() -> usize { 3000 }
