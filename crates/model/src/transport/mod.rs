//! provider HTTP transport、bounded body read 和取消传播。

pub(crate) mod http;
pub(crate) mod retry;
pub(crate) mod stream;

pub(crate) use http::*;
pub(crate) use retry::*;
pub(crate) use stream::*;

use std::fmt;
use std::time::Duration;

use serde_json::Value;
use singularity_core::CancellationToken;

use crate::error::{ModelError, ProviderError, ProviderErrorStage};
use crate::openai::{
    OpenAiCompletion, openai_chat_stream_request_payload, openai_reasoning_content_present,
    openai_responses_reasoning_content_present, openai_responses_stream_request_payload,
    parse_openai_response, parse_openai_responses_response, responses_endpoint,
};
use crate::provider::Provider;
use crate::provider::attempt::{
    ProviderAttemptInProgress, emit_provider_attempt_started, record_provider_attempt,
};
use crate::provider::contract::{
    ProviderApiProtocol, ProviderProtocolContract, provider_request_validation_error,
    request_uses_tool_protocol, validate_model_request_with_capabilities,
};
use crate::provider::runtime::{OpenAiProviderConfig, SelectedModel, WireRequestOptions};
use crate::provider::telemetry::{ProviderAttemptEvent, ProviderStreamEvent};
use crate::types::{ModelRole, ModelTurnRequest, ModelTurnResponse, ProviderToolReasoningMode};

/// 一次 provider 补全共享的单一已验证协议选择。
struct CompletionContext {
    capabilities: ProviderProtocolContract,
    api_protocol: ProviderApiProtocol,
}

struct HttpFailure {
    model_error: ModelError,
    retry_after: Option<Duration>,
    provider_diagnostic: Option<String>,
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
            ProviderApiProtocol::OpenAiChatCompletions => Self::Chat,
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
        wire: &WireRequestOptions,
        request: &ModelTurnRequest,
        model_name: &str,
        capabilities: &ProviderProtocolContract,
    ) -> Value {
        match self {
            Self::Chat => {
                openai_chat_stream_request_payload(request, model_name, capabilities, wire)
            }
            Self::Responses => {
                openai_responses_stream_request_payload(request, model_name, capabilities, wire)
            }
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

/// 一次 attempt 内、成功 HTTP 响应上的协议侧工作结果。`Retry` 表示协议侧
/// 允许调用方重发请求；`Failed` 禁止自动重放。
enum AttemptBodyOutcome {
    Completed { completion: Box<OpenAiCompletion> },
    Retry { error: ProviderError },
    Failed { error: ProviderError },
}

/// 把一次流式解码 attempt 折叠进 [`AttemptBodyOutcome`]。流失败仅在首个
/// 可见 delta 之前可重试：之后重发会重复已输出的内容。
fn streaming_outcome(
    attempt: Result<StreamAttemptSuccess, StreamAttemptFailure>,
    parse_payload: impl FnOnce(Value) -> Result<OpenAiCompletion, ProviderError>,
) -> AttemptBodyOutcome {
    match attempt {
        Ok(success) => match parse_payload(success.payload) {
            Ok(completion) => AttemptBodyOutcome::Completed {
                completion: Box::new(completion),
            },
            Err(error) => AttemptBodyOutcome::Failed { error },
        },
        Err(failure) if !failure.emitted_text_delta => AttemptBodyOutcome::Retry {
            error: failure.error,
        },
        Err(failure) => AttemptBodyOutcome::Failed {
            error: failure.error,
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
) -> Result<OpenAiCompletion, ProviderError> {
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
        .map(|response| OpenAiCompletion {
            response,
            reasoning_content_present,
        })
}

/// 流式读取一次 provider 响应的上下文：协议适配、HTTP 响应与事件回调。
struct SseReadContext<'a> {
    adapter: ProtocolAdapter,
    runtime: &'a tokio::runtime::Handle,
    cancellation: &'a CancellationToken,
    response: reqwest::Response,
    on_event: &'a mut dyn FnMut(ProviderStreamEvent),
}

/// 一次协议完成请求的上下文：协议契约、目录选择与事件回调。
struct ProtocolRequestContext<'a> {
    cancellation: &'a CancellationToken,
    api_protocol: ProviderApiProtocol,
    selection: &'a SelectedModel,
    on_event: &'a mut dyn FnMut(ProviderStreamEvent),
    on_attempt: &'a mut dyn FnMut(ProviderAttemptEvent),
}

/// 一次 HTTP attempt 的上下文：协议、选择器、端点与载荷。
struct AttemptContext<'a> {
    cancellation: &'a CancellationToken,
    api_protocol: ProviderApiProtocol,
    model_name: &'a str,
    endpoint: &'a str,
    request_payload: &'a Value,
}

fn read_protocol_sse(
    context: SseReadContext<'_>,
) -> Result<StreamAttemptSuccess, StreamAttemptFailure> {
    let SseReadContext {
        adapter,
        runtime,
        cancellation,
        response,
        on_event,
    } = context;
    match adapter {
        ProtocolAdapter::Chat => read_openai_chat_sse(runtime, cancellation, response, on_event),
        ProtocolAdapter::Responses => {
            read_openai_responses_sse(runtime, cancellation, response, on_event)
        }
    }
}

pub struct OpenAiProvider {
    config: OpenAiProviderConfig,
    selected_model: Option<SelectedModel>,
    client: reqwest::Client,
    runtime: tokio::runtime::Handle,
}

impl Clone for OpenAiProvider {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            selected_model: self.selected_model.clone(),
            client: self.client.clone(),
            runtime: self.runtime.clone(),
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
            .finish()
    }
}

impl OpenAiProvider {
    /// 创建并校验 OpenAI-compatible provider；异步执行一律使用调用方注入的
    /// runtime，读取超时固定为 `PROVIDER_TIMEOUT_SECONDS`。
    pub fn new(
        config: OpenAiProviderConfig,
        runtime_handle: tokio::runtime::Handle,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .read_timeout(Duration::from_secs(crate::PROVIDER_TIMEOUT_SECONDS))
            .user_agent(format!("singularity-agent/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(provider_client_initialization_error)?;
        Ok(Self {
            config,
            selected_model: None,
            client,
            runtime: runtime_handle,
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

    /// 返回目录克隆的完整选择器（`provider/model#effort`）；未选择目录模型时
    /// 返回 `None`。
    pub(crate) fn resolved_selector(&self) -> Option<String> {
        let selection = self.selected_model.as_ref()?;
        let mut selector = format!("{}/{}", self.config.provider_name, selection.model_name);
        if let Some(variant) = selection.reasoning_variant.as_deref() {
            selector.push('#');
            selector.push_str(variant);
        }
        Some(selector)
    }

    fn validate_reasoning_history(
        &self,
        request: &ModelTurnRequest,
        selection: &SelectedModel,
    ) -> Result<(), ProviderError> {
        if request.provider_reasoning_history.is_empty() {
            return Ok(());
        }
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
        selection: &SelectedModel,
    ) -> Result<CompletionContext, ProviderError> {
        // 静态能力声明：工具与非工具请求统一使用声明式契约；api_protocol 由
        // 目录选择决定。
        let capabilities = self.protocol_contract();
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
            api_protocol: selection.api_protocol,
        })
    }

    /// 一次完成的单一编排入口：一切模型调用走流式解码。
    /// 请求归一、能力校验、wire 协议选择与 tool-reasoning 契约校验只在这一个
    /// 入口实现。
    fn complete_internal<'a>(
        &'a self,
        request: &ModelTurnRequest,
        cancellation: &'a CancellationToken,
        on_event: &'a mut dyn FnMut(ProviderStreamEvent),
        on_attempt: &'a mut dyn FnMut(ProviderAttemptEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error());
        }
        // 快照不变量：到达请求路径的 provider 实例必带恰好一个目录选择；
        // 缺失选择是构造缺陷，fail closed。
        let Some(selection) = self.selected_model.as_ref() else {
            return Err(super::config::configuration_error(
                "provider request has no catalog model selection",
                "provider_configuration_missing",
            ));
        };
        // 选择器解析已前移到请求装配期：请求只携带裸 model id。这里只保留
        // 相等断言，防止与 provider 绑定不一致的模型名静默发出。
        if let Some(model_name) = request.model_preferences.model_name.as_deref()
            && model_name != self.config.model_name
        {
            return Err(super::config::model_selector_error(
                "model selector is not the fixed model for this provider turn",
                "provider_selector_unknown_model",
            ));
        }
        self.validate_reasoning_history(request, selection)?;
        let context = self.prepare_completion_context_observed(request, selection)?;
        let model_name = request
            .model_preferences
            .model_name
            .as_deref()
            .unwrap_or(&self.config.model_name);
        let completion = self.complete_protocol(
            request,
            &context.capabilities,
            model_name,
            ProtocolRequestContext {
                cancellation,
                api_protocol: context.api_protocol,
                selection,
                on_event,
                on_attempt,
            },
        )?;
        validate_response_tool_reasoning_contract(
            request_uses_tool_protocol(request),
            &completion,
            &context.capabilities,
            selection.requires_reasoning_content_for_tool_calls,
        )?;
        Ok(completion.response)
    }

    /// 单协议完成请求的执行：适配 payload、流式/非流式读取并合成完成。
    fn complete_protocol(
        &self,
        request: &ModelTurnRequest,
        capabilities: &ProviderProtocolContract,
        model_name: &str,
        context: ProtocolRequestContext<'_>,
    ) -> Result<OpenAiCompletion, ProviderError> {
        let ProtocolRequestContext {
            cancellation,
            api_protocol,
            selection,
            on_event,
            on_attempt,
        } = context;
        let adapter = ProtocolAdapter::for_api_protocol(api_protocol);
        let endpoint = adapter.endpoint(&self.config);
        let wire = WireRequestOptions::from_selection(selection);
        let request_payload = adapter.request_payload(&wire, request, model_name, capabilities);
        let reasoning_variant = selection.reasoning_variant.as_deref();
        self.complete_attempt(
            AttemptContext {
                cancellation,
                api_protocol,
                model_name,
                endpoint: &endpoint,
                request_payload: &request_payload,
            },
            on_attempt,
            &mut |response| {
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
                streaming_outcome(
                    read_protocol_sse(SseReadContext {
                        adapter,
                        runtime: &self.runtime,
                        cancellation,
                        response,
                        on_event: &mut *on_event,
                    }),
                    parse_payload,
                )
            },
        )
    }

    /// 两种 wire 协议、流式与非流式响应的共享完成骨架：执行一次 HTTP
    /// attempt，返回解析后的完成或携带重放安全性与 provider 定向延时的
    /// 类型化失败。
    fn complete_attempt(
        &self,
        context: AttemptContext<'_>,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent),
        read_response: &mut dyn FnMut(reqwest::Response) -> AttemptBodyOutcome,
    ) -> Result<OpenAiCompletion, ProviderError> {
        let AttemptContext {
            cancellation,
            api_protocol,
            model_name,
            endpoint,
            request_payload,
        } = context;
        let runtime = &self.runtime;
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error());
        }

        let occurrence =
            ProviderAttemptInProgress::new(&self.config.provider_name, model_name, api_protocol);
        emit_provider_attempt_started(&occurrence, on_attempt);
        let response = match block_on_provider_future(
            runtime,
            cancellation,
            "provider_request_send_failed",
            ProviderErrorStage::RequestSend,
            || {
                self.client
                    .post(endpoint)
                    .bearer_auth(&self.config.api_key)
                    .json(request_payload)
                    .send()
            },
        ) {
            Ok(response) => response,
            Err(error) => {
                record_provider_attempt(occurrence, Some(&error.error), None, on_attempt);
                return Err(error);
            }
        };

        let status = response.status();
        if !status.is_success() {
            let failure = self.classify_http_failure(response, cancellation, model_name);
            record_provider_attempt(occurrence, Some(&failure.model_error), None, on_attempt);
            let mut error = ProviderError::from_model_error(failure.model_error)
                .with_retry_after(failure.retry_after);
            if let Some(diagnostic) = failure.provider_diagnostic {
                // 追加到内层 message：Display 与重试诊断都从单一内层文案读取。
                error.error.message.push_str(" Provider diagnostic: ");
                error.error.message.push_str(&diagnostic);
            }
            return Err(error);
        }

        match read_response(response) {
            AttemptBodyOutcome::Completed { completion } => {
                let usage = completion
                    .response
                    .usage
                    .usage_present
                    .then(|| completion.response.usage.clone());
                record_provider_attempt(occurrence, None, usage, on_attempt);
                Ok(*completion)
            }
            AttemptBodyOutcome::Retry { error } => {
                record_provider_attempt(occurrence, Some(&error.error), None, on_attempt);
                Err(error)
            }
            AttemptBodyOutcome::Failed { error } => {
                record_provider_attempt(occurrence, Some(&error.error), None, on_attempt);
                Err(error.without_automatic_retry())
            }
        }
    }

    fn classify_http_failure(
        &self,
        response: reqwest::Response,
        cancellation: &CancellationToken,
        model_name: &str,
    ) -> HttpFailure {
        let status_code = response.status().as_u16();
        let retry_after = retry_after_delay(response.headers());
        let error_body =
            read_bounded_provider_response_body(&self.runtime, cancellation, response).ok();
        let error_fields = error_body.as_deref().map(parse_provider_error_body);
        let coded_kind = provider_error_kind_for_code(
            error_fields
                .as_ref()
                .and_then(|fields| fields.code.as_deref()),
        );
        let model_error = match coded_kind {
            Some(kind) => {
                let detail = error_fields
                    .as_ref()
                    .and_then(|fields| fields.message.as_deref())
                    .map(bounded_provider_error_diagnostic)
                    .filter(|text| !text.is_empty());
                let message = match detail {
                    Some(text) => format!("provider rejected the request: {text}"),
                    None => "provider rejected the request by wire error code".to_string(),
                };
                ModelError::new(kind, message)
                    .with_provider(self.config.provider_name.clone())
                    .with_model(model_name.to_string())
                    .with_provider_diagnostic(
                        "provider_rejected_by_error_code",
                        ProviderErrorStage::ResponseStatus,
                    )
            }
            None => {
                model_error_from_http_status(status_code, &self.config.provider_name, model_name)
            }
        };
        let provider_diagnostic = if coded_kind.is_some() {
            None
        } else {
            error_fields
                .as_ref()
                .and_then(|fields| fields.message.as_deref())
                .map(bounded_provider_error_diagnostic)
                .or_else(|| {
                    error_body.as_deref().map(|body| {
                        bounded_provider_error_diagnostic(&String::from_utf8_lossy(body))
                    })
                })
                .filter(|diagnostic| !diagnostic.is_empty())
        };
        HttpFailure {
            model_error,
            retry_after,
            provider_diagnostic,
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
    let response_has_tool_calls = !completion.response.tool_calls().is_empty();
    // Disabled 契约只约束需要 replay 的工具调用续接：无工具调用的回复即使
    // 携带 reasoning 也无 replay 需求，属合法（见函数文档）。
    let disabled_mode_not_honored = capabilities.tool_reasoning_mode
        == ProviderToolReasoningMode::DisabledForToolCalls
        && completion.reasoning_content_present
        && response_has_tool_calls;
    let reasoning_content_present = completion.reasoning_content_present;
    let missing_replay_for_present_reasoning = response_has_tool_calls
        && reasoning_content_present
        && completion.response.provider_reasoning_history.is_empty();
    let missing_required_reasoning = requires_reasoning_content_for_tool_calls
        && response_has_tool_calls
        && !reasoning_content_present;
    let replay_binding_invalid = completion.response.provider_reasoning_history.is_empty()
        || completion
            .response
            .provider_reasoning_history
            .iter()
            .any(|replay| replay.mode_internal() != capabilities.tool_reasoning_mode);
    if (disabled_mode_not_honored
        || missing_required_reasoning
        || missing_replay_for_present_reasoning)
        && replay_binding_invalid
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

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        self.complete_internal(request, cancellation, on_event, on_attempt)
    }
}

#[cfg(test)]
mod contract_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::*;
    use crate::{ModelToolCall, ModelToolParseStatus};

    fn completion(reasoning_present: bool, with_tool_call: bool) -> OpenAiCompletion {
        let mut response = ModelTurnResponse::completed("req-1", "resp-1", "text");
        if with_tool_call {
            #[allow(clippy::expect_used)]
            let message = response
                .assistant_message
                .as_mut()
                .expect("assistant message");
            message.tool_calls.push(ModelToolCall {
                tool_call_id: "call-1".to_string(),
                tool_name: "read".to_string(),
                arguments: serde_json::json!({"path": "a.rs"}),
                raw_arguments: "{\"path\":\"a.rs\"}".to_string(),
                parse_status: ModelToolParseStatus::Valid,
                validation_errors: Vec::new(),
            });
        }
        OpenAiCompletion {
            response,
            reasoning_content_present: reasoning_present,
        }
    }

    fn disabled_contract() -> ProviderProtocolContract {
        ProviderProtocolContract {
            tool_reasoning_mode: ProviderToolReasoningMode::DisabledForToolCalls,
            ..Default::default()
        }
    }

    /// Disabled 契约只约束需要 replay 的工具调用续接：携带 reasoning 的
    /// 无工具调用回复合法，不得被判为绑定违规（回归：off 模式误伤纯
    /// reasoning 回复）。
    #[test]
    fn disabled_mode_tolerates_reasoning_without_tool_calls() {
        validate_response_tool_reasoning_contract(
            true,
            &completion(true, false),
            &disabled_contract(),
            false,
        )
        .expect("reasoning-only reply is legal under Disabled mode");
    }

    /// 同一契约下，带工具调用且 reasoning 无 replay 的响应必须被拒绝。
    #[test]
    fn disabled_mode_rejects_tool_calls_with_unbound_reasoning() {
        assert!(
            validate_response_tool_reasoning_contract(
                true,
                &completion(true, true),
                &disabled_contract(),
                false,
            )
            .is_err(),
            "tool call with reasoning but no replay violates Disabled mode"
        );
    }
}
