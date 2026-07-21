#![deny(unsafe_code)]

//! 面向模型的消息、模型提供方能力契约和兼容 OpenAI 的传输。
//!
//! 模型提供方协商和校验位于此边界，使 `AgentLoop` 只执行选定模型提供方已声明或探测到的
//! 请求和 tool call。

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cap_fs_ext::{
    FollowSymlinks, MetadataExt as CapMetadataExt, OpenOptionsFollowExt, OpenOptionsSyncExt,
};
use cap_std::fs::{Dir as CapabilityDir, OpenOptions as CapabilityOpenOptions};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use singularity_core::CancellationToken;
use thiserror::Error;
use uuid::Uuid;

const DEFAULT_MAX_TOOL_CALLS: u32 = 1;
/// 单次模型请求的默认 tool 数量上限。
pub const DEFAULT_MAX_TOOLS_PER_REQUEST: u32 = 8;
/// 默认模型上下文 token 上限。
pub const DEFAULT_MAX_CONTEXT_TOKENS: u32 = 128_000;
/// 默认模型输出 token 上限。
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
const MAX_CONFIGURED_CONTEXT_TOKENS: u32 = 2_000_000;
const MAX_CONFIGURED_OUTPUT_TOKENS: u32 = 256_000;
const ENV_PROVIDER: &str = "SINGULARITY_MODEL_PROVIDER";
const ENV_MODEL: &str = "SINGULARITY_MODEL";
const ENV_CONTEXT_TOKENS: &str = "SINGULARITY_MODEL_CONTEXT_TOKENS";
const ENV_MAX_OUTPUT_TOKENS: &str = "SINGULARITY_MODEL_MAX_OUTPUT_TOKENS";
const ENV_BASE_URL: &str = "SINGULARITY_BASE_URL";
const ENV_API_KEY: &str = "SINGULARITY_API_KEY";
const PROJECT_ENV_FILE: &str = ".env";
const DEFAULT_PROVIDER_NAME: &str = "openai_compatible";
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const V1_CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
const RESPONSES_PATH: &str = "/responses";
const V1_RESPONSES_PATH: &str = "/v1/responses";
const PROVIDER_TIMEOUT_SECONDS: u64 = 120;
const PROVIDER_RUNTIME_WORKER_THREADS: usize = 2;
const PROVIDER_RUNTIME_INITIALIZATION_ERROR_CODE: &str = "provider_runtime_initialization_failed";
const PROVIDER_CANCELLATION_POLL_MS: u64 = 25;
const MAX_PROVIDER_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROVIDER_ATTEMPTS: u32 = 3;
const PROVIDER_RETRY_BASE_BACKOFF_MS: u64 = 50;
/// Provider boundary code used when a protocol has no normalized text stream.
pub const PROVIDER_STREAMING_UNSUPPORTED_CODE: &str = "provider_streaming_unsupported";
const PROVIDER_SNAPSHOT_ID_PREFIX: &str = "provider_snapshot_";
const REQUIRED_TOOL_CHOICE_MISSING_ERROR: &str = "required_tool_call_missing";
const REQUIRED_TOOL_CHOICE_REQUIRES_TOOLS_ERROR: &str = "required_tool_choice_requires_tools";
const REQUIRED_TOOL_CHOICE_UNSUPPORTED_ERROR: &str =
    "provider_does_not_support_required_tool_choice";
const TEXT_TOOL_CALL_ENVELOPE_ERROR: &str = "text_tool_call_envelope_not_supported";
const HTTP_STATUS_UNAUTHORIZED: u16 = 401;
const HTTP_STATUS_FORBIDDEN: u16 = 403;
const HTTP_STATUS_REQUEST_TIMEOUT: u16 = 408;
const HTTP_STATUS_NOT_FOUND: u16 = 404;
const HTTP_STATUS_BAD_REQUEST: u16 = 400;
const HTTP_STATUS_UNPROCESSABLE_ENTITY: u16 = 422;
const HTTP_STATUS_RATE_LIMITED: u16 = 429;
const HTTP_STATUS_INTERNAL_SERVER_ERROR: u16 = 500;
const CAPABILITY_PROBE_REQUEST_ID: &str = "singularity_capability_probe";
const CAPABILITY_PROBE_CONTINUATION_REQUEST_ID: &str = "singularity_capability_probe_continuation";
const CAPABILITY_PROBE_TOOL_A: &str = "singularity_capability_probe_a";
const CAPABILITY_PROBE_TOOL_B: &str = "singularity_capability_probe_b";
const CAPABILITY_PROBE_EXPECTED_LABEL: &str = "schema_sentinel_alpha";
const CAPABILITY_PROBE_ALTERNATE_LABEL: &str = "schema_sentinel_beta";
const CAPABILITY_PROBE_EXPECTED_VALUE: i64 = 7;
const CAPABILITY_PROBE_DEVELOPER_INSTRUCTION: &str =
    "Follow the fixed capability probe request using native structured tool calls.";
/// app-server 状态目录中 provider capability cache 的文件名。
pub const PROVIDER_CAPABILITY_CACHE_FILE_NAME: &str = "provider-capability-cache.json";
const PROVIDER_CAPABILITY_CACHE_LOCK_FILE_NAME: &str = "provider-capability-cache.lock";
const PROVIDER_CAPABILITY_CACHE_SCHEMA_VERSION: u32 = 1;
const PROVIDER_CAPABILITY_CACHE_TTL_SECONDS: u64 = 24 * 60 * 60;
const MAX_PROVIDER_CAPABILITY_CACHE_BYTES: usize = 1024 * 1024;
const MAX_PROVIDER_CAPABILITY_CACHE_RECORDS: usize = 256;
const PROVIDER_CAPABILITY_CACHE_LOCK_RETRY_MS: u64 = 25;
const PROVIDER_CAPABILITY_CACHE_KEY_LOCK_PREFIX: &str = ".provider-capability-cache.key-lock-";
const PROVIDER_CAPABILITY_CACHE_KEY_LOCK_SUFFIX: &str = ".lock";
const MAX_PROVIDER_CAPABILITY_CACHE_KEY_LOCK_FILES: usize = 256;
const CAPABILITY_PROBE_DEADLINE_SECONDS: u64 = 120;
const PROVIDER_ADAPTER_VERSION: u32 = 1;
const CAPABILITY_PROBE_CONTRACT_VERSION: u32 = 1;
const PROVIDER_CAPABILITY_CACHE_INVALIDATION_DEADLINE_CODE: &str =
    "provider_capability_cache_invalidation_deadline_exceeded";

mod capability;
mod config;
mod contract;
mod openai;
mod transport;

pub use config::resolve_provider_config;
pub use contract::{
    is_strict_tool_schema_compatible, validate_model_request,
    validate_model_request_with_capabilities, validate_model_response,
    validate_model_turn_response, validate_provider_config,
};
pub use openai::{chat_completions_endpoint, provider_error_response, responses_endpoint};

#[cfg(test)]
use capability::{
    ProviderCapabilityCache, ProviderCapabilityCacheError, capability_probe_metadata,
    capability_probe_tool_reasoning_error, replace_existing_atomic, sha256_hex,
};
#[cfg(test)]
use config::provider_initialization_blocker;
#[cfg(test)]
use transport::provider_runtime_error;

/// 面向模型的对话历史支持的角色。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

/// 面向模型提供方的消息，包括继续 turn 所需的 tool call 元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: String,
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ModelToolCall>,
}

impl ModelMessage {
    /// 创建普通文本消息。
    pub fn text(role: ModelRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    /// 创建带结构化 tool calls 的 assistant 消息。
    pub fn assistant_tool_calls(tool_calls: Vec<ModelToolCall>) -> Self {
        Self {
            role: ModelRole::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_calls,
        }
    }
}

/// 控制模型提供方可以选择 tool、必须选择一个 tool，还是不得调用 tool。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceMode {
    Auto,
    None,
    Required,
}

/// 应用于一次模型请求的 tool 选择限制和模式严格程度。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolChoicePolicy {
    pub mode: ToolChoiceMode,
    pub max_tool_calls: u32,
    pub strict_tool_schema: bool,
}

impl Default for ToolChoicePolicy {
    fn default() -> Self {
        Self {
            mode: ToolChoiceMode::Auto,
            max_tool_calls: DEFAULT_MAX_TOOL_CALLS,
            strict_tool_schema: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// 模型 tool call 的解析状态。
pub enum ModelToolParseStatus {
    Valid,
    InvalidJson,
    SchemaMismatch,
    UnknownTool,
}

/// 一个可执行 tool 面向模型提供方暴露的模式。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelToolSchema {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

/// 已解析的模型 tool call，以及原始参数和校验结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub raw_arguments: String,
    pub parse_status: ModelToolParseStatus,
    pub validation_errors: Vec<String>,
}

/// tool 推理内容是否符合模型提供方的 tool call 历史契约。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderToolReasoningMode {
    #[default]
    Unspecified,
    DisabledForToolCalls,
}

/// 为模型提供方完成请求选定的线路协议。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderApiProtocol {
    #[default]
    Declared,
    OpenAiResponses,
    OpenAiChatCompletions,
}

/// 模型提供方必须遵守、用于构建请求和校验响应的能力。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderProtocolContract {
    pub supports_tools: bool,
    pub supports_parallel_tool_calls: bool,
    pub supports_required_tool_choice: bool,
    pub supports_strict_tool_schema: bool,
    pub tool_reasoning_mode: ProviderToolReasoningMode,
    pub max_tools_per_request: u32,
    pub supports_json_mode: bool,
    pub supports_system_message: bool,
    pub supports_developer_message: bool,
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
}

impl Default for ProviderProtocolContract {
    fn default() -> Self {
        Self {
            supports_tools: true,
            supports_parallel_tool_calls: false,
            supports_required_tool_choice: false,
            supports_strict_tool_schema: false,
            tool_reasoning_mode: ProviderToolReasoningMode::Unspecified,
            max_tools_per_request: DEFAULT_MAX_TOOLS_PER_REQUEST,
            supports_json_mode: false,
            supports_system_message: true,
            supports_developer_message: true,
            max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        }
    }
}

/// 协商得到的模型提供方配置档案，用于诊断和后续请求校验。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapabilityProfile {
    Declared,
    StrictParallel,
    StrictSingle,
    NonStrictParallel,
    NonStrictSingle,
}

/// `AgentLoop` 为完成请求提供的可选模型参数。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelPreferences {
    pub model_name: Option<String>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub json_mode: bool,
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
    ProjectEnvFile,
}

impl ProviderConfigSource {
    /// 返回配置来源的稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProcessEnvironment => "process_env",
            Self::ProjectEnvFile => "project_env",
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

/// 从模型提供方完成和能力探测中累积的令牌与成本计数器。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_estimate: Option<f64>,
}

/// 描述能力探测和选定协议配置档案的清理后证据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderCapabilityCacheLookupResult {
    Hit,
    Miss,
}

/// 一次真实 capability-cache 逻辑查找的短生命周期 typed 结果。
///
/// Provider 只填充真实 cache boundary 与 Hit/Miss；AgentLoop 在真实
/// PromptAssembly ownership boundary 绑定 model-turn 和 parent occurrence。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCapabilityCacheObservation {
    pub api_protocol: ProviderApiProtocol,
    pub outcome: ProviderCapabilityCacheLookupResult,
    pub model_turn_ordinal: Option<u32>,
    pub parent_occurrence_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderCapabilityMetadata {
    pub api_protocol: ProviderApiProtocol,
    pub profile: ProviderCapabilityProfile,
    pub cache_hit: bool,
    pub profile_attempts: u32,
    pub fallback_count: u32,
    pub probe_usage: ModelUsage,
    pub probe_attempt_metadata: ProviderAttemptMetadata,
    /// Runtime-only lookup observations; persistence and public schema expose no entries.
    #[serde(skip)]
    #[schemars(skip)]
    pub cache_observations: Vec<ProviderCapabilityCacheObservation>,
}

/// 模型提供方能力协商返回的契约和诊断信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderProtocolNegotiation {
    pub contract: ProviderProtocolContract,
    pub metadata: ProviderCapabilityMetadata,
}

/// provider runtime 的脱敏稳定指纹；provider 与 model 身份保持可独立引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderRuntimeFingerprint {
    /// 绑定 provider、规范化 endpoint 摘要、adapter/probe 版本和配置上限，不包含 model 或凭证。
    pub provider_fingerprint: String,
    /// 仅绑定 effective model 的稳定脱敏摘要。
    pub model_fingerprint: String,
    /// 在已知最终 protocol/contract 时绑定前两个指纹及协商结果。
    pub negotiation_fingerprint: Option<String>,
}

/// 模型侧请求或响应的校验错误和非致命警告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelValidationResult {
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// 从模型提供方边界保留下来的具体失败类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorKind {
    Cancelled,
    NetworkError,
    Timeout,
    RateLimited,
    ProviderOverloaded,
    AuthError,
    InvalidRequest,
    ContextLengthExceeded,
    BudgetExceeded,
    ToolCallParseError,
    JsonSchemaViolation,
    ContentFilter,
    UnsupportedCapability,
    UnknownProviderError,
}

/// 供调用方决定状态和恢复行为的较粗错误类别。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorCategory {
    Cancelled,
    Authentication,
    Network,
    ModelConfiguration,
    InvalidRequest,
    ContextLengthExceeded,
    BudgetExceeded,
    ToolCallParse,
    JsonSchema,
    ContentFilter,
    UnsupportedCapability,
    ProviderUnavailable,
    UnknownProviderError,
}

/// 模型提供方请求或响应发生失败的阶段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorStage {
    ClientInitialization,
    RequestSend,
    ResponseStatus,
    ResponseBodyRead,
    ResponseJsonDecode,
    ResponseValidation,
    Cancelled,
}

/// 与模型或策略语义分开保留的传输层原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTransportCategory {
    Timeout,
    Connect,
    Request,
    BodyRead,
    Unknown,
}

/// 带类型分类和清理后模型提供方诊断信息的模型错误。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelError {
    pub kind: ModelErrorKind,
    pub message: String,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<ProviderErrorStage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_category: Option<ProviderTransportCategory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_errors: Vec<String>,
}

/// 模型错误中可以安全跨越模型提供方边界的诊断子集。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderDiagnostic {
    pub code: Option<String>,
    pub stage: Option<ProviderErrorStage>,
    pub transport_category: Option<ProviderTransportCategory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    pub http_status: Option<u16>,
    pub validation_errors: Vec<String>,
}

impl ModelError {
    /// 创建带稳定 kind 的模型错误。
    pub fn new(kind: ModelErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            provider_name: None,
            model_name: None,
            code: None,
            stage: None,
            transport_category: None,
            timeout_seconds: None,
            http_status: None,
            validation_errors: Vec::new(),
        }
    }

    /// 绑定 provider 名称。
    pub fn with_provider(mut self, provider_name: impl Into<String>) -> Self {
        self.provider_name = Some(provider_name.into());
        self
    }

    /// 绑定模型名称。
    pub fn with_model(mut self, model_name: impl Into<String>) -> Self {
        self.model_name = Some(model_name.into());
        self
    }

    /// 附加脱敏 provider 诊断。
    pub fn with_provider_diagnostic(
        mut self,
        code: impl Into<String>,
        stage: ProviderErrorStage,
    ) -> Self {
        self.code = Some(code.into());
        self.stage = Some(stage);
        self
    }

    /// 归类为公共模型错误类别。
    pub fn category(&self) -> ModelErrorCategory {
        classify_model_error(self)
    }

    /// 返回 provider 诊断的脱敏副本。
    pub fn provider_diagnostic(&self) -> ProviderDiagnostic {
        ProviderDiagnostic {
            code: self.code.clone(),
            stage: self.stage.clone(),
            transport_category: self.transport_category.clone(),
            timeout_seconds: self.timeout_seconds,
            http_status: self.http_status,
            validation_errors: self.validation_errors.clone(),
        }
    }
}

/// 将模型错误映射为稳定公共类别。
pub fn classify_model_error(error: &ModelError) -> ModelErrorCategory {
    contract::model_error_category(error)
}

/// 传给模型提供方的完整模型请求，包括可见 tool 和 tool 策略。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelTurnRequest {
    pub request_id: String,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ModelToolSchema>,
    pub tool_choice: ToolChoicePolicy,
    pub model_preferences: ModelPreferences,
}

impl ModelTurnRequest {
    /// 创建模型 turn 请求。
    pub fn new(request_id: impl Into<String>, messages: Vec<ModelMessage>) -> Self {
        Self {
            request_id: request_id.into(),
            messages,
            tools: Vec::new(),
            tool_choice: ToolChoicePolicy::default(),
            model_preferences: ModelPreferences::default(),
        }
    }
}

/// 模型提供方 turn 产生了有效完成，还是未通过校验。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnStatus {
    Success,
    Failed,
    Invalid,
}

/// 模型提供方完成结果及其配对的已解析 tool call、用量、校验和错误状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelTurnResponse {
    pub request_id: String,
    pub response_id: String,
    pub status: ModelTurnStatus,
    pub assistant_message: Option<ModelMessage>,
    pub tool_calls: Vec<ModelToolCall>,
    pub usage: ModelUsage,
    pub finish_reason: Option<String>,
    pub validation: Option<ModelValidationResult>,
    pub error: Option<ModelError>,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_attempt_metadata: Option<ProviderAttemptMetadata>,
    /// Runtime-only capability metadata delivered with this response/error.
    #[serde(skip)]
    #[schemars(skip)]
    pub provider_capability_metadata: Option<ProviderCapabilityMetadata>,
}

impl ModelTurnResponse {
    /// 构造已完成的模型响应。
    pub fn completed(
        request_id: impl Into<String>,
        response_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            response_id: response_id.into(),
            status: ModelTurnStatus::Success,
            assistant_message: Some(ModelMessage::text(ModelRole::Assistant, content)),
            tool_calls: Vec::new(),
            usage: ModelUsage::default(),
            finish_reason: None,
            validation: None,
            error: None,
            provider_name: None,
            model_name: None,
            provider_attempt_metadata: None,
            provider_capability_metadata: None,
        }
    }
}

/// Normalized provider stream data safe for the `AgentLoop` boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStreamEvent {
    /// A visible text delta from the Responses `response.output_text.delta` event.
    OutputTextDelta { delta: String },
}

/// Typed normalized text-stream capability for one selected provider protocol.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStreamingCapability {
    /// The selected protocol has no normalized text stream at this boundary.
    #[default]
    Unsupported,
    /// The selected protocol emits ordered visible text deltas.
    OutputTextDelta,
}

impl ProviderStreamingCapability {
    /// Resolve the normalized stream capability from the actual selected protocol.
    pub const fn for_protocol(protocol: ProviderApiProtocol) -> Self {
        match protocol {
            ProviderApiProtocol::OpenAiResponses => Self::OutputTextDelta,
            ProviderApiProtocol::Declared | ProviderApiProtocol::OpenAiChatCompletions => {
                Self::Unsupported
            }
        }
    }
}

/// 一次真实 provider HTTP attempt 所属的运行期操作阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAttemptOperationPhase {
    /// Provider capability negotiation probe.
    CapabilityProbe,
    /// A caller-requested model completion.
    Completion,
}

/// 一次真实 provider HTTP attempt 的终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAttemptStatus {
    /// The attempt produced a valid provider response.
    Ok,
    /// The attempt ended with a non-cancellation error.
    Error,
    /// The attempt ended because cancellation was observed.
    Cancelled,
}

/// 一次真实 provider HTTP attempt 的脱敏运行期观测。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderAttemptOccurrence {
    pub operation_phase: ProviderAttemptOperationPhase,
    pub provider_name: String,
    pub model_name: String,
    pub actual_api_protocol: ProviderApiProtocol,
    /// 该 aggregate 内按真实 HTTP attempt 顺序排列的 1-based 索引。
    pub attempt_index: u32,
    pub terminal_status: ProviderAttemptStatus,
    /// The wall-clock timestamp captured when this transport attempt was created.
    pub started_at_unix_ms: u64,
    /// The wall-clock timestamp captured when this transport attempt was terminalized.
    pub ended_at_unix_ms: u64,
    /// 从 attempt 创建到响应解析或失败终结的墙钟时长，不含 retry backoff。
    pub attempt_duration_ms: u64,
    /// 从发送请求到收到响应 headers；未收到 headers 时不可用。
    pub request_send_to_headers_ms: Option<u64>,
    /// 当前 transport 没有 admission queue，因此保持不可用。
    pub queue_duration_ms: Option<u64>,
    /// 仅流式 Responses 首个非空 output_text delta 的真实到达时长。
    pub time_to_first_text_delta_ms: Option<u64>,
    /// 该失败是否触发了有界 retry 调度。
    pub retry_scheduled: bool,
    /// retry 被调度时使用的独立 backoff，不计入发送时长。
    pub retry_backoff_ms: Option<u64>,
    pub error_category: Option<ModelErrorCategory>,
    pub error_stage: Option<ProviderErrorStage>,
    pub diagnostic_code: Option<String>,
    /// 成功响应明确提供 usage 时才存在。
    pub usage: Option<ModelUsage>,
    /// Bound by AgentLoop at the model-response ownership boundary.
    pub model_turn_ordinal: Option<u32>,
    /// Stable PromptAssembly parent occurrence bound by AgentLoop.
    pub parent_occurrence_id: Option<String>,
}

/// 一次模型提供方操作记录的 aggregate 尝试次数、重试次数和运行期 occurrences。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderAttemptMetadata {
    pub attempt_count: u32,
    pub retry_count: u32,
    pub latency_ms: u64,
    /// 运行期 typed observation；不进入现有 checkpoint、trace JSON 或公共 schema。
    #[serde(skip)]
    #[schemars(skip)]
    pub occurrences: Vec<ProviderAttemptOccurrence>,
}

impl ProviderAttemptMetadata {
    fn zero() -> Self {
        Self {
            attempt_count: 0,
            retry_count: 0,
            latency_ms: 0,
            occurrences: Vec::new(),
        }
    }
}

impl ProviderCapabilityMetadata {
    fn declared() -> Self {
        Self {
            api_protocol: ProviderApiProtocol::Declared,
            profile: ProviderCapabilityProfile::Declared,
            cache_hit: false,
            profile_attempts: 0,
            fallback_count: 0,
            probe_usage: ModelUsage::default(),
            probe_attempt_metadata: ProviderAttemptMetadata::zero(),
            cache_observations: Vec::new(),
        }
    }
}

impl ProviderProtocolNegotiation {
    fn declared(contract: ProviderProtocolContract) -> Self {
        Self {
            contract,
            metadata: ProviderCapabilityMetadata::declared(),
        }
    }
}

/// `AgentLoop` 用于能力协商和完成请求的模型提供方边界。
pub trait Provider {
    /// 在动态协商前返回模型提供方声明的基线契约。
    fn protocol_contract(&self) -> ProviderProtocolContract;

    /// 在发送带 tool 的请求前探测或解析 tool 能力。
    fn negotiate_tool_capabilities(
        &self,
        _model_preferences: &ModelPreferences,
        _cancellation: &CancellationToken,
    ) -> Result<ProviderProtocolNegotiation, ProviderError> {
        Ok(ProviderProtocolNegotiation::declared(
            self.protocol_contract(),
        ))
    }

    /// Report the typed stream capability for the protocol selected by this provider.
    ///
    /// Legacy providers default to unsupported, even if their unrelated protocol metadata uses
    /// the same enum values as the OpenAI-compatible adapter.
    fn streaming_capability(
        &self,
        _selected_protocol: ProviderApiProtocol,
    ) -> ProviderStreamingCapability {
        ProviderStreamingCapability::Unsupported
    }

    /// Stream normalized visible text when the selected protocol supports it.
    ///
    /// The callback never receives reasoning, raw provider payloads, or tool argument deltas.
    /// Providers without this protocol capability keep using `complete` unchanged.
    fn complete_stream(
        &self,
        _request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
        _on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        Err(provider_streaming_unsupported_error())
    }

    /// 完成一个已校验请求，同时保留取消和类型化模型提供方错误。
    fn complete(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError>;
}

/// 已解析的兼容 OpenAI 连接设置；敏感信息仅为传输使用而保留。
#[derive(Clone, PartialEq, Eq)]
pub struct OpenAiProviderConfig {
    pub provider_name: String,
    pub model_name: String,
    pub base_url: String,
    pub api_key: String,
    pub source: ProviderConfigSource,
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
}

/// Provider transport runtime ownership: an app-server borrows its existing handle, while
/// independent consumers own a dedicated runtime shared by provider clones.
#[derive(Clone)]
enum ProviderRuntime {
    External(tokio::runtime::Handle),
    Owned(Arc<tokio::runtime::Runtime>),
}

impl ProviderRuntime {
    fn block_on<F: Future>(&self, future: F) -> F::Output {
        match self {
            Self::External(handle) => handle.block_on(future),
            Self::Owned(runtime) => runtime.block_on(future),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct ProviderCapabilityCacheKey {
    provider_name: String,
    endpoint_sha256: String,
    model_name: String,
    api_protocol: ProviderApiProtocol,
    adapter_version: u32,
    probe_contract_version: u32,
    max_context_tokens: u32,
    max_output_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProviderCapabilityProbeKey {
    provider_name: String,
    endpoint_sha256: String,
    model_name: String,
    adapter_version: u32,
    probe_contract_version: u32,
    max_context_tokens: u32,
    max_output_tokens: u32,
}

/// 协商能力并校验每次完成请求的兼容 OpenAI 模型提供方。
#[derive(Clone)]
pub struct OpenAiProvider {
    config: OpenAiProviderConfig,
    client: reqwest::Client,
    /// 所有 provider clone 共享同一 runtime ownership 绑定。
    runtime: Arc<ProviderRuntime>,
    request_timeout_seconds: u64,
    capability_probe_deadline: Duration,
    tool_capability_cache: Arc<Mutex<capability::InMemoryProviderCapabilityCacheState>>,
    tool_capability_probe_in_flight:
        Arc<Mutex<HashMap<ProviderCapabilityProbeKey, Arc<capability::CapabilityProbeState>>>>,
    persistent_capability_cache: Option<Arc<capability::ProviderCapabilityCache>>,
    capability_cache_diagnostic: Arc<Mutex<Option<String>>>,
}

#[derive(Debug, Clone, PartialEq, Error)]
#[error("{message}")]
/// 模型提供方失败，包含类型化模型错误、尝试元数据和可选能力证据。
pub struct ProviderError {
    pub message: String,
    pub error: Box<ModelError>,
    pub provider_attempt_metadata: Option<ProviderAttemptMetadata>,
    pub capability_metadata: Option<Box<ProviderCapabilityMetadata>>,
}

impl ProviderError {
    /// 从模型错误创建 provider 错误。
    pub fn from_model_error(error: ModelError) -> Self {
        Self {
            message: error.message.clone(),
            error: Box::new(error),
            provider_attempt_metadata: None,
            capability_metadata: None,
        }
    }

    /// 附加一次 provider attempt 的脱敏元数据。
    pub fn with_provider_attempt_metadata(mut self, metadata: ProviderAttemptMetadata) -> Self {
        self.provider_attempt_metadata = Some(metadata);
        self
    }

    /// 附加能力协商的脱敏元数据。
    pub fn with_capability_metadata(mut self, metadata: ProviderCapabilityMetadata) -> Self {
        self.capability_metadata = Some(Box::new(metadata));
        self
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
            max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
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

    #[test]
    fn capability_cache_sha256_matches_standard_vector() {
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_replacement_covers_success_and_failure_paths() {
        let directory = tempfile::tempdir().expect("Windows replacement directory");
        let source = directory.path().join("source-测试.tmp");
        let destination = directory.path().join("destination-缓存.json");
        std::fs::write(&source, b"replacement").expect("source file");
        std::fs::write(&destination, b"old").expect("destination file");

        replace_existing_atomic(&source, &destination).expect("replace existing file");
        assert!(
            !source.exists(),
            "successful replacement consumes the source"
        );
        assert_eq!(
            std::fs::read(&destination).expect("replacement destination"),
            b"replacement"
        );

        let missing = directory.path().join("missing.tmp");
        let error = replace_existing_atomic(&missing, &destination)
            .expect_err("missing source must report the Windows error");
        assert!(error.raw_os_error().is_some());
        assert_eq!(
            std::fs::read(&destination).expect("destination after failed replacement"),
            b"replacement"
        );

        let embedded_nul = std::path::PathBuf::from("invalid\0source.tmp");
        let error = replace_existing_atomic(&embedded_nul, &destination)
            .expect_err("embedded NUL must be rejected before the FFI call");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            std::fs::read(&destination).expect("destination after invalid path"),
            b"replacement"
        );
    }

    #[test]
    fn capability_cache_key_lock_is_exclusive_across_instances() {
        let directory = tempfile::tempdir().expect("cache lock directory");
        let cache_path = directory.path().join(PROVIDER_CAPABILITY_CACHE_FILE_NAME);
        let first = ProviderCapabilityCache::new(cache_path.clone()).expect("first cache");
        let second = ProviderCapabilityCache::new(cache_path).expect("second cache");
        let key = ProviderCapabilityCacheKey {
            provider_name: "provider".to_string(),
            endpoint_sha256: "00".repeat(32),
            model_name: "model".to_string(),
            api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
            adapter_version: PROVIDER_ADAPTER_VERSION,
            probe_contract_version: CAPABILITY_PROBE_CONTRACT_VERSION,
            max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        };
        let first_lock = first
            .acquire_key_lock(
                &key,
                &CancellationToken::new(),
                Some(Instant::now() + Duration::from_secs(1)),
            )
            .expect("first key lock");
        let second_result = second.acquire_key_lock(
            &key,
            &CancellationToken::new(),
            Some(Instant::now() + Duration::from_millis(100)),
        );
        assert!(matches!(
            second_result,
            Err(ProviderCapabilityCacheError::Deadline)
        ));
        drop(first_lock);
    }

    #[test]
    fn capability_cache_lock_wait_is_cancellable() {
        let directory = tempfile::tempdir().expect("cache lock directory");
        let cache = ProviderCapabilityCache::new(
            directory.path().join(PROVIDER_CAPABILITY_CACHE_FILE_NAME),
        )
        .expect("cache");
        let holder = cache
            .acquire_global_lock(
                true,
                &CancellationToken::new(),
                Some(Instant::now() + Duration::from_secs(1)),
            )
            .expect("hold cache lock");
        let cancellation = CancellationToken::new();
        let cancellation_for_thread = cancellation.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            cancellation_for_thread.cancel();
        });
        let started = Instant::now();
        let result = cache.acquire_global_lock(
            false,
            &cancellation,
            Some(Instant::now() + Duration::from_secs(1)),
        );
        assert!(matches!(
            result,
            Err(ProviderCapabilityCacheError::Cancelled)
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(holder);
        canceller.join().expect("join cache lock canceller");
    }

    #[test]
    fn capability_cache_invalidation_reuses_caller_cancellation_while_lock_held() {
        let directory = tempfile::tempdir().expect("cache directory");
        let cache_path = directory.path().join(PROVIDER_CAPABILITY_CACHE_FILE_NAME);
        let provider = OpenAiProvider::new_with_cache_path(
            test_provider_config("http://127.0.0.1:1".to_string()),
            Some(cache_path),
        )
        .expect("provider");
        let key =
            provider.capability_cache_key("gpt-test", ProviderApiProtocol::OpenAiChatCompletions);
        let persistent_cache = Arc::clone(
            provider
                .persistent_capability_cache
                .as_ref()
                .expect("persistent cache"),
        );
        let holder = persistent_cache
            .acquire_global_lock(
                true,
                &CancellationToken::new(),
                Some(Instant::now() + Duration::from_secs(1)),
            )
            .expect("hold cache lock");
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let started = Instant::now();
        let error = provider
            .invalidate_tool_capability_negotiation(
                &key,
                &cancellation,
                Instant::now() + Duration::from_secs(5),
            )
            .expect_err("cancelled invalidation must report cancellation");
        assert_eq!(error.error.kind, ModelErrorKind::Cancelled);
        assert_eq!(
            error.error.code.as_deref(),
            Some("provider_request_cancelled")
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cancelled invalidation must not wait for the held persistent lock"
        );
        assert_eq!(
            provider
                .capability_cache_diagnostic
                .lock()
                .expect("cache diagnostic lock")
                .as_deref(),
            Some("cancelled")
        );
        drop(holder);
    }

    #[test]
    fn fresh_probe_rejection_invalidation_preserves_cause_when_cancelled() {
        let directory = tempfile::tempdir().expect("cache directory");
        let cache_path = directory.path().join(PROVIDER_CAPABILITY_CACHE_FILE_NAME);
        let provider = OpenAiProvider::new_with_cache_path(
            test_provider_config("http://127.0.0.1:1".to_string()),
            Some(cache_path),
        )
        .expect("provider");
        let persistent_cache = Arc::clone(
            provider
                .persistent_capability_cache
                .as_ref()
                .expect("persistent cache"),
        );
        let holder = persistent_cache
            .acquire_global_lock(
                true,
                &CancellationToken::new(),
                Some(Instant::now() + Duration::from_secs(1)),
            )
            .expect("hold cache lock");
        let mut rejection = capability_probe_tool_reasoning_error(
            &ModelTurnResponse::completed("probe", "response", "done"),
            "tool_reasoning_disable_not_honored",
        );
        rejection.capability_metadata = Some(Box::new(capability_probe_metadata(
            ProviderApiProtocol::OpenAiChatCompletions,
            ProviderCapabilityProfile::StrictSingle,
            1,
            0,
            &ModelUsage::default(),
            &ProviderAttemptMetadata::zero(),
        )));
        let original_code = rejection.error.code.clone();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let started = Instant::now();
        let rejection = provider.invalidate_fresh_probe_rejection(
            "gpt-test",
            &cancellation,
            Instant::now() + Duration::from_secs(5),
            rejection,
        );
        assert_eq!(rejection.error.code, original_code);
        assert!(
            rejection
                .error
                .validation_errors
                .contains(&"provider_request_cancelled".to_string())
        );
        assert!(
            !rejection
                .error
                .validation_errors
                .contains(&"provider_capability_cache_invalidation_failed".to_string())
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "fresh rejection invalidation must honor caller cancellation"
        );
        drop(holder);
    }

    #[test]
    fn capability_probe_total_deadline_does_not_publish_cache() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind deadline provider");
        let address = listener.local_addr().expect("deadline provider address");
        let (seen_tx, seen_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept deadline provider request");
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            reader
                .read_line(&mut request_line)
                .expect("read deadline provider request");
            seen_tx.send(()).expect("signal deadline request");
            thread::sleep(Duration::from_millis(250));
        });
        let directory = tempfile::tempdir().expect("deadline cache directory");
        let cache_path = directory.path().join(PROVIDER_CAPABILITY_CACHE_FILE_NAME);
        let mut provider = OpenAiProvider::new_with_cache_path(
            test_provider_config(format!("http://{address}")),
            Some(cache_path.clone()),
        )
        .expect("deadline provider");
        provider.capability_probe_deadline = Duration::from_millis(50);
        let error = provider
            .negotiate_openai_tool_capabilities("gpt-test", &CancellationToken::new())
            .expect_err("capability probe deadline must fail closed");
        assert_eq!(
            error.error.code.as_deref(),
            Some("provider_capability_probe_deadline_exceeded")
        );
        assert!(!cache_path.exists(), "deadline must not publish cache");
        seen_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("deadline provider request observed");
        server.join().expect("join deadline provider");
    }

    #[test]
    fn failed_persistent_invalidation_leaves_a_tombstone_and_diagnostic() {
        let directory = tempfile::tempdir().expect("cache directory");
        let cache_path = directory.path().join(PROVIDER_CAPABILITY_CACHE_FILE_NAME);
        std::fs::create_dir(&cache_path).expect("unwritable cache target directory");
        let provider = OpenAiProvider::new_with_cache_path(
            test_provider_config("http://127.0.0.1:1".to_string()),
            Some(cache_path),
        )
        .expect("provider");
        let key =
            provider.capability_cache_key("gpt-test", ProviderApiProtocol::OpenAiChatCompletions);
        let error = provider
            .invalidate_tool_capability_negotiation(
                &key,
                &CancellationToken::new(),
                Instant::now() + Duration::from_secs(1),
            )
            .expect_err("persistent invalidation must report the write/read failure");
        assert_eq!(
            error.error.code.as_deref(),
            Some("provider_capability_cache_invalidation_failed")
        );
        let cache = provider
            .tool_capability_cache
            .lock()
            .expect("cache state lock");
        assert!(cache.tombstones.contains_key(&key));
        drop(cache);
        assert_eq!(
            provider
                .capability_cache_diagnostic
                .lock()
                .expect("cache diagnostic lock")
                .as_deref(),
            Some("unavailable")
        );
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
            for _ in 0..MAX_PROVIDER_ATTEMPTS {
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
                max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
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
        assert_eq!(metadata.attempt_count, MAX_PROVIDER_ATTEMPTS);
        assert_eq!(metadata.retry_count, MAX_PROVIDER_ATTEMPTS - 1);
        assert_eq!(metadata.occurrences.len(), MAX_PROVIDER_ATTEMPTS as usize);
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
            assert_eq!(
                occurrence.retry_scheduled,
                index + 1 < MAX_PROVIDER_ATTEMPTS as usize
            );
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
            max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
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
            for _ in 0..MAX_PROVIDER_ATTEMPTS {
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
            MAX_PROVIDER_ATTEMPTS
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
}
