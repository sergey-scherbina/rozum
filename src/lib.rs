mod backend;
pub mod agent;
pub mod concurrency;
pub mod config;
pub mod constrain;
pub mod discord;
pub mod gateway;
pub mod gguf;
pub mod hf_hub;
pub mod meeting;
pub mod mistralrs_backend;
pub mod mlx_native_backend;
pub mod models;
pub mod modelscope;
pub mod obs;
pub mod anthropic_http;
pub mod openai_http;
pub mod proxy;
pub mod share;
pub mod telegram;
pub mod tui;
pub mod web;

#[cfg(feature = "local-models")]
pub use backend::CandleBackend;
pub use backend::{
    BackendAttempt, BackendConfig, BackendEngine, BackendOrchestrator, BackendPolicy,
    BackendRegistry, ChatBackend, ChatEvent, ChatRequest, ChatStream, ContentBlock,
    GEMMA_3_270M_IT_Q2_K, GenerationResponse, GgufModelSpec, HelloBackend, LlamaGgufCommandBackend,
    Message, ModelError, ModelResult, ModelRuntimeConfig, PlaceholderBackend, QWEN3_06B_Q4_K_M,
    Role, SMOLLM2_135M_INSTRUCT_Q2_K, SMOLLM2_135M_INSTRUCT_Q4_K_M, SamplingParams, StopReason,
    TINY_GGUF_MODELS, ToolDef, collect_to_string,
};
pub use config::{BackendChoice, ConfigError, Policy, RuntimeConfig};

#[derive(Clone, Copy, Debug, Default)]
pub struct AiModel<B = HelloBackend> {
    backend: B,
}

impl AiModel<HelloBackend> {
    pub const fn new() -> Self {
        Self {
            backend: HelloBackend::new(),
        }
    }

    pub fn respond(&self, _input: &str) -> &'static str {
        "hello!"
    }
}

impl AiModel<BackendOrchestrator> {
    pub fn from_runtime_config(config: ModelRuntimeConfig) -> Self {
        Self::from_backend(BackendOrchestrator::from_config(config))
    }
}

impl<B> AiModel<B> {
    pub const fn from_backend(backend: B) -> Self {
        Self { backend }
    }

    pub const fn backend(&self) -> &B {
        &self.backend
    }
}

impl<B: ChatBackend> AiModel<B> {
    pub async fn generate(&self, input: &str) -> ModelResult<String> {
        let req = ChatRequest::simple(input);
        let stream = self.backend.chat(req).await?;
        collect_to_string(stream).await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AiModel, BackendConfig, BackendEngine, BackendOrchestrator, BackendPolicy, HelloBackend,
        ModelError, ModelRuntimeConfig, SMOLLM2_135M_INSTRUCT_Q2_K, SMOLLM2_135M_INSTRUCT_Q4_K_M,
        TINY_GGUF_MODELS,
    };

    #[test]
    fn responds_with_hello() {
        let model = AiModel::new();
        assert_eq!(model.respond(""), "hello!");
        assert_eq!(model.respond("anything"), "hello!");
    }

    #[tokio::test]
    async fn generates_with_default_backend() {
        let model = AiModel::new();
        assert_eq!(model.generate("").await.unwrap(), "hello!");
        assert_eq!(model.generate("anything").await.unwrap(), "hello!");
    }

    #[tokio::test]
    async fn can_construct_from_backend() {
        let model = AiModel::from_backend(HelloBackend::new());
        assert_eq!(model.generate("test").await.unwrap(), "hello!");
    }

    #[test]
    fn tiny_catalog_contains_smallest_and_recommended_models() {
        assert!(TINY_GGUF_MODELS.contains(&SMOLLM2_135M_INSTRUCT_Q2_K));
        assert!(TINY_GGUF_MODELS.contains(&SMOLLM2_135M_INSTRUCT_Q4_K_M));
        assert!(SMOLLM2_135M_INSTRUCT_Q2_K.size_bytes < SMOLLM2_135M_INSTRUCT_Q4_K_M.size_bytes);
        assert!(SMOLLM2_135M_INSTRUCT_Q4_K_M.recommended);
        assert_eq!(SMOLLM2_135M_INSTRUCT_Q4_K_M.size_bytes, 105_454_432);
    }

    #[test]
    fn model_specs_build_download_urls() {
        let url = SMOLLM2_135M_INSTRUCT_Q4_K_M.download_url();
        assert!(url.starts_with("https://huggingface.co/"));
        assert!(url.ends_with("/SmolLM2-135M-Instruct-Q4_K_M.gguf"));
    }

    #[tokio::test]
    async fn default_orchestrator_uses_hello_backend() {
        let model = AiModel::from_runtime_config(ModelRuntimeConfig::default());
        assert_eq!(model.generate("test").await.unwrap(), "hello!");
    }

    #[test]
    fn preview_config_declares_multiple_backend_engines() {
        let config = ModelRuntimeConfig::tiny_multi_backend_preview();
        let engines: Vec<_> = config.backends.iter().map(|b| b.engine).collect();
        assert_eq!(
            engines,
            vec![
                BackendEngine::Candle,
                BackendEngine::LlamaGguf,
                BackendEngine::NativeRust,
                BackendEngine::Hello
            ]
        );
        assert_eq!(
            config.policy.describe(),
            "fallback:candle-smollm2-q4,llama-gguf-smollm2-q4,native-smollm2-q4,hello"
        );
    }

    #[tokio::test]
    async fn fallback_policy_returns_first_successful_backend() {
        let orchestrator = BackendOrchestrator::from_config(ModelRuntimeConfig {
            backends: vec![
                BackendConfig::new("candle", BackendEngine::Candle),
                BackendConfig::hello("hello"),
            ],
            policy: BackendPolicy::fallback(vec!["candle".to_owned(), "hello".to_owned()]),
        });
        let response = orchestrator.generate_detailed("test").await.unwrap();
        assert_eq!(response.backend_id, "hello");
        assert_eq!(response.output, "hello!");
    }

    #[tokio::test]
    async fn fanout_policy_exposes_all_backend_attempts() {
        let orchestrator = BackendOrchestrator::from_config(ModelRuntimeConfig {
            backends: vec![
                BackendConfig::new("candle", BackendEngine::Candle),
                BackendConfig::llama_gguf("llama-gguf", "models/missing.gguf"),
                BackendConfig::new("native", BackendEngine::NativeRust),
                BackendConfig::hello("hello"),
            ],
            policy: BackendPolicy::fanout(vec![
                "candle".to_owned(),
                "llama-gguf".to_owned(),
                "native".to_owned(),
                "hello".to_owned(),
            ]),
        });
        let attempts = orchestrator.generate_all("test").await;
        assert_eq!(attempts.len(), 4);
        assert_eq!(attempts[0].backend_id, "candle");
        assert!(!attempts[0].succeeded());
        assert_eq!(attempts[1].backend_id, "llama-gguf");
        assert!(!attempts[1].succeeded());
        assert_eq!(attempts[2].backend_id, "native");
        assert!(!attempts[2].succeeded());
        assert_eq!(attempts[3].backend_id, "hello");
        assert!(attempts[3].succeeded());
    }

    #[tokio::test]
    async fn missing_backend_id_is_reported_clearly() {
        let config = ModelRuntimeConfig {
            backends: vec![BackendConfig::hello("hello")],
            policy: BackendPolicy::single("missing"),
        };
        let orchestrator = BackendOrchestrator::from_config(config);
        assert_eq!(
            orchestrator.generate_detailed("test").await.unwrap_err(),
            ModelError::BackendNotFound("missing".to_owned())
        );
    }
}
