//! Runtime configuration: declare the gateway's backend list, selection policy,
//! and default model in `rozum.toml` instead of re-typing `--model` / `--backend`.
//!
//! Spec: `docs/specs/runtime-config.md`.
//!
//! This module is pure and Metal-free: serde structs + `toml` + file resolution
//! + adapters onto `backend.rs` types. The async construction of the
//! HTTP/native backends lives in the binary (`main.rs::build_choice`), which
//! walks [`RuntimeConfig::gateway_chain`]; this keeps the library/binary
//! boundary from `gateway-switch` intact.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use serde::Deserialize;

use crate::backend::{BackendConfig, BackendEngine, BackendPolicy, ModelRuntimeConfig};
use crate::cascade::CascadeSpec;

/// Selection policy over the backend list.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum Policy {
    /// First enabled backend only (or `[runtime].backend`).
    Single,
    /// Try each in order; first success wins.
    #[default]
    Fallback,
    /// Run all concurrently (meeting-room orchestrator only).
    FanOut,
}

impl Policy {
    fn parse(s: &str) -> Result<Self, ConfigError> {
        match s {
            "single" => Ok(Self::Single),
            "fallback" => Ok(Self::Fallback),
            "fanout" => Ok(Self::FanOut),
            other => Err(ConfigError::BadPolicy(other.to_owned())),
        }
    }
}

/// Engine names accepted in the `engine =` field, with their canonical form.
/// The gateway builds the HTTP/native ones; the sync registry maps them to a
/// placeholder (see [`engine_to_backend_engine`]).
pub const ACCEPTED_ENGINES: &[&str] = &[
    "gguf",
    "mistralrs",
    "lmstudio",
    "mlx",
    "mlx_lm",
    // Recognized so `engine = "x86-native"` is a clear "scaffolded, not built yet" path, not
    // an "unknown engine" error. See docs/specs/x86-native-runtime.md. (Aliases normalized below.)
    "x86-native",
    "url",
    "http",
    "hello",
    "candle",
    "llama-gguf",
    "native-rust",
    "external-command",
];

/// Normalize aliases to a single canonical engine name.
fn canonical_engine(engine: &str) -> &str {
    match engine {
        "mlx_lm" => "mlx",
        "http" => "url",
        "x86" | "vulkan" => "x86-native",
        other => other,
    }
}

/// One backend in the ordered list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendChoice {
    pub id: String,
    pub engine: String,
    pub model: Option<String>,
    pub n_ctx: Option<u32>,
    pub url: Option<String>,
    pub enabled: bool,
}

/// Parsed `rozum.toml`. After [`RuntimeConfig::load`] / [`from_toml_str`] the
/// `backends` list is non-empty and every engine name is validated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeConfig {
    pub model: Option<String>,
    pub n_ctx: Option<u32>,
    pub policy: Policy,
    pub backends: Vec<BackendChoice>,
    pub single_backend: Option<String>,
    /// Named cascade configs from `[cascade.<name>]` tables. `model: "cascade"` selects `default`,
    /// `model: "cascade:<name>"` selects `<name>`. See `docs/specs/cascade-router.md`.
    pub cascades: HashMap<String, CascadeSpec>,
    /// The `[sandbox]` table — persistent sandbox policy (the path lists env can't express).
    /// See `docs/specs/model-sandbox.md` "Config surface". Env vars override these.
    pub sandbox: SandboxConfig,
}

/// `[sandbox]` config (docs/specs/model-sandbox.md). The path lists are the value
/// over env (`ROZUM_SANDBOX*`): multiple writable workspaces, read-only reference
/// paths (Docker `:ro` mounts), and operator-specific secret dirs to deny — none of
/// which an env var expresses well. `network`/`backend` are config-level defaults that
/// the matching env var overrides. Empty/`None` everywhere = no `[sandbox]` table.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct SandboxConfig {
    /// Extra writable workspace paths beyond the launch cwd (`"."` = cwd, `"~/…"` expands `$HOME`).
    pub workspace: Vec<String>,
    /// Read-only paths — mounted `:ro` under Docker; under Seatbelt a non-writable path
    /// is already read-only (reads are broad), so these are informational there.
    pub read_only: Vec<String>,
    /// Extra secret dirs to deny (appended to the built-in denylist).
    pub secret_deny: Vec<String>,
    /// `none` | `gateway-only` | `gateway-strict` | `full`. Overridden by `ROZUM_SANDBOX_NETWORK`.
    pub network: Option<String>,
    /// `seatbelt` | `docker`. Overridden by `ROZUM_SANDBOX_BACKEND`.
    pub backend: Option<String>,
}

// ─── serde wire format ────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    runtime: RawRuntime,
    #[serde(default, rename = "backend")]
    backends: Vec<RawBackend>,
    /// `[cascade.<name>]` tables → named `CascadeSpec`s (`tiers`, `strategy`, `max_escalations`).
    #[serde(default)]
    cascade: HashMap<String, CascadeSpec>,
    /// The `[sandbox]` table.
    #[serde(default)]
    sandbox: RawSandbox,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawSandbox {
    #[serde(default)]
    workspace: Vec<String>,
    #[serde(default)]
    read_only: Vec<String>,
    #[serde(default)]
    secret_deny: Vec<String>,
    network: Option<String>,
    backend: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawRuntime {
    model: Option<String>,
    n_ctx: Option<u32>,
    policy: Option<String>,
    /// Which backend `single` policy uses (default: first enabled).
    backend: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBackend {
    id: Option<String>,
    engine: String,
    model: Option<String>,
    n_ctx: Option<u32>,
    url: Option<String>,
    enabled: Option<bool>,
}

// ─── errors ─────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ConfigError {
    /// `$ROZUM_CONFIG` pointed at a path that does not exist.
    ExplicitMissing(PathBuf),
    /// Read error on a config file.
    Io(PathBuf, std::io::Error),
    /// TOML parse error.
    Parse(String),
    /// Unknown `policy =` value.
    BadPolicy(String),
    /// Unknown `engine =` value.
    BadEngine(String),
    /// Two backends share an `id`.
    DuplicateId(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExplicitMissing(p) => {
                write!(f, "ROZUM_CONFIG points at a missing file: {}", p.display())
            }
            Self::Io(p, e) => write!(f, "reading {}: {e}", p.display()),
            Self::Parse(e) => write!(f, "rozum.toml parse error: {e}"),
            Self::BadPolicy(p) => write!(
                f,
                "unknown policy {p:?} (expected: single, fallback, fanout)"
            ),
            Self::BadEngine(e) => write!(
                f,
                "unknown engine {e:?} (expected one of: {})",
                ACCEPTED_ENGINES.join(", ")
            ),
            Self::DuplicateId(id) => write!(f, "duplicate backend id {id:?}"),
        }
    }
}

impl std::error::Error for ConfigError {}

// ─── load / parse ─────────────────────────────────────────────────────────────

impl Default for RuntimeConfig {
    /// The auto-detect chain, in code — the single source of truth for "no
    /// `rozum.toml`". `Fallback` over `[mlx, gguf, mistralrs, lmstudio, url]`
    /// (native MLX is the primary in-process backend; GGUF is the fallback).
    fn default() -> Self {
        let chain = ["mlx", "gguf", "mistralrs", "lmstudio", "url"];
        Self {
            model: None,
            n_ctx: None,
            policy: Policy::Fallback,
            backends: chain
                .iter()
                .map(|e| BackendChoice {
                    id: (*e).to_owned(),
                    engine: (*e).to_owned(),
                    model: None,
                    n_ctx: None,
                    url: None,
                    enabled: true,
                })
                .collect(),
            single_backend: None,
            cascades: HashMap::new(),
            sandbox: SandboxConfig::default(),
        }
    }
}

impl RuntimeConfig {
    /// Resolve a config file (see spec) and parse it; no file → [`Self::default`].
    pub fn load() -> Result<Self, ConfigError> {
        // `$ROZUM_CONFIG` set but missing is a hard error: a config the user
        // deliberately pointed at must not silently fall back to the default.
        if let Some(explicit) = std::env::var_os("ROZUM_CONFIG") {
            let path = PathBuf::from(explicit);
            if !path.is_file() {
                return Err(ConfigError::ExplicitMissing(path));
            }
            let body =
                std::fs::read_to_string(&path).map_err(|e| ConfigError::Io(path.clone(), e))?;
            return Self::from_toml_str(&body);
        }
        match Self::resolve_path() {
            Some(path) => {
                let body =
                    std::fs::read_to_string(&path).map_err(|e| ConfigError::Io(path.clone(), e))?;
                Self::from_toml_str(&body)
            }
            None => Ok(Self::default()),
        }
    }

    /// First existing of: `./rozum.toml`, `$XDG_CONFIG_HOME/rozum/rozum.toml`
    /// (or `~/.config/...`). `$ROZUM_CONFIG` is handled earlier in [`Self::load`].
    fn resolve_path() -> Option<PathBuf> {
        let cwd = PathBuf::from("rozum.toml");
        if cwd.is_file() {
            return Some(cwd);
        }
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|base| base.join("rozum").join("rozum.toml"));
        match xdg {
            Some(p) if p.is_file() => Some(p),
            _ => None,
        }
    }

    /// Parse + validate a TOML body. An empty body yields [`Self::default`]'s
    /// chain (with whatever `[runtime]` keys were given applied on top).
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        // Honor ROZUM_CONFIG-missing as a hard error at the load() layer; here
        // we only parse.
        let raw: RawConfig = toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))?;

        let policy = match raw.runtime.policy.as_deref() {
            Some(p) => Policy::parse(p)?,
            None => Policy::default(),
        };

        let mut backends = Vec::with_capacity(raw.backends.len());
        for b in raw.backends {
            if !ACCEPTED_ENGINES.contains(&b.engine.as_str()) {
                return Err(ConfigError::BadEngine(b.engine));
            }
            let id = b.id.unwrap_or_else(|| b.engine.clone());
            if backends.iter().any(|c: &BackendChoice| c.id == id) {
                return Err(ConfigError::DuplicateId(id));
            }
            backends.push(BackendChoice {
                id,
                engine: b.engine,
                model: b.model,
                n_ctx: b.n_ctx,
                url: b.url,
                enabled: b.enabled.unwrap_or(true),
            });
        }

        // No `[[backend]]` declared → inherit the default chain so a config that
        // only sets `[runtime].model` still gets the auto-detect backends.
        if backends.is_empty() {
            backends = Self::default().backends;
        }

        Ok(Self {
            model: raw.runtime.model,
            n_ctx: raw.runtime.n_ctx,
            policy,
            backends,
            single_backend: raw.runtime.backend,
            cascades: raw.cascade,
            sandbox: SandboxConfig {
                workspace: raw.sandbox.workspace,
                read_only: raw.sandbox.read_only,
                secret_deny: raw.sandbox.secret_deny,
                network: raw.sandbox.network,
                backend: raw.sandbox.backend,
            },
        })
    }

    /// The cascade config selected by a `model:` string: `""` (i.e. `model: "cascade"`) → the
    /// `default` table; `"<name>"` → the `<name>` table. `None` if it isn't declared in the TOML.
    pub fn cascade_spec(&self, name: &str) -> Option<&CascadeSpec> {
        let key = if name.is_empty() { "default" } else { name };
        self.cascades.get(key)
    }

    /// Enabled backends in the order the gateway should try them. For `Single`,
    /// just `[runtime].backend` (or the first enabled). For `Fallback`/`FanOut`,
    /// all enabled in declared order.
    pub fn gateway_chain(&self) -> Vec<&BackendChoice> {
        let enabled: Vec<&BackendChoice> = self.backends.iter().filter(|b| b.enabled).collect();
        match self.policy {
            Policy::Single => {
                let chosen = match &self.single_backend {
                    Some(id) => enabled.iter().find(|b| &b.id == id).copied(),
                    None => enabled.first().copied(),
                };
                chosen.into_iter().collect()
            }
            Policy::Fallback | Policy::FanOut => enabled,
        }
    }

    /// Adapter for the sync meeting-room `BackendRegistry`. HTTP/native engines
    /// map to a placeholder engine (built for real only on the gateway path).
    pub fn to_model_runtime_config(&self) -> ModelRuntimeConfig {
        let chain = self.gateway_chain();
        let backends: Vec<BackendConfig> = chain
            .iter()
            .map(|c| {
                let mut cfg = BackendConfig::new(c.id.clone(), engine_to_backend_engine(&c.engine));
                if let Some(model) = &c.model.clone().or_else(|| self.model.clone()) {
                    cfg.model_spec = Some(model.clone());
                }
                cfg
            })
            .collect();
        let ids: Vec<String> = backends.iter().map(|b| b.id.clone()).collect();
        let policy = match self.policy {
            Policy::Single => BackendPolicy::single(ids.first().cloned().unwrap_or_default()),
            Policy::Fallback => BackendPolicy::fallback(ids),
            Policy::FanOut => BackendPolicy::fanout(ids),
        };
        ModelRuntimeConfig { backends, policy }
    }
}

/// Map a config engine name to a sync-registry [`BackendEngine`]. The
/// HTTP/native engines (mistralrs/lmstudio/mlx/url) have no sync constructor, so
/// they map to `NativeRust`, which the registry turns into a `PlaceholderBackend`
/// — an explicit "use the gateway for these" boundary.
pub fn engine_to_backend_engine(engine: &str) -> BackendEngine {
    match canonical_engine(engine) {
        "gguf" => BackendEngine::Gguf,
        "hello" => BackendEngine::Hello,
        "candle" => BackendEngine::Candle,
        "llama-gguf" => BackendEngine::LlamaGguf,
        "external-command" => BackendEngine::ExternalCommand,
        // native-rust + all HTTP/native gateway engines → placeholder in sync path
        _ => BackendEngine::NativeRust,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize the env-mutating `load()` tests so a `ROZUM_CONFIG` set by one
    /// doesn't leak into another running in parallel.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn load_reads_explicit_config_and_errors_when_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("rozum-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("explicit.toml");
        std::fs::write(
            &path,
            "[runtime]\nmodel = \"pin/me\"\npolicy = \"single\"\n",
        )
        .unwrap();

        // SAFETY: serialized by ENV_LOCK; only load() tests touch ROZUM_CONFIG.
        unsafe { std::env::set_var("ROZUM_CONFIG", &path) };
        let cfg = RuntimeConfig::load().expect("explicit config loads");
        assert_eq!(cfg.model.as_deref(), Some("pin/me"));
        assert_eq!(cfg.policy, Policy::Single);

        unsafe { std::env::set_var("ROZUM_CONFIG", dir.join("does-not-exist.toml")) };
        assert!(matches!(
            RuntimeConfig::load(),
            Err(ConfigError::ExplicitMissing(_))
        ));

        unsafe { std::env::remove_var("ROZUM_CONFIG") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_mirrors_auto_detect_chain() {
        let cfg = RuntimeConfig::default();
        assert_eq!(cfg.policy, Policy::Fallback);
        let engines: Vec<&str> = cfg
            .gateway_chain()
            .iter()
            .map(|c| c.engine.as_str())
            .collect();
        assert_eq!(engines, ["mlx", "gguf", "mistralrs", "lmstudio", "url"]);
    }

    #[test]
    fn empty_body_yields_default_chain() {
        let cfg = RuntimeConfig::from_toml_str("").unwrap();
        assert_eq!(cfg, RuntimeConfig::default());
    }

    #[test]
    fn runtime_only_keeps_default_backends() {
        let cfg = RuntimeConfig::from_toml_str(
            r#"
            [runtime]
            model = "mlx-community/Qwen3-30B-A3B-4bit"
            n_ctx = 8192
        "#,
        )
        .unwrap();
        assert_eq!(
            cfg.model.as_deref(),
            Some("mlx-community/Qwen3-30B-A3B-4bit")
        );
        assert_eq!(cfg.n_ctx, Some(8192));
        let engines: Vec<&str> = cfg
            .gateway_chain()
            .iter()
            .map(|c| c.engine.as_str())
            .collect();
        assert_eq!(engines, ["mlx", "gguf", "mistralrs", "lmstudio", "url"]);
    }

    #[test]
    fn parses_all_policies() {
        for (s, want) in [
            ("single", Policy::Single),
            ("fallback", Policy::Fallback),
            ("fanout", Policy::FanOut),
        ] {
            let body = format!("[runtime]\npolicy = \"{s}\"\n");
            assert_eq!(RuntimeConfig::from_toml_str(&body).unwrap().policy, want);
        }
        assert!(matches!(
            RuntimeConfig::from_toml_str("[runtime]\npolicy=\"nope\"\n"),
            Err(ConfigError::BadPolicy(_))
        ));
    }

    #[test]
    fn parses_every_accepted_engine() {
        for engine in ACCEPTED_ENGINES {
            let body = format!("[[backend]]\nengine = \"{engine}\"\n");
            let cfg = RuntimeConfig::from_toml_str(&body)
                .unwrap_or_else(|e| panic!("engine {engine} should parse: {e}"));
            assert_eq!(cfg.backends.len(), 1);
            assert_eq!(cfg.backends[0].id, *engine);
        }
    }

    #[test]
    fn unknown_engine_is_error() {
        assert!(matches!(
            RuntimeConfig::from_toml_str("[[backend]]\nengine=\"frobnicate\"\n"),
            Err(ConfigError::BadEngine(_))
        ));
    }

    #[test]
    fn id_defaults_to_engine_and_dupes_error() {
        let cfg = RuntimeConfig::from_toml_str("[[backend]]\nengine=\"gguf\"\n").unwrap();
        assert_eq!(cfg.backends[0].id, "gguf");
        assert!(matches!(
            RuntimeConfig::from_toml_str(
                "[[backend]]\nid=\"x\"\nengine=\"gguf\"\n[[backend]]\nid=\"x\"\nengine=\"url\"\n"
            ),
            Err(ConfigError::DuplicateId(_))
        ));
    }

    #[test]
    fn per_backend_overrides_and_disabled() {
        let cfg = RuntimeConfig::from_toml_str(
            r#"
            [[backend]]
            id = "remote"
            engine = "url"
            url = "http://h:8000/v1"
            model = "pin/this"
            n_ctx = 4096

            [[backend]]
            id = "off"
            engine = "gguf"
            enabled = false
        "#,
        )
        .unwrap();
        assert_eq!(cfg.backends.len(), 2);
        let remote = &cfg.backends[0];
        assert_eq!(remote.url.as_deref(), Some("http://h:8000/v1"));
        assert_eq!(remote.model.as_deref(), Some("pin/this"));
        assert_eq!(remote.n_ctx, Some(4096));
        // disabled excluded from the chain
        let ids: Vec<&str> = cfg.gateway_chain().iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["remote"]);
    }

    #[test]
    fn single_policy_picks_named_or_first() {
        let body = r#"
            [runtime]
            policy = "single"
            backend = "second"

            [[backend]]
            id = "first"
            engine = "gguf"
            [[backend]]
            id = "second"
            engine = "mistralrs"
        "#;
        let cfg = RuntimeConfig::from_toml_str(body).unwrap();
        let chain = cfg.gateway_chain();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].id, "second");

        // without [runtime].backend, single picks the first enabled
        let body2 = "[runtime]\npolicy=\"single\"\n[[backend]]\nid=\"a\"\nengine=\"gguf\"\n[[backend]]\nid=\"b\"\nengine=\"url\"\n";
        let cfg2 = RuntimeConfig::from_toml_str(body2).unwrap();
        assert_eq!(cfg2.gateway_chain()[0].id, "a");
    }

    #[test]
    fn to_model_runtime_config_maps_engines_and_policy() {
        let cfg = RuntimeConfig::from_toml_str(
            r#"
            [runtime]
            policy = "fallback"
            [[backend]]
            engine = "gguf"
            [[backend]]
            engine = "lmstudio"
        "#,
        )
        .unwrap();
        let mrc = cfg.to_model_runtime_config();
        assert_eq!(mrc.backends.len(), 2);
        assert_eq!(mrc.backends[0].engine, BackendEngine::Gguf);
        // lmstudio has no sync constructor → placeholder engine
        assert_eq!(mrc.backends[1].engine, BackendEngine::NativeRust);
        assert!(matches!(mrc.policy, BackendPolicy::Fallback { .. }));
    }

    #[test]
    fn malformed_toml_is_error() {
        assert!(matches!(
            RuntimeConfig::from_toml_str("this is not = = toml ["),
            Err(ConfigError::Parse(_))
        ));
        // unknown key rejected (deny_unknown_fields)
        assert!(matches!(
            RuntimeConfig::from_toml_str("[runtime]\nbogus = 1\n"),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn parses_named_cascade_tables() {
        use crate::cascade::{Location, RemoteApi, StrategyName};
        let cfg = RuntimeConfig::from_toml_str(
            r#"
            [cascade.default]
            strategy = "classify"
            max_escalations = 1
            [[cascade.default.tiers]]
            model = "mlx-community:Qwen3-4B-4bit"
            [[cascade.default.tiers]]
            model = "claude-haiku-4-5"
            location = "remote"
            api = "anthropic"

            [cascade.fast]
            [[cascade.fast.tiers]]
            model = "mlx-community:Qwen3-4B-4bit"
            "#,
        )
        .unwrap();

        let def = cfg.cascade_spec("").expect("default cascade (model: cascade)");
        assert_eq!(def.tiers.len(), 2);
        assert_eq!(def.strategy, StrategyName::Classify);
        assert_eq!(def.max_escalations, Some(1));
        assert_eq!(def.tiers[0].location, Location::Local); // defaulted
        assert_eq!(def.tiers[1].location, Location::Remote);
        assert_eq!(def.tiers[1].api, RemoteApi::Anthropic);

        assert_eq!(cfg.cascade_spec("fast").expect("named cascade").tiers.len(), 1);
        assert!(cfg.cascade_spec("missing").is_none());
        // The default config (no [cascade]) has no cascades.
        assert!(RuntimeConfig::default().cascade_spec("").is_none());
    }

    #[test]
    fn parses_sandbox_table() {
        let cfg = RuntimeConfig::from_toml_str(
            r#"
            [sandbox]
            workspace = [".", "/tmp/scratch"]
            read_only = ["../shared-lib"]
            secret_deny = ["~/.my-secrets"]
            network = "gateway-strict"
            backend = "docker"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.sandbox.workspace, vec![".", "/tmp/scratch"]);
        assert_eq!(cfg.sandbox.read_only, vec!["../shared-lib"]);
        assert_eq!(cfg.sandbox.secret_deny, vec!["~/.my-secrets"]);
        assert_eq!(cfg.sandbox.network.as_deref(), Some("gateway-strict"));
        assert_eq!(cfg.sandbox.backend.as_deref(), Some("docker"));
        // No [sandbox] table → empty default; an unknown key is rejected.
        assert_eq!(RuntimeConfig::default().sandbox, SandboxConfig::default());
        assert!(RuntimeConfig::from_toml_str("[sandbox]\nbogus = 1\n").is_err());
    }
}
