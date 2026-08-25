//! provider HTTP transport、bounded body read 和取消传播。

pub(crate) mod http;
pub(crate) mod retry;
pub(crate) mod stream;

pub(crate) use http::*;
pub(crate) use retry::*;
pub(crate) use stream::*;

use std::fmt;
use std::time::{Duration, Instant};

use serde_json::Value;
use singularity_core::CancellationToken;

use crate::error::{ModelError, ModelErrorKind, ProviderError, ProviderErrorStage};
use crate::openai::{
    OpenAiCompletion, openai_chat_stream_request_payload, openai_reasoning_content_present,
    openai_request_payload, openai_responses_reasoning_content_present,
    openai_responses_request_payload, openai_responses_stream_request_payload,
    parse_openai_response, parse_openai_responses_response, responses_endpoint,
};
use crate::provider::Provider;
use crate::provider::contract::{
    ProviderApiProtocol, ProviderProtocolContract, ThinkingWireFormat,
    provider_request_validation_error, request_uses_tool_protocol,
    validate_model_request_with_capabilities,
};
use crate::provider::runtime::{OpenAiProviderConfig, SelectedModel};
use crate::provider::telemetry::{
    ProviderAttemptEvent, ProviderAttemptOccurrence, ProviderAttemptStarted, ProviderAttemptStatus,
    ProviderStreamEvent, ProviderStreamingCapability, provider_streaming_unsupported_error,
};
use crate::types::{
    ModelRole, ModelTurnRequest, ModelTurnResponse, ModelUsage, ProviderToolReasoningMode,
};

/// 一次 provider 补全共享的单一已验证协议选择。
struct CompletionContext {
    capabilities: ProviderProtocolContract,
    api_protocol: ProviderApiProtocol,
}

#[derive(Clone, Copy)]
enum ProtocolAdapter {
    Chat,
    Responses,
}

impl ProtocolAdapter {
    fn for_api_protocol(api_protocol: ProviderApiProtocol) -> Self {
        match api_protocol {
            ProviderApiProtocol::OpenAiResponses => Self::Responses,
            ProviderApiProtocol::Declared | ProviderApiProtocol::OpenAiChatCompletions => {
                Self::Chat
            }
        }
    }

    fn endpoint(self, config: &OpenAiProviderConfig) -> String {
        match self {
            Self::Chat => config.endpoint(),
            Self::Responses => responses_endpoint(&config.base_url),
        }
    }

    fn request_payload(
        self,
        provider: &OpenAiProvider,
        request: &ModelTurnRequest,
        model_name: &str,
        capabilities: &ProviderProtocolContract,
        streaming: bool,
    ) -> Value {
        let selection = provider.selected_model.as_ref();
        let reasoning_enabled = selection.is_some_and(|selection| selection.reasoning_enabled);
        let disable_reasoning = selection.is_some_and(|selection| !selection.reasoning_enabled);
        let reasoning_effort =
            selection.and_then(|selection| selection.wire_reasoning_effort.as_deref());
        let supports_tool_choice = selection.is_none_or(|selection| selection.supports_tool_choice);
        // developer 角色缺省为不支持（流式与非流式共用同一缺省值）：无
        // SelectedModel 的 legacy/env 路径没有 per-model 声明，wire 必须用
        // 通用的 system role——OpenAI 兼容端点普遍不接受 developer 角色。
        // 有 SelectedModel 时以模型声明的 supports_developer_role 为准。
        let supports_developer_role =
            selection.is_some_and(|selection| selection.supports_developer_role);
        let requires_assistant_content_for_tool_calls =
            selection.is_some_and(|selection| selection.requires_assistant_content_for_tool_calls);
        let thinking_wire_format = selection
            .map(|selection| selection.thinking_wire_format)
            .unwrap_or(ThinkingWireFormat::ThinkingType);
        match (self, streaming) {
            (Self::Chat, true) => openai_chat_stream_request_payload(
                request,
                model_name,
                capabilities,
                reasoning_enabled,
                disable_reasoning,
                reasoning_effort,
                thinking_wire_format,
                supports_developer_role,
                supports_tool_choice,
                requires_assistant_content_for_tool_calls,
            ),
            (Self::Chat, false) => openai_request_payload(
                request,
                model_name,
                capabilities,
                reasoning_enabled,
                disable_reasoning,
                reasoning_effort,
                thinking_wire_format,
                supports_developer_role,
                supports_tool_choice,
                requires_assistant_content_for_tool_calls,
            ),
            (Self::Responses, true) => openai_responses_stream_request_payload(
                request,
                model_name,
                capabilities,
                reasoning_enabled,
                disable_reasoning,
                reasoning_effort,
                supports_tool_choice,
            ),
            (Self::Responses, false) => openai_responses_request_payload(
                request,
                model_name,
                capabilities,
                reasoning_enabled,
                disable_reasoning,
                reasoning_effort,
                supports_tool_choice,
            ),
        }
    }

    fn reasoning_present(self, payload: &Value) -> bool {
        match self {
            Self::Chat => openai_reasoning_content_present(payload),
            Self::Responses => openai_responses_reasoning_content_present(payload),
        }
    }

    fn parse_response(
        self,
        request: &ModelTurnRequest,
        config: &OpenAiProviderConfig,
        payload: Value,
        capabilities: &ProviderProtocolContract,
        model_name: &str,
        reasoning_variant: Option<&str>,
    ) -> Result<ModelTurnResponse, ProviderError> {
        match self {
            Self::Chat => parse_openai_response(
                request,
                config,
                payload,
                capabilities,
                model_name,
                reasoning_variant,
            ),
            Self::Responses => parse_openai_responses_response(
                request,
                config,
                payload,
                capabilities,
                model_name,
                reasoning_variant,
            ),
        }
    }
}

/// 一次真实 provider HTTP attempt 的可变计时状态。
struct ProviderAttemptInProgress {
    provider_name: String,
    model_name: String,
    actual_api_protocol: ProviderApiProtocol,
    attempt_index: u32,
    started_at: Instant,
    started_at_unix_ms: u64,
    request_send_to_headers_ms: Option<u64>,
    time_to_first_text_delta_ms: Option<u64>,
}

impl ProviderAttemptInProgress {
    fn new(
        provider_name: &str,
        model_name: &str,
        actual_api_protocol: ProviderApiProtocol,
        attempt_index: u32,
    ) -> Self {
        Self {
            provider_name: provider_name.to_string(),
            model_name: model_name.to_string(),
            actual_api_protocol,
            attempt_index,
            started_at: Instant::now(),
            started_at_unix_ms: unix_timestamp_ms(),
            request_send_to_headers_ms: None,
            time_to_first_text_delta_ms: None,
        }
    }

    fn started_event(&self) -> ProviderAttemptEvent {
        ProviderAttemptEvent::Started(ProviderAttemptStarted {
            provider_name: self.provider_name.clone(),
            model_name: self.model_name.clone(),
            actual_api_protocol: self.actual_api_protocol,
            attempt_index: self.attempt_index,
            started_at_unix_ms: self.started_at_unix_ms,
        })
    }

    fn mark_response_headers_received(&mut self) {
        self.request_send_to_headers_ms = Some(duration_millis(self.started_at.elapsed()));
    }

    fn set_time_to_first_text_delta(&mut self, duration_ms: Option<u64>) {
        self.time_to_first_text_delta_ms = duration_ms;
    }

    fn finish(
        self,
        error: Option<&ModelError>,
        usage: Option<ModelUsage>,
    ) -> ProviderAttemptOccurrence {
        let terminal_status = match error.map(|error| &error.kind) {
            None => ProviderAttemptStatus::Ok,
            Some(ModelErrorKind::Cancelled) => ProviderAttemptStatus::Cancelled,
            Some(_) => ProviderAttemptStatus::Error,
        };
        let ended_at_unix_ms = unix_timestamp_ms().max(self.started_at_unix_ms);
        ProviderAttemptOccurrence {
            provider_name: self.provider_name,
            model_name: self.model_name,
            actual_api_protocol: self.actual_api_protocol,
            attempt_index: self.attempt_index,
            terminal_status,
            started_at_unix_ms: self.started_at_unix_ms,
            ended_at_unix_ms,
            attempt_duration_ms: duration_millis(self.started_at.elapsed()),
            request_send_to_headers_ms: self.request_send_to_headers_ms,
            time_to_first_text_delta_ms: self.time_to_first_text_delta_ms,
            error_category: error.map(ModelError::category),
            error_stage: error.and_then(|error| error.stage.clone()),
            diagnostic_code: error.and_then(|error| error.code.clone()),
            usage,
        }
    }
}

/// 一次 attempt 内、成功 HTTP 响应上的协议侧工作结果。`Retry` 表示协议侧
/// 允许调用方重发请求；`Failed` 禁止自动重放。
enum AttemptBodyOutcome {
    Completed {
        completion: Box<OpenAiCompletion>,
        wire_usage_present: bool,
        time_to_first_text_delta_ms: Option<u64>,
    },
    Retry {
        error: ProviderError,
        time_to_first_text_delta_ms: Option<u64>,
    },
    Failed {
        error: ProviderError,
        time_to_first_text_delta_ms: Option<u64>,
    },
}

/// 把一次流式解码 attempt 折叠进 [`AttemptBodyOutcome`]。流失败仅在首个
/// 可见 delta 之前可重试：之后重发会重复已输出的内容。
fn streaming_outcome(
    attempt: Result<StreamAttemptSuccess, StreamAttemptFailure>,
    parse_payload: impl FnOnce(Value) -> Result<(OpenAiCompletion, bool), ProviderError>,
) -> AttemptBodyOutcome {
    match attempt {
        Ok(success) => {
            let time_to_first_text_delta_ms = success.time_to_first_text_delta_ms;
            match parse_payload(success.payload) {
                Ok((completion, wire_usage_present)) => AttemptBodyOutcome::Completed {
                    completion: Box::new(completion),
                    wire_usage_present,
                    time_to_first_text_delta_ms,
                },
                Err(error) => AttemptBodyOutcome::Failed {
                    error,
                    time_to_first_text_delta_ms,
                },
            }
        }
        Err(failure) if !failure.emitted_text_delta => AttemptBodyOutcome::Retry {
            error: failure.error,
            time_to_first_text_delta_ms: failure.time_to_first_text_delta_ms,
        },
        Err(failure) => AttemptBodyOutcome::Failed {
            error: failure.error,
            time_to_first_text_delta_ms: failure.time_to_first_text_delta_ms,
        },
    }
}

fn non_streaming_outcome(
    body: Result<Vec<u8>, ProviderError>,
    parse_payload: impl FnOnce(Value) -> Result<(OpenAiCompletion, bool), ProviderError>,
) -> AttemptBodyOutcome {
    let body = match body {
        Ok(body) => body,
        Err(error) => {
            return AttemptBodyOutcome::Retry {
                error,
                time_to_first_text_delta_ms: None,
            };
        }
    };
    let payload = match serde_json::from_slice::<Value>(&body) {
        Ok(payload) => payload,
        Err(_) => {
            return AttemptBodyOutcome::Failed {
                error: ProviderError::from_model_error(provider_response_json_error()),
                time_to_first_text_delta_ms: None,
            };
        }
    };
    match parse_payload(payload) {
        Ok((completion, wire_usage_present)) => AttemptBodyOutcome::Completed {
            completion: Box::new(completion),
            wire_usage_present,
            time_to_first_text_delta_ms: None,
        },
        Err(error) => AttemptBodyOutcome::Failed {
            error,
            time_to_first_text_delta_ms: None,
        },
    }
}

fn parse_protocol_payload(
    adapter: ProtocolAdapter,
    request: &ModelTurnRequest,
    config: &OpenAiProviderConfig,
    payload: Value,
    capabilities: &ProviderProtocolContract,
    model_name: &str,
    reasoning_variant: Option<&str>,
) -> Result<(OpenAiCompletion, bool), ProviderError> {
    let wire_usage_present = payload.get("usage").is_some_and(Value::is_object);
    let reasoning_content_present = adapter.reasoning_present(&payload);
    adapter
        .parse_response(
            request,
            config,
            payload,
            capabilities,
            model_name,
            reasoning_variant,
        )
        .map(|response| {
            (
                OpenAiCompletion {
                    response,
                    reasoning_content_present,
                },
                wire_usage_present,
            )
        })
}

#[allow(clippy::too_many_arguments)]
fn read_protocol_sse(
    adapter: ProtocolAdapter,
    runtime: &tokio::runtime::Handle,
    cancellation: &CancellationToken,
    request_timeout_seconds: u64,
    response: reqwest::Response,
    on_event: &mut dyn FnMut(ProviderStreamEvent),
    attempt_started_at: Instant,
) -> Result<StreamAttemptSuccess, StreamAttemptFailure> {
    match adapter {
        ProtocolAdapter::Chat => read_openai_chat_sse(
            runtime,
            cancellation,
            request_timeout_seconds,
            response,
            on_event,
            attempt_started_at,
        ),
        ProtocolAdapter::Responses => read_openai_responses_sse(
            runtime,
            cancellation,
            request_timeout_seconds,
            response,
            on_event,
            attempt_started_at,
        ),
    }
}

pub struct OpenAiProvider {
    config: OpenAiProviderConfig,
    selected_model: Option<SelectedModel>,
    client: reqwest::Client,
    runtime: tokio::runtime::Handle,
    request_timeout_seconds: u64,
}

impl Clone for OpenAiProvider {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            selected_model: self.selected_model.clone(),
            client: self.client.clone(),
            runtime: self.runtime.clone(),
            request_timeout_seconds: self.request_timeout_seconds,
        }
    }
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

impl OpenAiProvider {
    /// 创建并校验 OpenAI-compatible provider；异步执行一律使用调用方注入的 runtime。
    pub fn new(
        config: OpenAiProviderConfig,
        runtime_handle: tokio::runtime::Handle,
    ) -> Result<Self, ProviderError> {
        Self::new_with_request_timeout(config, crate::PROVIDER_TIMEOUT_SECONDS, runtime_handle)
    }

    pub(crate) fn new_with_request_timeout(
        config: OpenAiProviderConfig,
        request_timeout_seconds: u64,
        runtime_handle: tokio::runtime::Handle,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .read_timeout(Duration::from_secs(request_timeout_seconds))
            .user_agent(format!("singularity-agent/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(provider_client_initialization_error)?;
        Ok(Self {
            config,
            selected_model: None,
            client,
            runtime: runtime_handle,
            request_timeout_seconds,
        })
    }

    /// 为单个白名单模型克隆 provider，同时冻结其协议与 token 限额；
    /// 克隆共享 HTTP 客户端、runtime 与缓存。
    pub(crate) fn with_selected_model(&self, selected_model: SelectedModel) -> Self {
        let mut selected = self.clone();
        selected.config.model_name = selected_model.model_name.clone();
        selected.config.max_context_tokens = selected_model.max_context_tokens;
        selected.config.max_output_tokens = selected_model.max_output_tokens;
        selected.selected_model = Some(selected_model);
        selected
    }

    pub(super) fn configured_provider_name(&self) -> &str {
        &self.config.provider_name
    }

    /// 返回目录克隆的完整选择器（`provider/model#effort`），否则返回裸
    /// legacy model id；未解析出模型时（未配置的 legacy provider）返回 `None`。
    pub(crate) fn resolved_selector(&self) -> Option<String> {
        let Some(selection) = self.selected_model.as_ref() else {
            return Some(self.config.model_name.clone());
        };
        let mut selector = format!("{}/{}", self.config.provider_name, selection.model_name);
        if let Some(variant) = selection.reasoning_variant.as_deref() {
            selector.push('#');
            selector.push_str(variant);
        }
        Some(selector)
    }

    pub(super) fn config_snapshot(&self) -> OpenAiProviderConfig {
        self.config.clone()
    }

    /// 返回目录选择的不可变 catalog 协议（若非目录选择则为 `None`）。
    pub fn selected_api_protocol(&self) -> Option<ProviderApiProtocol> {
        self.selected_model
            .as_ref()
            .map(|selection| selection.api_protocol)
    }

    /// 把内部复合选择器转换为裸上游 model id；显式目录克隆还会拒绝
    /// 在 turn 内更换所选模型。
    fn normalize_request_model(
        &self,
        request: &ModelTurnRequest,
    ) -> Result<ModelTurnRequest, ProviderError> {
        let Some(selector) = request.model_preferences.model_name.as_deref() else {
            // Legacy 路径：无 selector 时无法证明 replay 兼容。
            // reasoning disabled 或未解析出 selected_model 时不再静默清空，
            // 显式拒绝，避免旧私有状态被悄悄丢弃后继续。
            if self
                .selected_model
                .as_ref()
                .is_none_or(|selection| !selection.reasoning_enabled)
            {
                if !request.provider_reasoning_history.is_empty() {
                    return Err(provider_tool_reasoning_history_error(
                        ProviderToolReasoningMode::Unspecified,
                    ));
                }
                return Ok(request.clone());
            }
            let normalized = request.clone();
            self.validate_reasoning_history(&normalized)?;
            return Ok(normalized);
        };
        let mut normalized = request.clone();
        let model_name = if let Some((provider_name, model_and_effort)) = selector.split_once('/') {
            if provider_name.is_empty() || model_and_effort.is_empty() {
                return Err(super::config::model_selector_error(
                    "provider/model selector must contain non-empty provider and model ids",
                    "provider_selector_invalid",
                ));
            }
            if provider_name != self.config.provider_name {
                return Err(super::config::model_selector_error(
                    "model selector references an unknown provider",
                    "provider_selector_unknown_provider",
                ));
            }
            model_and_effort
        } else {
            selector
        };
        let (model_name, requested_effort) = match model_name.rsplit_once('#') {
            Some((model_name, effort)) if !model_name.is_empty() && !effort.is_empty() => {
                (model_name, Some(effort))
            }
            Some(_) => {
                return Err(super::config::model_selector_error(
                    "model selector reasoning variant is malformed",
                    "provider_selector_invalid",
                ));
            }
            None => (model_name, None),
        };
        if self.selected_model.is_some() && model_name != self.config.model_name {
            return Err(super::config::model_selector_error(
                "model selector is not the fixed model for this provider turn",
                "provider_selector_unknown_model",
            ));
        }
        let requested_effort_matches = match requested_effort {
            Some(effort) => {
                self.selected_model
                    .as_ref()
                    .and_then(|selection| selection.reasoning_variant.as_deref())
                    == Some(effort)
            }
            None => true,
        };
        if !requested_effort_matches {
            return Err(super::config::model_selector_error(
                "model selector is not the fixed reasoning variant for this provider turn",
                "provider_selector_unknown_reasoning_variant",
            ));
        }
        normalized.model_preferences.model_name = Some(model_name.to_string());
        if self
            .selected_model
            .as_ref()
            .is_none_or(|selection| !selection.reasoning_enabled)
        {
            if !normalized.provider_reasoning_history.is_empty() {
                return Err(provider_tool_reasoning_history_error(
                    ProviderToolReasoningMode::Unspecified,
                ));
            }
        } else {
            self.validate_reasoning_history(&normalized)?;
        }
        Ok(normalized)
    }

    fn validate_reasoning_history(&self, request: &ModelTurnRequest) -> Result<(), ProviderError> {
        if request.provider_reasoning_history.is_empty() {
            return Ok(());
        }
        let Some(selection) = self.selected_model.as_ref() else {
            return Err(provider_tool_reasoning_history_error(
                ProviderToolReasoningMode::Unspecified,
            ));
        };
        if !selection.reasoning_enabled
            || selection.tool_reasoning_mode == ProviderToolReasoningMode::Unspecified
        {
            return Err(provider_tool_reasoning_history_error(
                selection.tool_reasoning_mode,
            ));
        }
        // 无变体选择（selection.reasoning_variant=None）同样是合法绑定侧；
        // 变体一致性由 validate_for 的 Option 语义判定。
        let variant = selection.reasoning_variant.as_deref();
        for replay in &request.provider_reasoning_history {
            if replay
                .validate_for(
                    &self.config.provider_name,
                    &self.config.model_name,
                    variant,
                    selection.tool_reasoning_mode,
                )
                .is_err()
                || !replay.is_bound_to_messages(&request.messages)
            {
                return Err(provider_tool_reasoning_history_error(
                    selection.tool_reasoning_mode,
                ));
            }
        }
        for message in request.messages.iter().filter(|message| {
            message.role == ModelRole::Assistant && !message.tool_calls.is_empty()
        }) {
            let ids = message
                .tool_calls
                .iter()
                .map(|call| call.tool_call_id.clone())
                .collect::<Vec<_>>();
            let bound_replay_count = request
                .provider_reasoning_history
                .iter()
                .filter(|replay| replay.matches_tool_call_ids(&ids))
                .count();
            // 只拒绝重复绑定（同一工具消息被多个 replay 绑定必然是错误）。
            // 消息无绑定 replay 是合法形态：DeepSeek/Kimi 的 400 约束是"有
            // reasoning 历史的工具消息必须回传自己的 reasoning_content"
            // （opencode issues #24190/#24722），旧会话（v3 迁移）中本无
            // reasoning 的工具消息不需要 replay；"有 thinking 的消息必有
            // replay"由 agent 侧投影保证。
            if bound_replay_count > 1 {
                return Err(provider_tool_reasoning_history_error(
                    selection.tool_reasoning_mode,
                ));
            }
        }
        Ok(())
    }
}

impl OpenAiProvider {
    fn prepare_completion_context_observed(
        &self,
        request: &ModelTurnRequest,
    ) -> Result<CompletionContext, ProviderError> {
        // 静态能力声明：工具与非工具请求统一使用声明式契约；api_protocol 由
        // selected_model 或 endpoint 后缀决定。
        let capabilities = self.protocol_contract();
        let api_protocol = self
            .selected_model
            .as_ref()
            .map(|selection| selection.api_protocol)
            .unwrap_or_else(|| self.config.completion_protocol_without_tools());
        let request_validation =
            validate_model_request_with_capabilities(request, Some(&capabilities));
        if !request_validation.valid {
            return Err(provider_request_validation_error(
                request_validation,
                &self.config,
            ));
        }
        Ok(CompletionContext {
            capabilities,
            api_protocol,
        })
    }

    /// 一次完成的单一编排入口：`on_event` 为 `Some` 时走流式解码并强制
    /// 协议流能力声明，为 `None` 时走有界 body 读取。请求归一、能力校验、
    /// wire 协议选择与 tool-reasoning 契约校验只在这一个入口实现，杜绝
    /// 流式/非流式双轨各自维护导致的静默漂移。
    fn complete_internal(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: Option<&mut dyn FnMut(ProviderStreamEvent)>,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<ModelTurnResponse, ProviderError> {
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error());
        }
        let request = self.normalize_request_model(request)?;
        let context = self.prepare_completion_context_observed(&request)?;
        if on_event.is_some()
            && self.streaming_capability(context.api_protocol)
                != ProviderStreamingCapability::OutputTextDelta
        {
            return Err(provider_streaming_unsupported_error());
        }
        let model_name = request
            .model_preferences
            .model_name
            .as_deref()
            .unwrap_or(&self.config.model_name);
        let completion = self.complete_protocol(
            &request,
            cancellation,
            &context.capabilities,
            context.api_protocol,
            model_name,
            on_event,
            on_attempt,
        )?;
        validate_response_tool_reasoning_contract(
            request_uses_tool_protocol(&request),
            &completion,
            &context.capabilities,
            self.selected_model
                .as_ref()
                .is_some_and(|selection| selection.requires_reasoning_content_for_tool_calls),
        )?;
        Ok(completion.response)
    }

    #[allow(clippy::too_many_arguments)]
    fn complete_protocol(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        capabilities: &ProviderProtocolContract,
        api_protocol: ProviderApiProtocol,
        model_name: &str,
        mut on_event: Option<&mut dyn FnMut(ProviderStreamEvent)>,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<OpenAiCompletion, ProviderError> {
        let streaming = on_event.is_some();
        if streaming {
            self.validate_reasoning_history(request)?;
        }
        let adapter = ProtocolAdapter::for_api_protocol(api_protocol);
        let endpoint = adapter.endpoint(&self.config);
        let request_payload =
            adapter.request_payload(self, request, model_name, capabilities, streaming);
        let reasoning_variant = self
            .selected_model
            .as_ref()
            .and_then(|selection| selection.reasoning_variant.as_deref());
        self.complete_attempt(
            cancellation,
            api_protocol,
            model_name,
            &endpoint,
            &request_payload,
            on_attempt,
            &mut |response, attempt_started_at| {
                let parse_payload = |payload| {
                    parse_protocol_payload(
                        adapter,
                        request,
                        &self.config,
                        payload,
                        capabilities,
                        model_name,
                        reasoning_variant,
                    )
                };
                if let Some(on_event) = on_event.as_deref_mut() {
                    streaming_outcome(
                        read_protocol_sse(
                            adapter,
                            &self.runtime,
                            cancellation,
                            self.request_timeout_seconds,
                            response,
                            on_event,
                            attempt_started_at,
                        ),
                        parse_payload,
                    )
                } else {
                    non_streaming_outcome(
                        read_bounded_provider_response_body(
                            &self.runtime,
                            cancellation,
                            self.request_timeout_seconds,
                            response,
                        ),
                        parse_payload,
                    )
                }
            },
        )
    }

    /// 两种 wire 协议、流式与非流式响应的共享完成骨架：执行一次 HTTP
    /// attempt，返回解析后的完成或携带重放安全性与 provider 定向延时的
    /// 类型化失败。
    #[allow(clippy::too_many_arguments)]
    fn complete_attempt(
        &self,
        cancellation: &CancellationToken,
        api_protocol: ProviderApiProtocol,
        model_name: &str,
        endpoint: &str,
        request_payload: &Value,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
        read_response: &mut dyn FnMut(reqwest::Response, Instant) -> AttemptBodyOutcome,
    ) -> Result<OpenAiCompletion, ProviderError> {
        let runtime = &self.runtime;
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error());
        }

        let mut occurrence =
            ProviderAttemptInProgress::new(&self.config.provider_name, model_name, api_protocol, 1);
        emit_provider_attempt_started(&occurrence, on_attempt)?;
        let response = match block_on_provider_future(
            runtime,
            cancellation,
            "provider_request_send_failed",
            ProviderErrorStage::RequestSend,
            self.request_timeout_seconds,
            || {
                self.client
                    .post(endpoint)
                    .bearer_auth(&self.config.api_key)
                    .json(request_payload)
                    .send()
            },
        ) {
            Ok(response) => {
                occurrence.mark_response_headers_received();
                response
            }
            Err(error) => {
                record_provider_attempt(occurrence, Some(&error.error), None, on_attempt)?;
                return Err(error);
            }
        };

        let status = response.status();
        let status_code = status.as_u16();
        if !status.is_success() {
            let retry_after = retry_after_delay(response.headers());
            let error_body = read_bounded_provider_response_body(
                runtime,
                cancellation,
                self.request_timeout_seconds,
                response,
            )
            .ok();
            let error_fields = error_body.as_deref().map(parse_provider_error_body);
            let context_length_exceeded = is_context_length_exceeded_code(
                error_fields
                    .as_ref()
                    .and_then(|fields| fields.code.as_deref()),
            );
            let model_error = if context_length_exceeded {
                let mut context_error = ModelError::new(
                    ModelErrorKind::ContextLengthExceeded,
                    "provider rejected the request: context length exceeded",
                )
                .with_provider(self.config.provider_name.clone())
                .with_model(model_name.to_string())
                .with_provider_diagnostic(
                    "provider_context_length_exceeded",
                    ProviderErrorStage::ResponseStatus,
                );
                context_error.http_status = Some(status_code);
                context_error
            } else {
                model_error_from_http_status(status_code, &self.config.provider_name, model_name)
            };
            let provider_diagnostic = if context_length_exceeded {
                None
            } else {
                error_fields
                    .as_ref()
                    .and_then(|fields| fields.message.as_deref())
                    .map(|message| bounded_provider_error_diagnostic(message, &self.config.api_key))
                    .or_else(|| {
                        error_body.as_deref().map(|body| {
                            bounded_provider_error_diagnostic(
                                &String::from_utf8_lossy(body),
                                &self.config.api_key,
                            )
                        })
                    })
                    .filter(|diagnostic| !diagnostic.is_empty())
            };
            let mut display_message = model_error.message.clone();
            if let Some(diagnostic) = provider_diagnostic {
                display_message.push_str(" Provider diagnostic: ");
                display_message.push_str(&diagnostic);
            }
            record_provider_attempt(occurrence, Some(&model_error), None, on_attempt)?;
            let mut error =
                ProviderError::from_model_error(model_error).with_retry_after(retry_after);
            error.message = display_message;
            return Err(error);
        }

        match read_response(response, occurrence.started_at) {
            AttemptBodyOutcome::Completed {
                completion,
                wire_usage_present,
                time_to_first_text_delta_ms,
            } => {
                occurrence.set_time_to_first_text_delta(time_to_first_text_delta_ms);
                let occurrence_error = completion.response.error.as_ref();
                let usage = (wire_usage_present && occurrence_error.is_none())
                    .then(|| completion.response.usage.clone());
                record_provider_attempt(occurrence, occurrence_error, usage, on_attempt)?;
                Ok(*completion)
            }
            AttemptBodyOutcome::Retry {
                error,
                time_to_first_text_delta_ms,
            } => {
                occurrence.set_time_to_first_text_delta(time_to_first_text_delta_ms);
                record_provider_attempt(occurrence, Some(&error.error), None, on_attempt)?;
                Err(error)
            }
            AttemptBodyOutcome::Failed {
                error,
                time_to_first_text_delta_ms,
            } => {
                occurrence.set_time_to_first_text_delta(time_to_first_text_delta_ms);
                record_provider_attempt(occurrence, Some(&error.error), None, on_attempt)?;
                Err(error.without_automatic_retry())
            }
        }
    }
}

/// 在完成的响应上强制执行已声明的工具推理契约：仅在契约确实被违反时
/// 拒绝——provider 返回了 reasoning 但声明为 `DisabledForToolCalls`，
/// 或响应携带工具调用但缺少模式匹配的 reasoning replay。仅有 reasoning
/// 的无工具调用回复是合法、不需要 replay 的。
fn validate_response_tool_reasoning_contract(
    request_used_tool_protocol: bool,
    completion: &OpenAiCompletion,
    capabilities: &ProviderProtocolContract,
    requires_reasoning_content_for_tool_calls: bool,
) -> Result<(), ProviderError> {
    if !request_used_tool_protocol {
        return Ok(());
    }
    let response_has_tool_calls = !completion.response.tool_calls.is_empty();
    let disabled_mode_not_honored = capabilities.tool_reasoning_mode
        == ProviderToolReasoningMode::DisabledForToolCalls
        && completion.reasoning_content_present;
    let reasoning_content_present = completion.reasoning_content_present;
    let missing_replay_for_present_reasoning = response_has_tool_calls
        && reasoning_content_present
        && completion.response.provider_reasoning_history.is_empty();
    let missing_required_reasoning = requires_reasoning_content_for_tool_calls
        && response_has_tool_calls
        && !reasoning_content_present;
    if (disabled_mode_not_honored
        || missing_required_reasoning
        || missing_replay_for_present_reasoning)
        && (completion.response.provider_reasoning_history.is_empty()
            || completion
                .response
                .provider_reasoning_history
                .iter()
                .any(|replay| replay.mode_internal() != capabilities.tool_reasoning_mode))
    {
        return Err(provider_tool_reasoning_history_error(
            capabilities.tool_reasoning_mode,
        ));
    }
    Ok(())
}

impl Provider for OpenAiProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        let mut contract = self.config.protocol_contract();
        // reasoning 变体关闭时 selection.tool_reasoning_mode 已收敛为
        // DisabledForToolCalls（config.rs 选择器解析），契约直接透传。
        contract.tool_reasoning_mode = self
            .selected_model
            .as_ref()
            .map(|selection| selection.tool_reasoning_mode)
            .unwrap_or(ProviderToolReasoningMode::Unspecified);
        contract
    }

    fn streaming_capability(
        &self,
        selected_protocol: ProviderApiProtocol,
    ) -> ProviderStreamingCapability {
        ProviderStreamingCapability::for_protocol(selected_protocol)
    }

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        let mut ignore_attempt = |_| true;
        self.complete_stream_observed(request, cancellation, on_event, &mut ignore_attempt)
    }

    fn complete_stream_observed(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<ModelTurnResponse, ProviderError> {
        self.complete_internal(request, cancellation, Some(on_event), on_attempt)
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        let mut ignore_attempt = |_| true;
        self.complete_observed(request, cancellation, &mut ignore_attempt)
    }

    fn complete_observed(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<ModelTurnResponse, ProviderError> {
        self.complete_internal(request, cancellation, None, on_attempt)
    }
}

/// 记录一次终态 attempt，不改变聚合重试语义。
fn emit_provider_attempt_started(
    occurrence: &ProviderAttemptInProgress,
    on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
) -> Result<(), ProviderError> {
    if on_attempt(occurrence.started_event()) {
        Ok(())
    } else {
        Err(provider_attempt_observer_error())
    }
}

fn record_provider_attempt(
    occurrence: ProviderAttemptInProgress,
    error: Option<&ModelError>,
    usage: Option<ModelUsage>,
    on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
) -> Result<(), ProviderError> {
    let occurrence = occurrence.finish(error, usage);
    if !on_attempt(ProviderAttemptEvent::Finished(Box::new(occurrence))) {
        return Err(provider_attempt_observer_error());
    }
    Ok(())
}

fn provider_attempt_observer_error() -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::UnknownProviderError,
            "provider attempt observer rejected the event",
        )
        .with_provider_diagnostic(
            "provider_attempt_observer_failed",
            ProviderErrorStage::ResponseValidation,
        ),
    )
}

#[cfg(test)]
#[path = "../transport_tests.rs"]
mod tests;
