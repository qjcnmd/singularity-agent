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
use singularity_core::{CancellationToken, duration_millis};

use crate::config::ModelConfigurationSnapshot;
use crate::error::{
    ProviderError, bounded_provider_error_diagnostic, parse_provider_error_body,
    provider_error_kind_for_code,
};
use crate::openai::{
    chat_completions_endpoint, openai_chat_stream_request_payload,
    openai_responses_stream_request_payload, responses_endpoint,
};
use crate::provider::contract::{
    ProviderApiProtocol, provider_request_validation_error, validate_model_request,
};
use crate::provider::runtime::{OpenAiProviderConfig, SelectedModel};
use crate::provider::telemetry::{
    ProviderAttemptEvent, ProviderAttemptOccurrence, ProviderAttemptStarted, ProviderAttemptStatus,
    ProviderStreamEvent,
};
use crate::provider::{Provider, ProviderCallError};
use crate::types::{ModelTurnRequest, ModelTurnResponse};

fn request_payload(selection: &SelectedModel, request: &ModelTurnRequest) -> Value {
    match selection.api_protocol {
        ProviderApiProtocol::Chat => openai_chat_stream_request_payload(request, selection),
        ProviderApiProtocol::Responses => {
            openai_responses_stream_request_payload(request, selection)
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

/// 进程内唯一的上游 HTTP 客户端：连接池与 TLS 会话因此跨 turn 复用。
/// 客户端配置对同一进程恒定，构造点只保留这一处。
fn provider_client() -> Result<reqwest::Client, ProviderError> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = reqwest::Client::builder()
        .read_timeout(Duration::from_secs(crate::PROVIDER_TIMEOUT_SECONDS))
        .user_agent(format!("singularity-agent/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(provider_client_initialization_error)?;
    // 并发构造时保留先到者：两者配置相同，落败实例直接丢弃。
    Ok(CLIENT.get_or_init(|| client).clone())
}

impl OpenAiProvider {
    /// 创建并校验 OpenAI-compatible provider；异步执行一律使用调用方注入的
    /// runtime，读取超时固定为 PROVIDER_TIMEOUT_SECONDS。
    pub(crate) fn new(
        config: OpenAiProviderConfig,
        selected_model: SelectedModel,
        runtime_handle: tokio::runtime::Handle,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            config,
            selected_model,
            client: provider_client()?,
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
    ) -> Result<ModelTurnResponse, ProviderCallError> {
        let selection = &self.selected_model;
        let api_protocol = selection.api_protocol;
        let model_name = &selection.model_name;
        let endpoint = match api_protocol {
            ProviderApiProtocol::Chat => chat_completions_endpoint(&self.config.base_url),
            ProviderApiProtocol::Responses => responses_endpoint(&self.config.base_url),
        };
        let request_payload = request_payload(selection, request);
        let runtime = &self.runtime;
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error().into());
        }

        let started_at = std::time::Instant::now();
        record_attempt(ProviderAttemptEvent::Started(ProviderAttemptStarted {
            provider_name: self.config.provider_name.clone(),
            model_name: model_name.to_string(),
            actual_api_protocol: api_protocol,
        }))?;
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

        // 因缺少 replay 而被拒绝的响应，仍已产生其上报的用量。
        let usage = completion
            .as_ref()
            .ok()
            .map(|response| &response.usage)
            .filter(|usage| usage.usage_present)
            .cloned();
        let completion = completion.and_then(|response| {
            validate_response_reasoning(
                &response,
                selection.requires_reasoning_content_for_tool_calls,
            )
            .map_err(ProviderError::without_automatic_retry)?;
            Ok(response)
        });
        let error = completion.as_ref().err();
        let retry_after_ms = error
            .and_then(|error| error.retry_after)
            .map(duration_millis);
        record_attempt(ProviderAttemptEvent::Finished(Box::new(
            ProviderAttemptOccurrence {
                provider_name: self.config.provider_name.clone(),
                model_name: model_name.to_string(),
                actual_api_protocol: api_protocol,
                terminal_status: match error {
                    None => ProviderAttemptStatus::Ok,
                    Some(error) if error.kind == crate::ModelErrorKind::Cancelled => {
                        ProviderAttemptStatus::Cancelled
                    }
                    Some(_) => ProviderAttemptStatus::Error,
                },
                attempt_duration_ms: duration_millis(started_at.elapsed()),
                error_category: error.map(ProviderError::category),
                diagnostic_code: error.and_then(|error| error.code.clone()),
                retry_after_ms,
                retry_after_source: retry_after_ms
                    .map(|_| singularity_protocol::RetryAfterSource::ProviderHeader),
                usage,
            },
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
/// 续接材料是否存在由解析结果本身决定，不再另存存在性标志。
fn validate_response_reasoning(
    response: &ModelTurnResponse,
    requires_reasoning_content_for_tool_calls: bool,
) -> Result<(), ProviderError> {
    let message = &response.assistant_message;
    match message.provider_reasoning_replay.as_ref() {
        Some(replay) => replay
            .validate_message(message)
            .map_err(provider_reasoning_history_error),
        None if requires_reasoning_content_for_tool_calls && !message.tool_calls.is_empty() => {
            Err(provider_reasoning_history_error(
                "provider response is missing required continuation data",
            ))
        }
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
        let response = self.complete_attempt(request, cancellation, on_event, record_attempt)?;
        Ok(response)
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
            api_protocol: ProviderApiProtocol::Chat,
            max_context_tokens: 32_000,
            max_output_tokens: 4096,
            reasoning_variant: None,
            reasoning_enabled: false,
            wire_reasoning_effort: None,
            thinking_wire_format: ThinkingWireFormat::ReasoningEffort,
            chat_output_tokens_field: crate::provider::contract::DEFAULT_CHAT_OUTPUT_TOKENS_FIELD
                .to_string(),
            supports_developer_role: false,
            supports_tool_choice: true,
            requires_reasoning_content_for_tool_calls: false,
            requires_assistant_content_for_tool_calls: false,
        }
    }

    /// 用一个本地 SSE 夹具跑一次完整 provider 调用：两种协议共用真实 HTTP
    /// 读取与写入路径，返回结果和 attempt 事件。
    fn complete_against_sse(
        runtime: &tokio::runtime::Runtime,
        protocol: ProviderApiProtocol,
        require_reasoning: bool,
        body: &'static str,
    ) -> (
        Result<ModelTurnResponse, ProviderCallError>,
        Vec<ProviderAttemptEvent>,
    ) {
        use std::io::{BufRead, BufReader, Read, Write};

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
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let mut model = selection();
        model.api_protocol = protocol;
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
        (result, events)
    }

    /// 正文里的 `<tool_call>` 文本是普通文本，不是被拒绝的协议形状。
    ///
    /// 端点没有返回结构化工具调用就是不调用工具；正文是否"看起来像"调用由
    /// 用户和模型判断，中间层不猜测，也不据此把一次有效回复判为失败。
    #[test]
    fn an_envelope_shaped_reply_is_plain_text_and_not_a_tool_call() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let envelope = concat!(
            "I will not call a tool. Here is the pattern you asked about: ",
            "<tool_call>{\"name\":\"read\"}</tool_call>"
        );
        let chat_body: &'static str = Box::leak(
            format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"choices":[{"index":0,"delta":{"content":envelope},"finish_reason":"stop"}]}),
                serde_json::json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":30}}),
            )
            .into_boxed_str(),
        );
        let responses_body: &'static str = Box::leak(
            format!(
                "data: {}\n\n",
                serde_json::json!({"type":"response.completed","response":{"id":"response","status":"completed",
                    "output":[{"type":"message","id":"m","role":"assistant",
                        "content":[{"type":"output_text","text":envelope}]}],
                    "usage":{"input_tokens":12,"output_tokens":30}}}),
            )
            .into_boxed_str(),
        );
        for (protocol, body) in [
            (ProviderApiProtocol::Chat, chat_body),
            (ProviderApiProtocol::Responses, responses_body),
        ] {
            let (result, events) = complete_against_sse(&runtime, protocol, false, body);
            let response = result.unwrap_or_else(|error| panic!("{protocol:?}: {error}"));
            assert!(
                response.tool_calls().is_empty(),
                "{protocol:?}: text must not become a tool call"
            );
            assert_eq!(response.assistant_message.content, envelope, "{protocol:?}");
            let ProviderAttemptEvent::Finished(finished) = &events[1] else {
                panic!("missing terminal");
            };
            assert_eq!(
                finished.terminal_status,
                crate::ProviderAttemptStatus::Ok,
                "{protocol:?}"
            );
        }
    }

    #[test]
    fn sse_continuation_validation_precedes_finished_commit() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        // 两种协议各有一个「带工具调用但没有续接材料」的真实响应夹具：
        // 必需续接缺失的判定只看解析结果里的续接对象，不依赖协议特有的标志。
        let chat_body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n"
        );
        let responses_body = concat!(
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"response\",\"status\":\"completed\",",
            "\"output\":[{\"type\":\"function_call\",\"call_id\":\"call\",\"name\":\"read\",\"arguments\":\"{}\"}],",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n\n"
        );
        for (protocol, body) in [
            (ProviderApiProtocol::Chat, chat_body),
            (ProviderApiProtocol::Responses, responses_body),
        ] {
            for require_reasoning in [false, true] {
                let (result, events) =
                    complete_against_sse(&runtime, protocol, require_reasoning, body);
                assert_eq!(events.len(), 2, "{protocol:?}");
                let ProviderAttemptEvent::Finished(finished) = &events[1] else {
                    panic!("missing terminal");
                };
                if require_reasoning {
                    assert!(
                        matches!(result, Err(ProviderCallError::Provider(ref error)) if error.code.as_deref() == Some("provider_reasoning_history_invalid")),
                        "{protocol:?}: {result:?}"
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
                    assert_eq!(finished.usage.as_ref().unwrap().total_tokens, 12);
                } else {
                    assert_eq!(result.unwrap().tool_calls()[0].tool_call_id, "call");
                    assert_eq!(finished.terminal_status, crate::ProviderAttemptStatus::Ok);
                    // 夹具只上报输入与输出：两种协议的解析入口都按已知计数补出总数。
                    let usage = finished.usage.as_ref().unwrap();
                    assert!(usage.usage_present);
                    assert_eq!(usage.total_tokens, 12);
                }
            }
        }
    }

    /// 无工具调用且正文只有空白不是一次有效回复：该拒绝发生在 provider 边界，
    /// 终止结果不再对最终正文做第二次复核。
    #[test]
    fn an_empty_reply_without_tool_calls_is_rejected_at_the_provider_boundary() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let chat_body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"  \"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}\n\n",
            "data: [DONE]\n\n"
        );
        let responses_body = concat!(
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"response\",\"status\":\"completed\",",
            "\"output\":[{\"type\":\"message\",\"id\":\"m\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"  \"}]}],",
            "\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n"
        );
        for (protocol, body) in [
            (ProviderApiProtocol::Chat, chat_body),
            (ProviderApiProtocol::Responses, responses_body),
        ] {
            let (result, events) = complete_against_sse(&runtime, protocol, false, body);
            let Err(ProviderCallError::Provider(error)) = &result else {
                panic!("{protocol:?}: {result:?}");
            };
            assert_eq!(error.code.as_deref(), Some("provider_response_invalid"));
            assert!(error.to_string().contains("empty_response"), "{protocol:?}");
            let ProviderAttemptEvent::Finished(finished) = &events[1] else {
                panic!("missing terminal");
            };
            assert_eq!(
                finished.terminal_status,
                crate::ProviderAttemptStatus::Error
            );
        }
    }

    #[test]
    fn attempt_commit_follows_validation_and_prevents_http_send_on_failure() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for protocol in [ProviderApiProtocol::Chat, ProviderApiProtocol::Responses] {
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
            // 保留回归用例，覆盖意外发送 bounded 而不等待服务端的情况。
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

    /// 输出上限字段由选择决定，并出现在真实发出的请求体上。
    ///
    /// 配置里写的字段名就是发出的字段名，且一次只发这一个——不会同时发两个
    /// 字段让端点自行取舍。
    #[test]
    fn the_chat_output_limit_uses_the_field_declared_by_the_selection() {
        for (field, absent) in [
            ("max_tokens", "max_completion_tokens"),
            ("max_completion_tokens", "max_tokens"),
            // 端点用语不是这两个时同样按配置原样发送。
            ("max_new_tokens", "max_tokens"),
        ] {
            let (path, body) = capture_chat_request(field);
            assert_eq!(path, "/v1/chat/completions");
            assert_eq!(
                body[field], 4096,
                "the declared field carries the limit: {body}"
            );
            assert!(
                body.get(absent).is_none(),
                "the unused field must not be sent as well: {body}"
            );
        }
    }

    /// 发一次真实的 Chat 请求并返回请求路径与请求体。
    fn capture_chat_request(field: &str) -> (String, serde_json::Value) {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut path = String::new();
            let mut content_length = 0;
            let mut first = true;
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if first {
                    path = line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    first = false;
                }
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                DONE_ONLY.len(),
                DONE_ONLY
            )
            .unwrap();
            (
                path,
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            )
        });

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut model = selection();
        model.chat_output_tokens_field = field.to_string();
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
        let mut request = ModelTurnRequest::new(
            "request",
            vec![ModelMessage::text(ModelRole::User, "hello")],
        );
        request.model_preferences.max_output_tokens = Some(4096);
        // 夹具只发 [DONE]：响应本身失败，但请求已经真实发出。
        let _ = provider.complete_stream(
            &request,
            &CancellationToken::new(),
            &mut |_| {},
            &mut |_| Ok(()),
        );
        server.join().unwrap()
    }

    /// 只含流终止标记的 SSE 夹具。
    const DONE_ONLY: &str = "data: [DONE]\n\n";

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
            let wire = request_payload(&selected, &prepared);
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
                _ => selected.api_protocol = ProviderApiProtocol::Responses,
            }
            let prepared = changed_provider
                .prepare_reasoning_history(&original, &selected)
                .unwrap();
            let wire = request_payload(&selected, &prepared);
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
    #[test]
    fn sse_direct_loop_preserves_split_frames_and_supports_blocking_callbacks() {
        use std::io::{BufRead, BufReader, Write};

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"split \"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"frame\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
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
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            for chunk in body.as_bytes().chunks(3) {
                stream.write_all(chunk).unwrap();
                stream.flush().unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let provider = OpenAiProvider::new(
            OpenAiProviderConfig {
                provider_name: "fixture".into(),
                base_url: format!("http://{address}/v1"),
                api_key: "unused".into(),
            },
            selection(),
            runtime.handle().clone(),
        )
        .unwrap();
        let request = ModelTurnRequest::new(
            "request",
            vec![ModelMessage::text(ModelRole::User, "hello")],
        );
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<String>(1);
        sender.blocking_send(String::new()).unwrap();
        let (callback_started, callback_ready) = tokio::sync::oneshot::channel();
        let mut callback_started = Some(callback_started);
        let consumer = runtime.spawn(async move {
            // 首个回调遇到已满的 channel，随后在 channel 排空时继续。
            callback_ready.await.unwrap();
            tokio::time::sleep(Duration::from_millis(25)).await;
            let mut text = String::new();
            while let Some(delta) = receiver.recv().await {
                text.push_str(&delta);
            }
            text
        });
        let visible = tokio::sync::Mutex::new(String::new());
        let response = provider
            .complete_stream(
                &request,
                &CancellationToken::new(),
                &mut |event| {
                    if let ProviderStreamEvent::OutputTextDelta { delta } = event {
                        visible.blocking_lock().push_str(&delta);
                        if let Some(started) = callback_started.take() {
                            started.send(()).unwrap();
                        }
                        sender.blocking_send(delta).unwrap();
                    }
                },
                &mut |_| Ok(()),
            )
            .unwrap();
        drop(sender);
        server.join().unwrap();
        assert_eq!(response.assistant_message.content, "split frame");
        assert_eq!(*visible.blocking_lock(), "split frame");
        assert_eq!(runtime.block_on(consumer).unwrap(), "split frame");
    }

    #[test]
    fn sse_partial_output_failures_preserve_cause_and_forbid_retry() {
        use crate::ModelErrorKind;
        use std::io::{BufRead, BufReader, Read, Write};
        use std::sync::mpsc;

        let runtime = tokio::runtime::Runtime::new().unwrap();
        for expected in [
            ModelErrorKind::Cancelled,
            ModelErrorKind::Timeout,
            ModelErrorKind::NetworkError,
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let (release, wait_release) = mpsc::channel();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
                // 故意让 body 保持未完成，直到取消、超时或断开。
                stream.write_all(concat!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"visible\"}}]}\n\n"
                ).as_bytes()).unwrap();
                wait_release.recv_timeout(Duration::from_secs(5)).unwrap();
            });
            let mut provider = OpenAiProvider::new(
                OpenAiProviderConfig {
                    provider_name: "fixture".into(),
                    base_url: format!("http://{address}/v1"),
                    api_key: "unused".into(),
                },
                selection(),
                runtime.handle().clone(),
            )
            .unwrap();
            provider.client = reqwest::Client::builder()
                .read_timeout(Duration::from_millis(500))
                .build()
                .unwrap();
            let cancellation = CancellationToken::new();
            let cancel = cancellation.clone();
            let (delta_sent, delta_seen) = mpsc::channel();
            let controller = std::thread::spawn(move || {
                delta_seen.recv_timeout(Duration::from_secs(5)).unwrap();
                if expected == ModelErrorKind::Cancelled {
                    // 在 HTTP body 停滞期间取消，而不是在请求之前。
                    std::thread::sleep(Duration::from_millis(25));
                    cancel.cancel();
                }
            });
            let mut visible = String::new();
            let mut attempts = Vec::new();
            let result = provider.complete_stream(
                &ModelTurnRequest::new(
                    "request",
                    vec![ModelMessage::text(ModelRole::User, "hello")],
                ),
                &cancellation,
                &mut |event| {
                    if let ProviderStreamEvent::OutputTextDelta { delta } = event {
                        visible.push_str(&delta);
                        delta_sent.send(()).unwrap();
                        if expected == ModelErrorKind::NetworkError {
                            release.send(()).unwrap();
                        }
                    }
                },
                &mut |event| {
                    attempts.push(event);
                    Ok(())
                },
            );
            if expected != ModelErrorKind::NetworkError {
                release.send(()).unwrap();
            }
            server.join().unwrap();
            controller.join().unwrap();
            let Err(ProviderCallError::Provider(error)) = result else {
                panic!("expected {expected:?}, got {result:?}");
            };
            assert_eq!(error.kind, expected);
            assert!(!error.automatic_retry_allowed);
            assert_eq!(visible, "visible");
            assert!(
                matches!(attempts.as_slice(), [ProviderAttemptEvent::Started(_), ProviderAttemptEvent::Finished(finished)]
                    if finished.terminal_status == if expected == ModelErrorKind::Cancelled {
                        crate::ProviderAttemptStatus::Cancelled
                    } else { crate::ProviderAttemptStatus::Error }
                )
            );
        }
    }
}
