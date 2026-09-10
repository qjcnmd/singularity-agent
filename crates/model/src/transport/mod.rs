//! provider HTTP transport、bounded body read 和取消传播。

pub(crate) mod http;
pub(crate) mod retry;
pub(crate) mod stream;

pub(crate) use http::*;
pub(crate) use retry::*;
pub(crate) use stream::*;

use std::borrow::Cow;
use std::fmt;
use std::time::Duration;

use serde_json::Value;
use singularity_core::CancellationToken;

use crate::config::ModelConfigurationSnapshot;
use crate::error::ProviderError;
use crate::openai::{
    OpenAiCompletion, openai_chat_stream_request_payload, openai_reasoning_content_present,
    openai_responses_reasoning_content_present, openai_responses_stream_request_payload,
    parse_openai_response, parse_openai_responses_response, responses_endpoint,
};
use crate::provider::Provider;
use crate::provider::attempt::{
    ProviderAttemptInProgress, duration_millis, record_provider_attempt,
};
use crate::provider::contract::{
    ProviderApiProtocol, ProviderProtocolContract, provider_request_validation_error,
    validate_model_request_with_capabilities,
};
use crate::provider::policy::TurnRetryPolicy;
use crate::provider::runtime::{OpenAiProviderConfig, SelectedModel};
use crate::provider::telemetry::{ProviderAttemptEvent, ProviderStreamEvent};
use crate::types::{ModelTurnRequest, ModelTurnResponse};

impl ProviderApiProtocol {
    fn endpoint(self, config: &OpenAiProviderConfig) -> String {
        match self {
            Self::OpenAiChatCompletions => config.endpoint(),
            Self::OpenAiResponses => responses_endpoint(&config.base_url),
        }
    }

    fn request_payload(
        self,
        selection: &SelectedModel,
        request: &ModelTurnRequest,
        model_name: &str,
    ) -> Value {
        match self {
            Self::OpenAiChatCompletions => {
                openai_chat_stream_request_payload(request, model_name, selection)
            }
            Self::OpenAiResponses => {
                openai_responses_stream_request_payload(request, model_name, selection)
            }
        }
    }

    fn reasoning_present(self, payload: &Value) -> bool {
        match self {
            Self::OpenAiChatCompletions => openai_reasoning_content_present(payload),
            Self::OpenAiResponses => openai_responses_reasoning_content_present(payload),
        }
    }

    fn parse_response(
        self,
        request: &ModelTurnRequest,
        config: &OpenAiProviderConfig,
        payload: Value,
        model_name: &str,
        reasoning_variant: Option<&str>,
    ) -> Result<ModelTurnResponse, ProviderError> {
        match self {
            Self::OpenAiChatCompletions => {
                parse_openai_response(request, config, payload, model_name, reasoning_variant)
            }
            Self::OpenAiResponses => parse_openai_responses_response(
                request,
                config,
                payload,
                model_name,
                reasoning_variant,
            ),
        }
    }
}

/// 一次协议完成请求的上下文：协议契约、目录选择与事件回调。
struct ProtocolRequestContext<'a> {
    cancellation: &'a CancellationToken,
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

#[derive(Clone)]
pub struct OpenAiProvider {
    config: OpenAiProviderConfig,
    selected_model: Option<SelectedModel>,
    client: reqwest::Client,
    runtime: tokio::runtime::Handle,
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
    /// runtime，读取超时固定为 PROVIDER_TIMEOUT_SECONDS。
    pub(crate) fn new(
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
        selected.selected_model = Some(selected_model);
        selected
    }

    /// 返回目录克隆的完整选择器（provider/model#effort）；未选择目录模型时
    /// 返回 None。
    pub(crate) fn resolved_selector(&self) -> Option<String> {
        let selection = self.selected_model.as_ref()?;
        Some(super::config::compose_model_selector(
            &self.config.provider_name,
            &selection.model_name,
            selection.reasoning_variant.as_deref(),
        ))
    }

    fn prepare_reasoning_history<'a>(
        &self,
        request: &'a ModelTurnRequest,
        selection: &SelectedModel,
    ) -> Result<Cow<'a, ModelTurnRequest>, ProviderError> {
        let mut prepared = Cow::Borrowed(request);
        for (index, message) in request.messages.iter().enumerate() {
            let Some(replay) = message.provider_reasoning_replay.as_ref() else {
                continue;
            };
            if replay.is_for_model(
                &self.config.provider_name,
                &selection.model_name,
                selection.api_protocol,
            ) {
                replay
                    .validate_message(message)
                    .map_err(provider_reasoning_history_error)?;
            } else {
                // 私有签名只属于产生它的模型及协议，切换模型保留公开消息。
                prepared.to_mut().messages[index].provider_reasoning_replay = None;
            }
        }
        Ok(prepared)
    }
}

impl OpenAiProvider {
    /// 单协议完成请求的执行：适配 payload、流式/非流式读取并合成完成。
    fn complete_protocol(
        &self,
        request: &ModelTurnRequest,
        context: ProtocolRequestContext<'_>,
    ) -> Result<OpenAiCompletion, ProviderError> {
        let ProtocolRequestContext {
            cancellation,
            selection,
            on_event,
            on_attempt,
        } = context;
        let adapter = selection.api_protocol;
        let endpoint = adapter.endpoint(&self.config);
        let request_payload = adapter.request_payload(selection, request, &selection.model_name);
        let reasoning_variant = selection.reasoning_variant.as_deref();
        self.complete_attempt(
            AttemptContext {
                cancellation,
                api_protocol: selection.api_protocol,
                model_name: &selection.model_name,
                endpoint: &endpoint,
                request_payload: &request_payload,
            },
            on_attempt,
            &mut |response| {
                read_openai_sse(
                    adapter,
                    &self.runtime,
                    cancellation,
                    response,
                    &mut *on_event,
                )
                .and_then(|payload| {
                    let reasoning_content_present = adapter.reasoning_present(&payload);
                    adapter
                        .parse_response(
                            request,
                            &self.config,
                            payload,
                            &selection.model_name,
                            reasoning_variant,
                        )
                        .map(|response| OpenAiCompletion {
                            response,
                            reasoning_content_present,
                        })
                        .map_err(ProviderError::without_automatic_retry)
                })
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
        read_response: &mut dyn FnMut(reqwest::Response) -> Result<OpenAiCompletion, ProviderError>,
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
        on_attempt(occurrence.started_event());
        let response = match block_on_provider_future(
            runtime,
            cancellation,
            "provider_request_send_failed",
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
                record_provider_attempt(
                    occurrence,
                    Some(&error),
                    None,
                    error.retry_after.map(duration_millis),
                    on_attempt,
                );
                return Err(error);
            }
        };

        let status = response.status();
        if !status.is_success() {
            let error = self.classify_http_failure(response, cancellation);
            record_provider_attempt(
                occurrence,
                Some(&error),
                None,
                error.retry_after.map(duration_millis),
                on_attempt,
            );
            return Err(error);
        }

        match read_response(response) {
            Ok(completion) => {
                let usage = completion
                    .response
                    .usage
                    .usage_present
                    .then(|| completion.response.usage.clone());
                record_provider_attempt(occurrence, None, usage, None, on_attempt);
                Ok(completion)
            }
            Err(error) => {
                record_provider_attempt(
                    occurrence,
                    Some(&error),
                    None,
                    error.retry_after.map(duration_millis),
                    on_attempt,
                );
                Err(error)
            }
        }
    }

    fn classify_http_failure(
        &self,
        response: reqwest::Response,
        cancellation: &CancellationToken,
    ) -> ProviderError {
        let status_code = response.status().as_u16();
        let retry_after = retry_after_delay(response.headers());
        let error_body =
            read_bounded_provider_response_body(&self.runtime, cancellation, response).ok();
        let error_fields = error_body
            .as_deref()
            .map(parse_provider_error_body)
            .unwrap_or_default();
        let coded_kind = provider_error_kind_for_code(error_fields.code.as_deref());
        let model_error = match coded_kind {
            Some(kind) => {
                let detail = error_fields
                    .message
                    .as_deref()
                    .map(bounded_provider_error_diagnostic)
                    .filter(|text| !text.is_empty());
                let message = match detail {
                    Some(text) => format!("provider rejected the request: {text}"),
                    None => "provider rejected the request by wire error code".to_string(),
                };
                ProviderError::new(kind, message).with_code("provider_rejected_by_error_code")
            }
            None => provider_error_from_http_status(status_code),
        };
        let provider_diagnostic = if coded_kind.is_some() {
            None
        } else {
            error_fields
                .message
                .as_deref()
                .map(bounded_provider_error_diagnostic)
                .or_else(|| {
                    error_body.as_deref().map(|body| {
                        bounded_provider_error_diagnostic(&String::from_utf8_lossy(body))
                    })
                })
                .filter(|diagnostic| !diagnostic.is_empty())
        };
        let mut error = model_error.with_retry_after(retry_after);
        if let Some(diagnostic) = provider_diagnostic {
            error.message.push_str(" Provider diagnostic: ");
            error.message.push_str(&diagnostic);
        }
        error
    }
}

/// 对真实响应检查续接完整性，缺少必需数据时保留可定位的失败。
fn validate_response_reasoning(
    completion: &OpenAiCompletion,
    requires_reasoning_content_for_tool_calls: bool,
) -> Result<(), ProviderError> {
    let message = &completion.response.assistant_message;
    let required = completion.reasoning_content_present
        || (requires_reasoning_content_for_tool_calls && !message.tool_calls.is_empty());
    match message.provider_reasoning_replay.as_ref() {
        Some(replay) => replay
            .validate_message(message)
            .map_err(provider_reasoning_history_error),
        None if required => Err(provider_reasoning_history_error(
            "provider response is missing required continuation data",
        )),
        None => Ok(()),
    }
}

impl Provider for OpenAiProvider {
    fn model_configuration(&self) -> ModelConfigurationSnapshot {
        let Some(selection) = self.selected_model.as_ref() else {
            panic!("model configuration requested before model selection");
        };
        let capabilities = ProviderProtocolContract {
            max_context_tokens: selection.max_context_tokens,
            max_output_tokens: selection.max_output_tokens,
        };
        ModelConfigurationSnapshot {
            provider: self.config.provider_name.clone(),
            model: selection.model_name.clone(),
            reasoning_variant: selection.reasoning_variant.clone(),
            protocol: selection.api_protocol,
            capabilities,
            credential_provenance: format!(
                "{}:{}",
                crate::USER_AUTH_FILE_NAME,
                self.config.provider_name
            ),
            retry: TurnRetryPolicy::default(),
        }
    }

    /// 一次完成的单一编排入口：一切模型调用走流式解码。
    /// 请求归一、能力校验、wire 协议选择与 tool-reasoning 契约校验只在这一个
    /// 入口实现。
    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent),
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
            && model_name != selection.model_name
        {
            return Err(super::config::configuration_error(
                "model selector is not the fixed model for this provider turn",
                "provider_selector_unknown_model",
            ));
        }
        let prepared = self.prepare_reasoning_history(request, selection)?;
        let request = prepared.as_ref();
        // 静态能力声明：工具与非工具请求统一使用声明式契约；api_protocol 由
        // 目录选择决定。
        let capabilities = self.model_configuration().capabilities;
        if let Err(errors) = validate_model_request_with_capabilities(request, &capabilities) {
            return Err(provider_request_validation_error(errors));
        }
        let completion = self.complete_protocol(
            request,
            ProtocolRequestContext {
                cancellation,
                selection,
                on_event,
                on_attempt,
            },
        )?;
        validate_response_reasoning(
            &completion,
            selection.requires_reasoning_content_for_tool_calls,
        )?;
        Ok(completion.response)
    }
}

#[cfg(test)]
mod continuation_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::{ModelMessage, ModelRole, ProviderReasoningReplay, ThinkingWireFormat};

    fn selection() -> SelectedModel {
        SelectedModel {
            model_name: "model".into(),
            api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
            max_context_tokens: Some(32_000),
            max_output_tokens: 4096,
            reasoning_variant: None,
            reasoning_enabled: false,
            wire_reasoning_effort: None,
            thinking_wire_format: ThinkingWireFormat::ReasoningEffort,
            supports_developer_role: false,
            supports_tool_choice: true,
            requires_reasoning_content_for_tool_calls: false,
            requires_assistant_content_for_tool_calls: false,
        }
    }

    #[test]
    fn continuation_follows_model_identity_across_effort_changes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let provider = OpenAiProvider::new(
            OpenAiProviderConfig {
                provider_name: "provider".into(),
                base_url: "https://example.invalid/v1".into(),
                api_key: "unused".into(),
            },
            runtime.handle().clone(),
        )
        .unwrap();
        let mut message = ModelMessage::text(ModelRole::Assistant, "public answer");
        message.provider_reasoning_replay = Some(ProviderReasoningReplay::Chat {
            provider_name: "provider".into(),
            model_name: "model".into(),
            reasoning_effort: Some("high".into()),
            tool_call_ids: vec![],
            reasoning_content: "private continuation".into(),
            reasoning_field: "reasoning".into(),
            reasoning_details: vec![],
        });
        let original = ModelTurnRequest::new("request", vec![message]);
        for effort in [None, Some("low"), Some("off")] {
            let mut selected = selection();
            selected.reasoning_variant = effort.map(str::to_string);
            selected.reasoning_enabled = effort.is_some_and(|effort| effort != "off");
            selected.wire_reasoning_effort =
                effort.filter(|effort| *effort != "off").map(str::to_string);
            let prepared = provider
                .prepare_reasoning_history(&original, &selected)
                .unwrap();
            let wire = selected
                .api_protocol
                .request_payload(&selected, &prepared, "model");
            assert_eq!(wire["messages"][0]["reasoning"], "private continuation");
            match effort {
                None => assert!(wire.get("reasoning_effort").is_none()),
                Some("off") => assert_eq!(wire["reasoning_effort"], "none"),
                Some(value) => assert_eq!(wire["reasoning_effort"], value),
            }
        }
        for change in ["provider", "model", "protocol"] {
            let mut changed_provider = provider.clone();
            let mut selected = selection();
            match change {
                "provider" => changed_provider.config.provider_name = "other".into(),
                "model" => selected.model_name = "other".into(),
                _ => selected.api_protocol = ProviderApiProtocol::OpenAiResponses,
            }
            let prepared = changed_provider
                .prepare_reasoning_history(&original, &selected)
                .unwrap();
            let wire =
                selected
                    .api_protocol
                    .request_payload(&selected, &prepared, &selected.model_name);
            let text = wire.to_string();
            assert!(text.contains("public answer"));
            assert!(!text.contains("private continuation"));
            assert!(original.messages[0].provider_reasoning_replay.is_some());
        }
        let mut corrupted = original;
        if let Some(ProviderReasoningReplay::Chat { tool_call_ids, .. }) =
            &mut corrupted.messages[0].provider_reasoning_replay
        {
            tool_call_ids.push("unrelated-call".into());
        }
        let error = provider
            .prepare_reasoning_history(&corrupted, &selection())
            .unwrap_err();
        assert_eq!(
            error.code.as_deref(),
            Some("provider_reasoning_history_invalid")
        );
        assert!(!error.to_string().contains("private continuation"));
    }
}
