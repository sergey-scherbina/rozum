// rozum-gateway — the outward-facing serving layer (the OpenAI/Anthropic-compatible
// HTTP gateway + the upstream HTTP provider shims). Extracted from the `rozum`
// monolith as part of Phase 3 of the workspace split (docs/specs/workspace-split.md).
// Module names are preserved and the upstream `rozum-core` / `rozum-models` modules
// it uses are re-exported into this crate's root so the moved files'
// `crate::backend::…` / `crate::obs::…` / `crate::models::…` paths resolve unchanged.
// It reads MLX telemetry through `crate::obs` (the rozum-core hook), never the engine.
pub mod anthropic_http;
pub(crate) mod codex_patch;
/// Starting and stopping child processes, on a platform with signals and on one without. Every
/// `libc::kill` / `std::os::unix` in this crate lives here (`docs/specs/windows-spawn-seams.md`).
pub(crate) mod procctl;
/// The browser console: routes, RBAC and the passkey ceremonies. Behind `ucc` because it is the one
/// consumer of `webauthn-rs` → OpenSSL, and a model server does not need a passkey stack to serve
/// models (`docs/specs/ucc-optional.md`). The snapshot it serves lives in `status`, ungated.
#[cfg(feature = "ucc")]
pub mod control;
pub mod status;
pub mod agents;
#[cfg(feature = "ucc")]
pub mod auth;
pub mod auto_context;
pub mod chat;
pub mod coders;
pub mod defaults;
pub mod errors;
pub mod paths;
pub mod private_store;
pub mod projects;
pub mod sessions;
pub mod spawn_support;
pub mod wire_body;
pub mod view_tokens;
pub mod gateway_control;
pub mod matrix;
pub mod messenger;
pub mod serving;
pub mod switchboard;
pub mod gateway;
pub(crate) mod codex_lean;
pub(crate) mod loopbreak;
pub(crate) mod oai_api;
pub(crate) mod anthropic_api;
pub(crate) mod responses_api;
pub mod openai_http;

pub(crate) use rozum_core::{backend, concurrency, obs, share, shed};
pub(crate) use rozum_models::{models, resident};
