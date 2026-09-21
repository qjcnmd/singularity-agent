//! 具体 OpenAI-compatible provider：协议选择、一次调用编排与响应终结。
//!
//! 两种协议（Chat/Responses）的请求编码、SSE 解码与响应终结各自位于本包的
//! 协议模块；这里只做“选哪一种协议”和“一次 attempt 的完整编排”。
//! 可取消网络等待、有界读取与 SSE 帧切分由 transport 提供，本模块不反向
//! 被 transport 依赖。

use std::fmt;

use singularity_core::{CancellationToken, duration_millis};

use crate::config::selection::{OpenAiProviderConfig, SelectedModel};
use crate::config::{ModelConfigurationSnapshot, ProviderConfigSnapshot};
use crate::error::{
    ProviderError, bounded_provider_error_diagnostic, parse_provider_error_body,
    provider_error_kind_for_code,
};
use crate::openai::{
    chat_completions_endpoint, openai_chat_stream_request_payload,
    openai_responses_stream_request_payload, read_chat_sse_stream, read_responses_sse_stream,
    reasoning_replay_for, responses_endpoint,
};
use crate::provider::contract::{
    ProviderApiProtocol, provider_request_validation_error, validate_model_request,
};
use crate::provider::telemetry::{
    ProviderAttemptEvent, ProviderAttemptOccurrence, ProviderAttemptStarted, ProviderStreamEvent,
};
use crate::provider::{Provider, ProviderCallError};
use crate::transport::{
    block_on_provider_future, provider_cancelled_error, provider_client,
    provider_error_from_http_status, provider_reasoning_history_error,
    read_bounded_provider_response_body, retry_after_delay,
};
use crate::types::{ModelTurnRequest, ModelTurnResponse};
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
        Ok(Self {
            config,
            selected_model,
            client: provider_client()?,
            runtime: runtime_handle,
        })
    }

    /// 用一次冻结的配置快照与 selector 建立执行客户端。
    ///
    /// 快照只提供配置事实，执行环境（Tokio handle）由实际执行/装配层持有并
    /// 显式传入，因此配置对象不携带句柄、也不创建网络对象。
    pub fn from_snapshot(
        snapshot: &ProviderConfigSnapshot,
        selector: Option<&str>,
        runtime_handle: tokio::runtime::Handle,
    ) -> Result<Self, ProviderError> {
        let (config, selected_model) = snapshot.resolve(selector)?;
        Self::new(config, selected_model, runtime_handle)
    }

    /// 私有续接在编码边界上的一次校验：身份匹配当前 provider/model/协议的续接
    /// 必须与它附着的 assistant 消息一致，否则本次请求失败；身份不匹配的续接不由
    /// 这里处理——encoder 借用身份规则直接略过它，公开消息照常发送。
    ///
    /// 校验对象只可能是本 Provider 当前选择的那份身份：不接受第二份可能与
    /// self.config 不一致的 selection。
    ///
    /// 账本请求与其中的消息都不被复制或改写，因此同一份历史可以反复用于不同模型
    /// 的请求，而不会为清掉一个私有字段复制整份请求。
    fn validate_reasoning_history(&self, request: &ModelTurnRequest) -> Result<(), ProviderError> {
        let selection = &self.selected_model;
        for message in &request.messages {
            let Some(replay) = reasoning_replay_for(message, selection, &self.config.provider_name)
            else {
                continue;
            };
            replay
                .validate_message(message)
                .map_err(provider_reasoning_history_error)?;
        }
        Ok(())
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
        let provider_name = &self.config.provider_name;
        // 一次 attempt 的协议差异只有两处：端点与 payload 都由协议模块提供。
        let (endpoint, request_payload) = match api_protocol {
            ProviderApiProtocol::Chat => (
                chat_completions_endpoint(&self.config.base_url),
                openai_chat_stream_request_payload(request, selection, provider_name),
            ),
            ProviderApiProtocol::Responses => (
                responses_endpoint(&self.config.base_url),
                openai_responses_stream_request_payload(request, selection, provider_name),
            ),
        };
        let runtime = &self.runtime;
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error().into());
        }

        let started_at = std::time::Instant::now();
        let started = ProviderAttemptStarted {
            provider_name: self.config.provider_name.clone(),
            model_name: model_name.to_string(),
            actual_api_protocol: api_protocol,
        };
        record_attempt(ProviderAttemptEvent::Started(started.clone()))?;
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
                self.read_streamed_response(cancellation, response, on_event)
            }
            Ok(response) => Err(self.read_http_failure(response, cancellation)),
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
            )?;
            Ok(response)
        });
        record_attempt(ProviderAttemptEvent::Finished(Box::new(
            ProviderAttemptOccurrence::finished(
                started,
                duration_millis(started_at.elapsed()),
                usage,
                completion.as_ref().err(),
            ),
        )))?;
        completion.map_err(Into::into)
    }

    /// 按本次选择分派到具体协议模块读取流式响应。解码与终结都在协议模块内
    /// 完成；complete_attempt 统一校验响应并记录请求终态。
    fn read_streamed_response(
        &self,
        cancellation: &CancellationToken,
        response: reqwest::Response,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        let selection = &self.selected_model;
        match selection.api_protocol {
            ProviderApiProtocol::Chat => read_chat_sse_stream(
                &self.runtime,
                cancellation,
                response,
                on_event,
                &self.config,
                selection,
            ),
            ProviderApiProtocol::Responses => read_responses_sse_stream(
                &self.runtime,
                cancellation,
                response,
                on_event,
                &self.config,
                selection,
            ),
        }
    }

    fn read_http_failure(
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
                let detail = crate::error::bounded_wire_detail(&error_fields);
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
        // 分类不改变原始协议事实：coded 分类重写 kind/code 后，HTTP 状态与 wire
        // code/type 仍以有界诊断保留；未命中 coded 分类时状态已在消息里，不重复。
        let status_fact = coded_kind.is_some().then_some(status_code);
        let facts = crate::error::provider_wire_facts(status_fact, &error_fields);
        if !facts.is_empty() {
            error.message.push_str(" (");
            error.message.push_str(&facts.join(", "));
            error.message.push(')');
        }
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
        self.validate_reasoning_history(request)?;
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
    use crate::http_test_support::{spawn_http_server, test_config, test_selection};
    use crate::{ModelMessage, ModelRole, ProviderReasoningReplay};
    use std::time::Duration;

    #[test]
    fn http_error_body_failure_preserves_status_and_cancellation() {
        use std::io::Write;

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let provider = OpenAiProvider::new(
            test_config("http://127.0.0.1/v1"),
            test_selection(ProviderApiProtocol::Chat),
            runtime.handle().clone(),
        )
        .unwrap();
        for cancelled in [false, true] {
            let (address, server) = spawn_http_server(move |mut stream, _| {
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
            let error = provider.read_http_failure(response, &cancellation);
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

    /// 用一个本地 SSE 夹具跑一次完整 provider 调用：两种协议共用真实 HTTP
    /// 读取与写入路径，返回结果和 attempt 事件。
    fn complete_against_sse(
        runtime: &tokio::runtime::Runtime,
        protocol: ProviderApiProtocol,
        require_reasoning: bool,
        body: &str,
    ) -> (
        Result<ModelTurnResponse, ProviderCallError>,
        Vec<ProviderAttemptEvent>,
    ) {
        use std::io::Write;

        let body = body.to_string();
        let (address, server) = spawn_http_server(move |mut stream, _| {
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let mut model = test_selection(protocol);
        model.requires_reasoning_content_for_tool_calls = require_reasoning;
        let provider = OpenAiProvider::new(
            test_config(format!("http://{address}/v1")),
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
        let chat_body = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({"choices":[{"index":0,"delta":{"content":envelope},"finish_reason":"stop"}]}),
            serde_json::json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":30}}),
        );
        let responses_body = format!(
            "data: {}\n\n",
            serde_json::json!({"type":"response.completed","response":{"id":"response","status":"completed",
                    "output":[{"type":"message","id":"m","role":"assistant",
                        "content":[{"type":"output_text","text":envelope}]}],
                    "usage":{"input_tokens":12,"output_tokens":30}}}),
        );
        for (protocol, body) in [
            (ProviderApiProtocol::Chat, chat_body),
            (ProviderApiProtocol::Responses, responses_body),
        ] {
            let (result, events) = complete_against_sse(&runtime, protocol, false, &body);
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

    /// 工具是否存在由工具注册表判定：请求里没有声明的工具名必须原样穿过真实
    /// 适配器（带原 call_id），协议层不把它提前终结为传输失败——模型据此看到
    /// 一次可纠正的工具失败，而不是整轮请求失败。
    #[test]
    fn an_unlisted_tool_name_crosses_the_real_adapter_as_a_tool_call() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let chat_body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"type\":\"function\",\"function\":{\"name\":\"not_a_tool\",\"arguments\":\"{\\\"x\\\":1}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n"
        );
        let responses_body = concat!(
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"response\",\"status\":\"completed\",",
            "\"output\":[{\"type\":\"function_call\",\"call_id\":\"call-1\",\"name\":\"not_a_tool\",\"arguments\":\"{\\\"x\\\":1}\"}],",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n\n"
        );
        for (protocol, body) in [
            (ProviderApiProtocol::Chat, chat_body),
            (ProviderApiProtocol::Responses, responses_body),
        ] {
            let (result, events) = complete_against_sse(&runtime, protocol, false, body);
            let response = result.unwrap_or_else(|error| panic!("{protocol:?}: {error}"));
            let calls = response.tool_calls();
            assert_eq!(calls.len(), 1, "{protocol:?}");
            assert_eq!(calls[0].tool_call_id, "call-1", "{protocol:?}");
            assert_eq!(calls[0].tool_name, "not_a_tool", "{protocol:?}");
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
            let model = test_selection(protocol);
            let mut provider = OpenAiProvider::new(
                test_config(format!("http://{}/v1", listener.local_addr().unwrap())),
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
        use std::io::Write;

        let (address, server) = spawn_http_server(move |mut stream, request| {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                DONE_ONLY.len(),
                DONE_ONLY
            )
            .unwrap();
            (
                request.target,
                serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
            )
        });

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut model = test_selection(ProviderApiProtocol::Chat);
        model.chat_output_tokens_field = field.to_string();
        let provider = OpenAiProvider::new(
            test_config(format!("http://{address}/v1")),
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
        let mut provider = OpenAiProvider::new(
            OpenAiProviderConfig {
                provider_name: "provider".into(),
                base_url: "https://example.invalid/v1".into(),
                api_key: "unused".into(),
            },
            test_selection(ProviderApiProtocol::Chat),
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
            let mut selected = test_selection(ProviderApiProtocol::Chat);
            selected.reasoning_variant = effort.map(str::to_string);
            selected.reasoning_enabled = effort.is_some_and(|effort| effort != "off");
            selected.wire_reasoning_effort =
                effort.filter(|effort| *effort != "off").map(str::to_string);
            provider.selected_model = selected.clone();
            provider
                .validate_reasoning_history(&original)
                .expect("a replay that matches its model still validates");
            let wire = openai_chat_stream_request_payload(
                &original,
                &selected,
                &provider.config.provider_name,
            );
            assert_eq!(wire["messages"][0]["reasoning"], "private continuation");
            match effort {
                None => assert!(wire.get("reasoning_effort").is_none()),
                Some("off") => assert_eq!(wire["reasoning_effort"], "none"),
                Some(value) => assert_eq!(wire["reasoning_effort"], value),
            }
        }
        // 身份不同的续接不发送，但请求与账本一个字节都不改：无需为清掉一个私有
        // 字段复制整份请求。
        for change in ["provider", "model", "protocol"] {
            let mut changed_provider = provider.clone();
            let mut selected = test_selection(ProviderApiProtocol::Chat);
            let identity = match change {
                "provider" => {
                    changed_provider.config.provider_name = "other".into();
                    "other"
                }
                "model" => {
                    selected.model_name = "other".into();
                    "provider"
                }
                _ => {
                    selected.api_protocol = ProviderApiProtocol::Responses;
                    "provider"
                }
            };
            changed_provider.selected_model = selected.clone();
            changed_provider
                .validate_reasoning_history(&original)
                .expect("a foreign replay is not this request's contract");
            let wire = match selected.api_protocol {
                ProviderApiProtocol::Chat => {
                    openai_chat_stream_request_payload(&original, &selected, identity)
                }
                ProviderApiProtocol::Responses => {
                    openai_responses_stream_request_payload(&original, &selected, identity)
                }
            };
            let text = wire.to_string();
            assert!(text.contains("public answer"));
            assert!(!text.contains("private continuation"));
            assert!(original.messages[0].provider_reasoning_replay.is_some());
        }
        // 切回原模型：同一份请求对象重新携带它自己的续接，账本从未被改写。
        let back = openai_chat_stream_request_payload(
            &original,
            &test_selection(ProviderApiProtocol::Chat),
            &provider.config.provider_name,
        );
        assert_eq!(back["messages"][0]["reasoning"], "private continuation");

        // 匹配身份的续接必须与消息一致：损坏的续接仍然让本次请求失败。
        let mut corrupted = original;
        if let Some(ProviderReasoningReplay::Chat { tool_call_ids, .. }) =
            &mut corrupted.messages[0].provider_reasoning_replay
        {
            tool_call_ids.push("unrelated-call".into());
        }
        let error = provider.validate_reasoning_history(&corrupted).unwrap_err();
        assert_eq!(
            error.code.as_deref(),
            Some("provider_reasoning_history_invalid")
        );
        assert!(!error.to_string().contains("private continuation"));
    }
    /// 分类不删除原始协议事实：同一服务端错误经 HTTP 错误体或 SSE error 到达
    /// 时内部类别一致，原始 wire code/type 与 HTTP 状态都可定位。
    #[test]
    fn http_and_sse_errors_keep_the_same_wire_facts() {
        use std::io::Write;

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let provider = OpenAiProvider::new(
            test_config("http://127.0.0.1/v1"),
            test_selection(ProviderApiProtocol::Chat),
            runtime.handle().clone(),
        )
        .unwrap();
        let body = br#"{"error":{"code":"insufficient_quota","type":"insufficient_quota","message":"You exceeded your current quota"}}"#;

        let (address, server) = spawn_http_server(move |mut stream, _| {
            write!(
                stream,
                "HTTP/1.1 429 Too Many Requests\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
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
        let http_error = provider.read_http_failure(response, &CancellationToken::new());
        // insufficient_quota 的既有分类不变（不可重试的认证/账务类）。
        assert_eq!(http_error.kind, crate::ModelErrorKind::AuthError);
        for fact in [
            "HTTP 429",
            "provider_error_code=insufficient_quota",
            "provider_error_type=insufficient_quota",
        ] {
            assert!(
                http_error.message.contains(fact),
                "missing {fact}: {}",
                http_error.message
            );
        }

        // 同一个服务端错误经 SSE error 帧到达：类别相同，wire 事实同样保留。
        let (result, _) = complete_against_sse(
            &runtime,
            ProviderApiProtocol::Chat,
            false,
            "data: {\"error\":{\"code\":\"insufficient_quota\",\"type\":\"insufficient_quota\",\"message\":\"You exceeded your current quota\"}}\n\n",
        );
        let Err(ProviderCallError::Provider(sse_error)) = result else {
            panic!("an SSE error frame must fail the request");
        };
        assert_eq!(sse_error.kind, http_error.kind);
        assert!(
            sse_error
                .message
                .contains("provider_error_code=insufficient_quota"),
            "{}",
            sse_error.message
        );
        assert!(
            sse_error
                .message
                .contains("provider_error_type=insufficient_quota"),
            "{}",
            sse_error.message
        );
    }

    #[test]
    fn sse_direct_loop_preserves_split_frames_and_supports_blocking_callbacks() {
        use std::io::Write;

        let runtime = tokio::runtime::Runtime::new().unwrap();

        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"split \"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"frame\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (address, server) = spawn_http_server(move |mut stream, _| {
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
            test_config(format!("http://{address}/v1")),
            test_selection(ProviderApiProtocol::Chat),
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
    fn sse_partial_output_failures_preserve_cause_and_retryability() {
        use crate::ModelErrorKind;
        use std::io::Write;
        use std::sync::mpsc;

        let runtime = tokio::runtime::Runtime::new().unwrap();
        for expected in [
            ModelErrorKind::Cancelled,
            ModelErrorKind::Timeout,
            ModelErrorKind::NetworkError,
        ] {
            let (release, wait_release) = mpsc::channel();
            let (address, server) = spawn_http_server(move |mut stream, _| {
                // 故意让 body 保持未完成，直到取消、超时或断开。
                stream.write_all(concat!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"visible\"}}]}\n\n"
                ).as_bytes()).unwrap();
                wait_release.recv_timeout(Duration::from_secs(5)).unwrap();
            });
            let mut provider = OpenAiProvider::new(
                test_config(format!("http://{address}/v1")),
                test_selection(ProviderApiProtocol::Chat),
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
            assert_eq!(error.is_retryable(), expected != ModelErrorKind::Cancelled);
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

    #[test]
    fn terminal_failures_remain_retryable_after_text_or_reasoning() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for protocol in [ProviderApiProtocol::Chat, ProviderApiProtocol::Responses] {
            for output in [None, Some("content"), Some("reasoning")] {
                let mut body = String::new();
                if let Some(output) = output {
                    let delta = match protocol {
                        ProviderApiProtocol::Chat => {
                            serde_json::json!({"choices":[{"delta":{(output):"visible"}}]})
                        }
                        ProviderApiProtocol::Responses => serde_json::json!({
                            "type": if output == "content" { "response.output_text.delta" }
                                else { "response.reasoning_summary_text.delta" },
                            "delta":"visible"
                        }),
                    };
                    body.push_str(&format!("data: {delta}\n\n"));
                }
                body.push_str(match protocol {
                    ProviderApiProtocol::Chat => concat!(
                        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"network_error\"}]}\n\n",
                        "data: [DONE]\n\n"
                    ),
                    ProviderApiProtocol::Responses => concat!(
                        "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",",
                        "\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"try later\"}}}\n\n"
                    ),
                });
                let (result, events) = complete_against_sse(&runtime, protocol, false, &body);
                let Err(ProviderCallError::Provider(error)) = result else {
                    panic!("terminal failure must fail the attempt");
                };
                assert!(error.is_retryable(), "{protocol:?} {output:?}");
                assert!(
                    matches!(events.as_slice(), [ProviderAttemptEvent::Started(_), ProviderAttemptEvent::Finished(finished)]
                    if finished.terminal_status == crate::ProviderAttemptStatus::Error)
                );
            }
        }
    }

    /// 本地夹具：发一段永远不完整的 SSE body 并保持连接不结束，直到测试放行。
    /// Content-Length 大于实际发送量，只有协议终态能让调用返回。
    fn serve_incomplete_sse_body(
        body: &'static str,
    ) -> (
        std::net::SocketAddr,
        std::sync::mpsc::Sender<()>,
        std::thread::JoinHandle<()>,
    ) {
        use std::io::Write;

        let (release, wait_release) = std::sync::mpsc::channel();
        let (address, server) = spawn_http_server(move |mut stream, _| {
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n").unwrap();
            stream.write_all(body.as_bytes()).unwrap();
            stream.flush().unwrap();
            wait_release.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        (address, release, server)
    }

    /// 协议终态已经到达时，调用当即返回：已完成响应不再依赖 HTTP body 结束，
    /// 终态之后的停滞与无关损坏尾帧都不能把它改判失败。
    #[test]
    fn a_protocol_terminal_completes_the_response_before_the_body_ends() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for (protocol, body) in [
            (
                ProviderApiProtocol::Chat,
                concat!(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"complete\"},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n",
                    "data: {not json}\n\n"
                ),
            ),
            (
                ProviderApiProtocol::Responses,
                concat!(
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"response\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"m\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"complete\"}]}]}}\n\n",
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"late\"}\n\n"
                ),
            ),
        ] {
            let (address, release, server) = serve_incomplete_sse_body(body);
            let model = test_selection(protocol);
            let mut provider = OpenAiProvider::new(
                test_config(format!("http://{address}/v1")),
                model,
                runtime.handle().clone(),
            )
            .unwrap();
            provider.client = reqwest::Client::builder()
                .read_timeout(Duration::from_millis(500))
                .build()
                .unwrap();
            let response = provider
                .complete_stream(
                    &ModelTurnRequest::new(
                        "request",
                        vec![ModelMessage::text(ModelRole::User, "hello")],
                    ),
                    &CancellationToken::new(),
                    &mut |_| {},
                    &mut |_| Ok(()),
                )
                .unwrap_or_else(|error| panic!("{protocol:?}: {error}"));
            assert_eq!(
                response.assistant_message.content, "complete",
                "{protocol:?}"
            );
            let _ = release.send(());
            server.join().unwrap();
        }
    }

    /// 首块之前失败是普通的暂时故障：传输层不得把它标成不可重试；只有已经
    /// 发射可见增量之后的失败才被抑制。
    #[test]
    fn a_failure_before_any_visible_delta_stays_retryable() {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        let (address, release, server) = serve_incomplete_sse_body(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        );
        let mut provider = OpenAiProvider::new(
            test_config(format!("http://{address}/v1")),
            test_selection(ProviderApiProtocol::Chat),
            runtime.handle().clone(),
        )
        .unwrap();
        provider.client = reqwest::Client::builder()
            .read_timeout(Duration::from_millis(300))
            .build()
            .unwrap();
        let result = provider.complete_stream(
            &ModelTurnRequest::new(
                "request",
                vec![ModelMessage::text(ModelRole::User, "hello")],
            ),
            &CancellationToken::new(),
            &mut |_| {},
            &mut |_| Ok(()),
        );
        let _ = release.send(());
        server.join().unwrap();

        let Err(ProviderCallError::Provider(error)) = result else {
            panic!("expected a transport failure, got {result:?}");
        };
        assert_eq!(error.kind, crate::ModelErrorKind::Timeout);
        assert!(
            error.is_retryable(),
            "a failure before any visible output stays retryable"
        );
    }
}
