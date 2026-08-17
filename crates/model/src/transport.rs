//! provider HTTP transport、retry、bounded body read 和取消传播。

use super::contract::{provider_request_validation_error, request_uses_tool_protocol};
use super::openai::{
    OpenAiCompletion, models_endpoint, openai_reasoning_content_present, openai_request_payload,
    openai_responses_reasoning_content_present, openai_responses_request_payload,
    openai_responses_stream_request_payload, parse_openai_response,
    parse_openai_responses_response,
};
use super::{
    HTTP_STATUS_FORBIDDEN, HTTP_STATUS_INTERNAL_SERVER_ERROR, HTTP_STATUS_NOT_FOUND,
    HTTP_STATUS_RATE_LIMITED, HTTP_STATUS_REQUEST_TIMEOUT, HTTP_STATUS_UNAUTHORIZED,
    MAX_PROVIDER_ATTEMPTS, MAX_PROVIDER_RESPONSE_BODY_BYTES, ModelError, ModelErrorKind, ModelRole,
    ModelTurnRequest, ModelTurnResponse, ModelUsage, OpenAiProvider, OpenAiProviderConfig,
    PROVIDER_CANCELLATION_POLL_MS, PROVIDER_RETRY_BASE_BACKOFF_MS,
    PROVIDER_RUNTIME_INITIALIZATION_ERROR_CODE, PROVIDER_RUNTIME_WORKER_THREADS,
    PROVIDER_TIMEOUT_SECONDS, Provider, ProviderApiProtocol, ProviderAttemptEvent,
    ProviderAttemptMetadata, ProviderAttemptOccurrence, ProviderAttemptOperationPhase,
    ProviderAttemptStarted, ProviderAttemptStatus, ProviderError, ProviderErrorStage,
    ProviderProtocolContract, ProviderRuntime, ProviderStreamEvent, ProviderStreamingCapability,
    ProviderToolReasoningMode, ProviderTransportCategory, provider_streaming_unsupported_error,
    responses_endpoint, validate_model_request, validate_model_request_with_capabilities,
};
use serde_json::Value;
use singularity_core::CancellationToken;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

/// The single validated protocol choice shared by one provider completion.
struct CompletionContext {
    capabilities: ProviderProtocolContract,
    api_protocol: ProviderApiProtocol,
}

/// A stream attempt error plus whether retrying could duplicate visible text.
struct StreamAttemptFailure {
    error: ProviderError,
    emitted_text_delta: bool,
    time_to_first_text_delta_ms: Option<u64>,
}

/// A completed stream decode plus timing captured at the decoder boundary.
struct StreamAttemptSuccess {
    payload: Value,
    time_to_first_text_delta_ms: Option<u64>,
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
            parent_occurrence_id: None,
        }
    }
}

impl OpenAiProvider {
    /// 创建并校验 OpenAI-compatible provider。
    pub fn new(config: OpenAiProviderConfig) -> Result<Self, ProviderError> {
        Self::new_with_request_timeout(config, PROVIDER_TIMEOUT_SECONDS)
    }

    /// 创建 provider，并绑定调用方已经拥有的 Tokio runtime handle。
    pub fn new_with_runtime_handle(
        config: OpenAiProviderConfig,
        runtime_handle: tokio::runtime::Handle,
    ) -> Result<Self, ProviderError> {
        Self::new_with_runtime(
            config,
            PROVIDER_TIMEOUT_SECONDS,
            ProviderRuntime::External(runtime_handle),
        )
    }

    pub(super) fn new_with_request_timeout(
        config: OpenAiProviderConfig,
        request_timeout_seconds: u64,
    ) -> Result<Self, ProviderError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(PROVIDER_RUNTIME_WORKER_THREADS)
            .enable_all()
            .build()
            .map_err(provider_runtime_error)?;
        Self::new_with_runtime(
            config,
            request_timeout_seconds,
            ProviderRuntime::Owned(Arc::new(runtime)),
        )
    }

    fn new_with_runtime(
        config: OpenAiProviderConfig,
        request_timeout_seconds: u64,
        runtime: ProviderRuntime,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            // Provider completions are streamed: a long response must not be
            // rejected merely because its total generation time exceeds the
            // request budget.  Keep the budget as an idle read timeout so a
            // stalled connection still fails without relying on an outer
            // evaluation deadline.
            .read_timeout(Duration::from_secs(request_timeout_seconds))
            // 显式 UA：部分网关（如 opencode.ai 的 Cloudflare 保护）对默认/无 UA
            // 请求做机器人拦截（实测 HTTP 403 error 1010），自家 UA 实测可放行。
            .user_agent(format!("singularity-agent/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(provider_client_initialization_error)?;
        Ok(Self {
            config,
            selected_model: None,
            client,
            runtime: Arc::new(runtime),
            request_timeout_seconds,
        })
    }

    /// 从环境加载 OpenAI-compatible provider。
    pub fn from_env<F>(get_env: F) -> Result<Self, ProviderError>
    where
        F: FnMut(&str) -> Option<String>,
    {
        super::ProviderConfigSnapshot::capture(get_env, None).provider()
    }

    /// Discover public model ids from the provider's standard `/models` endpoint.
    ///
    /// The response is intentionally reduced to ids only; it never becomes a
    /// capability source and no API key is included in the returned value.
    pub fn discover_model_ids(&self) -> Result<Vec<String>, ProviderError> {
        let endpoint = models_endpoint(&self.config.base_url);
        let runtime = self.runtime.as_ref();
        let cancellation = CancellationToken::new();
        let response = block_on_provider_future(
            runtime,
            &cancellation,
            "provider_models_request_failed",
            ProviderErrorStage::RequestSend,
            self.request_timeout_seconds,
            || {
                self.client
                    .get(&endpoint)
                    .bearer_auth(&self.config.api_key)
                    .send()
            },
        )?;
        let status = response.status();
        if !status.is_success() {
            return Err(ProviderError::from_model_error(
                model_error_from_http_status(status.as_u16(), &self.config.provider_name, "models"),
            ));
        }
        let body = read_bounded_provider_response_body(
            runtime,
            &cancellation,
            self.request_timeout_seconds,
            response,
        )?;
        if body.len() > super::MAX_DISCOVERY_RESPONSE_BYTES {
            return Err(provider_response_body_too_large_error());
        }
        let payload: Value = serde_json::from_slice(&body).map_err(|_| {
            ProviderError::from_model_error(
                super::ModelError::new(
                    super::ModelErrorKind::JsonSchemaViolation,
                    "provider models response was not valid JSON",
                )
                .with_provider_diagnostic(
                    "provider_models_json_decode_failed",
                    ProviderErrorStage::ResponseJsonDecode,
                ),
            )
        })?;
        let data = payload
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                ProviderError::from_model_error(
                    super::ModelError::new(
                        super::ModelErrorKind::JsonSchemaViolation,
                        "provider models response did not contain a data array",
                    )
                    .with_provider_diagnostic(
                        "provider_models_schema_invalid",
                        ProviderErrorStage::ResponseValidation,
                    ),
                )
            })?;
        if data.len() > super::MAX_DISCOVERED_MODEL_IDS {
            return Err(ProviderError::from_model_error(
                super::ModelError::new(
                    super::ModelErrorKind::JsonSchemaViolation,
                    "provider models response exceeded the model id safety limit",
                )
                .with_provider_diagnostic(
                    "provider_models_too_many_ids",
                    ProviderErrorStage::ResponseValidation,
                ),
            ));
        }
        let mut model_ids = Vec::with_capacity(data.len());
        let mut seen_ids = HashSet::with_capacity(data.len());
        for item in data {
            let id = item.get("id").and_then(Value::as_str).ok_or_else(|| {
                ProviderError::from_model_error(
                    super::ModelError::new(
                        super::ModelErrorKind::JsonSchemaViolation,
                        "provider models response contained an entry without a model id",
                    )
                    .with_provider_diagnostic(
                        "provider_models_schema_invalid",
                        ProviderErrorStage::ResponseValidation,
                    ),
                )
            })?;
            if id.is_empty()
                || id.chars().count() > super::MAX_MODEL_ID_LENGTH
                || id
                    .chars()
                    .any(|character| character.is_control() || character.is_whitespace())
            {
                return Err(ProviderError::from_model_error(
                    super::ModelError::new(
                        super::ModelErrorKind::JsonSchemaViolation,
                        "provider models response contained a malformed model id",
                    )
                    .with_provider_diagnostic(
                        "provider_models_schema_invalid",
                        ProviderErrorStage::ResponseValidation,
                    ),
                ));
            }
            if !seen_ids.insert(id) {
                return Err(ProviderError::from_model_error(
                    super::ModelError::new(
                        super::ModelErrorKind::JsonSchemaViolation,
                        "provider models response contained duplicate model ids",
                    )
                    .with_provider_diagnostic(
                        "provider_models_schema_invalid",
                        ProviderErrorStage::ResponseValidation,
                    ),
                ));
            }
            model_ids.push(id.to_string());
        }
        if model_ids.is_empty() {
            return Err(ProviderError::from_model_error(
                super::ModelError::new(
                    super::ModelErrorKind::JsonSchemaViolation,
                    "provider models response did not contain model ids",
                )
                .with_provider_diagnostic(
                    "provider_models_empty",
                    ProviderErrorStage::ResponseValidation,
                ),
            ));
        }
        Ok(model_ids)
    }

    /// Clone a provider for one allowlisted model while freezing its protocol
    /// and token limits. The clone shares the HTTP client, runtime and caches.
    pub(super) fn with_selected_model(&self, selected_model: super::SelectedModel) -> Self {
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
        let variant = selection.reasoning_variant.as_deref().ok_or_else(|| {
            provider_tool_reasoning_history_error(
                &ModelTurnResponse::completed(
                    request.request_id.clone(),
                    "provider_reasoning_history",
                    "",
                ),
                selection.tool_reasoning_mode,
            )
        })?;
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
        // selected_model 或 endpoint 后缀决定（删 probe 后不再探测协议）。
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
        let runtime = self.runtime.as_ref();
        let started_at = Instant::now();
        let mut metadata = ProviderAttemptMetadata::zero();
        let endpoint = responses_endpoint(&self.config.base_url);
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
                ProviderApiProtocol::OpenAiResponses,
                metadata.attempt_count,
            );
            emit_provider_attempt_started(&occurrence, on_attempt)?;
            let response = match block_on_provider_future(
                runtime,
                cancellation,
                "provider_request_send_failed",
                ProviderErrorStage::RequestSend,
                self.request_timeout_seconds,
                || {
                    self.client
                        .post(&endpoint)
                        .bearer_auth(&self.config.api_key)
                        .json(&request_payload)
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
                    let retry_backoff =
                        record_provider_retry(&mut metadata, occurrence, &error.error, on_attempt)?;
                    wait_stream_retry_backoff(
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
                    return Err(
                        error.with_provider_attempt_metadata(provider_attempt_metadata(
                            &metadata, started_at,
                        )),
                    );
                }
            };
            let status = response.status();
            if !status.is_success() {
                let error = ProviderError::from_model_error(model_error_from_http_status(
                    status.as_u16(),
                    &self.config.provider_name,
                    model_name,
                ));
                if metadata.attempt_count < MAX_PROVIDER_ATTEMPTS
                    && http_status_is_retryable(status.as_u16())
                {
                    let retry_backoff =
                        record_provider_retry(&mut metadata, occurrence, &error.error, on_attempt)?;
                    wait_stream_retry_backoff(
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
                    Some(&error.error),
                    None,
                    on_attempt,
                )?;
                return Err(
                    error.with_provider_attempt_metadata(provider_attempt_metadata(
                        &metadata, started_at,
                    )),
                );
            }
            let attempt = read_openai_responses_sse(
                runtime,
                cancellation,
                self.request_timeout_seconds,
                response,
                on_event,
                occurrence.started_at,
            );
            let payload = match attempt {
                Ok(success) => {
                    occurrence.set_time_to_first_text_delta(success.time_to_first_text_delta_ms);
                    success.payload
                }
                Err(failure)
                    if !failure.emitted_text_delta
                        && metadata.attempt_count < MAX_PROVIDER_ATTEMPTS
                        && provider_error_is_retryable(&failure.error) =>
                {
                    occurrence.set_time_to_first_text_delta(failure.time_to_first_text_delta_ms);
                    let retry_backoff = record_provider_retry(
                        &mut metadata,
                        occurrence,
                        &failure.error.error,
                        on_attempt,
                    )?;
                    wait_stream_retry_backoff(
                        runtime,
                        cancellation,
                        retry_backoff,
                        &metadata,
                        started_at,
                    )?;
                    continue;
                }
                Err(failure) => {
                    occurrence.set_time_to_first_text_delta(failure.time_to_first_text_delta_ms);
                    record_provider_attempt(
                        &mut metadata,
                        occurrence,
                        Some(&failure.error.error),
                        None,
                        on_attempt,
                    )?;
                    return Err(failure.error.with_provider_attempt_metadata(
                        provider_attempt_metadata(&metadata, started_at),
                    ));
                }
            };
            let reasoning_content_present = openai_responses_reasoning_content_present(&payload);
            let usage_available = payload.get("usage").is_some_and(Value::is_object);
            let parsed = parse_openai_responses_response(
                request,
                &self.config,
                payload,
                capabilities,
                model_name,
                self.selected_model
                    .as_ref()
                    .and_then(|selection| selection.reasoning_variant.as_deref()),
            );
            return match parsed {
                Ok(mut response) => {
                    let occurrence_error = response.error.as_ref();
                    let usage = (usage_available && occurrence_error.is_none())
                        .then(|| response.usage.clone());
                    record_provider_attempt(
                        &mut metadata,
                        occurrence,
                        occurrence_error,
                        usage,
                        on_attempt,
                    )?;
                    response.provider_attempt_metadata =
                        Some(provider_attempt_metadata(&metadata, started_at));
                    Ok(OpenAiCompletion {
                        response,
                        reasoning_content_present,
                    })
                }
                Err(error) => {
                    record_provider_attempt(
                        &mut metadata,
                        occurrence,
                        Some(&error.error),
                        None,
                        on_attempt,
                    )?;
                    Err(
                        error.with_provider_attempt_metadata(provider_attempt_metadata(
                            &metadata, started_at,
                        )),
                    )
                }
            };
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn complete_with_contract_details_until(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        capabilities: &ProviderProtocolContract,
        api_protocol: ProviderApiProtocol,
        model_name: &str,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<OpenAiCompletion, ProviderError> {
        let runtime = self.runtime.as_ref();
        let started_at = Instant::now();
        let mut metadata = ProviderAttemptMetadata::zero();
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
                        .unwrap_or(super::ThinkingWireFormat::ThinkingType),
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
        let operation_phase = ProviderAttemptOperationPhase::Completion;
        loop {
            if cancellation.is_cancelled() {
                return Err(provider_cancelled_error().with_provider_attempt_metadata(
                    provider_attempt_metadata(&metadata, started_at),
                ));
            }
            metadata.attempt_count += 1;
            let mut occurrence = ProviderAttemptInProgress::new(
                operation_phase,
                &self.config.provider_name,
                model_name,
                api_protocol,
                metadata.attempt_count,
            );
            emit_provider_attempt_started(&occurrence, on_attempt)?;
            let response = match block_on_provider_future(
                runtime,
                cancellation,
                "provider_request_send_failed",
                ProviderErrorStage::RequestSend,
                self.request_timeout_seconds,
                || {
                    self.client
                        .post(&endpoint)
                        .bearer_auth(&self.config.api_key)
                        .json(&request_payload)
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
                    let retry_backoff =
                        record_provider_retry(&mut metadata, occurrence, &error.error, on_attempt)?;
                    wait_provider_backoff(runtime, cancellation, retry_backoff).map_err(
                        |cancelled| {
                            cancelled.with_provider_attempt_metadata(provider_attempt_metadata(
                                &metadata, started_at,
                            ))
                        },
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
                    return Err(
                        error.with_provider_attempt_metadata(provider_attempt_metadata(
                            &metadata, started_at,
                        )),
                    );
                }
            };
            let status = response.status();
            if !status.is_success() {
                let error = ProviderError::from_model_error(model_error_from_http_status(
                    status.as_u16(),
                    &self.config.provider_name,
                    model_name,
                ));
                if metadata.attempt_count < MAX_PROVIDER_ATTEMPTS
                    && http_status_is_retryable(status.as_u16())
                {
                    let retry_backoff =
                        record_provider_retry(&mut metadata, occurrence, &error.error, on_attempt)?;
                    wait_provider_backoff(runtime, cancellation, retry_backoff).map_err(
                        |cancelled| {
                            cancelled.with_provider_attempt_metadata(provider_attempt_metadata(
                                &metadata, started_at,
                            ))
                        },
                    )?;
                    continue;
                }
                record_provider_attempt(
                    &mut metadata,
                    occurrence,
                    Some(&error.error),
                    None,
                    on_attempt,
                )?;
                return Err(
                    error.with_provider_attempt_metadata(provider_attempt_metadata(
                        &metadata, started_at,
                    )),
                );
            }
            let body = match read_bounded_provider_response_body(
                runtime,
                cancellation,
                self.request_timeout_seconds,
                response,
            ) {
                Ok(body) => body,
                Err(error)
                    if metadata.attempt_count < MAX_PROVIDER_ATTEMPTS
                        && provider_error_is_retryable(&error) =>
                {
                    let retry_backoff =
                        record_provider_retry(&mut metadata, occurrence, &error.error, on_attempt)?;
                    wait_provider_backoff(runtime, cancellation, retry_backoff).map_err(
                        |cancelled| {
                            cancelled.with_provider_attempt_metadata(provider_attempt_metadata(
                                &metadata, started_at,
                            ))
                        },
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
                    return Err(
                        error.with_provider_attempt_metadata(provider_attempt_metadata(
                            &metadata, started_at,
                        )),
                    );
                }
            };
            let payload =
                match serde_json::from_slice::<Value>(&body) {
                    Ok(payload) => payload,
                    Err(_) => {
                        let error = ProviderError::from_model_error(provider_response_json_error());
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
            let usage_available = payload.get("usage").is_some_and(Value::is_object);
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
                    self.selected_model
                        .as_ref()
                        .and_then(|selection| selection.reasoning_variant.as_deref()),
                ),
                ProviderApiProtocol::Declared | ProviderApiProtocol::OpenAiChatCompletions => {
                    parse_openai_response(
                        request,
                        &self.config,
                        payload,
                        capabilities,
                        model_name,
                        self.selected_model
                            .as_ref()
                            .and_then(|selection| selection.reasoning_variant.as_deref()),
                    )
                }
            };
            return match parsed {
                Ok(mut response) => {
                    let occurrence_error = response.error.as_ref();
                    let usage = (usage_available && occurrence_error.is_none())
                        .then(|| response.usage.clone());
                    record_provider_attempt(
                        &mut metadata,
                        occurrence,
                        occurrence_error,
                        usage,
                        on_attempt,
                    )?;
                    response.provider_attempt_metadata =
                        Some(provider_attempt_metadata(&metadata, started_at));
                    Ok(OpenAiCompletion {
                        response,
                        reasoning_content_present,
                    })
                }
                Err(error) => {
                    record_provider_attempt(
                        &mut metadata,
                        occurrence,
                        Some(&error.error),
                        None,
                        on_attempt,
                    )?;
                    Err(
                        error.with_provider_attempt_metadata(provider_attempt_metadata(
                            &metadata, started_at,
                        )),
                    )
                }
            };
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
        // Catalog 克隆：用户显式能力声明（config.json `capabilities` 块）叠加到静态基线；
        // 顶层字段优先的合并已在配置解析时完成，这里只做声明 → 契约投影。
        // reasoning 变体关闭时 selection.tool_reasoning_mode 已收敛为
        // DisabledForToolCalls（config.rs 选择器解析），契约直接透传。
        contract.tool_reasoning_mode = self
            .selected_model
            .as_ref()
            .map(|selection| selection.tool_reasoning_mode)
            .unwrap_or(ProviderToolReasoningMode::Unspecified);
        if let Some(overrides) = self
            .selected_model
            .as_ref()
            .and_then(|selection| selection.capability_overrides.as_ref())
        {
            contract.supports_tools = overrides.supports_tools.unwrap_or(contract.supports_tools);
            contract.supports_parallel_tool_calls = overrides
                .supports_parallel_tool_calls
                .unwrap_or(contract.supports_parallel_tool_calls);
            contract.supports_required_tool_choice = overrides
                .supports_required_tool_choice
                .unwrap_or(contract.supports_required_tool_choice);
            contract.supports_strict_tool_schema = overrides
                .supports_strict_tool_schema
                .unwrap_or(contract.supports_strict_tool_schema);
            contract.supports_json_mode = overrides
                .supports_json_mode
                .unwrap_or(contract.supports_json_mode);
            contract.supports_system_message = overrides
                .supports_system_message
                .unwrap_or(contract.supports_system_message);
            contract.supports_developer_message = overrides
                .supports_developer_message
                .unwrap_or(contract.supports_developer_message);
            contract.max_tools_per_request = overrides
                .max_tools_per_request
                .unwrap_or(contract.max_tools_per_request);
            contract.max_parallel_tool_calls = overrides
                .max_parallel_tool_calls
                .unwrap_or(contract.max_parallel_tool_calls);
            contract.max_context_tokens =
                overrides.max_context_tokens.or(contract.max_context_tokens);
            contract.max_output_tokens = overrides
                .max_output_tokens
                .unwrap_or(contract.max_output_tokens);
        }
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
        self.complete_responses_stream(
            &request,
            cancellation,
            &context.capabilities,
            model_name,
            on_event,
            on_attempt,
        )
        .and_then(|completion| {
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

fn wait_stream_retry_backoff(
    runtime: &ProviderRuntime,
    cancellation: &CancellationToken,
    duration: Duration,
    metadata: &ProviderAttemptMetadata,
    started_at: Instant,
) -> Result<(), ProviderError> {
    wait_provider_backoff(runtime, cancellation, duration).map_err(|error| {
        error.with_provider_attempt_metadata(provider_attempt_metadata(metadata, started_at))
    })
}

/// Decode one Responses body while preserving arbitrary HTTP chunk and SSE frame boundaries.
fn read_openai_responses_sse(
    runtime: &ProviderRuntime,
    cancellation: &CancellationToken,
    request_timeout_seconds: u64,
    mut response: reqwest::Response,
    on_event: &mut dyn FnMut(ProviderStreamEvent),
    attempt_started_at: Instant,
) -> Result<StreamAttemptSuccess, StreamAttemptFailure> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BODY_BYTES as u64)
    {
        return Err(StreamAttemptFailure {
            error: provider_response_stream_too_large_error(),
            emitted_text_delta: false,
            time_to_first_text_delta_ms: None,
        });
    }
    let mut decoder = ResponsesSseDecoder::new(on_event, attempt_started_at);
    loop {
        let chunk = match block_on_provider_future(
            runtime,
            cancellation,
            "provider_response_body_read_failed",
            ProviderErrorStage::ResponseBodyRead,
            request_timeout_seconds,
            || response.chunk(),
        ) {
            Ok(chunk) => chunk,
            Err(error) => {
                return Err(StreamAttemptFailure {
                    error,
                    emitted_text_delta: decoder.emitted_text_delta,
                    time_to_first_text_delta_ms: decoder.time_to_first_text_delta_ms,
                });
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        if let Err(error) = decoder.push(&chunk) {
            return Err(StreamAttemptFailure {
                error,
                emitted_text_delta: decoder.emitted_text_delta,
                time_to_first_text_delta_ms: decoder.time_to_first_text_delta_ms,
            });
        }
    }
    match decoder.finish() {
        Ok(payload) => Ok(StreamAttemptSuccess {
            payload,
            time_to_first_text_delta_ms: decoder.time_to_first_text_delta_ms,
        }),
        Err(error) => Err(StreamAttemptFailure {
            error,
            emitted_text_delta: decoder.emitted_text_delta,
            time_to_first_text_delta_ms: decoder.time_to_first_text_delta_ms,
        }),
    }
}

/// Incremental, total-size-bounded SSE decoder for the Responses event contract.
struct ResponsesSseDecoder<'a> {
    pending: Vec<u8>,
    event_data: Vec<u8>,
    event_name: Option<String>,
    total_bytes: usize,
    terminal_response: Option<Value>,
    emitted_text_delta: bool,
    attempt_started_at: Instant,
    time_to_first_text_delta_ms: Option<u64>,
    on_event: &'a mut dyn FnMut(ProviderStreamEvent),
}

impl<'a> ResponsesSseDecoder<'a> {
    fn new(on_event: &'a mut dyn FnMut(ProviderStreamEvent), attempt_started_at: Instant) -> Self {
        Self {
            pending: Vec::new(),
            event_data: Vec::new(),
            event_name: None,
            total_bytes: 0,
            terminal_response: None,
            emitted_text_delta: false,
            attempt_started_at,
            time_to_first_text_delta_ms: None,
            on_event,
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        self.total_bytes = self
            .total_bytes
            .checked_add(chunk.len())
            .ok_or_else(provider_response_stream_too_large_error)?;
        if self.total_bytes > MAX_PROVIDER_RESPONSE_BODY_BYTES {
            return Err(provider_response_stream_too_large_error());
        }
        self.pending.extend_from_slice(chunk);
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.process_line(&line)?;
        }
        Ok(())
    }

    fn process_line(&mut self, line: &[u8]) -> Result<(), ProviderError> {
        if line.is_empty() {
            return self.dispatch_event();
        }
        if line.first() == Some(&b':') {
            return Ok(());
        }
        let (field, value) = if let Some(separator) = line.iter().position(|byte| *byte == b':') {
            let value = line.get(separator + 1..).unwrap_or_default();
            let value = if value.first() == Some(&b' ') {
                value.get(1..).unwrap_or_default()
            } else {
                value
            };
            (line.get(..separator).unwrap_or_default(), value)
        } else {
            (line, &[] as &[u8])
        };
        match field {
            b"data" => {
                let additional = value.len().saturating_add(1);
                if self.event_data.len().saturating_add(additional)
                    > MAX_PROVIDER_RESPONSE_BODY_BYTES
                {
                    return Err(provider_response_stream_too_large_error());
                }
                if !self.event_data.is_empty() {
                    self.event_data.push(b'\n');
                }
                self.event_data.extend_from_slice(value);
            }
            b"event" => {
                let event = std::str::from_utf8(value)
                    .map_err(|_| provider_responses_stream_malformed_error("event_name_invalid"))?;
                self.event_name = Some(event.to_string());
            }
            b"id" | b"retry" => {}
            _ => {}
        }
        Ok(())
    }

    fn dispatch_event(&mut self) -> Result<(), ProviderError> {
        if self.event_data.is_empty() {
            self.event_name = None;
            return Ok(());
        }
        let payload = serde_json::from_slice::<Value>(&self.event_data)
            .map_err(|_| provider_responses_stream_malformed_error("event_data_invalid_json"))?;
        self.event_data.clear();
        let payload_type = payload
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| provider_responses_stream_malformed_error("event_type_missing"))?;
        if self
            .event_name
            .as_deref()
            .is_some_and(|event_name| event_name != payload_type)
        {
            return Err(provider_responses_stream_malformed_error(
                "event_type_mismatch",
            ));
        }
        self.event_name = None;
        if payload_type == "ping" {
            return Ok(());
        }
        if self.terminal_response.is_some() {
            return Err(provider_responses_stream_malformed_error(
                "event_after_terminal",
            ));
        }
        match payload_type {
            "response.output_text.delta" => {
                let delta = payload
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        provider_responses_stream_malformed_error("output_text_delta_missing")
                    })?;
                if !delta.is_empty() {
                    if self.time_to_first_text_delta_ms.is_none() {
                        self.time_to_first_text_delta_ms =
                            Some(duration_millis(self.attempt_started_at.elapsed()));
                    }
                    self.emitted_text_delta = true;
                    (self.on_event)(ProviderStreamEvent::OutputTextDelta {
                        delta: delta.to_string(),
                    });
                }
            }
            "response.completed" => {
                let response = payload.get("response").cloned().ok_or_else(|| {
                    provider_responses_stream_malformed_error("completed_response_missing")
                })?;
                if !response.is_object() {
                    return Err(provider_responses_stream_malformed_error(
                        "completed_response_invalid",
                    ));
                }
                self.terminal_response = Some(response);
            }
            "error" => {
                return Err(provider_responses_stream_terminal_error(
                    "responses_stream_error",
                    "provider Responses stream returned an error",
                ));
            }
            "response.failed" => {
                return Err(provider_responses_stream_terminal_error(
                    "responses_stream_failed",
                    "provider Responses stream failed",
                ));
            }
            "response.incomplete" => {
                // OpenAI Responses 语义：provider 主动宣告响应未完成，最常见原因是
                // max_output_tokens 截断。事件 data 是完整 Response 对象，原因在
                // `response.incomplete_details.reason`（openai-python
                // `ResponseIncompleteEvent.response: Response` + vLLM E2E 同构）。
                // 把 reason 带进错误文本，避免诊断时只能看到笼统的
                // "stream was incomplete"。
                let reason = payload
                    .get("response")
                    .and_then(|response| response.get("incomplete_details"))
                    .and_then(|details| details.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let mut error = ModelError::new(
                    ModelErrorKind::UnknownProviderError,
                    format!("provider Responses stream was incomplete (reason: {reason})"),
                )
                .with_provider_diagnostic(
                    "responses_stream_incomplete",
                    ProviderErrorStage::ResponseValidation,
                );
                error
                    .validation_errors
                    .push("responses_stream_incomplete".to_string());
                return Err(ProviderError::from_model_error(error));
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Value, ProviderError> {
        if !self.pending.is_empty() || !self.event_data.is_empty() || self.event_name.is_some() {
            return Err(provider_responses_stream_malformed_error(
                "event_frame_unterminated",
            ));
        }
        self.terminal_response
            .clone()
            .ok_or_else(provider_responses_stream_terminal_missing_error)
    }
}

fn provider_responses_stream_malformed_error(reason: &'static str) -> ProviderError {
    let mut error = ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider Responses stream was malformed",
    )
    .with_provider_diagnostic(
        "responses_stream_malformed",
        ProviderErrorStage::ResponseValidation,
    );
    error.validation_errors.push(reason.to_string());
    ProviderError::from_model_error(error)
}

fn provider_responses_stream_terminal_missing_error() -> ProviderError {
    let mut error = ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider Responses stream did not contain a completed terminal",
    )
    .with_provider_diagnostic(
        "responses_stream_terminal_missing",
        ProviderErrorStage::ResponseValidation,
    );
    error
        .validation_errors
        .push("responses_stream_terminal_missing".to_string());
    ProviderError::from_model_error(error)
}

fn provider_responses_stream_terminal_error(
    code: &'static str,
    message: &'static str,
) -> ProviderError {
    let mut error = ModelError::new(ModelErrorKind::UnknownProviderError, message)
        .with_provider_diagnostic(code, ProviderErrorStage::ResponseValidation);
    error.validation_errors.push(code.to_string());
    ProviderError::from_model_error(error)
}

fn provider_response_stream_too_large_error() -> ProviderError {
    let mut error = ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider Responses stream exceeded the fixed safety limit",
    )
    .with_provider_diagnostic(
        "provider_response_stream_too_large",
        ProviderErrorStage::ResponseBodyRead,
    );
    error
        .validation_errors
        .push("provider_response_stream_too_large".to_string());
    ProviderError::from_model_error(error)
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
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
    on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
) -> Result<Duration, ProviderError> {
    metadata.retry_count += 1;
    let retry_backoff = provider_retry_backoff(metadata.retry_count);
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

pub(super) fn model_error_from_http_status(
    status: u16,
    provider_name: &str,
    model_name: &str,
) -> ModelError {
    let kind = match status {
        HTTP_STATUS_UNAUTHORIZED | HTTP_STATUS_FORBIDDEN => ModelErrorKind::AuthError,
        HTTP_STATUS_REQUEST_TIMEOUT => ModelErrorKind::Timeout,
        HTTP_STATUS_NOT_FOUND => ModelErrorKind::InvalidRequest,
        HTTP_STATUS_RATE_LIMITED => ModelErrorKind::RateLimited,
        status if status >= HTTP_STATUS_INTERNAL_SERVER_ERROR => ModelErrorKind::ProviderOverloaded,
        _ => ModelErrorKind::UnknownProviderError,
    };
    let message = format!("Provider returned HTTP {status}.");
    let mut error = ModelError::new(kind, message)
        .with_provider(provider_name.to_string())
        .with_model(model_name.to_string())
        .with_provider_diagnostic("provider_http_status", ProviderErrorStage::ResponseStatus);
    error.http_status = Some(status);
    error
}

pub(super) fn provider_transport_error(
    error: reqwest::Error,
    code: &'static str,
    stage: ProviderErrorStage,
    request_timeout_seconds: Option<u64>,
) -> ProviderError {
    let kind = if error.is_timeout() {
        ModelErrorKind::Timeout
    } else {
        ModelErrorKind::NetworkError
    };
    let timeout = error.is_timeout();
    let category = if error.is_timeout() {
        ProviderTransportCategory::Timeout
    } else if error.is_connect() {
        ProviderTransportCategory::Connect
    } else if error.is_request() {
        ProviderTransportCategory::Request
    } else if error.is_body() {
        ProviderTransportCategory::BodyRead
    } else {
        ProviderTransportCategory::Unknown
    };
    // 消息带 reqwest 原因（如 connection refused / timeout），否则只剩笼统的
    // "provider transport failed"，无法区分连接、超时还是响应体读取失败。
    // 用 `without_url()` 去掉 URL：reqwest 错误原文含请求地址（脱敏测试要求
    // 错误序列化不得含地址/密钥），原因本身保留。
    let message = format!("provider transport failed: {}", error.without_url());
    let mut model_error = ModelError::new(kind, message).with_provider_diagnostic(code, stage);
    model_error.transport_category = Some(category);
    if timeout {
        model_error.timeout_seconds = request_timeout_seconds;
    }
    ProviderError::from_model_error(model_error)
}

pub(super) fn provider_runtime_error(_error: std::io::Error) -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::UnknownProviderError,
            "provider runtime initialization failed",
        )
        .with_provider_diagnostic(
            PROVIDER_RUNTIME_INITIALIZATION_ERROR_CODE,
            ProviderErrorStage::ClientInitialization,
        ),
    )
}

pub(super) fn provider_client_initialization_error(error: reqwest::Error) -> ProviderError {
    provider_transport_error(
        error,
        "provider_client_initialization_failed",
        ProviderErrorStage::ClientInitialization,
        None,
    )
}

pub(super) fn provider_cancelled_error() -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(ModelErrorKind::Cancelled, "provider request cancelled")
            .with_provider_diagnostic("provider_request_cancelled", ProviderErrorStage::Cancelled),
    )
}

pub(super) fn provider_tool_reasoning_history_error(
    response: &ModelTurnResponse,
    mode: ProviderToolReasoningMode,
) -> ProviderError {
    let (code, evidence) = if mode == ProviderToolReasoningMode::DisabledForToolCalls {
        (
            "provider_tool_reasoning_mode_not_honored",
            "tool_reasoning_disable_not_honored",
        )
    } else {
        (
            "provider_tool_reasoning_history_unsupported",
            "tool_reasoning_content_requires_adapter_history_support",
        )
    };
    let mut error = ModelError::new(
        ModelErrorKind::UnsupportedCapability,
        "provider returned tool reasoning that cannot be safely replayed",
    )
    .with_provider_diagnostic(code, ProviderErrorStage::ResponseValidation);
    error.validation_errors.push(evidence.to_string());
    let provider_error = ProviderError::from_model_error(error);
    if let Some(metadata) = &response.provider_attempt_metadata {
        provider_error.with_provider_attempt_metadata(metadata.clone())
    } else {
        provider_error
    }
}

pub(super) fn provider_attempt_metadata(
    metadata: &ProviderAttemptMetadata,
    started_at: Instant,
) -> ProviderAttemptMetadata {
    ProviderAttemptMetadata {
        attempt_count: metadata.attempt_count,
        retry_count: metadata.retry_count,
        latency_ms: started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        occurrences: metadata.occurrences.clone(),
    }
}

pub(super) fn provider_error_is_retryable(error: &ProviderError) -> bool {
    // 只对网络层快速失败重试（连接失败/中断）。挂起超时不重试：120s 无响应后
    // 重试大概率仍无响应，且 6 次重试 × 120s 会让单个挂起请求拖到 12 分钟
    // （评估实测：模型请求挂起时 cell 被拖满 1800s 超时）。
    matches!(error.error.kind, ModelErrorKind::NetworkError)
        && !matches!(
            error.error.transport_category,
            Some(ProviderTransportCategory::Request)
        )
}

pub(super) fn http_status_is_retryable(status: u16) -> bool {
    // 对齐 Pi 重试条件：408（服务器明确返回请求超时）/409/429/5xx。
    // 注意与客户端挂起超时的区别：客户端 120s 无响应是 fail-fast（不重试），
    // 408 是服务端明确信号（临时过载，值得重试）。
    status == HTTP_STATUS_REQUEST_TIMEOUT
        || status == HTTP_STATUS_RATE_LIMITED
        || status >= HTTP_STATUS_INTERNAL_SERVER_ERROR
}

pub(super) fn provider_retry_backoff(retry_count: u32) -> Duration {
    let shift = retry_count.saturating_sub(1).min(10);
    let multiplier = 1_u64 << shift;
    Duration::from_millis(PROVIDER_RETRY_BASE_BACKOFF_MS.saturating_mul(multiplier))
}

pub(super) fn wait_provider_backoff(
    runtime: &ProviderRuntime,
    cancellation: &CancellationToken,
    duration: Duration,
) -> Result<(), ProviderError> {
    let deadline = Instant::now() + duration;
    loop {
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        let poll = remaining.min(Duration::from_millis(PROVIDER_CANCELLATION_POLL_MS));
        runtime.block_on(async {
            tokio::time::sleep(poll).await;
        });
    }
}

pub(super) fn block_on_provider_future<C, F, T>(
    runtime: &ProviderRuntime,
    cancellation: &CancellationToken,
    error_code: &'static str,
    error_stage: ProviderErrorStage,
    request_timeout_seconds: u64,
    create_future: C,
) -> Result<T, ProviderError>
where
    C: FnOnce() -> F,
    F: Future<Output = Result<T, reqwest::Error>>,
{
    let mut future = match runtime {
        ProviderRuntime::External(handle) => {
            let _runtime_context = handle.enter();
            Box::pin(create_future())
        }
        ProviderRuntime::Owned(runtime) => {
            let _runtime_context = runtime.enter();
            Box::pin(create_future())
        }
    };
    loop {
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error());
        }
        let poll = Duration::from_millis(PROVIDER_CANCELLATION_POLL_MS);
        match runtime.block_on(async { tokio::time::timeout(poll, future.as_mut()).await }) {
            Ok(result) => {
                return result.map_err(|error| {
                    provider_transport_error(
                        error,
                        error_code,
                        error_stage.clone(),
                        Some(request_timeout_seconds),
                    )
                });
            }
            Err(_) => continue,
        }
    }
}

pub(super) fn read_bounded_provider_response_body(
    runtime: &ProviderRuntime,
    cancellation: &CancellationToken,
    request_timeout_seconds: u64,
    mut response: reqwest::Response,
) -> Result<Vec<u8>, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BODY_BYTES as u64)
    {
        return Err(provider_response_body_too_large_error());
    }
    let initial_capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or_default()
        .min(MAX_PROVIDER_RESPONSE_BODY_BYTES);
    let mut body = Vec::with_capacity(initial_capacity);
    loop {
        let chunk = block_on_provider_future(
            runtime,
            cancellation,
            "provider_response_body_read_failed",
            ProviderErrorStage::ResponseBodyRead,
            request_timeout_seconds,
            || response.chunk(),
        )?;
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        if body.len().saturating_add(chunk.len()) > MAX_PROVIDER_RESPONSE_BODY_BYTES {
            return Err(provider_response_body_too_large_error());
        }
        body.extend_from_slice(&chunk);
    }
}

pub(super) fn provider_response_body_too_large_error() -> ProviderError {
    let mut error = ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider response body exceeded the fixed safety limit",
    )
    .with_provider_diagnostic(
        "provider_response_body_too_large",
        ProviderErrorStage::ResponseBodyRead,
    );
    error.validation_errors = vec!["provider_response_body_too_large".to_string()];
    ProviderError::from_model_error(error)
}

pub(super) fn provider_response_json_error() -> ModelError {
    ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider response was not valid JSON",
    )
    .with_provider_diagnostic(
        "provider_response_json_decode_failed",
        ProviderErrorStage::ResponseJsonDecode,
    )
}

#[cfg(test)]
mod tests {
    use crate::{
        DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, ModelMessage, ModelRole,
        ModelToolCall, ModelToolParseStatus, ModelTurnRequest, OpenAiProvider,
        OpenAiProviderConfig, ProviderApiProtocol, ProviderConfigSource, ProviderReasoningReplay,
        ProviderToolReasoningMode, SelectedModel, ThinkingWireFormat,
    };

    fn tool_result_message(call_id: &str, text: &str) -> ModelMessage {
        let mut message = ModelMessage::text(ModelRole::Tool, text);
        message.tool_call_id = Some(call_id.to_string());
        message
    }

    fn selected_provider() -> OpenAiProvider {
        let config = OpenAiProviderConfig {
            provider_name: "openai_compatible".to_string(),
            model_name: "gpt-test".to_string(),
            base_url: "http://127.0.0.1:1/v1".to_string(),
            api_key: "sk-secret-value".to_string(),
            source: ProviderConfigSource::ProcessEnvironment,
            max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        };
        OpenAiProvider::new(config)
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
            })
    }

    /// 请求侧校验：无 reasoning 历史的旧工具消息（如 v3 迁移）允许无绑定 replay；
    /// 只有重复绑定才拒绝（对齐 Pi：无 thinking 块的消息原样发送，不伪造 replay）。
    #[test]
    fn validate_reasoning_history_allows_unbound_legacy_tool_message() {
        let provider = selected_provider();
        let mut legacy = ModelMessage::assistant_tool_calls(vec![ModelToolCall {
            tool_call_id: "legacy_call".to_string(),
            tool_name: "read".to_string(),
            arguments: serde_json::json!({"path": "x"}),
            raw_arguments: "{\"path\":\"x\"}".to_string(),
            parse_status: ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        }]);
        legacy.content = "legacy".to_string();
        let mut fresh = ModelMessage::assistant_tool_calls(vec![ModelToolCall {
            tool_call_id: "fresh_call".to_string(),
            tool_name: "read".to_string(),
            arguments: serde_json::json!({"path": "y"}),
            raw_arguments: "{\"path\":\"y\"}".to_string(),
            parse_status: ModelToolParseStatus::Valid,
            validation_errors: Vec::new(),
        }]);
        fresh.content = "fresh".to_string();
        let mut request = ModelTurnRequest::new(
            "validate_unbound",
            vec![
                ModelMessage::text(ModelRole::User, "hi"),
                legacy,
                tool_result_message("legacy_call", "legacy result"),
                fresh,
                tool_result_message("fresh_call", "fresh result"),
            ],
        );
        request.model_preferences.model_name = Some("provider/model#on".to_string());
        request.provider_reasoning_history = vec![ProviderReasoningReplay::Chat {
            provider_name: "openai_compatible".to_string(),
            model_name: "gpt-test".to_string(),
            reasoning_effort: "on".to_string(),
            tool_call_ids: vec!["fresh_call".to_string()],
            reasoning_content: "reasoning for fresh".to_string(),
        }];
        // legacy_call 无绑定 replay 是合法形态（v3 迁移兼容）。
        provider
            .validate_reasoning_history(&request)
            .expect("legacy tool message without replay must be accepted");
        // 重复绑定必须拒绝。
        let mut duplicated = request.clone();
        duplicated.provider_reasoning_history = vec![
            request.provider_reasoning_history[0].clone(),
            ProviderReasoningReplay::Chat {
                provider_name: "openai_compatible".to_string(),
                model_name: "gpt-test".to_string(),
                reasoning_effort: "on".to_string(),
                tool_call_ids: vec!["fresh_call".to_string()],
                reasoning_content: "another replay for fresh".to_string(),
            },
        ];
        assert!(provider.validate_reasoning_history(&duplicated).is_err());
    }
}
