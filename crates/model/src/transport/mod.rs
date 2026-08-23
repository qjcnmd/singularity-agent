//! provider HTTP transport、retry、bounded body read 和取消传播。

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

use crate::MAX_PROVIDER_ATTEMPTS;
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
    provider_request_validation_error, request_uses_tool_protocol, validate_model_request,
    validate_model_request_with_capabilities,
};
use crate::provider::runtime::{OpenAiProviderConfig, SelectedModel};
use crate::provider::telemetry::{
    ProviderAttemptEvent, ProviderAttemptMetadata, ProviderAttemptOccurrence,
    ProviderAttemptOperationPhase, ProviderAttemptStarted, ProviderAttemptStatus,
    ProviderStreamEvent, ProviderStreamingCapability, provider_streaming_unsupported_error,
};
use crate::types::{
    ModelRole, ModelTurnRequest, ModelTurnResponse, ModelUsage, ProviderToolReasoningMode,
};

/// The single validated protocol choice shared by one provider completion.
struct CompletionContext {
    capabilities: ProviderProtocolContract,
    api_protocol: ProviderApiProtocol,
}

/// Mutable timing state for exactly one real provider HTTP attempt.
struct ProviderAttemptInProgress {
    operation_phase: ProviderAttemptOperationPhase,
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
        operation_phase: ProviderAttemptOperationPhase,
        provider_name: &str,
        model_name: &str,
        actual_api_protocol: ProviderApiProtocol,
        attempt_index: u32,
    ) -> Self {
        Self {
            operation_phase,
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
            operation_phase: self.operation_phase,
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
        retry_backoff: Option<Duration>,
    ) -> ProviderAttemptOccurrence {
        let terminal_status = match error.map(|error| &error.kind) {
            None => ProviderAttemptStatus::Ok,
            Some(ModelErrorKind::Cancelled) => ProviderAttemptStatus::Cancelled,
            Some(_) => ProviderAttemptStatus::Error,
        };
        let ended_at_unix_ms = unix_timestamp_ms().max(self.started_at_unix_ms);
        ProviderAttemptOccurrence {
            operation_phase: self.operation_phase,
            provider_name: self.provider_name,
            model_name: self.model_name,
            actual_api_protocol: self.actual_api_protocol,
            attempt_index: self.attempt_index,
            terminal_status,
            started_at_unix_ms: self.started_at_unix_ms,
            ended_at_unix_ms,
            attempt_duration_ms: duration_millis(self.started_at.elapsed()),
            request_send_to_headers_ms: self.request_send_to_headers_ms,
            queue_duration_ms: None,
            time_to_first_text_delta_ms: self.time_to_first_text_delta_ms,
            retry_scheduled: retry_backoff.is_some(),
            retry_backoff_ms: retry_backoff.map(duration_millis),
            error_category: error.map(ModelError::category),
            error_stage: error.and_then(|error| error.stage.clone()),
            diagnostic_code: error.and_then(|error| error.code.clone()),
            usage,
            model_turn_ordinal: None,
        }
    }
}

/// Outcome of the protocol-specific work performed on a successful HTTP
/// response inside one attempt. `Retry` asserts that this protocol side fully
/// permits resending the request (for streams additionally that no visible
/// delta has reached the caller yet); the shared loop only adds its
/// remaining-attempts budget on top.
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

/// Fold one streaming decode attempt into [`AttemptBodyOutcome`]. A stream
/// failure stays retryable only before the first visible delta: afterwards a
/// resend could duplicate already-emitted output.
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
        Err(failure)
            if !failure.emitted_text_delta && provider_error_is_retryable(&failure.error) =>
        {
            AttemptBodyOutcome::Retry {
                error: failure.error,
                time_to_first_text_delta_ms: failure.time_to_first_text_delta_ms,
            }
        }
        Err(failure) => AttemptBodyOutcome::Failed {
            error: failure.error,
            time_to_first_text_delta_ms: failure.time_to_first_text_delta_ms,
        },
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

    /// 从环境加载 OpenAI-compatible provider。
    pub fn from_env<F>(
        get_env: F,
        runtime_handle: tokio::runtime::Handle,
    ) -> Result<Self, ProviderError>
    where
        F: FnMut(&str) -> Option<String>,
    {
        crate::config::ProviderConfigSnapshot::capture(get_env, runtime_handle).provider()
    }

    /// Discover public model ids from the provider's standard `/models` endpoint.
    pub fn discover_model_ids(&self) -> Result<Vec<String>, ProviderError> {
        crate::discovery::discover_provider_models(
            &self.config,
            &self.client,
            &self.runtime,
            self.request_timeout_seconds,
        )
    }

    /// Clone a provider for one allowlisted model while freezing its protocol
    /// and token limits. The clone shares the HTTP client, runtime and caches.
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

    /// Return the fully resolved selector (`provider/model#effort`) for catalog
    /// clones, or the bare legacy model id otherwise. `None` when the provider
    /// has no resolved model (unconfigured legacy provider).
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

    /// Return the immutable catalog protocol, if this is a catalog selection.
    pub fn selected_api_protocol(&self) -> Option<ProviderApiProtocol> {
        self.selected_model
            .as_ref()
            .map(|selection| selection.api_protocol)
    }

    /// Convert the internal composite selector to the bare upstream model id.
    /// Explicit catalog clones also reject attempts to change their selected
    /// model inside a turn.
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
                        &ModelTurnResponse::completed(
                            request.request_id.clone(),
                            "provider_reasoning_history",
                            "",
                        ),
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
                    &ModelTurnResponse::completed(
                        request.request_id.clone(),
                        "provider_reasoning_history",
                        "",
                    ),
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
                &ModelTurnResponse::completed(
                    request.request_id.clone(),
                    "provider_reasoning_history",
                    "",
                ),
                ProviderToolReasoningMode::Unspecified,
            ));
        };
        if !selection.reasoning_enabled
            || selection.tool_reasoning_mode == ProviderToolReasoningMode::Unspecified
        {
            return Err(provider_tool_reasoning_history_error(
                &ModelTurnResponse::completed(
                    request.request_id.clone(),
                    "provider_reasoning_history",
                    "",
                ),
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
                    &ModelTurnResponse::completed(
                        request.request_id.clone(),
                        "provider_reasoning_history",
                        "",
                    ),
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
                    &ModelTurnResponse::completed(
                        request.request_id.clone(),
                        "provider_reasoning_history",
                        "",
                    ),
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
        let local_validation = validate_model_request(request);
        if !local_validation.valid {
            return Err(provider_request_validation_error(
                local_validation,
                &self.config,
            ));
        }
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

    fn complete_with_contract_observed(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        capabilities: &ProviderProtocolContract,
        api_protocol: ProviderApiProtocol,
        model_name: &str,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<ModelTurnResponse, ProviderError> {
        let completion = self.complete_with_contract_details_until(
            request,
            cancellation,
            capabilities,
            api_protocol,
            model_name,
            on_attempt,
        )?;
        validate_response_tool_reasoning_contract(
            request_uses_tool_protocol(request),
            &completion,
            capabilities,
            self.selected_model
                .as_ref()
                .is_some_and(|selection| selection.requires_reasoning_content_for_tool_calls),
        )?;
        Ok(completion.response)
    }

    /// Execute a bounded Chat Completions SSE attempt sequence without exposing raw events.
    fn complete_chat_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        capabilities: &ProviderProtocolContract,
        model_name: &str,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<OpenAiCompletion, ProviderError> {
        self.validate_reasoning_history(request)?;
        let request_payload = openai_chat_stream_request_payload(
            request,
            model_name,
            capabilities,
            self.selected_model
                .as_ref()
                .is_some_and(|selection| selection.reasoning_enabled),
            self.selected_model
                .as_ref()
                .is_some_and(|selection| !selection.reasoning_enabled),
            self.selected_model
                .as_ref()
                .and_then(|selection| selection.wire_reasoning_effort.as_deref()),
            self.selected_model
                .as_ref()
                .map(|selection| selection.thinking_wire_format)
                .unwrap_or(ThinkingWireFormat::ThinkingType),
            self.selected_model
                .as_ref()
                .is_none_or(|selection| selection.supports_developer_role),
            self.selected_model
                .as_ref()
                .is_none_or(|selection| selection.supports_tool_choice),
            self.selected_model
                .as_ref()
                .is_some_and(|selection| selection.requires_assistant_content_for_tool_calls),
        );
        let reasoning_variant = self
            .selected_model
            .as_ref()
            .and_then(|selection| selection.reasoning_variant.as_deref());
        self.complete_attempts(
            cancellation,
            ProviderApiProtocol::OpenAiChatCompletions,
            model_name,
            &self.config.endpoint(),
            &request_payload,
            on_attempt,
            &mut |response, attempt_started_at| {
                streaming_outcome(
                    read_openai_chat_sse(
                        &self.runtime,
                        cancellation,
                        self.request_timeout_seconds,
                        response,
                        on_event,
                        attempt_started_at,
                    ),
                    |payload| {
                        let wire_usage_present = payload.get("usage").is_some_and(Value::is_object);
                        let reasoning_content_present = openai_reasoning_content_present(&payload);
                        parse_openai_response(
                            request,
                            &self.config,
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
                    },
                )
            },
        )
    }

    /// Execute a bounded Responses SSE attempt sequence without exposing raw events.
    fn complete_responses_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        capabilities: &ProviderProtocolContract,
        model_name: &str,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<OpenAiCompletion, ProviderError> {
        self.validate_reasoning_history(request)?;
        let request_payload = openai_responses_stream_request_payload(
            request,
            model_name,
            capabilities,
            self.selected_model
                .as_ref()
                .is_some_and(|selection| selection.reasoning_enabled),
            self.selected_model
                .as_ref()
                .is_some_and(|selection| !selection.reasoning_enabled),
            self.selected_model
                .as_ref()
                .and_then(|selection| selection.wire_reasoning_effort.as_deref()),
            self.selected_model
                .as_ref()
                .is_none_or(|selection| selection.supports_tool_choice),
        );
        let reasoning_variant = self
            .selected_model
            .as_ref()
            .and_then(|selection| selection.reasoning_variant.as_deref());
        self.complete_attempts(
            cancellation,
            ProviderApiProtocol::OpenAiResponses,
            model_name,
            &responses_endpoint(&self.config.base_url),
            &request_payload,
            on_attempt,
            &mut |response, attempt_started_at| {
                streaming_outcome(
                    read_openai_responses_sse(
                        &self.runtime,
                        cancellation,
                        self.request_timeout_seconds,
                        response,
                        on_event,
                        attempt_started_at,
                    ),
                    |payload| {
                        let wire_usage_present = payload.get("usage").is_some_and(Value::is_object);
                        let reasoning_content_present =
                            openai_responses_reasoning_content_present(&payload);
                        parse_openai_responses_response(
                            request,
                            &self.config,
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
                    },
                )
            },
        )
    }

    pub(super) fn complete_with_contract_details_until(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        capabilities: &ProviderProtocolContract,
        api_protocol: ProviderApiProtocol,
        model_name: &str,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<OpenAiCompletion, ProviderError> {
        let endpoint = match api_protocol {
            ProviderApiProtocol::OpenAiResponses => responses_endpoint(&self.config.base_url),
            ProviderApiProtocol::Declared | ProviderApiProtocol::OpenAiChatCompletions => {
                self.config.endpoint()
            }
        };
        let request_payload = match api_protocol {
            ProviderApiProtocol::OpenAiResponses => openai_responses_request_payload(
                request,
                model_name,
                capabilities,
                self.selected_model
                    .as_ref()
                    .is_some_and(|selection| selection.reasoning_enabled),
                self.selected_model
                    .as_ref()
                    .is_some_and(|selection| !selection.reasoning_enabled),
                self.selected_model
                    .as_ref()
                    .and_then(|selection| selection.wire_reasoning_effort.as_deref()),
                self.selected_model
                    .as_ref()
                    .is_none_or(|selection| selection.supports_tool_choice),
            ),
            ProviderApiProtocol::Declared | ProviderApiProtocol::OpenAiChatCompletions => {
                openai_request_payload(
                    request,
                    model_name,
                    capabilities,
                    self.selected_model
                        .as_ref()
                        .is_some_and(|selection| selection.reasoning_enabled),
                    self.selected_model
                        .as_ref()
                        .is_some_and(|selection| !selection.reasoning_enabled),
                    self.selected_model
                        .as_ref()
                        .and_then(|selection| selection.wire_reasoning_effort.as_deref()),
                    self.selected_model
                        .as_ref()
                        .map(|selection| selection.thinking_wire_format)
                        .unwrap_or(ThinkingWireFormat::ThinkingType),
                    // 无 selected_model（env 配置路径）时按 false 处理：OpenAI
                    // 兼容 chat 端点对 developer role 的支持并不通用（dashscope
                    // compatible-mode 实测 HTTP 400），wire 统一投影为 system；
                    // 显式声明的模型保持用户配置的投影行为。
                    self.selected_model
                        .as_ref()
                        .is_some_and(|selection| selection.supports_developer_role),
                    self.selected_model
                        .as_ref()
                        .is_none_or(|selection| selection.supports_tool_choice),
                    self.selected_model.as_ref().is_some_and(|selection| {
                        selection.requires_assistant_content_for_tool_calls
                    }),
                )
            }
        };
        let runtime = &self.runtime;
        let request_timeout_seconds = self.request_timeout_seconds;
        let reasoning_variant = self
            .selected_model
            .as_ref()
            .and_then(|selection| selection.reasoning_variant.as_deref());
        self.complete_attempts(
            cancellation,
            api_protocol,
            model_name,
            &endpoint,
            &request_payload,
            on_attempt,
            &mut |response, _attempt_started_at| {
                let body = match read_bounded_provider_response_body(
                    runtime,
                    cancellation,
                    request_timeout_seconds,
                    response,
                ) {
                    Ok(body) => body,
                    Err(error) => {
                        return if provider_error_is_retryable(&error) {
                            AttemptBodyOutcome::Retry {
                                error,
                                time_to_first_text_delta_ms: None,
                            }
                        } else {
                            AttemptBodyOutcome::Failed {
                                error,
                                time_to_first_text_delta_ms: None,
                            }
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
                let wire_usage_present = payload.get("usage").is_some_and(Value::is_object);
                let reasoning_content_present = match api_protocol {
                    ProviderApiProtocol::OpenAiResponses => {
                        openai_responses_reasoning_content_present(&payload)
                    }
                    ProviderApiProtocol::Declared | ProviderApiProtocol::OpenAiChatCompletions => {
                        openai_reasoning_content_present(&payload)
                    }
                };
                let parsed = match api_protocol {
                    ProviderApiProtocol::OpenAiResponses => parse_openai_responses_response(
                        request,
                        &self.config,
                        payload,
                        capabilities,
                        model_name,
                        reasoning_variant,
                    ),
                    ProviderApiProtocol::Declared | ProviderApiProtocol::OpenAiChatCompletions => {
                        parse_openai_response(
                            request,
                            &self.config,
                            payload,
                            capabilities,
                            model_name,
                            reasoning_variant,
                        )
                    }
                };
                match parsed {
                    Ok(response) => AttemptBodyOutcome::Completed {
                        completion: Box::new(OpenAiCompletion {
                            response,
                            reasoning_content_present,
                        }),
                        wire_usage_present,
                        time_to_first_text_delta_ms: None,
                    },
                    Err(error) => AttemptBodyOutcome::Failed {
                        error,
                        time_to_first_text_delta_ms: None,
                    },
                }
            },
        )
    }

    /// Shared completion skeleton for both wire protocols and both streaming
    /// shapes: one bounded attempt loop owning the cancellation check, attempt
    /// telemetry, retry classification (`Retry-After` first, full-jitter
    /// backoff otherwise) and the observable start/finish event pairs. The
    /// protocol-specific body read, decode and parse run through
    /// `read_response`; its `Retry` outcome carries everything needed to
    /// schedule the next attempt.
    #[allow(clippy::too_many_arguments)]
    fn complete_attempts(
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
        let started_at = Instant::now();
        let mut metadata = ProviderAttemptMetadata::zero();
        loop {
            if cancellation.is_cancelled() {
                return Err(provider_cancelled_error().with_provider_attempt_metadata(
                    provider_attempt_metadata(&metadata, started_at),
                ));
            }
            metadata.attempt_count += 1;
            let mut occurrence = ProviderAttemptInProgress::new(
                ProviderAttemptOperationPhase::Completion,
                &self.config.provider_name,
                model_name,
                api_protocol,
                metadata.attempt_count,
            );
            emit_provider_attempt_started(&occurrence, on_attempt)?;
            let response =
                match block_on_provider_future(
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
                    Err(error)
                        if metadata.attempt_count < MAX_PROVIDER_ATTEMPTS
                            && provider_error_is_retryable(&error) =>
                    {
                        let retry_backoff = record_provider_retry(
                            &mut metadata,
                            occurrence,
                            &error.error,
                            None,
                            on_attempt,
                        )?;
                        wait_retry_backoff(
                            runtime,
                            cancellation,
                            retry_backoff,
                            &metadata,
                            started_at,
                        )?;
                        continue;
                    }
                    Err(error) => {
                        record_provider_attempt(
                            &mut metadata,
                            occurrence,
                            Some(&error.error),
                            None,
                            on_attempt,
                        )?;
                        return Err(error.with_provider_attempt_metadata(
                            provider_attempt_metadata(&metadata, started_at),
                        ));
                    }
                };
            let status = response.status();
            let status_code = status.as_u16();
            if !status.is_success() {
                // Retry-After 必须在响应体读取消费 response 之前提取。
                let retry_after = retry_after_delay(response.headers());
                // 有界读取非 2xx 响应体，供结构化解析与诊断增强；读取失败
                // 不掩盖原始状态码错误，降级为无 body 继续。
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
                    model_error_from_http_status(
                        status_code,
                        &self.config.provider_name,
                        model_name,
                    )
                };
                // 有界诊断只进入对外展示文本（ProviderError.message），不写入
                // 序列化的 ModelError：错误体内容经过有界、单行化与脱敏后仍不
                // 进入结构化边界字段。结构化上下文超限使用固定文案。
                let provider_diagnostic = if context_length_exceeded {
                    None
                } else {
                    error_fields
                        .as_ref()
                        .and_then(|fields| fields.message.as_deref())
                        .map(|message| {
                            bounded_provider_error_diagnostic(message, &self.config.api_key)
                        })
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
                // 结构化上下文超限不可重试：重发同一请求必然再次超限，即使
                // provider 将其映射到通常可重试的状态码（429/5xx）上。
                if metadata.attempt_count < MAX_PROVIDER_ATTEMPTS
                    && !context_length_exceeded
                    && http_status_is_retryable(status_code)
                {
                    let retry_backoff = record_provider_retry(
                        &mut metadata,
                        occurrence,
                        &model_error,
                        retry_after,
                        on_attempt,
                    )?;
                    wait_retry_backoff(
                        runtime,
                        cancellation,
                        retry_backoff,
                        &metadata,
                        started_at,
                    )?;
                    continue;
                }
                record_provider_attempt(
                    &mut metadata,
                    occurrence,
                    Some(&model_error),
                    None,
                    on_attempt,
                )?;
                let mut error = ProviderError::from_model_error(model_error)
                    .with_provider_attempt_metadata(provider_attempt_metadata(
                        &metadata, started_at,
                    ));
                error.message = display_message;
                return Err(error);
            }
            let response_retry_after = retry_after_delay(response.headers());
            let failure = match read_response(response, occurrence.started_at) {
                AttemptBodyOutcome::Completed {
                    completion,
                    wire_usage_present,
                    time_to_first_text_delta_ms,
                } => {
                    occurrence.set_time_to_first_text_delta(time_to_first_text_delta_ms);
                    let occurrence_error = completion.response.error.as_ref();
                    let usage = (wire_usage_present && occurrence_error.is_none())
                        .then(|| completion.response.usage.clone());
                    record_provider_attempt(
                        &mut metadata,
                        occurrence,
                        occurrence_error,
                        usage,
                        on_attempt,
                    )?;
                    let mut completion = *completion;
                    completion.response.provider_attempt_metadata =
                        Some(provider_attempt_metadata(&metadata, started_at));
                    return Ok(completion);
                }
                AttemptBodyOutcome::Retry {
                    error,
                    time_to_first_text_delta_ms,
                } if metadata.attempt_count < MAX_PROVIDER_ATTEMPTS => {
                    occurrence.set_time_to_first_text_delta(time_to_first_text_delta_ms);
                    let retry_backoff = record_provider_retry(
                        &mut metadata,
                        occurrence,
                        &error.error,
                        response_retry_after,
                        on_attempt,
                    )?;
                    wait_retry_backoff(
                        runtime,
                        cancellation,
                        retry_backoff,
                        &metadata,
                        started_at,
                    )?;
                    continue;
                }
                AttemptBodyOutcome::Retry {
                    error,
                    time_to_first_text_delta_ms,
                }
                | AttemptBodyOutcome::Failed {
                    error,
                    time_to_first_text_delta_ms,
                } => (error, time_to_first_text_delta_ms),
            };
            let (error, time_to_first_text_delta_ms) = failure;
            occurrence.set_time_to_first_text_delta(time_to_first_text_delta_ms);
            record_provider_attempt(
                &mut metadata,
                occurrence,
                Some(&error.error),
                None,
                on_attempt,
            )?;
            return Err(error
                .with_provider_attempt_metadata(provider_attempt_metadata(&metadata, started_at)));
        }
    }
}

/// Enforce the declared tool-reasoning contract on a completed response.
///
/// A response is rejected only when the contract is actually violated: the
/// provider returned reasoning content despite `DisabledForToolCalls`, or the
/// response carries tool calls without a mode-matching reasoning replay. A
/// reasoning-only final answer without tool calls is legal and does not
/// require a replay.
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
            &completion.response,
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
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error()
                .with_provider_attempt_metadata(ProviderAttemptMetadata::zero()));
        }
        let request = self.normalize_request_model(request)?;
        let context = self.prepare_completion_context_observed(&request)?;
        if self.streaming_capability(context.api_protocol)
            != ProviderStreamingCapability::OutputTextDelta
        {
            return Err(provider_streaming_unsupported_error());
        }
        let model_name = request
            .model_preferences
            .model_name
            .as_deref()
            .unwrap_or(&self.config.model_name);
        let completion = match context.api_protocol {
            ProviderApiProtocol::OpenAiResponses => self.complete_responses_stream(
                &request,
                cancellation,
                &context.capabilities,
                model_name,
                on_event,
                on_attempt,
            ),
            ProviderApiProtocol::OpenAiChatCompletions => self.complete_chat_stream(
                &request,
                cancellation,
                &context.capabilities,
                model_name,
                on_event,
                on_attempt,
            ),
            ProviderApiProtocol::Declared => Err(provider_streaming_unsupported_error()),
        }?;
        Ok(completion).and_then(|completion| {
            validate_response_tool_reasoning_contract(
                request_uses_tool_protocol(&request),
                &completion,
                &context.capabilities,
                self.selected_model
                    .as_ref()
                    .is_some_and(|selection| selection.requires_reasoning_content_for_tool_calls),
            )
            .map(|()| completion.response)
        })
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
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error()
                .with_provider_attempt_metadata(ProviderAttemptMetadata::zero()));
        }
        let request = self.normalize_request_model(request)?;
        let context = self.prepare_completion_context_observed(&request)?;
        let effective_model_name = request
            .model_preferences
            .model_name
            .as_deref()
            .unwrap_or(&self.config.model_name);
        self.complete_with_contract_observed(
            &request,
            cancellation,
            &context.capabilities,
            context.api_protocol,
            effective_model_name,
            on_attempt,
        )
    }
}

/// Cancellation-aware backoff sleep that attaches the aggregate attempt
/// telemetry when the wait is interrupted by cancellation.
fn wait_retry_backoff(
    runtime: &tokio::runtime::Handle,
    cancellation: &CancellationToken,
    duration: Duration,
    metadata: &ProviderAttemptMetadata,
    started_at: Instant,
) -> Result<(), ProviderError> {
    wait_provider_backoff(runtime, cancellation, duration).map_err(|error| {
        error.with_provider_attempt_metadata(provider_attempt_metadata(metadata, started_at))
    })
}

/// Record one terminal attempt without changing aggregate retry semantics.
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
    metadata: &mut ProviderAttemptMetadata,
    occurrence: ProviderAttemptInProgress,
    error: Option<&ModelError>,
    usage: Option<ModelUsage>,
    on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
) -> Result<(), ProviderError> {
    let occurrence = occurrence.finish(error, usage, None);
    if !on_attempt(ProviderAttemptEvent::Finished(Box::new(occurrence.clone()))) {
        return Err(provider_attempt_observer_error());
    }
    metadata.occurrences.push(occurrence);
    Ok(())
}

/// Atomically record the retry aggregate and the occurrence that scheduled it.
fn record_provider_retry(
    metadata: &mut ProviderAttemptMetadata,
    occurrence: ProviderAttemptInProgress,
    error: &ModelError,
    retry_after: Option<Duration>,
    on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
) -> Result<Duration, ProviderError> {
    metadata.retry_count += 1;
    let retry_backoff = retry_after.unwrap_or_else(|| provider_retry_backoff(metadata.retry_count));
    let occurrence = occurrence.finish(Some(error), None, Some(retry_backoff));
    if !on_attempt(ProviderAttemptEvent::Finished(Box::new(occurrence.clone()))) {
        return Err(provider_attempt_observer_error());
    }
    metadata.occurrences.push(occurrence);
    Ok(retry_backoff)
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
