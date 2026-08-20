#![deny(unsafe_code)]

//! 面向模型的消息、模型提供方能力契约和兼容 OpenAI 的传输。
//!
//! 模型提供方协商和校验位于此边界，使 `AgentLoop` 只执行选定模型提供方已声明或探测到的
//! 请求和 tool call。

use std::fmt;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use singularity_core::CancellationToken;

const DEFAULT_MAX_TOOL_CALLS: u32 = 1;
/// 单次模型请求的默认 tool 数量上限。
pub const DEFAULT_MAX_TOOLS_PER_REQUEST: u32 = 8;
/// 默认模型上下文 token 上限。
pub const DEFAULT_MAX_CONTEXT_TOKENS: u32 = 128_000;
/// 默认模型输出 token 上限。
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
const MAX_CONFIGURED_CONTEXT_TOKENS: u32 = 2_000_000;
// Keep the configured ceiling above the currently documented 384k-output
// models; each model still must satisfy output < context.
const MAX_CONFIGURED_OUTPUT_TOKENS: u32 = 1_000_000;
const ENV_PROVIDER: &str = "SINGULARITY_MODEL_PROVIDER";
const ENV_MODEL: &str = "SINGULARITY_MODEL";
/// Explicit path to the immutable multi-provider model configuration JSON.
pub const ENV_MODELS_CONFIG: &str = "SINGULARITY_MODELS_CONFIG";
const ENV_CONTEXT_TOKENS: &str = "SINGULARITY_MODEL_CONTEXT_TOKENS";
const ENV_MAX_OUTPUT_TOKENS: &str = "SINGULARITY_MODEL_MAX_OUTPUT_TOKENS";
const ENV_BASE_URL: &str = "SINGULARITY_BASE_URL";
const ENV_API_KEY: &str = "SINGULARITY_API_KEY";
const DEFAULT_PROVIDER_NAME: &str = "openai_compatible";
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const V1_CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
const RESPONSES_PATH: &str = "/responses";
const V1_RESPONSES_PATH: &str = "/v1/responses";
const USER_CONFIG_DIR_NAME: &str = ".singularity";
const USER_CONFIG_FILE_NAME: &str = "config.json";
const USER_AUTH_GENERATION_PREFIX: &str = "auth.v1-";
const USER_AUTH_SCHEMA_VERSION: u32 = 1;
const USER_MODELS_CACHE_FILE_NAME: &str = "models-cache.json";
const USER_MODELS_CACHE_SCHEMA_VERSION: u32 = 1;
const USER_MODELS_CACHE_TTL_SECONDS: u64 = 24 * 60 * 60;
const MAX_DISCOVERED_MODEL_IDS: usize = 1024;
const MAX_MODEL_ID_LENGTH: usize = 512;
const MAX_DISCOVERY_RESPONSE_BYTES: usize = 1024 * 1024;
const PROVIDER_TIMEOUT_SECONDS: u64 = 120;
const PROVIDER_RUNTIME_WORKER_THREADS: usize = 2;
const PROVIDER_RUNTIME_INITIALIZATION_ERROR_CODE: &str = "provider_runtime_initialization_failed";
const PROVIDER_CANCELLATION_POLL_MS: u64 = 25;
const MAX_PROVIDER_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;
/// 单次 provider complete 的最大 HTTP attempt 次数（首次尝试之外最多重试 5 次）。
const MAX_PROVIDER_ATTEMPTS: u32 = 6;
const PROVIDER_RETRY_BASE_BACKOFF_MS: u64 = 500;
const PROVIDER_RETRY_MAX_BACKOFF_MS: u64 = 60_000;
/// Provider boundary code used when a protocol has no normalized text stream.
pub const PROVIDER_STREAMING_UNSUPPORTED_CODE: &str = "provider_streaming_unsupported";
const PROVIDER_SNAPSHOT_ID_PREFIX: &str = "provider_snapshot_";
const TEXT_TOOL_CALL_ENVELOPE_ERROR: &str = "text_tool_call_envelope_not_supported";
const HTTP_STATUS_UNAUTHORIZED: u16 = 401;
const HTTP_STATUS_FORBIDDEN: u16 = 403;
const HTTP_STATUS_REQUEST_TIMEOUT: u16 = 408;
const HTTP_STATUS_NOT_FOUND: u16 = 404;
const HTTP_STATUS_RATE_LIMITED: u16 = 429;
const HTTP_STATUS_INTERNAL_SERVER_ERROR: u16 = 500;

mod builtin_models;
mod config;
mod contract;
mod discovery;
mod error;
mod openai;
mod provider;
mod transport;
mod types;

pub use config::{import_env_to_user_config, read_user_model_catalog, resolve_provider_config};
pub use contract::{
    is_strict_tool_schema_compatible, validate_model_request,
    validate_model_request_with_capabilities, validate_model_response,
    validate_model_turn_response, validate_provider_config,
};
pub use error::*;
pub use openai::{
    chat_completions_endpoint, models_endpoint, provider_error_response, responses_endpoint,
};
pub use provider::runtime::OpenAiProviderConfig;
pub(crate) use provider::runtime::{ProviderRuntime, SelectedModel};
pub use provider::*;
pub use types::*;

#[cfg(test)]
use config::provider_initialization_blocker;
#[cfg(test)]
use transport::provider_runtime_error;

/// 为模型提供方完成请求选定的线路协议。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderApiProtocol {
    #[default]
    Declared,
    OpenAiResponses,
    OpenAiChatCompletions,
}

/// Chat Completions reasoning fields are selected explicitly by the model
/// catalog.  No provider or model name is interpreted to choose a wire shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingWireFormat {
    /// Existing `thinking: {"type": "enabled|disabled"}` fields.
    ThinkingType,
    /// Top-level `enable_thinking` boolean used by providers that document it.
    EnableThinking,
}

/// State of one provider's `/models` discovery record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelDiscoveryStatus {
    Fresh,
    Stale,
    Unavailable,
    NotConfigured,
}

/// Result of reading or refreshing the optional user model discovery cache.
///
/// Cache state never changes whether the provider configuration itself is
/// usable; it only explains why discovery used live, stale, or no cached ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelCacheStatus {
    NotPresent,
    Valid,
    Invalid,
    ReadFailed,
    WriteFailed,
}

/// A discovered model id and whether an explicit capability override makes it
/// safe to select for execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserModelCatalogEntry {
    pub id: String,
    pub discovered: bool,
    pub explicit: bool,
    pub selectable: bool,
    pub max_context_tokens: Option<u32>,
    pub reasoning_variants: Vec<String>,
    pub default_variant: Option<String>,
}

/// Redacted user-level provider catalog returned by `sg config models`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserProviderModelCatalog {
    pub provider_name: String,
    pub base_url_present: bool,
    pub api_key_present: bool,
    pub discovery: ModelDiscoveryStatus,
    pub models: Vec<UserModelCatalogEntry>,
    pub error: Option<String>,
}

/// Redacted user-level model catalog.  It never contains a base URL or secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserModelCatalog {
    pub default_selector: Option<String>,
    pub cache_status: ModelCacheStatus,
    pub providers: Vec<UserProviderModelCatalog>,
}

/// Outcome of importing a dotenv file into the user-level split config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserConfigImportResult {
    pub config_path: String,
    pub auth_path: String,
    pub provider_name: String,
    pub default_selector: Option<String>,
    pub selectable: bool,
}

/// 模型提供方必须遵守、用于构建请求和校验响应的能力。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderProtocolContract {
    pub supports_tools: bool,
    pub supports_parallel_tool_calls: bool,
    pub supports_strict_tool_schema: bool,
    pub tool_reasoning_mode: ProviderToolReasoningMode,
    pub max_tools_per_request: u32,
    pub supports_system_message: bool,
    pub supports_developer_message: bool,
    /// 单次请求与本地工具工作窗口的最大并行调用数。
    pub max_parallel_tool_calls: u32,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: u32,
}

impl Default for ProviderProtocolContract {
    fn default() -> Self {
        Self {
            supports_tools: true,
            supports_parallel_tool_calls: false,
            supports_strict_tool_schema: false,
            tool_reasoning_mode: ProviderToolReasoningMode::Unspecified,
            max_tools_per_request: DEFAULT_MAX_TOOLS_PER_REQUEST,
            supports_system_message: true,
            supports_developer_message: true,
            max_parallel_tool_calls: 1,
            max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        }
    }
}

/// 模型条目的显式能力声明（旧 probe 时代 config.json 的 `capabilities` 块）。
///
/// 全部字段可选：顶层配置字段优先，其次本声明，最后 `ProviderProtocolContract`
/// 默认值。`supports_reasoning` 只解析接受；`max_parallel_tool_calls` 同时约束
/// 请求声明与 Agent 本地工具工作窗口。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct ProviderCapabilityDeclaration {
    pub supports_tools: Option<bool>,
    pub supports_parallel_tool_calls: Option<bool>,
    pub supports_strict_tool_schema: Option<bool>,
    pub supports_system_message: Option<bool>,
    pub supports_developer_message: Option<bool>,
    pub supports_reasoning: Option<bool>,
    pub max_tools_per_request: Option<u32>,
    pub max_parallel_tool_calls: Option<u32>,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

/// Wire form for the persisted capability block. The two underscored fields
/// were removed from the active contract but remain in configurations written
/// by earlier releases; accepting them here preserves those user files while
/// keeping the removed concepts out of the runtime declaration.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
struct ProviderCapabilityDeclarationWire {
    supports_tools: Option<bool>,
    supports_parallel_tool_calls: Option<bool>,
    #[serde(rename = "supports_required_tool_choice", default)]
    _supports_required_tool_choice: Option<bool>,
    supports_strict_tool_schema: Option<bool>,
    #[serde(rename = "supports_json_mode", default)]
    _supports_json_mode: Option<bool>,
    supports_system_message: Option<bool>,
    supports_developer_message: Option<bool>,
    supports_reasoning: Option<bool>,
    max_tools_per_request: Option<u32>,
    max_parallel_tool_calls: Option<u32>,
    max_context_tokens: Option<u32>,
    max_output_tokens: Option<u32>,
}

impl<'de> serde::Deserialize<'de> for ProviderCapabilityDeclaration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ProviderCapabilityDeclarationWire::deserialize(deserializer)?;
        Ok(Self {
            supports_tools: wire.supports_tools,
            supports_parallel_tool_calls: wire.supports_parallel_tool_calls,
            supports_strict_tool_schema: wire.supports_strict_tool_schema,
            supports_system_message: wire.supports_system_message,
            supports_developer_message: wire.supports_developer_message,
            supports_reasoning: wire.supports_reasoning,
            max_tools_per_request: wire.max_tools_per_request,
            max_parallel_tool_calls: wire.max_parallel_tool_calls,
            max_context_tokens: wire.max_context_tokens,
            max_output_tokens: wire.max_output_tokens,
        })
    }
}

/// `AgentLoop` 为完成请求提供的可选模型参数。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelPreferences {
    pub model_name: Option<String>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,
}

/// 脱敏的模型提供方配置存在性信息；这里永不存储敏感信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelProviderConfig {
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub base_url_present: bool,
    pub api_key_present: bool,
}

/// 解析出有效模型提供方配置值的配置层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderConfigSource {
    ProcessEnvironment,
    UserConfigFile,
}

impl ProviderConfigSource {
    /// 返回配置来源的稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProcessEnvironment => "process_env",
            Self::UserConfigFile => "user_config",
        }
    }
}

/// 构建模型提供方前解析得到的来源和脱敏配置状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfigResolution {
    pub source: Option<ProviderConfigSource>,
    pub config: ModelProviderConfig,
}

/// 服务级模型提供方配置快照，包含脱敏状态和已初始化的模型提供方。
///
/// 只捕获一次，使 `AppServer` 报告和使用同一份配置，同时不暴露 API 密钥或其他原始环境值。
#[derive(Clone)]
pub struct ProviderConfigSnapshot {
    snapshot_id: String,
    source: Option<ProviderConfigSource>,
    redacted_config: ModelProviderConfig,
    configuration: ProviderConfigurationStatus,
    provider: Result<OpenAiProvider, ProviderError>,
    model_selection: Option<std::sync::Arc<config::ModelSelectionSnapshot>>,
}

/// 模型提供方初始化无法继续时报告的稳定阻塞类别。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelBlockerKind {
    RequiredEnvMissing,
    AuthenticationProviderError,
    BaseUrlNetworkError,
    ModelNameConfigError,
    ProviderRuntimeUnavailable,
}

impl ModelBlockerKind {
    /// 返回阻塞类别代码。
    pub fn code(&self) -> &'static str {
        match self {
            Self::RequiredEnvMissing => "required_env_missing",
            Self::AuthenticationProviderError => "authentication_provider_error",
            Self::BaseUrlNetworkError => "base_url_network_error",
            Self::ModelNameConfigError => "model_name_config_error",
            Self::ProviderRuntimeUnavailable => "provider_runtime_unavailable",
        }
    }

    /// 返回阻塞类别说明。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RequiredEnvMissing => "required env missing",
            Self::AuthenticationProviderError => "authentication/provider error",
            Self::BaseUrlNetworkError => "base_url/network error",
            Self::ModelNameConfigError => "model name/config error",
            Self::ProviderRuntimeUnavailable => "provider runtime unavailable",
        }
    }
}

/// 暴露给 `AppServer` 的脱敏模型提供方就绪状态和阻塞信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderConfigurationStatus {
    pub configured: bool,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub api_key_status: String,
    pub base_url_status: String,
    pub blocker: Option<ModelBlockerKind>,
}

/// 协商能力并校验每次完成请求的兼容 OpenAI 模型提供方。
#[derive(Clone)]
pub struct OpenAiProvider {
    config: OpenAiProviderConfig,
    /// Explicit model-catalog clones carry one immutable, fully resolved selection.
    selected_model: Option<SelectedModel>,
    client: reqwest::Client,
    /// 所有 provider clone 共享同一 runtime ownership 绑定。
    runtime: Arc<ProviderRuntime>,
    request_timeout_seconds: u64,
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("config", &self.config)
            .field("client", &"[redacted]")
            .field("runtime", &"[shared]")
            .field("request_timeout_seconds", &self.request_timeout_seconds)
            .finish()
    }
}

/// Build the stable unsupported result used by Chat, Declared, and legacy providers.
pub(crate) fn provider_streaming_unsupported_error() -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::UnsupportedCapability,
            "provider streaming is unsupported for this protocol",
        )
        .with_provider_diagnostic(
            PROVIDER_STREAMING_UNSUPPORTED_CODE,
            ProviderErrorStage::ResponseValidation,
        ),
    )
}

#[cfg(test)]
mod transport_tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::Duration;

    use super::*;

    fn test_provider_config(base_url: String) -> OpenAiProviderConfig {
        OpenAiProviderConfig {
            provider_name: "openai_compatible".to_string(),
            model_name: "gpt-test".to_string(),
            base_url,
            api_key: "sk-secret-value".to_string(),
            source: ProviderConfigSource::ProcessEnvironment,
            max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        }
    }

    fn read_test_provider_request(stream: &TcpStream) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone provider stream"));
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("read provider request line");
        assert!(line.contains("/v1/chat/completions"));
    }

    fn write_test_provider_response(stream: &mut TcpStream) {
        let body = r#"{"id":"response_1","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("write provider response");
    }

    fn concurrent_provider_server() -> (String, Receiver<usize>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind concurrent provider");
        let address = listener.local_addr().expect("concurrent provider address");
        let (maximum_tx, maximum_rx) = mpsc::channel();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let server = thread::spawn({
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            move || {
                let mut workers = Vec::new();
                for _ in 0..2 {
                    let (mut stream, _) = listener.accept().expect("accept concurrent request");
                    let active = Arc::clone(&active);
                    let maximum = Arc::clone(&maximum);
                    workers.push(thread::spawn(move || {
                        read_test_provider_request(&stream);
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        maximum.fetch_max(current, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(150));
                        write_test_provider_response(&mut stream);
                        active.fetch_sub(1, Ordering::SeqCst);
                    }));
                }
                for worker in workers {
                    worker.join().expect("join concurrent provider request");
                }
                maximum_tx
                    .send(maximum.load(Ordering::SeqCst))
                    .expect("send concurrent provider maximum");
            }
        });
        (format!("http://{address}"), maximum_rx, server)
    }

    fn cancellation_followup_server() -> (String, Receiver<()>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind cancellation provider");
        let address = listener
            .local_addr()
            .expect("cancellation provider address");
        let (first_request_tx, first_request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (first_stream, _) = listener.accept().expect("accept cancelled request");
            read_test_provider_request(&first_stream);
            first_request_tx
                .send(())
                .expect("send cancelled request started");

            let (mut followup_stream, _) = listener.accept().expect("accept follow-up request");
            read_test_provider_request(&followup_stream);
            write_test_provider_response(&mut followup_stream);
            thread::sleep(Duration::from_millis(100));
        });
        (format!("http://{address}"), first_request_rx, server)
    }

    #[test]
    fn configured_deadline_is_reported_from_a_real_transport_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind hanging provider");
        let address = listener.local_addr().expect("provider address");
        let server = thread::spawn(move || {
            let mut streams = Vec::new();
            // 超时（挂起）不再重试：只接受 1 个连接。
            for _ in 0..1 {
                let (stream, _) = listener.accept().expect("accept provider request");
                let mut reader = BufReader::new(stream.try_clone().expect("clone provider stream"));
                let mut line = String::new();
                reader.read_line(&mut line).expect("read request line");
                streams.push(stream);
            }
            thread::sleep(Duration::from_secs(2));
        });
        let provider = OpenAiProvider::new_with_request_timeout(
            OpenAiProviderConfig {
                provider_name: "openai_compatible".to_string(),
                model_name: "gpt-test".to_string(),
                base_url: format!("http://{address}"),
                api_key: "sk-secret-value".to_string(),
                source: ProviderConfigSource::ProcessEnvironment,
                max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
                max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            },
            1,
        )
        .expect("provider");
        let request = ModelTurnRequest::new(
            "request_timeout",
            vec![ModelMessage::text(ModelRole::User, "hello")],
        );

        let error = provider
            .complete(&request, &CancellationToken::new())
            .expect_err("provider request must time out");

        assert_eq!(error.error.kind, ModelErrorKind::Timeout);
        assert_eq!(
            error.error.code.as_deref(),
            Some("provider_request_send_failed")
        );
        assert_eq!(error.error.stage, Some(ProviderErrorStage::RequestSend));
        assert_eq!(
            error.error.transport_category,
            Some(ProviderTransportCategory::Timeout)
        );
        assert_eq!(error.error.timeout_seconds, Some(1));
        let metadata = error
            .provider_attempt_metadata
            .as_ref()
            .expect("timeout attempt metadata");
        // 超时（挂起）不再重试：单次 120s 超时即失败，避免 6 次重试拖 12 分钟。
        assert_eq!(metadata.attempt_count, 1);
        assert_eq!(metadata.retry_count, 0);
        assert_eq!(metadata.occurrences.len(), 1);
        for (index, occurrence) in metadata.occurrences.iter().enumerate() {
            assert_eq!(occurrence.attempt_index, index as u32 + 1);
            assert_eq!(occurrence.terminal_status, ProviderAttemptStatus::Error);
            assert_eq!(occurrence.error_category, Some(ModelErrorCategory::Network));
            assert_eq!(
                occurrence.error_stage,
                Some(ProviderErrorStage::RequestSend)
            );
            assert_eq!(
                occurrence.diagnostic_code.as_deref(),
                Some("provider_request_send_failed")
            );
            assert!(occurrence.request_send_to_headers_ms.is_none());
            assert!(!occurrence.retry_scheduled);
        }
        let serialized = serde_json::to_string(&error.error).expect("serialize timeout");
        for secret in ["sk-secret-value", &address.to_string(), "authorization"] {
            assert!(
                !serialized
                    .to_ascii_lowercase()
                    .contains(&secret.to_ascii_lowercase())
            );
        }
        server.join().expect("provider server");
    }

    #[test]
    fn oversized_success_body_is_rejected_before_buffering() {
        const OVERSIZED_RESPONSE_BYTES: usize = 8 * 1024 * 1024 + 1;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind oversized provider");
        let address = listener.local_addr().expect("provider address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone provider stream"));
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read request");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {OVERSIZED_RESPONSE_BYTES}\r\nconnection: close\r\n\r\n"
            )
            .expect("write oversized response headers");
        });
        let provider = OpenAiProvider::new(OpenAiProviderConfig {
            provider_name: "openai_compatible".to_string(),
            model_name: "gpt-test".to_string(),
            base_url: format!("http://{address}"),
            api_key: "sk-secret-value".to_string(),
            source: ProviderConfigSource::ProcessEnvironment,
            max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        })
        .expect("provider");
        let request = ModelTurnRequest::new(
            "request_oversized",
            vec![ModelMessage::text(ModelRole::User, "hello")],
        );

        let error = provider
            .complete(&request, &CancellationToken::new())
            .expect_err("oversized provider response must fail closed");

        assert_eq!(error.error.kind, ModelErrorKind::JsonSchemaViolation);
        assert_eq!(
            error.error.code.as_deref(),
            Some("provider_response_body_too_large")
        );
        assert_eq!(
            error.error.stage,
            Some(ProviderErrorStage::ResponseBodyRead)
        );
        assert_eq!(
            error.error.validation_errors,
            ["provider_response_body_too_large"]
        );
        let metadata = error
            .provider_attempt_metadata
            .as_ref()
            .expect("oversized response attempt metadata");
        assert_eq!(metadata.attempt_count, 1);
        assert_eq!(metadata.retry_count, 0);
        server.join().expect("provider server");
    }

    #[test]
    fn provider_clones_share_a_runtime_and_requests_progress_concurrently() {
        let (base_url, maximum_rx, server) = concurrent_provider_server();
        let provider = OpenAiProvider::new(test_provider_config(base_url)).expect("provider");
        let cloned = provider.clone();
        assert!(Arc::ptr_eq(&provider.runtime, &cloned.runtime));

        let provider = Arc::new(provider);
        let start = Arc::new(std::sync::Barrier::new(3));
        let (result_tx, result_rx) = mpsc::channel();
        let mut callers = Vec::new();
        for request_id in ["request_concurrent_a", "request_concurrent_b"] {
            let provider = Arc::clone(&provider);
            let start = Arc::clone(&start);
            let result_tx = result_tx.clone();
            callers.push(thread::spawn(move || {
                let request = ModelTurnRequest::new(
                    request_id,
                    vec![ModelMessage::text(ModelRole::User, "hello")],
                );
                start.wait();
                result_tx
                    .send(provider.complete(&request, &CancellationToken::new()))
                    .expect("send concurrent provider result");
            }));
        }
        start.wait();

        for _ in 0..2 {
            result_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("concurrent provider request completed")
                .expect("concurrent provider request succeeded");
        }
        for caller in callers {
            caller.join().expect("join concurrent provider caller");
        }
        assert_eq!(
            maximum_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("concurrent provider maximum"),
            2,
            "shared runtime must not serialize provider requests"
        );
        server.join().expect("join concurrent provider server");
    }

    #[test]
    fn cancelled_request_does_not_poison_followup_on_the_shared_runtime() {
        let (base_url, first_request_rx, server) = cancellation_followup_server();
        let provider = OpenAiProvider::new(test_provider_config(base_url)).expect("provider");
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let worker_provider = provider.clone();
        let worker = thread::spawn(move || {
            let request = ModelTurnRequest::new(
                "request_cancel_shared_runtime",
                vec![ModelMessage::text(ModelRole::User, "wait")],
            );
            worker_provider.complete(&request, &worker_cancellation)
        });

        first_request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cancelled request started");
        cancellation.cancel();
        let error = worker
            .join()
            .expect("join cancelled provider caller")
            .expect_err("cancelled request must fail");
        assert_eq!(error.error.kind, ModelErrorKind::Cancelled);

        let followup = ModelTurnRequest::new(
            "request_after_cancel_shared_runtime",
            vec![ModelMessage::text(ModelRole::User, "hello")],
        );
        provider
            .complete(&followup, &CancellationToken::new())
            .expect("follow-up request must still succeed");
        server.join().expect("join cancellation provider server");
    }

    #[test]
    fn timed_out_request_does_not_poison_followup_on_the_shared_runtime() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind timeout provider");
        let address = listener.local_addr().expect("timeout provider address");
        let server = thread::spawn(move || {
            let mut hanging_streams = Vec::new();
            // 超时（挂起）不再重试：只接受 1 个挂起连接。
            for _ in 0..1 {
                let (stream, _) = listener.accept().expect("accept timed-out request");
                read_test_provider_request(&stream);
                hanging_streams.push(stream);
            }

            let (mut followup_stream, _) = listener.accept().expect("accept timeout follow-up");
            read_test_provider_request(&followup_stream);
            write_test_provider_response(&mut followup_stream);
            drop(hanging_streams);
        });
        let provider = OpenAiProvider::new_with_request_timeout(
            test_provider_config(format!("http://{address}")),
            1,
        )
        .expect("provider");
        let timed_out_request = ModelTurnRequest::new(
            "request_timeout_shared_runtime",
            vec![ModelMessage::text(ModelRole::User, "wait")],
        );
        let error = provider
            .complete(&timed_out_request, &CancellationToken::new())
            .expect_err("provider request must time out");
        assert_eq!(error.error.kind, ModelErrorKind::Timeout);
        assert_eq!(
            error
                .provider_attempt_metadata
                .as_ref()
                .expect("timeout metadata")
                .attempt_count,
            // 超时（挂起）不再重试：单次超时即失败。
            1
        );

        let followup = ModelTurnRequest::new(
            "request_after_timeout_shared_runtime",
            vec![ModelMessage::text(ModelRole::User, "hello")],
        );
        provider
            .complete(&followup, &CancellationToken::new())
            .expect("follow-up request must still succeed");
        server.join().expect("join timeout provider server");
    }

    #[test]
    fn streaming_response_timeout_is_idle_not_total() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind slow streaming provider");
        let address = listener
            .local_addr()
            .expect("slow streaming provider address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept streaming request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone streaming stream"));
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).expect("read streaming request");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n"
            )
            .expect("write streaming headers");
            let events = [
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"do\"}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ne\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
            ];
            for (index, event) in events.iter().enumerate() {
                if index > 0 {
                    thread::sleep(Duration::from_millis(400));
                }
                write!(stream, "{:X}\r\n{}\r\n", event.len(), event)
                    .expect("write streaming event");
                stream.flush().expect("flush streaming event");
            }
            write!(stream, "0\r\n\r\n").expect("write streaming terminator");
        });

        let provider = OpenAiProvider::new_with_request_timeout(
            test_provider_config(format!("http://{address}/v1")),
            1,
        )
        .expect("provider")
        .with_selected_model(SelectedModel {
            model_name: "gpt-test".to_string(),
            api_protocol: ProviderApiProtocol::OpenAiResponses,
            max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            reasoning_variant: None,
            reasoning_enabled: false,
            wire_reasoning_effort: None,
            thinking_wire_format: ThinkingWireFormat::ThinkingType,
            tool_reasoning_mode: ProviderToolReasoningMode::DisabledForToolCalls,
            supports_developer_role: true,
            supports_tool_choice: true,
            requires_reasoning_content_for_tool_calls: false,
            requires_assistant_content_for_tool_calls: false,
            capability_overrides: None,
        });
        let request = ModelTurnRequest::new(
            "slow_streaming_response",
            vec![ModelMessage::text(ModelRole::User, "hello")],
        );

        let mut events = Vec::new();
        let response = provider
            .complete_stream(&request, &CancellationToken::new(), &mut |event| {
                events.push(event);
            })
            .expect("streaming response must survive a total duration above the idle timeout");

        assert_eq!(events.len(), 2);
        assert_eq!(
            response
                .assistant_message
                .as_ref()
                .expect("assistant message")
                .content,
            "done"
        );
        server.join().expect("join slow streaming provider");
    }

    #[test]
    fn runtime_initialization_failure_maps_to_a_stable_provider_blocker() {
        let error = provider_runtime_error(std::io::Error::other(
            "synthetic runtime initialization failure",
        ));
        assert_eq!(
            error.error.code.as_deref(),
            Some(PROVIDER_RUNTIME_INITIALIZATION_ERROR_CODE)
        );
        assert_eq!(
            provider_initialization_blocker(&error.error),
            Some(ModelBlockerKind::ProviderRuntimeUnavailable)
        );
    }

    /// 契约透传：选中模型的 tool_reasoning_mode 必须反映到 protocol_contract()
    /// （修复前硬编码 Unspecified，agent 侧续接投影永远被跳过）。
    #[test]
    fn protocol_contract_exposes_selected_tool_reasoning_mode() {
        let config = test_provider_config("http://127.0.0.1:1/v1".to_string());
        let provider = OpenAiProvider::new(config)
            .expect("provider")
            .with_selected_model(SelectedModel {
                model_name: "gpt-test".to_string(),
                api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
                max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
                max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
                reasoning_variant: Some("on".to_string()),
                reasoning_enabled: true,
                wire_reasoning_effort: None,
                thinking_wire_format: ThinkingWireFormat::ThinkingType,
                tool_reasoning_mode: ProviderToolReasoningMode::ReplayReasoningContent,
                supports_developer_role: true,
                supports_tool_choice: true,
                requires_reasoning_content_for_tool_calls: true,
                requires_assistant_content_for_tool_calls: false,
                capability_overrides: None,
            });
        assert_eq!(
            provider.protocol_contract().tool_reasoning_mode,
            ProviderToolReasoningMode::ReplayReasoningContent
        );
    }
}
