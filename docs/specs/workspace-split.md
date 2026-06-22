# Workspace split — decompose the `rozum` monolith into a layered Cargo workspace

## Overview

`rozum` is today a single ~47K-LOC crate (78 `.rs` files, one `Cargo.toml` with
feature flags). This spec decomposes it into a **Cargo workspace of ~7–8 crates**
along the dependency layers the code *already* has, so each concern (the SPI, the
per-hardware engines, model sourcing, the agent/cascade, the gateway, the meeting
room) is its own crate with an enforced boundary — not a convention. The binary
`rozum` keeps owning the CLI / `launch` orchestration and wires the crates together.

This is the crate-level continuation of `architecture-spi.md` (which made the
*seams* legible at the module level) and realizes the North Star (`SPEC.md`): the
`ChatBackend` SPI is the durable, hardware-agnostic foundation (`rozum-core`); the
engines (`rozum-mlx` / `rozum-gguf` / `rozum-mistralrs` / `rozum-x86`) and
device-aware placement (`rozum-hardware`) sit above it; everything else builds on
that contract. **Behaviour-preserving and green at every phase** — same binary,
same features, same matrix.

## Interface

The contract callers depend on is the **crate graph** (a strict DAG — no cycles
across crate boundaries) and the layering rule: a crate may only `use` crates in a
lower or equal tier.

```
L4  rozum (bin)        main.rs, doctor, launch/CLI wiring, subcommand dispatch
L3  rozum-meeting      meeting daemon + proxy + service + clients (tui/web/discord/telegram)
L3  rozum-gateway      gateway, openai_http, anthropic_http, share
L2  rozum-agent        agent, cascade, router, rag_lite, builtin_tools, memory_store
L1  rozum-mlx          mlx_native_backend, specdecode(+_backend)        [feature mlx-native]
L1  rozum-gguf         gguf                                             [feature gguf]
L1  rozum-mistralrs    mistralrs_backend                                [feature mistralrs]
L1  rozum-x86          x86/                                             [feature x86-native]
L1  rozum-models       models, model_source, hf_hub, modelscope, resident
L1  rozum-hardware     NEW: device detect + placement policy            (North Star)
L0  rozum-core         backend (SPI), concurrency, obs, engine, serving,
                       sampler, constrain, harmony, config(base)
```

Crate-name prefix `rozum-`; the bin stays `rozum`. Feature flags
(`mlx-native`, `gguf`, `mistralrs`, `x86-native`) move to the workspace root and
forward to the engine crates as optional `dep:` — `cargo build` and
`--no-default-features` produce the **same** artifacts as today.

## Behavior
- [ ] `cargo build` (default features) produces a byte-equivalent `rozum` binary; CLI surface unchanged.
- [ ] `cargo build --no-default-features` (the `linux-core` CI seam) builds `rozum-core` + portable crates with no Metal toolchain.
- [ ] Each feature flag (`mlx-native`/`gguf`/`mistralrs`/`x86-native`) toggles exactly its engine crate; matrix unchanged.
- [ ] No crate-boundary dependency cycle (`cargo` enforces this — a cycle fails to resolve).
- [ ] `rozum-meeting` builds and tests **without** any engine crate in its dep tree (proves the daemon↔runtime separation).
- [ ] Every phase leaves `cargo check` + `cargo test` green; no phase requires a behaviour change to land.
- [ ] Lib test count is preserved across each move (tests travel with their module).

## Out of scope
- **Separate repositories** — this is ONE repo, ONE workspace (see Decisions).
- **Plugin-ization** (dylib/WASM/out-of-process) — in-tree crate boundaries only; consistent with `architecture-spi.md`'s "no plugins" decision.
- **Re-abstracting `ChatBackend` / `ToolSource`** — already correct; they move, they don't change.
- **Designing the placement engine itself** — `rozum-hardware` gets its own spec/sprint; here it is only reserved as a crate slot.
- **Splitting the vendored forks** (`.vendor/mlx-lm`, `mistral.rs`) — they stay as pinned git deps via `[patch.crates-io]` at the workspace root.

## Design

### The dependency graph today (the evidence this plan rests on)

Scanned `crate::<module>` imports across all 78 files. The **production** graph is
already a clean DAG flowing into `backend.rs` (the SPI). The few "wrong-way" edges
are **test-only** and do not constrain the split:

- `agent → mlx_native_backend` — `agent.rs:928`, tests start `:538` → **test-only**.
- `router → mlx_native_backend` — `router.rs:603,651`, tests start `:397` → **test-only**.
- `backend → gguf` — `backend.rs:1000` (bottom, test-scoped); in prod `gguf → backend`.

Real edges that need handling (below).

### The 5 knots to untangle (and the fix for each)

1. **Core knot `backend ↔ concurrency ↔ obs`** — mutually recursive (`backend→concurrency`,
   `concurrency→backend,obs`, `obs→backend`). **Keep all three in `rozum-core`.** Not a problem, just don't split them.
2. **`config → cascade`** (`config.rs:19 use crate::cascade::CascadeSpec`, prod) — the only
   ordering inversion: low-level `config` pulls an L2 type. **Fix:** move the cascade-config
   *types* (`CascadeSpec`, `Location`, `RemoteApi`, `StrategyName`) down into `rozum-core::config`,
   leave the cascade *logic* in `rozum-agent`. (Or split `config` into `core::config` base +
   `agent::cascade_config`.)
3. **`gateway → mlx_native_backend`** (`gateway.rs:2778,2787`: `mlx_memory_mb()`, `batch_stats()`,
   prod) — gateway reaches into a concrete engine for telemetry. **Fix:** expose these via an
   SPI hook on `rozum-core` (e.g. `trait BackendStats { fn memory_mb(); fn batch_stats(); }` or
   extend `ChatBackend`), so gateway depends on core, not on `rozum-mlx`.
4. **Test-only `agent/router/backend → mlx/gguf`** — gate or relocate the test helpers
   (`ensure_model_dir`, `MlxNativeBackend` constructions) so the prod dep tree is clean. Lowest effort.
5. **`rozum-hardware` is NEW work, not a move** — placement is implicitly scattered across
   `gateway`/`resident`/`engine` (no `placement`/`hardware` module exists; `device` lives only
   in `x86/device.rs` + `engine.rs`). Reserve the crate; design it in its own spec.

### Phase plan (each phase = one mergeable, green step)

- **Phase 0 — Scaffold.** Convert root to `[workspace]`; create `crates/`; move the SPI
  cluster into `rozum-core` (fix knot 1 by co-locating; knot 2 by moving config types down).
  Bin still builds. *Gate: `cargo check` default + `--no-default-features` green.*
- **Phase 1 — Extract `rozum-meeting`** (proof-of-concept; it has **0 internal deps** → lowest
  risk, highest signal: the daemon stops recompiling when an engine changes). Includes proxy,
  service, and the clients (tui/web/discord/telegram). *Gate: `rozum-meeting` test suite green
  with no engine crate in its tree.*
- **Phase 2 — `rozum-models`** then the engines (`rozum-mlx`/`-gguf`/`-mistralrs`/`-x86`) under
  their feature flags forwarded from the workspace root. *Gate: each feature matrix unchanged.*
- **Phase 3 — `rozum-agent` + `rozum-gateway`** (fix knots 3 & 4 here). *Gate: agentic matrix unchanged.*
- **Phase 4 — `rozum-hardware`** — separate spec; design device detect + placement policy and
  route the gateway/resident placement decisions through it.

### Granularity choice

~7–8 library crates + 1 bin (the per-engine split). Rejected the coarse 4–5 crate
option (engines+models+hardware fused into one `rozum-runtime`): it saves Cargo
plumbing but does **not** enforce the engine boundaries — which is the main point of
the split (an engine change shouldn't recompile the daemon or the other engines).

## Decisions
- **Cargo workspace, one repo** — chosen because the crates share `[patch.crates-io]`
  forks (mlx-rs, mistral.rs), need atomic cross-boundary refactors, and run under one
  multi-agent `origin/master` flow. Rejected: separate repositories (would fragment the
  shared forks and break atomic refactors for no isolation benefit).
- **~7–8 crates (per-engine), not 4–5** — chosen so each engine boundary is *enforced* by
  the compiler. Rejected: a fused `rozum-runtime` (cheaper plumbing, but the boundary that
  matters most — engine isolation — would be unguarded).
- **`launch` stays in the `rozum` bin, not a crate** — it is CLI/orchestration glue
  (`main.rs` + `service`/`share`/`doctor`), not a reusable library. Rejected: a `rozum-launch`
  lib crate (nothing else would depend on it).
- **`rozum-meeting` extracted first** — chosen because it is the only top subsystem with **zero**
  internal `crate::` deps, so it is the cleanest, lowest-risk proof of the workspace mechanics.
- **`rozum-hardware` reserved, designed later** — chosen because placement is new work (no
  module today), not a mechanical move; bundling its design into this refactor would couple a
  risky greenfield with a behaviour-preserving move.

## Results
<Fill in after each phase: crate count, build-time delta (incremental rebuild of bin
after an engine edit — the key win), test counts preserved, matrix parity.>
