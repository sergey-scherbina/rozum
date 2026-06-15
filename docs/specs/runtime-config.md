# runtime-config — declare backends & policy in `rozum.toml`

## Overview

Today the gateway's backend selection is a hardcoded auto-detect chain
(`build_gateway_backend`: in-process GGUF → mistralrs → LM Studio HTTP →
`mlx_lm.server` → `ROZUM_BACKEND_URL`), and the default model must be re-typed
as `--model` on every `rozum gateway` / `rozum launch`. `runtime-config` lets a
user write that once, declaratively, in a `rozum.toml`: an ordered backend list,
a selection **policy** (`single` / `fallback` / `fanout`), and a **default
model + context window**. When no config file exists, the loaded config is
byte-for-byte the current auto-detect behaviour, so nothing changes for users
who never write one.

The headline use case (from SPRINT): a user who routinely switches between
several local + remote backends across sessions sets them up once and stops
re-typing `--model` / `--backend`.

## Interface

### File location

Resolved by `RuntimeConfig::load()`, first hit wins:

1. `$ROZUM_CONFIG` (explicit path; if set but missing → hard error)
2. `./rozum.toml` (project-local, cwd)
3. `$XDG_CONFIG_HOME/rozum/rozum.toml` (or `~/.config/rozum/rozum.toml`)

None found → `RuntimeConfig::default()` (the auto-detect chain). A malformed
file is a hard error (never silently fall back — a typo'd config the user wrote
on purpose must be surfaced).

### TOML schema

```toml
[runtime]
model  = "mlx-community/Qwen3-30B-A3B-4bit"   # optional: default when --model omitted
n_ctx  = 8192                                  # optional: default context window
policy = "fallback"                            # single | fallback | fanout (default: fallback)

# Ordered backend list. Order = fallback/fanout order. `single` uses the first
# enabled backend (or `[runtime].backend` if set). Omit the whole list to get
# the default auto-detect chain.
[[backend]]
id     = "gguf"            # optional, defaults to engine name (must be unique)
engine = "gguf"            # required
# model   = "..."          # optional: override [runtime].model for this backend
# n_ctx   = 4096           # optional: override [runtime].n_ctx for this backend
# url     = "http://..."   # optional: endpoint for http engines (lmstudio/mlx/url)
# enabled = true           # optional, default true

[[backend]]
id      = "remote"
engine  = "url"
url     = "https://my-host:8000/v1"
```

`[runtime].backend = "lmstudio"` (optional) names which backend `single` policy
uses; default is the first enabled entry.

#### `[cascade.<name>]` — named cascade configs (optional)

Declare frugal/escalation cascades (see `cascade-router.md`) the gateway selects via
`model: "cascade"` (→ `default`) or `model: "cascade:<name>"`. Each is a `CascadeSpec`:

```toml
[cascade.default]
strategy        = "classify"   # alwaysCheapest (default) | classify | learned
max_escalations = 1            # optional escalation-hop cap

  [[cascade.default.tiers]]    # cost-ordered, cheapest first
  model = "mlx-community/Qwen3-4B-4bit"   # local: resolved via the backend chain above

  [[cascade.default.tiers]]
  model       = "claude-haiku-4-5"
  location    = "remote"
  api         = "anthropic"    # openai (default) | anthropic
  # endpoint    = "..."        # optional (anthropic defaults to https://api.anthropic.com)
  # api_key_env = "..."        # optional (defaults: ANTHROPIC_API_KEY / OPENAI_API_KEY)
  # pool        = "gpu0"       # optional residency-lane override
```

A `[cascade.<name>]` table takes precedence over the env JSON (`ROZUM_CASCADE` /
`ROZUM_CASCADE_<NAME>`), which remains as a fallback. A tier that can't be built (missing key /
endpoint) is skipped; only an all-empty cascade errors.

**No table needed for the common case** — just list the models and rozum builds an auto-ordered
cascade (cheapest→most-capable; `claude…` → Anthropic, `gpt…/o1…` → OpenAI, else local). `--model` is
repeatable (or use one comma-separated value), and `--strategy` picks the start-tier strategy
(`classify` default | `learned` | `alwaysCheapest`):

```
rozum launch --model "mlx-community/Qwen3-4B-4bit,claude-haiku-4-5,gpt-4o"
rozum launch --model qwen3-4b --model claude-haiku-4-5 --strategy learned   # same cascade
```

The interactive launch picker (shown when `--model` is omitted) lists hosted Anthropic + OpenAI
models alongside local ones; selecting several forms a cascade.

### Engine names

The `engine` field accepts every engine rozum can name:

| `engine` | meaning | gateway path | registry/meeting path |
|---|---|---|---|
| `gguf` | in-process GGUF (llama-cpp-2) | `try_build_gguf_backend` | `BackendEngine::Gguf` |
| `mistralrs` | in-process native-MLX | `try_build_mistralrs_backend` | Placeholder (async-only)¹ |
| `lmstudio` | LM Studio local OpenAI server | `try_lmstudio_http` | Placeholder¹ |
| `mlx` / `mlx_lm` | `mlx_lm.server` (Python) | `try_mlx_server` | Placeholder¹ |
| `url` / `http` | any OpenAI-compatible HTTP | `OpenAiHttpBackend` | Placeholder¹ |
| `hello` | echo stub | n/a | `BackendEngine::Hello` |
| `candle` | candle GGUF (CPU) | n/a | `BackendEngine::Candle` |
| `llama-gguf` | external `llama` command | n/a | `BackendEngine::LlamaGguf` |
| `native-rust` | native-rust stub | n/a | `BackendEngine::NativeRust` |
| `external-command` | external command | n/a | `BackendEngine::ExternalCommand` |

¹ The sync meeting-room `BackendRegistry` cannot async-build the HTTP/native
engines, so in that path they resolve to a `PlaceholderBackend` (an explicit
"use the gateway for these" boundary). The **gateway** path builds them for
real. Unknown engine name → hard parse error listing the accepted set.

### Rust API (`src/config.rs`, re-exported from `lib.rs`)

```rust
pub struct RuntimeConfig {            // parsed rozum.toml
    pub model:   Option<String>,
    pub n_ctx:   Option<u32>,
    pub policy:  Policy,              // Single | Fallback | FanOut
    pub backends: Vec<BackendChoice>, // ordered; never empty after load()
    pub single_backend: Option<String>,
}
pub struct BackendChoice {
    pub id: String, pub engine: String,
    pub model: Option<String>, pub n_ctx: Option<u32>,
    pub url: Option<String>,   pub enabled: bool,
}
pub enum Policy { Single, Fallback, FanOut }

impl RuntimeConfig {
    pub fn load() -> Result<Self, ConfigError>;            // file resolution + default
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError>;
    pub fn default() -> Self;                              // mirrors auto-detect chain
    pub fn gateway_chain(&self) -> Vec<&BackendChoice>;    // enabled, in policy order
    pub fn to_model_runtime_config(&self) -> ModelRuntimeConfig; // sync registry path
}
```

The binary (`main.rs`) owns the async build: `build_choice(&BackendChoice,
requested_model, requested_n_ctx)` resolves overrides (`choice.model ??
requested_model`, `choice.url` for http engines) and calls the existing
`build_gateway_backend_forced` family. The injected `BackendBuilder` (from
`gateway-switch`) walks `gateway_chain()` and returns the first backend that
builds (fallback semantics; `single` = first only). `--backend B` on the CLI
still forces exactly one engine, bypassing the config chain.

## Behavior

- [x] No config file anywhere → `load()` returns `default()`, whose
      `gateway_chain()` is exactly `[gguf, mistralrs, lmstudio, mlx, url]` in
      that order with `Fallback` policy (current auto-detect chain unchanged).
      *(`default_mirrors_auto_detect_chain`, `empty_body_yields_default_chain`)*
- [x] `$ROZUM_CONFIG` set to an existing file is loaded; set to a missing file
      is a hard error (`ExplicitMissing`).
      *(`load_reads_explicit_config_and_errors_when_missing`)*
- [x] `./rozum.toml` is preferred over the XDG path; XDG path used when no cwd
      file. *(by construction in `resolve_path` — cwd checked first, then XDG;
      not unit-tested because cwd is process-global state.)*
- [x] `policy = "single" | "fallback" | "fanout"` parse to the matching
      `Policy`; absent → `Fallback`; any other string → error.
      *(`parses_all_policies`)*
- [x] Every engine name in the table above parses; an unknown engine name is a
      hard error naming the accepted set.
      *(`parses_every_accepted_engine`, `unknown_engine_is_error`)*
- [x] `[[backend]]` with no `id` defaults its id to the engine name; duplicate
      ids are a hard error. *(`id_defaults_to_engine_and_dupes_error`)*
- [x] Per-backend `model` / `n_ctx` / `url` / `enabled` parse and override the
      `[runtime]` defaults; a disabled backend is excluded from
      `gateway_chain()`. *(`per_backend_overrides_and_disabled`)*
- [x] `gateway_chain()` returns enabled backends in declared order; for `single`
      it returns just `[runtime].backend` (or the first enabled).
      *(`single_policy_picks_named_or_first`)*
- [x] `to_model_runtime_config()` maps in-process engines to their
      `BackendEngine` and HTTP/native engines to a placeholder, preserving order
      and policy. *(`to_model_runtime_config_maps_engines_and_policy`)*
- [x] A malformed TOML body is a hard error (not a silent default), including an
      unknown key (`deny_unknown_fields`). *(`malformed_toml_is_error`)*
- [x] The gateway's injected builder, given the default config, walks the same
      `[gguf, mistralrs, lmstudio, mlx, url]` order as the pre-config
      `build_gateway_backend` chain (regression guard via
      `default_mirrors_auto_detect_chain` + `main.rs::build_from_config`).

## Out of scope

- Live config reload / file-watching — config is read once at process start.
  (Runtime swaps are `gateway-switch`'s job; a config change takes effect on the
  next `rozum gateway` / `reload`.)
- Per-request routing or model→backend matchmaking. The chain is global to the
  daemon, which stays single-resident (`gateway-switch`). `fanout` only applies
  to the meeting-room orchestrator path, not the single-resident gateway.
- Async construction of HTTP/native backends inside the sync `BackendRegistry`.
- Writing/editing `rozum.toml` from the CLI (`rozum config` subcommands) — a
  possible follow-up; this phase only reads.

## Design

`src/config.rs` is pure and Metal-free: serde structs + `toml` (already a dep) +
file resolution + adapters. It depends only on `backend.rs` types
(`ModelRuntimeConfig`, `BackendConfig`, `BackendEngine`, `BackendPolicy`) — no
async, no features — so the whole module is unit-testable without Xcode.

The split keeps the library/binary boundary from `gateway-switch` intact: the
config (what to try, in what order) lives in the library; the actual async
backend construction (which needs `--features mistralrs`, HTTP, etc.) stays in
the binary's builder family. `gateway_chain()` returns the plan;
`main.rs::build_choice` executes it.

`default()` is the single source of truth for "the auto-detect chain" — it
constructs the five `BackendChoice`s with no overrides and `Fallback`. The
existing hardcoded order in `build_gateway_backend` is replaced by walking this
chain, so the default and the code can't drift.

## Decisions

- **`engine` as a string, not the `BackendEngine` enum** — chosen because the
  gateway engines (`mistralrs`/`lmstudio`/`mlx`/`url`) are not `BackendEngine`
  variants and are built by a different (async) path than the registry. A string
  with a validated accept-set spans both worlds without forcing every gateway
  shape into the sync registry enum. Rejected: extending `BackendEngine` with
  HTTP variants (they'd be dead `Placeholder`s in every sync path and still need
  the async builder anyway).
- **Default config == auto-detect chain, in code** — chosen so users who never
  write `rozum.toml` see zero behaviour change and the chain has one definition.
  Rejected: shipping a `rozum.toml` on first run (surprising; pollutes the home
  dir).
- **Malformed/missing-explicit config is a hard error** — chosen because a
  config the user deliberately wrote (or pointed `$ROZUM_CONFIG` at) failing
  silently to the default would hide their intent. Rejected: warn-and-default.
- **Read-once, no hot reload** — chosen to keep the phase bounded; `gateway
  reload` already re-execs and picks up a changed file. Live reload is a
  follow-up.

## Results

Landed as `src/config.rs` (`RuntimeConfig` + `BackendChoice` + `Policy` +
`ConfigError`), re-exported from `lib.rs`, with the gateway wiring in `main.rs`
(`load_runtime_config_or_exit`, `build_from_config`, `build_choice`, and a
config-capturing `gateway_backend_builder(Arc<RuntimeConfig>)`).

- **12 unit tests** in `config.rs`, all Metal-free (no Xcode). Full lib suite
  101 passing (was 89 at gateway-switch; +12 config, −0). `cargo fmt --check`
  clean; `cargo build` and `cargo check --features mistralrs` clean.
- **Zero behaviour change without a `rozum.toml`**: `RuntimeConfig::default()` is
  the auto-detect chain in code (`[gguf, mistralrs, lmstudio, mlx, url]`,
  `Fallback`), and the daemon's initial load + every `gateway switch` now walk
  it via `build_from_config` instead of the old hardcoded `build_gateway_backend`
  body. `--backend B` still force-bypasses the chain.
- **`[runtime].model` / `[runtime].n_ctx`** are consulted when `--model` /
  `--n-ctx` are omitted on `rozum gateway`, so a configured default model no
  longer has to be re-typed.
- **Per-backend `url`** lets a `lmstudio`/`mlx`/`url` entry pin an explicit
  endpoint (built directly as an `OpenAiHttpBackend`); per-backend `model` /
  `n_ctx` override the `[runtime]` defaults for that entry.
- **Library/binary split preserved** (from `gateway-switch`): the plan
  (`gateway_chain()`) lives in the library; the async build (`build_choice`,
  which needs `--features mistralrs` / HTTP) stays in the binary.

### Incidental build fix

Verifying this phase surfaced that the `gateway-switch` commit (`0edfdee`) had
swept in stray, incomplete `channel-wakeup` WIP: the `exec_agent` /
`exec_agent_anthropic` call sites passed a `&channels` argument the function
signatures never accepted, so `master` failed to build on default features
(`e50b271` built; `0edfdee` did not). Fixed in a separate commit by threading
`ChannelWakeup` through and applying `flags_for()` — which also completes the
`channel-wakeup-launch-flag` mechanism (a capable `claude` now gets
`--dangerously-load-development-channels` appended at launch).
