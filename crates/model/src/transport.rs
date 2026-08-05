//! provider HTTP transport、retry、bounded body read 和取消传播。

use super::capability::{
    BoundProviderProtocolNegotiation, InMemoryProviderCapabilityCacheState,
    ProviderCapabilityCache, capability_probe_deadline_error, is_stable_capability_rejection,
};
use super::contract::{
    attach_capability_metadata, provider_request_validation_error, request_uses_tool_protocol,
};
use super::openai::{
    OpenAiCompletion, models_endpoint, openai_reasoning_content_present, openai_request_payload,
    openai_responses_reasoning_content_present, openai_responses_request_payload,
    openai_responses_stream_request_payload, parse_openai_response,
    parse_openai_responses_response,
};
use super::{
    CAPABILITY_PROBE_DEADLINE_SECONDS, HTTP_STATUS_FORBIDDEN, HTTP_STATUS_INTERNAL_SERVER_ERROR,
    HTTP_STATUS_NOT_FOUND, HTTP_STATUS_RATE_LIMITED, HTTP_STATUS_REQUEST_TIMEOUT,
    HTTP_STATUS_UNAUTHORIZED, MAX_PROVIDER_ATTEMPTS, MAX_PROVIDER_RESPONSE_BODY_BYTES, ModelError,
    ModelErrorKind, ModelPreferences, ModelRole, ModelTurnRequest, ModelTurnResponse, ModelUsage,
    OpenAiProvider, OpenAiProviderConfig, PROVIDER_CANCELLATION_POLL_MS,
    PROVIDER_RETRY_BASE_BACKOFF_MS, PROVIDER_RUNTIME_INITIALIZATION_ERROR_CODE,
    PROVIDER_RUNTIME_WORKER_THREADS, PROVIDER_TIMEOUT_SECONDS, Provider, ProviderApiProtocol,
    ProviderAttemptEvent, ProviderAttemptMetadata, ProviderAttemptOccurrence,
    ProviderAttemptOperationPhase, ProviderAttemptStarted, ProviderAttemptStatus,
    ProviderCapabilityMetadata, ProviderError, ProviderErrorStage, ProviderProtocolContract,
    ProviderProtocolNegotiation, ProviderRuntime, ProviderStreamEvent, ProviderStreamingCapability,
    ProviderToolReasoningMode, ProviderTransportCategory, provider_streaming_unsupported_error,
    responses_endpoint, validate_model_request, validate_model_request_with_capabilities,
};
use serde_json::Value;
use singularity_core::CancellationToken;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

/// The single validated protocol choice shared by one provider completion.
struct CompletionContext {
    capabilities: ProviderProtocolContract,
    capability_metadata: Option<ProviderCapabilityMetadata>,
    api_protocol: ProviderApiProtocol,
    capability_binding: Option<BoundProviderProtocolNegotiation>,
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
        Self::new_with_runtime_handle_and_cache_path(
            config,
            PROVIDER_TIMEOUT_SECONDS,
            None,
            runtime_handle,
        )
    }

    /// 创建 provider，并显式绑定可选的持久 capability cache 文件。
    pub fn new_with_cache_path(
        config: OpenAiProviderConfig,
        cache_path: Option<PathBuf>,
    ) -> Result<Self, ProviderError> {
        Self::new_with_request_timeout_and_cache_path(config, PROVIDER_TIMEOUT_SECONDS, cache_path)
    }

    pub(super) fn new_with_request_timeout(
        config: OpenAiProviderConfig,
        request_timeout_seconds: u64,
    ) -> Result<Self, ProviderError> {
        Self::new_with_request_timeout_and_cache_path(config, request_timeout_seconds, None)
    }

    pub(super) fn new_with_request_timeout_and_cache_path(
        config: OpenAiProviderConfig,
        request_timeout_seconds: u64,
        cache_path: Option<PathBuf>,
    ) -> Result<Self, ProviderError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(PROVIDER_RUNTIME_WORKER_THREADS)
            .enable_all()
            .build()
            .map_err(provider_runtime_error)?;
        Self::new_with_runtime(
            config,
            request_timeout_seconds,
            cache_path,
            ProviderRuntime::Owned(Arc::new(runtime)),
        )
    }

    pub(super) fn new_with_runtime_handle_and_cache_path(
        config: OpenAiProviderConfig,
        request_timeout_seconds: u64,
        cache_path: Option<PathBuf>,
        runtime_handle: tokio::runtime::Handle,
    ) -> Result<Self, ProviderError> {
        Self::new_with_runtime(
            config,
            request_timeout_seconds,
            cache_path,
            ProviderRuntime::External(runtime_handle),
        )
    }

    fn new_with_runtime(
        config: OpenAiProviderConfig,
        request_timeout_seconds: u64,
        cache_path: Option<PathBuf>,
        runtime: ProviderRuntime,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(request_timeout_seconds))
            .build()
            .map_err(provider_client_initialization_error)?;
        Ok(Self {
            config,
            selected_model: None,
            client,
            runtime: Arc::new(runtime),
            request_timeout_seconds,
            capability_probe_deadline: Duration::from_secs(CAPABILITY_PROBE_DEADLINE_SECONDS),
            tool_capability_cache: Arc::new(
                Mutex::new(InMemoryProviderCapabilityCacheState::new()),
            ),
            tool_capability_probe_in_flight: Arc::new(Mutex::new(HashMap::new())),
            persistent_capability_cache: cache_path
                .and_then(ProviderCapabilityCache::new)
                .map(Arc::new),
            capability_cache_diagnostic: Arc::new(Mutex::new(None)),
        })
    }

    /// 从环境加载 OpenAI-compatible provider。
    pub fn from_env<F>(get_env: F) -> Result<Self, ProviderError>
    where
        F: FnMut(&str) -> Option<String>,
    {
        super::ProviderConfigSnapshot::capture(get_env, None, None).provider()
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
            None,
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
            None,
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

    pub(super) fn protocol_candidates(&self) -> Vec<ProviderApiProtocol> {
        self.selected_model
            .as_ref()
            .map(|selection| vec![selection.api_protocol])
            .unwrap_or_else(|| self.config.api_protocol_candidates())
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
            if bound_replay_count > 1
                || (selection.requires_reasoning_content_for_tool_calls && bound_replay_count != 1)
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
        Ok(())
    }
}

impl OpenAiProvider {
    fn prepare_completion_context_observed(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<CompletionContext, ProviderError> {
        let local_validation = validate_model_request(request);
        if !local_validation.valid {
            return Err(provider_request_validation_error(
                local_validation,
                &self.config,
            ));
        }
        let mut capability_binding = None;
        let (capabilities, capability_metadata, api_protocol) =
            if !request_uses_tool_protocol(request) {
                let mut capabilities = self.protocol_contract();
                if self.selected_model.is_some() {
                    // A catalog's developer-role flag controls wire projection;
                    // it must not reject valid internal system/developer history.
                    capabilities.supports_system_message = true;
                    capabilities.supports_developer_message = true;
                }
                (
                    capabilities,
                    None,
                    self.selected_model
                        .as_ref()
                        .map(|selection| selection.api_protocol)
                        .unwrap_or_else(|| self.config.completion_protocol_without_tools()),
                )
            } else {
                let effective_model_name = request
                    .model_preferences
                    .model_name
                    .as_deref()
                    .unwrap_or(&self.config.model_name);
                let binding = self.negotiate_openai_tool_capabilities_bound_observed(
                    effective_model_name,
                    cancellation,
                    on_attempt,
                )?;
                let api_protocol = binding.negotiation.metadata.api_protocol;
                capability_binding = Some(binding.clone());
                (
                    binding.negotiation.contract,
                    Some(binding.negotiation.metadata),
                    api_protocol,
                )
            };
        let request_validation =
            validate_model_request_with_capabilities(request, Some(&capabilities));
        if !request_validation.valid {
            let provider_error =
                provider_request_validation_error(request_validation, &self.config);
            return Err(attach_capability_metadata(
                provider_error,
                &capability_metadata,
            ));
        }
        Ok(CompletionContext {
            capabilities,
            capability_metadata,
            api_protocol,
            capability_binding,
        })
    }

    /// Apply capability-cache invalidation and attach safe negotiation metadata once.
    fn finish_completion_result<T>(
        &self,
        result: Result<T, ProviderError>,
        cancellation: &CancellationToken,
        context: &CompletionContext,
    ) -> Result<T, ProviderError> {
        let cache_invalidation_deadline =
            Instant::now() + Duration::from_secs(self.request_timeout_seconds);
        let result = if let (Some(binding), Err(error)) = (&context.capability_binding, &result)
            && is_stable_capability_rejection(error)
        {
            match self.invalidate_tool_capability_negotiation(
                &binding.key,
                cancellation,
                cache_invalidation_deadline,
            ) {
                Ok(()) => result,
                Err(invalidation_error) => result.map_err(|mut original| {
                    original.error.validation_errors.push(
                        invalidation_error.error.code.unwrap_or_else(|| {
                            "provider_capability_cache_invalidation_failed".to_string()
                        }),
                    );
                    original
                }),
            }
        } else {
            result
        };
        result.map_err(|error| attach_capability_metadata(error, &context.capability_metadata))
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
            None,
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
                None,
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
        probe_deadline: Option<Instant>,
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
                    self.selected_model
                        .as_ref()
                        .is_none_or(|selection| selection.supports_developer_role),
                    self.selected_model
                        .as_ref()
                        .is_none_or(|selection| selection.supports_tool_choice),
                    self.selected_model.as_ref().is_some_and(|selection| {
                        selection.requires_assistant_content_for_tool_calls
                    }),
                )
            }
        };
        let operation_phase = if probe_deadline.is_some() {
            ProviderAttemptOperationPhase::CapabilityProbe
        } else {
            ProviderAttemptOperationPhase::Completion
        };
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
                probe_deadline,
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
                    wait_provider_backoff(runtime, cancellation, retry_backoff, probe_deadline)
                        .map_err(|cancelled| {
                            cancelled.with_provider_attempt_metadata(provider_attempt_metadata(
                                &metadata, started_at,
                            ))
                        })?;
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
                    wait_provider_backoff(runtime, cancellation, retry_backoff, probe_deadline)
                        .map_err(|cancelled| {
                            cancelled.with_provider_attempt_metadata(provider_attempt_metadata(
                                &metadata, started_at,
                            ))
                        })?;
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
                probe_deadline,
                response,
            ) {
                Ok(body) => body,
                Err(error)
                    if metadata.attempt_count < MAX_PROVIDER_ATTEMPTS
                        && provider_error_is_retryable(&error) =>
                {
                    let retry_backoff =
                        record_provider_retry(&mut metadata, occurrence, &error.error, on_attempt)?;
                    wait_provider_backoff(runtime, cancellation, retry_backoff, probe_deadline)
                        .map_err(|cancelled| {
                            cancelled.with_provider_attempt_metadata(provider_attempt_metadata(
                                &metadata, started_at,
                            ))
                        })?;
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

/// Enforce the negotiated tool-reasoning contract on a completed response.
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
        self.config.protocol_contract()
    }

    fn streaming_capability(
        &self,
        selected_protocol: ProviderApiProtocol,
    ) -> ProviderStreamingCapability {
        ProviderStreamingCapability::for_protocol(selected_protocol)
    }

    fn negotiate_tool_capabilities(
        &self,
        model_preferences: &ModelPreferences,
        cancellation: &CancellationToken,
    ) -> Result<ProviderProtocolNegotiation, ProviderError> {
        let mut ignore_attempt = |_| true;
        self.negotiate_tool_capabilities_observed(
            model_preferences,
            cancellation,
            &mut ignore_attempt,
        )
    }

    fn negotiate_tool_capabilities_observed(
        &self,
        model_preferences: &ModelPreferences,
        cancellation: &CancellationToken,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<ProviderProtocolNegotiation, ProviderError> {
        let model_preferences = if model_preferences.model_name.is_some() {
            let request = ModelTurnRequest {
                request_id: "provider_capability_selection".to_string(),
                messages: Vec::new(),
                tools: Vec::new(),
                tool_choice: Default::default(),
                model_preferences: model_preferences.clone(),
                provider_reasoning_history: Vec::new(),
            };
            self.normalize_request_model(&request)?.model_preferences
        } else {
            model_preferences.clone()
        };
        self.negotiate_openai_tool_capabilities_bound_observed(
            model_preferences
                .model_name
                .as_deref()
                .unwrap_or(&self.config.model_name),
            cancellation,
            on_attempt,
        )
        .map(|bound| bound.negotiation)
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
        let context =
            self.prepare_completion_context_observed(&request, cancellation, on_attempt)?;
        if self.streaming_capability(context.api_protocol)
            != ProviderStreamingCapability::OutputTextDelta
        {
            return Err(attach_capability_metadata(
                provider_streaming_unsupported_error(),
                &context.capability_metadata,
            ));
        }
        let model_name = request
            .model_preferences
            .model_name
            .as_deref()
            .unwrap_or(&self.config.model_name);
        let result = self
            .complete_responses_stream(
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
                    self.selected_model.as_ref().is_some_and(|selection| {
                        selection.requires_reasoning_content_for_tool_calls
                    }),
                )
                .map(|()| completion.response)
            })
            .map(|mut response| {
                response.provider_capability_metadata = context.capability_metadata.clone();
                response
            });
        self.finish_completion_result(result, cancellation, &context)
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
        let context =
            self.prepare_completion_context_observed(&request, cancellation, on_attempt)?;
        let effective_model_name = request
            .model_preferences
            .model_name
            .as_deref()
            .unwrap_or(&self.config.model_name);
        let result = self
            .complete_with_contract_observed(
                &request,
                cancellation,
                &context.capabilities,
                context.api_protocol,
                effective_model_name,
                on_attempt,
            )
            .map(|mut response| {
                response.provider_capability_metadata = context.capability_metadata.clone();
                response
            });
        self.finish_completion_result(result, cancellation, &context)
    }
}

fn wait_stream_retry_backoff(
    runtime: &ProviderRuntime,
    cancellation: &CancellationToken,
    duration: Duration,
    metadata: &ProviderAttemptMetadata,
    started_at: Instant,
) -> Result<(), ProviderError> {
    wait_provider_backoff(runtime, cancellation, duration, None).map_err(|error| {
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
            None,
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
                return Err(provider_responses_stream_terminal_error(
                    "responses_stream_incomplete",
                    "provider Responses stream was incomplete",
                ));
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

pub(super) fn add_provider_attempt_metadata(
    total: &mut ProviderAttemptMetadata,
    metadata: &ProviderAttemptMetadata,
) {
    let first_attempt_index = total.attempt_count.saturating_add(1);
    total.attempt_count = total.attempt_count.saturating_add(metadata.attempt_count);
    total.retry_count = total.retry_count.saturating_add(metadata.retry_count);
    total.latency_ms = total.latency_ms.saturating_add(metadata.latency_ms);
    total
        .occurrences
        .extend(metadata.occurrences.iter().cloned().enumerate().map(
            |(offset, mut occurrence)| {
                occurrence.attempt_index =
                    first_attempt_index.saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
                occurrence
            },
        ));
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
    if !on_attempt(ProviderAttemptEvent::Finished(occurrence.clone())) {
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
    if !on_attempt(ProviderAttemptEvent::Finished(occurrence.clone())) {
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
    let mut model_error =
        ModelError::new(kind, "provider transport failed").with_provider_diagnostic(code, stage);
    model_error.transport_category = Some(category);
    if error.is_timeout() {
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
    matches!(
        error.error.kind,
        ModelErrorKind::NetworkError | ModelErrorKind::Timeout
    ) && !matches!(
        error.error.transport_category,
        Some(ProviderTransportCategory::Request)
    )
}

pub(super) fn http_status_is_retryable(status: u16) -> bool {
    status == HTTP_STATUS_RATE_LIMITED || status >= HTTP_STATUS_INTERNAL_SERVER_ERROR
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
    probe_deadline: Option<Instant>,
) -> Result<(), ProviderError> {
    let deadline = Instant::now() + duration;
    loop {
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error());
        }
        if probe_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(capability_probe_deadline_error());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        let poll = probe_deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(remaining)
            .min(remaining)
            .min(Duration::from_millis(PROVIDER_CANCELLATION_POLL_MS));
        if poll.is_zero() {
            return Err(capability_probe_deadline_error());
        }
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
    probe_deadline: Option<Instant>,
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
        if probe_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(capability_probe_deadline_error());
        }
        let poll = probe_deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|| Duration::from_millis(PROVIDER_CANCELLATION_POLL_MS))
            .min(Duration::from_millis(PROVIDER_CANCELLATION_POLL_MS));
        if poll.is_zero() {
            return Err(capability_probe_deadline_error());
        }
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
    probe_deadline: Option<Instant>,
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
            probe_deadline,
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
