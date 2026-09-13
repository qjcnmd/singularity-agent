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
    OpenAiCompletion, chat_completions_endpoint, openai_chat_stream_request_payload,
    openai_responses_stream_request_payload, responses_endpoint,
};
use crate::provider::attempt::{ProviderAttemptInProgress, duration_millis};
use crate::provider::contract::{
    ProviderApiProtocol, provider_request_validation_error, validate_model_request,
};
use crate::provider::policy::TurnRetryPolicy;
use crate::provider::runtime::{OpenAiProviderConfig, SelectedModel};
use crate::provider::telemetry::{ProviderAttemptEvent, ProviderStreamEvent};
use crate::provider::{Provider, ProviderCallError};
use crate::types::{ModelTurnRequest, ModelTurnResponse};

impl ProviderApiProtocol {
    fn endpoint(self, config: &OpenAiProviderConfig) -> String {
        match self {
            Self::OpenAiChatCompletions => chat_completions_endpoint(&config.base_url),
            Self::OpenAiResponses => responses_endpoint(&config.base_url),
        }
    }

    fn request_payload(self, selection: &SelectedModel, request: &ModelTurnRequest) -> Value {
        match self {
            Self::OpenAiChatCompletions => openai_chat_stream_request_payload(request, selection),
            Self::OpenAiResponses => openai_responses_stream_request_payload(request, selection),
        }
    }
}

#[derive(Clone)]
pub struct OpenAiProvider {
    config: OpenAiProviderConfig,
    selected_model: SelectedModel,
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
        selected_model: SelectedModel,
        runtime_handle: tokio::runtime::Handle,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .read_timeout(Duration::from_secs(crate::PROVIDER_TIMEOUT_SECONDS))
            .user_agent(format!("singularity-agent/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(provider_client_initialization_error)?;
        Ok(Self {
            config,
            selected_model,
            client,
            runtime: runtime_handle,
        })
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
    /// 执行一次流式 HTTP attempt，响应校验完成后才记录成功终态。
    fn complete_attempt(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        record_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> std::io::Result<()>,
    ) -> Result<OpenAiCompletion, ProviderCallError> {
        let selection = &self.selected_model;
        let api_protocol = selection.api_protocol;
        let model_name = &selection.model_name;
        let endpoint = api_protocol.endpoint(&self.config);
        let request_payload = api_protocol.request_payload(selection, request);
        let runtime = &self.runtime;
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error().into());
        }

        let occurrence =
            ProviderAttemptInProgress::new(&self.config.provider_name, model_name, api_protocol);
        record_attempt(occurrence.started_event())?;
        let completion = match block_on_provider_future(
            runtime,
            cancellation,
            "provider_request_send_failed",
            || {
                self.client
                    .post(endpoint)
                    .bearer_auth(&self.config.api_key)
                    .json(&request_payload)
                    .send()
            },
        ) {
            Ok(response) if response.status().is_success() => {
                read_openai_sse(self, request, cancellation, response, on_event)
            }
            Ok(response) => Err(self.classify_http_failure(response, cancellation)),
            Err(error) => Err(error),
        };

        // A response rejected for missing replay still incurred its reported usage.
        let usage = completion
            .as_ref()
            .ok()
            .map(|completion| &completion.response.usage)
            .filter(|usage| usage.usage_present)
            .cloned();
        let completion = completion.and_then(|completion| {
            validate_response_reasoning(
                &completion,
                selection.requires_reasoning_content_for_tool_calls,
            )
            .map_err(ProviderError::without_automatic_retry)?;
            Ok(completion)
        });
        let error = completion.as_ref().err();
        record_attempt(ProviderAttemptEvent::Finished(Box::new(
            occurrence.finish(
                error,
                usage,
                error
                    .and_then(|error| error.retry_after)
                    .map(duration_millis),
            ),
        )))?;
        completion.map_err(Into::into)
    }

    fn classify_http_failure(
        &self,
        response: reqwest::Response,
        cancellation: &CancellationToken,
    ) -> ProviderError {
        let status_code = response.status().as_u16();
        let retry_after = retry_after_delay(response.headers());
        let error_body =
            match read_bounded_provider_response_body(&self.runtime, cancellation, response) {
                Ok(body) => body,
                Err(error) if error.kind == crate::ModelErrorKind::Cancelled => return error,
                Err(error) => {
                    let mut failure =
                        provider_error_from_http_status(status_code).with_retry_after(retry_after);
                    failure.message.push_str(&format!(
                        " Could not read provider error response: {}",
                        error.message
                    ));
                    return failure;
                }
            };
        let error_fields = parse_provider_error_body(&error_body);
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
                    Some(bounded_provider_error_diagnostic(&String::from_utf8_lossy(
                        &error_body,
                    )))
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
        let selection = &self.selected_model;
        ModelConfigurationSnapshot {
            provider: self.config.provider_name.clone(),
            model: selection.model_name.clone(),
            reasoning_variant: selection.reasoning_variant.clone(),
            protocol: selection.api_protocol,
            max_context_tokens: selection.max_context_tokens,
            max_output_tokens: selection.max_output_tokens,
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
        record_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> std::io::Result<()>,
    ) -> Result<ModelTurnResponse, ProviderCallError> {
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error().into());
        }
        let selection = &self.selected_model;
        let prepared = self.prepare_reasoning_history(request, selection)?;
        let request = prepared.as_ref();
        // 静态能力声明：工具与非工具请求统一使用声明式契约；api_protocol 由
        // 目录选择决定。
        if let Err(errors) = validate_model_request(request, selection.max_output_tokens) {
            return Err(provider_request_validation_error(errors).into());
        }
        let completion = self.complete_attempt(request, cancellation, on_event, record_attempt)?;
        Ok(completion.response)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::{ModelMessage, ModelRole, ProviderReasoningReplay, ThinkingWireFormat};

    #[test]
    fn http_error_body_failure_preserves_status_and_cancellation() {
        use std::io::{BufRead, BufReader, Write};

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let provider = OpenAiProvider::new(
            OpenAiProviderConfig {
                provider_name: "fixture".into(),
                base_url: "http://127.0.0.1/v1".into(),
                api_key: "unused".into(),
            },
            selection(),
            runtime.handle().clone(),
        )
        .unwrap();
        for cancelled in [false, true] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                loop {
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 100\r\nRetry-After: 2\r\nConnection: close\r\n\r\nshort").unwrap();
            });
            let response = runtime
                .block_on(async {
                    provider
                        .client
                        .get(format!("http://{address}/"))
                        .send()
                        .await
                })
                .unwrap();
            server.join().unwrap();
            let cancellation = CancellationToken::new();
            if cancelled {
                cancellation.cancel();
            }
            let error = provider.classify_http_failure(response, &cancellation);
            if cancelled {
                assert_eq!(error.kind, crate::ModelErrorKind::Cancelled);
            } else {
                assert_eq!(error.kind, crate::ModelErrorKind::AuthError);
                assert_eq!(error.retry_after, Some(Duration::from_secs(2)));
                assert!(error.message.contains("HTTP 401"));
                assert!(error.message.contains("provider transport failed"));
            }
        }
    }

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
    fn sse_continuation_validation_precedes_finished_commit() {
        use std::io::{BufRead, BufReader, Read, Write};
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for require_reasoning in [false, true] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut content_length = 0;
                loop {
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse::<usize>().unwrap();
                    }
                }
                reader.read_exact(&mut vec![0; content_length]).unwrap();
                let body = concat!(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
                    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n",
                    "data: [DONE]\n\n"
                );
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            });
            let mut model = selection();
            model.requires_reasoning_content_for_tool_calls = require_reasoning;
            let provider = OpenAiProvider::new(
                OpenAiProviderConfig {
                    provider_name: "fixture".into(),
                    base_url: format!("http://{address}/v1"),
                    api_key: "unused".into(),
                },
                model,
                runtime.handle().clone(),
            )
            .unwrap();
            let mut request =
                ModelTurnRequest::new("request", vec![ModelMessage::text(ModelRole::User, "read")]);
            request.tools.push(crate::ModelToolSchema {
                name: "read".into(),
                description: "read".into(),
                parameters_schema: serde_json::json!({"type":"object"}),
            });
            let mut events = Vec::new();
            let result = provider.complete_stream(
                &request,
                &CancellationToken::new(),
                &mut |_| {},
                &mut |event| {
                    events.push(event);
                    Ok(())
                },
            );
            server.join().unwrap();
            assert_eq!(events.len(), 2);
            let ProviderAttemptEvent::Finished(finished) = &events[1] else {
                panic!("missing terminal");
            };
            if require_reasoning {
                assert!(
                    matches!(result, Err(ProviderCallError::Provider(error)) if error.code.as_deref() == Some("provider_reasoning_history_invalid"))
                );
                assert_eq!(
                    finished.terminal_status,
                    crate::ProviderAttemptStatus::Error
                );
                assert_eq!(
                    finished.diagnostic_code.as_deref(),
                    Some("provider_reasoning_history_invalid")
                );
                assert_eq!(finished.usage.as_ref().unwrap().input_tokens, 10);
                assert_eq!(finished.usage.as_ref().unwrap().output_tokens, 2);
            } else {
                assert_eq!(result.unwrap().tool_calls()[0].tool_call_id, "call");
                assert_eq!(finished.terminal_status, crate::ProviderAttemptStatus::Ok);
                assert!(finished.usage.as_ref().unwrap().usage_present);
            }
        }
    }

    #[test]
    fn attempt_commit_follows_validation_and_prevents_http_send_on_failure() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for protocol in [
            ProviderApiProtocol::OpenAiChatCompletions,
            ProviderApiProtocol::OpenAiResponses,
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let mut model = selection();
            model.api_protocol = protocol;
            let mut provider = OpenAiProvider::new(
                OpenAiProviderConfig {
                    provider_name: "fixture".into(),
                    base_url: format!("http://{}/v1", listener.local_addr().unwrap()),
                    api_key: "unused".into(),
                },
                model,
                runtime.handle().clone(),
            )
            .unwrap();
            // Keep a regression that accidentally sends bounded instead of waiting for a server.
            provider.client = reqwest::Client::builder()
                .timeout(Duration::from_millis(200))
                .build()
                .unwrap();
            let mut request = ModelTurnRequest::new(
                "request",
                vec![ModelMessage::text(ModelRole::User, "hello")],
            );
            request.model_preferences.max_output_tokens = Some(u32::MAX);
            let cancellation = CancellationToken::new();
            let mut recorded = 0;
            let mut commit = |event| {
                assert!(matches!(event, ProviderAttemptEvent::Started(_)));
                recorded += 1;
                Err(std::io::Error::from_raw_os_error(123))
            };
            let mut stream = |_| panic!("a blocked request cannot stream");
            let invalid =
                provider.complete_stream(&request, &cancellation, &mut stream, &mut commit);
            assert!(
                matches!(invalid, Err(ProviderCallError::Provider(error)) if error.code.as_deref() == Some("provider_request_invalid"))
            );
            request.model_preferences.max_output_tokens = None;
            let blocked =
                provider.complete_stream(&request, &cancellation, &mut stream, &mut commit);
            assert!(
                matches!(blocked, Err(ProviderCallError::Recording(error)) if error.raw_os_error() == Some(123))
            );
            assert_eq!(
                recorded, 1,
                "invalid requests must fail before recording starts"
            );
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
            );
            assert!(!cancellation.is_cancelled());
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
            selection(),
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
            let wire = selected.api_protocol.request_payload(&selected, &prepared);
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
            let wire = selected.api_protocol.request_payload(&selected, &prepared);
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
