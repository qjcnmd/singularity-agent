//! 具体的 OpenAI-compatible provider：选协议、编排一次调用、终结响应。
//!
//! 两种协议的请求编码、SSE 解码和响应终结在各自的协议模块里；可取消的网络等待、
//! 有界读取和 SSE 帧切分由 transport 提供（本模块依赖它，它不依赖本模块）。

use std::fmt;

use singularity_core::duration_millis;
use tokio_util::sync::CancellationToken;

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
use crate::provider::contract::ProviderApiProtocol;
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
    /// 创建并校验 OpenAI-compatible provider；异步执行一律用调用方注入的 runtime，
    /// 读取超时固定为 PROVIDER_TIMEOUT_SECONDS。
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

    /// 用一份冻结的配置快照和 selector 建立执行客户端。快照只提供配置事实，执行环境
    /// （Tokio handle）由装配层显式传入，所以配置对象既不带句柄也不创建网络对象。
    pub fn from_snapshot(
        snapshot: &ProviderConfigSnapshot,
        selector: Option<&str>,
        runtime_handle: tokio::runtime::Handle,
    ) -> Result<Self, ProviderError> {
        let (config, selected_model) = snapshot.resolve(selector)?;
        Self::new(config, selected_model, runtime_handle)
    }

    /// 在编码边界上校验私有续接：身份与当前 provider、模型、协议匹配的续接必须和它附着
    /// 的 assistant 消息一致，否则本次请求失败；身份不匹配的由编码器按身份规则略过，公开
    /// 消息照常发送。校验对象只可能是本 Provider 当前选择的那份身份，不接受第二份可能与
    /// self.config 不一致的 selection。
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
    /// 执行一次流式 HTTP attempt；响应校验通过后才记录成功终态。
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
        // 一次 attempt 里两种协议的差异只有两处：端点和 payload，都由协议模块提供。
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
        let mut first_token_at = None;
        let mut timed_event = |event| {
            first_token_at.get_or_insert_with(std::time::Instant::now);
            on_event(event);
        };
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
                self.read_streamed_response(cancellation, response, &mut timed_event)
            }
            Ok(response) => Err(self.read_http_failure(response, cancellation)),
            Err(error) => Err(error),
        };

        // 即使响应随后因缺少续接材料被拒绝，它上报的用量也已经产生了。
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
        let mut occurrence = ProviderAttemptOccurrence::finished(
            started,
            duration_millis(started_at.elapsed()),
            usage,
            completion.as_ref().err(),
        );
        occurrence.decode_ms = first_token_at.map(|first| duration_millis(first.elapsed()));
        record_attempt(ProviderAttemptEvent::Finished(Box::new(occurrence)))?;
        completion.map_err(Into::into)
    }

    /// 按本次选择分派到协议模块读取流式响应；解码和终结都在那里完成。
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
        // 分类不改变原始的协议事实：按 wire 错误码分类会重写 kind/code，此时 HTTP 状态
        // 和 wire code/type 仍以有界诊断保留；没命中错误码分类时，状态已经写在消息里，
        // 不重复。
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

/// 对真实响应检查续接是否完整；有没有续接材料由解析结果本身决定，不另存存在性标志。
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

    /// 一次完成的唯一编排入口：请求归一、能力校验、wire 协议选择和 tool-reasoning
    /// 契约校验都只在这里实现，所有模型调用都走流式解码。
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
        self.validate_reasoning_history(request)?;
        let response = self.complete_attempt(request, cancellation, on_event, record_attempt)?;
        Ok(response)
    }
}
