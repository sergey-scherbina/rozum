use std::error::Error;
use std::fmt;
#[cfg(feature = "local-models")]
use std::fs::File;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
#[cfg(feature = "local-models")]
use std::sync::Mutex;

use futures_util::StreamExt as _;
use tokio_util::sync::CancellationToken;

pub type ModelResult<T> = Result<T, ModelError>;

// ─── Error types ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelError {
    BackendUnavailable(String),
    BackendNotFound(String),
    NoBackendSucceeded(Vec<String>),
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BackendUnavailable(msg) => write!(f, "backend unavailable: {msg}"),
            Self::BackendNotFound(id) => write!(f, "backend not found: {id}"),
            Self::NoBackendSucceeded(errors) => {
                write!(f, "no backend succeeded")?;
                if !errors.is_empty() {
                    write!(f, ": {}", errors.join("; "))?;
                }
                Ok(())
            }
        }
    }
}

impl Error for ModelError {}

// ─── Backward-compatible response types (used in tests/orchestrator) ─────────

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenerationResponse {
    pub backend_id: String,
    pub output: String,
}

impl GenerationResponse {
    pub fn new(backend_id: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            backend_id: backend_id.into(),
            output: output.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendAttempt {
    pub backend_id: String,
    pub result: ModelResult<String>,
}

impl BackendAttempt {
    pub fn new(backend_id: impl Into<String>, result: ModelResult<String>) -> Self {
        Self {
            backend_id: backend_id.into(),
            result,
        }
    }

    pub fn succeeded(&self) -> bool {
        self.result.is_ok()
    }

    fn error_message(&self) -> Option<String> {
        self.result
            .as_ref()
            .err()
            .map(|e| format!("{}: {}", self.backend_id, e))
    }
}

// ─── Chat Backend SPI ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Clone, Debug)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Clone, Debug, Default)]
pub struct SamplingParams {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub repeat_penalty: Option<f32>,
    pub seed: Option<u64>,
    pub max_tokens: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub sampling: SamplingParams,
    pub cancel: CancellationToken,
    /// Advisory hint for prompt-prefix cache keying.
    pub session_id: Option<String>,
}

impl ChatRequest {
    pub fn simple(text: impl Into<String>) -> Self {
        Self {
            messages: vec![Message::user(text)],
            tools: Vec::new(),
            sampling: SamplingParams::default(),
            cancel: CancellationToken::new(),
            session_id: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    Cancelled,
}

#[derive(Debug)]
pub enum ChatEvent {
    TextDelta {
        text: String,
    },
    ToolUseStart {
        id: String,
        name: String,
    },
    ToolUseDelta {
        id: String,
        input_json_delta: String,
    },
    ToolUseEnd {
        id: String,
    },
    Done {
        input_tokens: u32,
        output_tokens: u32,
        stop_reason: StopReason,
    },
}

pub type ChatStream = Pin<Box<dyn futures::Stream<Item = ModelResult<ChatEvent>> + Send>>;

#[async_trait::async_trait]
pub trait ChatBackend: Send + Sync {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream>;
    /// Maximum context length in tokens this backend accepts.
    fn context_window(&self) -> u32;
}

/// Drain a `ChatStream` to a plain text string (text deltas only).
pub async fn collect_to_string(mut stream: ChatStream) -> ModelResult<String> {
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event? {
            ChatEvent::TextDelta { text: delta } => text.push_str(&delta),
            ChatEvent::Done { .. } => break,
            _ => {}
        }
    }
    Ok(text)
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

fn messages_to_string(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn make_simple_stream(result: ModelResult<String>) -> ChatStream {
    Box::pin(async_stream::stream! {
        match result {
            Ok(text) => {
                yield Ok(ChatEvent::TextDelta { text });
                yield Ok(ChatEvent::Done {
                    input_tokens: 0,
                    output_tokens: 0,
                    stop_reason: StopReason::EndTurn,
                });
            }
            Err(e) => yield Err(e),
        }
    })
}

// ─── HelloBackend ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HelloBackend;

impl HelloBackend {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl ChatBackend for HelloBackend {
    async fn chat(&self, _req: ChatRequest) -> ModelResult<ChatStream> {
        Ok(make_simple_stream(Ok("hello!".to_owned())))
    }

    fn context_window(&self) -> u32 {
        u32::MAX
    }
}

// ─── BackendEngine / BackendConfig / BackendPolicy / ModelRuntimeConfig ───────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendEngine {
    Hello,
    Candle,
    LlamaGguf,
    NativeRust,
    ExternalCommand,
    Gguf,
}

impl fmt::Display for BackendEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hello => write!(f, "hello"),
            Self::Candle => write!(f, "candle"),
            Self::LlamaGguf => write!(f, "llama-gguf"),
            Self::NativeRust => write!(f, "native-rust"),
            Self::ExternalCommand => write!(f, "external-command"),
            Self::Gguf => write!(f, "gguf"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendConfig {
    pub id: String,
    pub engine: BackendEngine,
    pub model_path: Option<PathBuf>,
    pub model_spec: Option<String>,
    pub command: Option<String>,
    pub enabled: bool,
}

impl BackendConfig {
    pub fn new(id: impl Into<String>, engine: BackendEngine) -> Self {
        Self {
            id: id.into(),
            engine,
            model_path: None,
            model_spec: None,
            command: None,
            enabled: true,
        }
    }

    pub fn hello(id: impl Into<String>) -> Self {
        Self::new(id, BackendEngine::Hello)
    }

    pub fn candle_gguf(id: impl Into<String>, model_path: impl Into<PathBuf>) -> Self {
        Self::new(id, BackendEngine::Candle).with_model_path(model_path)
    }

    pub fn llama_gguf(id: impl Into<String>, model_path: impl Into<PathBuf>) -> Self {
        Self::new(id, BackendEngine::LlamaGguf).with_model_path(model_path)
    }

    pub fn native_gguf(id: impl Into<String>, model_path: impl Into<PathBuf>) -> Self {
        Self::new(id, BackendEngine::NativeRust).with_model_path(model_path)
    }

    pub fn external_command(id: impl Into<String>, command: impl Into<String>) -> Self {
        Self::new(id, BackendEngine::ExternalCommand).with_command(command)
    }

    /// In-process GGUF backend (feature `gguf`).
    /// `model_spec` may be an absolute path, `lmstudio:<repo>`, or `ollama:<name>`.
    pub fn gguf(id: impl Into<String>, model_spec: impl Into<String>) -> Self {
        let id = id.into();
        let spec = model_spec.into();
        Self {
            id: id.clone(),
            engine: BackendEngine::Gguf,
            model_path: None,
            model_spec: Some(spec),
            command: None,
            enabled: true,
        }
    }

    pub fn with_model_path(mut self, model_path: impl Into<PathBuf>) -> Self {
        self.model_path = Some(model_path.into());
        self
    }

    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.command = Some(command.into());
        self
    }

    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendPolicy {
    Single { backend_id: String },
    Fallback { backend_ids: Vec<String> },
    FanOut { backend_ids: Vec<String> },
}

impl BackendPolicy {
    pub fn single(backend_id: impl Into<String>) -> Self {
        Self::Single {
            backend_id: backend_id.into(),
        }
    }

    pub fn fallback(backend_ids: Vec<String>) -> Self {
        Self::Fallback { backend_ids }
    }

    pub fn fanout(backend_ids: Vec<String>) -> Self {
        Self::FanOut { backend_ids }
    }

    pub fn backend_ids(&self) -> &[String] {
        match self {
            Self::Single { backend_id } => std::slice::from_ref(backend_id),
            Self::Fallback { backend_ids } | Self::FanOut { backend_ids } => backend_ids,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Single { backend_id } => format!("single:{backend_id}"),
            Self::Fallback { backend_ids } => format!("fallback:{}", backend_ids.join(",")),
            Self::FanOut { backend_ids } => format!("fanout:{}", backend_ids.join(",")),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRuntimeConfig {
    pub backends: Vec<BackendConfig>,
    pub policy: BackendPolicy,
}

impl Default for ModelRuntimeConfig {
    fn default() -> Self {
        Self {
            backends: vec![BackendConfig::hello("hello")],
            policy: BackendPolicy::single("hello"),
        }
    }
}

impl ModelRuntimeConfig {
    pub fn tiny_multi_backend_preview() -> Self {
        let model_path = PathBuf::from("models").join(SMOLLM2_135M_INSTRUCT_Q4_K_M.filename);
        let backend_ids = vec![
            "candle-smollm2-q4".to_owned(),
            "llama-gguf-smollm2-q4".to_owned(),
            "native-smollm2-q4".to_owned(),
            "hello".to_owned(),
        ];
        Self {
            backends: vec![
                BackendConfig::candle_gguf(&backend_ids[0], model_path.clone()),
                BackendConfig::llama_gguf(&backend_ids[1], model_path.clone()),
                BackendConfig::native_gguf(&backend_ids[2], model_path),
                BackendConfig::hello(&backend_ids[3]),
            ],
            policy: BackendPolicy::fallback(backend_ids),
        }
    }

    pub fn tiny_fanout_preview() -> Self {
        let mut config = Self::tiny_multi_backend_preview();
        config.policy = BackendPolicy::fanout(config.policy.backend_ids().to_vec());
        config
    }
}

// ─── PlaceholderBackend ───────────────────────────────────────────────────────

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaceholderBackend {
    id: String,
    engine: BackendEngine,
}

impl PlaceholderBackend {
    pub fn new(id: impl Into<String>, engine: BackendEngine) -> Self {
        Self {
            id: id.into(),
            engine,
        }
    }
}

#[async_trait::async_trait]
impl ChatBackend for PlaceholderBackend {
    async fn chat(&self, _req: ChatRequest) -> ModelResult<ChatStream> {
        Err(ModelError::BackendUnavailable(format!(
            "{} adapter '{}' is not implemented yet",
            self.engine, self.id
        )))
    }

    fn context_window(&self) -> u32 {
        0
    }
}

// ─── CandleBackend ────────────────────────────────────────────────────────────

#[cfg(feature = "local-models")]
pub struct CandleBackend {
    id: String,
    model_path: PathBuf,
    max_tokens: usize,
    state: Mutex<Option<CandleState>>,
}

#[cfg(feature = "local-models")]
struct CandleState {
    model: candle_transformers::models::quantized_llama::ModelWeights,
    tokenizer: tokenizers::Tokenizer,
    device: candle::Device,
    eos_token_id: Option<u32>,
}

#[cfg(feature = "local-models")]
impl CandleBackend {
    pub fn from_config(config: &BackendConfig) -> ModelResult<Self> {
        let model_path = config.model_path.clone().ok_or_else(|| {
            ModelError::BackendUnavailable(format!(
                "candle backend '{}' is missing model_path",
                config.id
            ))
        })?;
        Ok(Self {
            id: config.id.clone(),
            model_path,
            max_tokens: 32,
            state: Mutex::new(None),
        })
    }

    pub fn new(id: impl Into<String>, model_path: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            model_path: model_path.into(),
            max_tokens: 32,
            state: Mutex::new(None),
        }
    }

    fn load_state(&self) -> ModelResult<CandleState> {
        if !self.model_path.exists() {
            return Err(ModelError::BackendUnavailable(format!(
                "{} model file is missing: {}",
                self.id,
                self.model_path.display()
            )));
        }
        let device = candle::Device::Cpu;
        let mut file = File::open(&self.model_path).map_err(|e| {
            ModelError::BackendUnavailable(format!(
                "{} failed to open {}: {}",
                self.id,
                self.model_path.display(),
                e
            ))
        })?;
        let content = candle::quantized::gguf_file::Content::read(&mut file).map_err(|e| {
            ModelError::BackendUnavailable(format!(
                "{} failed to read GGUF metadata from {}: {}",
                self.id,
                self.model_path.display(),
                e
            ))
        })?;
        let eos_token_id = content
            .metadata
            .get("tokenizer.ggml.eos_token_id")
            .and_then(|v| v.to_u32().ok());
        let tokenizer =
            <tokenizers::Tokenizer as candle::quantized::tokenizer::TokenizerFromGguf>::from_gguf(
                &content,
            )
            .map_err(|e| {
                ModelError::BackendUnavailable(format!(
                    "{} failed to load tokenizer from GGUF: {}",
                    self.id, e
                ))
            })?;
        let model = candle_transformers::models::quantized_llama::ModelWeights::from_gguf(
            content, &mut file, &device,
        )
        .map_err(|e| {
            ModelError::BackendUnavailable(format!(
                "{} failed to load quantized LLaMA weights: {}",
                self.id, e
            ))
        })?;
        Ok(CandleState {
            model,
            tokenizer,
            device,
            eos_token_id,
        })
    }

    fn generate_sync(&self, input: &str) -> ModelResult<String> {
        let mut guard = self.state.lock().map_err(|_| {
            ModelError::BackendUnavailable(format!("{} candle state lock was poisoned", self.id))
        })?;
        if guard.is_none() {
            *guard = Some(self.load_state()?);
        }
        let state = guard.as_mut().expect("state just loaded");

        let mut tokens = state
            .tokenizer
            .encode(input, true)
            .map_err(|e| {
                ModelError::BackendUnavailable(format!(
                    "{} failed to encode prompt: {}",
                    self.id, e
                ))
            })?
            .get_ids()
            .to_vec();
        if tokens.is_empty() {
            return Ok(String::new());
        }

        let mut generated = Vec::with_capacity(self.max_tokens);
        let mut logits_processor =
            candle_transformers::generation::LogitsProcessor::new(0, None, None);
        let mut index_pos = 0;

        for step in 0..self.max_tokens {
            let context_size = if step == 0 { tokens.len() } else { 1 };
            let start_pos = tokens.len().saturating_sub(context_size);
            let input_tokens = candle::Tensor::new(&tokens[start_pos..], &state.device)
                .and_then(|t| t.unsqueeze(0))
                .map_err(|e| {
                    ModelError::BackendUnavailable(format!(
                        "{} failed to create input tensor: {}",
                        self.id, e
                    ))
                })?;
            let logits = state.model.forward(&input_tokens, index_pos).map_err(|e| {
                ModelError::BackendUnavailable(format!(
                    "{} failed during forward pass: {}",
                    self.id, e
                ))
            })?;
            let logits = logits.squeeze(0).map_err(|e| {
                ModelError::BackendUnavailable(format!(
                    "{} failed to normalize logits shape: {}",
                    self.id, e
                ))
            })?;
            index_pos += context_size;
            let next_token = logits_processor.sample(&logits).map_err(|e| {
                ModelError::BackendUnavailable(format!(
                    "{} failed to sample next token: {}",
                    self.id, e
                ))
            })?;
            tokens.push(next_token);
            if Some(next_token) == state.eos_token_id {
                break;
            }
            generated.push(next_token);
        }

        state
            .tokenizer
            .decode(&generated, true)
            .map(|o| o.trim().to_owned())
            .map_err(|e| {
                ModelError::BackendUnavailable(format!(
                    "{} failed to decode generated tokens: {}",
                    self.id, e
                ))
            })
    }
}

#[cfg(feature = "local-models")]
#[async_trait::async_trait]
impl ChatBackend for CandleBackend {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
        if !req.tools.is_empty() {
            return Err(ModelError::BackendUnavailable(format!(
                "{} does not support tool-use",
                self.id
            )));
        }
        let input = messages_to_string(&req.messages);
        let output = self.generate_sync(&input);
        Ok(make_simple_stream(output))
    }

    fn context_window(&self) -> u32 {
        4096
    }
}

// ─── LlamaGgufCommandBackend ──────────────────────────────────────────────────

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LlamaGgufCommandBackend {
    id: String,
    binary_path: PathBuf,
    model_path: PathBuf,
    max_tokens: usize,
    temperature: &'static str,
}

impl LlamaGgufCommandBackend {
    pub fn from_config(config: &BackendConfig) -> ModelResult<Self> {
        let model_path = config.model_path.clone().ok_or_else(|| {
            ModelError::BackendUnavailable(format!(
                "llama-gguf backend '{}' is missing model_path",
                config.id
            ))
        })?;
        Ok(Self {
            id: config.id.clone(),
            binary_path: llama_gguf_binary_path(),
            model_path,
            max_tokens: 32,
            temperature: "0",
        })
    }

    pub fn new(
        id: impl Into<String>,
        binary_path: impl Into<PathBuf>,
        model_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            id: id.into(),
            binary_path: binary_path.into(),
            model_path: model_path.into(),
            max_tokens: 32,
            temperature: "0",
        }
    }

    fn clean_stdout(&self, prompt: &str, stdout: &str) -> String {
        let without_generated_line = stdout
            .lines()
            .filter(|line| {
                let t = line.trim();
                !(t.starts_with("Generated ") && t.ends_with(" tokens"))
            })
            .collect::<Vec<_>>()
            .join("\n");
        without_generated_line
            .strip_prefix(prompt)
            .unwrap_or(&without_generated_line)
            .trim()
            .to_owned()
    }

    fn generate_sync(&self, input: &str) -> ModelResult<String> {
        if !self.model_path.exists() {
            return Err(ModelError::BackendUnavailable(format!(
                "{} model file is missing: {}",
                self.id,
                self.model_path.display()
            )));
        }
        if !command_exists(&self.binary_path) {
            return Err(ModelError::BackendUnavailable(format!(
                "{} binary is missing: {}",
                self.id,
                self.binary_path.display()
            )));
        }
        let output = Command::new(&self.binary_path)
            .arg("run")
            .arg(&self.model_path)
            .arg("-p")
            .arg(input)
            .arg("-n")
            .arg(self.max_tokens.to_string())
            .arg("--temperature")
            .arg(self.temperature)
            .output()
            .map_err(|e| {
                ModelError::BackendUnavailable(format!(
                    "{} failed to start '{}': {}",
                    self.id,
                    self.binary_path.display(),
                    e
                ))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ModelError::BackendUnavailable(format!(
                "{} exited with {}: {}",
                self.id,
                output.status,
                stderr.trim()
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(self.clean_stdout(input, &stdout))
    }
}

#[async_trait::async_trait]
impl ChatBackend for LlamaGgufCommandBackend {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
        if !req.tools.is_empty() {
            return Err(ModelError::BackendUnavailable(format!(
                "{} does not support tool-use",
                self.id
            )));
        }
        let input = messages_to_string(&req.messages);
        let output = self.generate_sync(&input);
        Ok(make_simple_stream(output))
    }

    fn context_window(&self) -> u32 {
        2048
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn llama_gguf_binary_path() -> PathBuf {
    std::env::var_os("ROZUM_LLAMA_GGUF_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".tools/bin/llama-gguf"))
}

fn command_exists(path: &Path) -> bool {
    path.exists() || path.components().count() == 1
}

// ─── BackendRegistry ──────────────────────────────────────────────────────────

struct BackendEntry {
    config: BackendConfig,
    backend: Arc<dyn ChatBackend>,
}

#[derive(Default)]
pub struct BackendRegistry {
    entries: Vec<BackendEntry>,
}

impl BackendRegistry {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn from_configs(configs: impl IntoIterator<Item = BackendConfig>) -> Self {
        let mut registry = Self::new();
        for config in configs {
            if !config.enabled {
                continue;
            }
            match config.engine {
                BackendEngine::Hello => registry.add_backend(config, HelloBackend::new()),
                BackendEngine::Candle => add_candle_backend(&mut registry, config),
                BackendEngine::LlamaGguf => match LlamaGgufCommandBackend::from_config(&config) {
                    Ok(b) => registry.add_backend(config, b),
                    Err(_) => registry.add_backend(
                        config.clone(),
                        PlaceholderBackend::new(config.id.clone(), config.engine),
                    ),
                },
                BackendEngine::Gguf => add_gguf_backend(&mut registry, config),
                engine => registry.add_backend(
                    config.clone(),
                    PlaceholderBackend::new(config.id.clone(), engine),
                ),
            }
        }
        registry
    }

    pub fn add_backend<B>(&mut self, config: BackendConfig, backend: B)
    where
        B: ChatBackend + 'static,
    {
        self.entries.push(BackendEntry {
            config,
            backend: Arc::new(backend),
        });
    }

    pub fn get_backend(&self, backend_id: &str) -> Option<Arc<dyn ChatBackend>> {
        self.entries
            .iter()
            .find(|e| e.config.id == backend_id)
            .map(|e| Arc::clone(&e.backend))
    }

    pub fn configs(&self) -> impl Iterator<Item = &BackendConfig> {
        self.entries.iter().map(|e| &e.config)
    }

    pub fn backend_ids(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.config.id.as_str()).collect()
    }

    pub fn contains(&self, backend_id: &str) -> bool {
        self.get_backend(backend_id).is_some()
    }
}

#[cfg(feature = "local-models")]
fn add_candle_backend(registry: &mut BackendRegistry, config: BackendConfig) {
    match CandleBackend::from_config(&config) {
        Ok(b) => registry.add_backend(config, b),
        Err(_) => registry.add_backend(
            config.clone(),
            PlaceholderBackend::new(config.id.clone(), config.engine),
        ),
    }
}

#[cfg(not(feature = "local-models"))]
fn add_candle_backend(registry: &mut BackendRegistry, config: BackendConfig) {
    registry.add_backend(
        config.clone(),
        PlaceholderBackend::new(config.id.clone(), config.engine),
    );
}

#[cfg(feature = "gguf")]
fn add_gguf_backend(registry: &mut BackendRegistry, config: BackendConfig) {
    use crate::gguf::{GgufBackend, GgufOptions, resolve_model_path};
    let spec = config.model_spec.clone().or_else(|| {
        config
            .model_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
    });
    let path = spec.as_deref().and_then(resolve_model_path);
    match path {
        None => {
            tracing::warn!(
                id = %config.id,
                spec = ?spec,
                "gguf backend: could not resolve model path; using placeholder. \
                 Specify an absolute path or a 'lmstudio:' / 'ollama:' prefix."
            );
            registry.add_backend(
                config.clone(),
                PlaceholderBackend::new(config.id.clone(), config.engine),
            );
        }
        Some(model_path) => match GgufBackend::new(model_path, GgufOptions::default()) {
            Ok(b) => registry.add_backend(config, b),
            Err(e) => {
                tracing::warn!(id = %config.id, error = %e, "gguf backend: load failed; using placeholder");
                registry.add_backend(
                    config.clone(),
                    PlaceholderBackend::new(config.id.clone(), config.engine),
                );
            }
        },
    }
}

#[cfg(not(feature = "gguf"))]
fn add_gguf_backend(registry: &mut BackendRegistry, config: BackendConfig) {
    registry.add_backend(
        config.clone(),
        PlaceholderBackend::new(config.id.clone(), config.engine),
    );
}

// ─── BackendOrchestrator ──────────────────────────────────────────────────────

pub struct BackendOrchestrator {
    registry: BackendRegistry,
    policy: BackendPolicy,
}

impl Default for BackendOrchestrator {
    fn default() -> Self {
        Self::from_config(ModelRuntimeConfig::default())
    }
}

impl BackendOrchestrator {
    pub fn from_config(config: ModelRuntimeConfig) -> Self {
        Self {
            registry: BackendRegistry::from_configs(config.backends),
            policy: config.policy,
        }
    }

    pub const fn registry(&self) -> &BackendRegistry {
        &self.registry
    }

    pub const fn policy(&self) -> &BackendPolicy {
        &self.policy
    }

    /// Text generation with backend attribution (backward-compatible helper).
    pub async fn generate_detailed(&self, input: &str) -> ModelResult<GenerationResponse> {
        let req = ChatRequest::simple(input);
        match &self.policy {
            BackendPolicy::Single { backend_id } => {
                let backend = self
                    .registry
                    .get_backend(backend_id)
                    .ok_or_else(|| ModelError::BackendNotFound(backend_id.clone()))?;
                let stream = backend.chat(req).await?;
                let output = collect_to_string(stream).await?;
                Ok(GenerationResponse::new(backend_id.clone(), output))
            }
            BackendPolicy::Fallback { backend_ids } => {
                let mut errors = Vec::new();
                for backend_id in backend_ids {
                    match self.registry.get_backend(backend_id) {
                        None => errors.push(format!("{backend_id}: not found")),
                        Some(backend) => match backend.chat(req.clone()).await {
                            Err(e) => errors.push(format!("{backend_id}: {e}")),
                            Ok(stream) => match collect_to_string(stream).await {
                                Err(e) => errors.push(format!("{backend_id}: {e}")),
                                Ok(output) => {
                                    return Ok(GenerationResponse::new(backend_id.clone(), output));
                                }
                            },
                        },
                    }
                }
                Err(ModelError::NoBackendSucceeded(errors))
            }
            BackendPolicy::FanOut { backend_ids } => {
                let attempts = self.run_all_backends(backend_ids, input).await;
                for a in &attempts {
                    if let Ok(output) = &a.result {
                        return Ok(GenerationResponse::new(
                            a.backend_id.clone(),
                            output.clone(),
                        ));
                    }
                }
                Err(ModelError::NoBackendSucceeded(
                    attempts
                        .iter()
                        .filter_map(BackendAttempt::error_message)
                        .collect(),
                ))
            }
        }
    }

    /// Run all backends concurrently; return all attempts in policy order.
    pub async fn generate_all(&self, input: &str) -> Vec<BackendAttempt> {
        self.run_all_backends(self.policy.backend_ids(), input)
            .await
    }

    async fn run_all_backends(&self, backend_ids: &[String], input: &str) -> Vec<BackendAttempt> {
        let mut handles = Vec::new();
        let req = ChatRequest::simple(input);
        for backend_id in backend_ids {
            let backend_id = backend_id.clone();
            match self.registry.get_backend(&backend_id) {
                None => {
                    handles.push(tokio::spawn(async move {
                        BackendAttempt::new(
                            backend_id.clone(),
                            Err(ModelError::BackendNotFound(backend_id)),
                        )
                    }));
                }
                Some(backend) => {
                    let req = req.clone();
                    handles.push(tokio::spawn(async move {
                        let result = async {
                            let stream = backend.chat(req).await?;
                            collect_to_string(stream).await
                        }
                        .await;
                        BackendAttempt::new(backend_id, result)
                    }));
                }
            }
        }
        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            match handle.await {
                Ok(attempt) => results.push(attempt),
                Err(e) => results.push(BackendAttempt::new(
                    "unknown",
                    Err(ModelError::BackendUnavailable(format!(
                        "task panicked: {e}"
                    ))),
                )),
            }
        }
        results
    }
}

#[async_trait::async_trait]
impl ChatBackend for BackendOrchestrator {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
        match &self.policy {
            BackendPolicy::Single { backend_id } => {
                let backend = self
                    .registry
                    .get_backend(backend_id)
                    .ok_or_else(|| ModelError::BackendNotFound(backend_id.clone()))?;
                backend.chat(req).await
            }
            BackendPolicy::Fallback { backend_ids } => {
                let mut last_err = ModelError::NoBackendSucceeded(vec![]);
                for backend_id in backend_ids {
                    match self.registry.get_backend(backend_id) {
                        None => last_err = ModelError::BackendNotFound(backend_id.clone()),
                        Some(backend) => match backend.chat(req.clone()).await {
                            Ok(stream) => return Ok(stream),
                            Err(e) => last_err = e,
                        },
                    }
                }
                Err(last_err)
            }
            BackendPolicy::FanOut { backend_ids } => {
                for backend_id in backend_ids {
                    if let Some(backend) = self.registry.get_backend(backend_id) {
                        if let Ok(stream) = backend.chat(req.clone()).await {
                            return Ok(stream);
                        }
                    }
                }
                Err(ModelError::NoBackendSucceeded(vec![]))
            }
        }
    }

    fn context_window(&self) -> u32 {
        self.policy
            .backend_ids()
            .iter()
            .filter_map(|id| self.registry.get_backend(id))
            .map(|b| b.context_window())
            .min()
            .unwrap_or(0)
    }
}

// ─── Model catalog ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GgufModelSpec {
    pub id: &'static str,
    pub repository: &'static str,
    pub filename: &'static str,
    pub quantization: &'static str,
    pub size_bytes: u64,
    pub recommended: bool,
}

impl GgufModelSpec {
    pub fn download_url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/main/{}",
            self.repository, self.filename
        )
    }

    pub fn size_mb(&self) -> f64 {
        self.size_bytes as f64 / 1_000_000.0
    }
}

pub const SMOLLM2_135M_INSTRUCT_Q2_K: GgufModelSpec = GgufModelSpec {
    id: "smollm2-135m-instruct-q2-k",
    repository: "bartowski/SmolLM2-135M-Instruct-GGUF",
    filename: "SmolLM2-135M-Instruct-Q2_K.gguf",
    quantization: "Q2_K",
    size_bytes: 88_202_080,
    recommended: false,
};

pub const SMOLLM2_135M_INSTRUCT_Q4_K_M: GgufModelSpec = GgufModelSpec {
    id: "smollm2-135m-instruct-q4-k-m",
    repository: "bartowski/SmolLM2-135M-Instruct-GGUF",
    filename: "SmolLM2-135M-Instruct-Q4_K_M.gguf",
    quantization: "Q4_K_M",
    size_bytes: 105_454_432,
    recommended: true,
};

pub const GEMMA_3_270M_IT_Q2_K: GgufModelSpec = GgufModelSpec {
    id: "gemma-3-270m-it-q2-k",
    repository: "second-state/gemma-3-270m-it-GGUF",
    filename: "gemma-3-270m-it-Q2_K.gguf",
    quantization: "Q2_K",
    size_bytes: 237_079_040,
    recommended: false,
};

pub const QWEN3_06B_Q4_K_M: GgufModelSpec = GgufModelSpec {
    id: "qwen3-0.6b-q4-k-m",
    repository: "second-state/Qwen3-0.6B-GGUF",
    filename: "Qwen3-0.6B-Q4_K_M.gguf",
    quantization: "Q4_K_M",
    size_bytes: 484_219_552,
    recommended: false,
};

pub const TINY_GGUF_MODELS: &[GgufModelSpec] = &[
    SMOLLM2_135M_INSTRUCT_Q2_K,
    SMOLLM2_135M_INSTRUCT_Q4_K_M,
    GEMMA_3_270M_IT_Q2_K,
    QWEN3_06B_Q4_K_M,
];
