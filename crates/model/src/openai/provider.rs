//! OpenAI 兼容的提供方：按配置选择协议，发送一次流式请求并返回检查过的回复。
//!
//! 两种协议的编码和解码在各自的模块；可取消的网络等待和 SSE 分帧在 transport。

use tokio_util::sync::CancellationToken;

use crate::config::selection::{OpenAiProviderConfig, SelectedModel};
use crate::config::{ModelConfigManager, ModelConfigurationSnapshot};
use crate::error::{
    ProviderError, bounded_provider_error_diagnostic, parse_provider_error_body,
    provider_error_kind_for_code, provider_error_kind_for_http_status,
};
use crate::openai::{
    chat_completions_endpoint, openai_chat_stream_request_payload, openai_responses_stream_request_payload,
    read_chat_sse_stream, read_responses_sse_stream, responses_endpoint,
};
use crate::provider::ProviderStreamEvent;
use crate::provider::contract::ProviderApiProtocol;
use crate::transport::{provider_client, provider_future, retry_after_delay};
use crate::types::{ModelTurnRequest, ModelTurnResponse};

/// 按所选模型的协议发送 Chat Completions 或 Responses 流式请求。
pub struct OpenAiProvider {
    config: OpenAiProviderConfig,
    selected_model: SelectedModel,
    client: reqwest::Client,
}

impl OpenAiProvider {
    /// 读取当前模型配置，创建本轮使用的提供方。
    pub fn new(
        models: &ModelConfigManager,
        selection: &singularity_protocol::ModelSelection,
    ) -> Result<Self, ProviderError> {
        let (config, selected_model) = models.resolve(selection)?;
        Ok(Self {
            config,
            selected_model,
            client: provider_client()?,
        })
    }

    /// 本次所选模型的上下文窗口和最大输出，用于输出预算和压缩判断。
    pub fn model_configuration(&self) -> ModelConfigurationSnapshot {
        let selection = &self.selected_model;
        ModelConfigurationSnapshot {
            max_context_tokens: selection.max_context_tokens,
            max_output_tokens: selection.max_output_tokens,
        }
    }

    /// 提供方名称和模型名称，写进请求记录。
    pub fn model_identity(&self) -> (&str, &str) {
        (&self.config.provider_name, &self.selected_model.model_name)
    }

    /// 发送一次请求，返回回复和实际用量；请求记录、计时和重试由调用方负责。
    pub async fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    ) -> ModelTurnResponse {
        let selection = &self.selected_model;
        let provider_name = &self.config.provider_name;
        let (endpoint, payload) = match selection.api_protocol {
            ProviderApiProtocol::Chat => {
                let endpoint = chat_completions_endpoint(&self.config.base_url);
                let payload =
                    openai_chat_stream_request_payload(request, selection, provider_name, &endpoint);
                (endpoint, payload)
            }
            ProviderApiProtocol::Responses => {
                let endpoint = responses_endpoint(&self.config.base_url);
                let payload =
                    openai_responses_stream_request_payload(request, selection, provider_name, &endpoint);
                (endpoint, payload)
            }
        };
        let response = match self.send_request(&endpoint, &payload, cancellation).await {
            Ok(response) => response,
            Err(error) => {
                return ModelTurnResponse::failed(
                    error,
                    crate::Message::assistant(String::new(), String::new(), Vec::new(), None),
                );
            }
        };
        match selection.api_protocol {
            ProviderApiProtocol::Chat => {
                read_chat_sse_stream(cancellation, response, on_event, &self.config, selection, &endpoint)
                    .await
            }
            ProviderApiProtocol::Responses => {
                read_responses_sse_stream(
                    cancellation,
                    response,
                    on_event,
                    &self.config,
                    selection,
                    &endpoint,
                )
                .await
            }
        }
    }

    async fn send_request(
        &self,
        endpoint: &str,
        payload: &serde_json::Value,
        cancellation: &CancellationToken,
    ) -> Result<reqwest::Response, ProviderError> {
        let mut request = self.client.post(endpoint).json(payload);
        if !self.config.api_key.is_empty() {
            request = request.bearer_auth(&self.config.api_key);
        }
        let response = provider_future(cancellation, request.send()).await?;
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(self.read_http_failure(response, cancellation).await)
        }
    }

    async fn read_http_failure(
        &self,
        response: reqwest::Response,
        cancellation: &CancellationToken,
    ) -> ProviderError {
        let status_code = response.status().as_u16();
        let mut error = ProviderError::new(
            provider_error_kind_for_http_status(status_code),
            format!("Provider returned HTTP {status_code}."),
        )
        .with_retry_after(retry_after_delay(response.headers()));
        let body = match provider_future(cancellation, response.bytes()).await {
            Ok(body) => body,
            Err(read_error) if read_error.kind == crate::ModelErrorKind::Cancelled => return read_error,
            Err(read_error) => {
                error
                    .message
                    .push_str(&format!(" Could not read provider error response: {}", read_error.message));
                return error;
            }
        };
        let fields = parse_provider_error_body(&body);
        // 已知错误码比 HTTP 状态更准确，例如 400 里的上下文超限需要触发压缩。
        if let Some(kind) = provider_error_kind_for_code(fields.code.as_deref()) {
            error.kind = kind;
        }
        let facts = crate::error::provider_wire_facts(&fields);
        if !facts.is_empty() {
            error.message.push_str(&format!(" ({})", facts.join(", ")));
        }
        let diagnostic = match fields.message.as_deref() {
            Some(message) => bounded_provider_error_diagnostic(message),
            None => bounded_provider_error_diagnostic(&String::from_utf8_lossy(&body)),
        };
        if !diagnostic.is_empty() {
            error.message.push_str(&format!(" Provider diagnostic: {diagnostic}"));
        }
        error
    }
}
