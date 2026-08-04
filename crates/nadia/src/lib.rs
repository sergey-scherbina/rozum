//! nadia — a coding agent as an application.
//!
//! The tiers below it already exist and are not re-derived here:
//!
//! - `rozum-agent` — the loop, tool dispatch, budgets, the transcript (Contracts 2–3 of
//!   `docs/specs/integration.md`).
//! - `rozum-gateway` — the OpenAI-compatible client backend, and, server-side, the
//!   per-family rendering of tool schemas into the syntax a model family was trained on
//!   plus the parsing of the reply back. **nadia never does that**: it emits neutral
//!   tool definitions and lets the gateway own the dialect. A second parser here would
//!   be a second source of truth, and its failures read as model failures.
//!
//! What lives here is what an application owns: the tools, the sandbox they act in, the
//! prompt, the budgets, and the two front-ends.
//!
//! Twin of the ScalaScript implementation in the `nadia` repo (see `REPOS.md`): same
//! spec, same six tools, same safety model. This one is the executable reference.

pub mod approval;
pub mod commands;
pub mod gate;
pub mod mcp;
pub mod sandbox;
pub mod serve;
pub mod session;
pub mod supervisor;
pub mod tools;
