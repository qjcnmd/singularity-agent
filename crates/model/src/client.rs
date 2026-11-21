//! 协议无关的模型客户端入口：按提供方协议分发到 Messages 或 Responses 编码器与流解码器。

use std::fmt::Write as _;

use protocol::ProviderApiProtocol;
use tokio_util::sync::CancellationToken;

use crate::ANTHROPIC_VERSION;
use crate::config::selection::{ProviderConfig, SelectedModel};
use crate::config::{ModelConfigManager, ModelConfigurationSnapshot};
use crate::error::{
    ProviderError, append_provider_http_diagnostic, parse_provider_error_body,
    provider_error_kind_for_fields, provider_error_kind_for_http_status,
};
use crate::messages::{messages_stream_request_payload, read_messages_sse_stream};
use crate::provider::{ProviderStreamEvent, messages_endpoint, responses_endpoint};
use crate::responses::{openai_responses_stream_request_payload, read_responses_sse_stream};
use crate::transport::http::{
    BoundedBodyReadError, MAX_HTTP_ERROR_BODY_BYTES, provider_cancelled_error, provider_transport_error,
    read_bounded_error_body, sensitive_header_value,
};
use crate::transport::{provider_client, provider_future, retry_after_delay};
use crate::types::{ModelTurnRequest, ModelTurnResponse};

/// 调用方（`agent` / `app`）唯一面向的模型客户端抽象。
pub struct ModelClient {
    config: ProviderConfig,
    selected_model: SelectedModel,
    client: reqwest::Client,
}

impl std::fmt::Debug for ModelClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelClient")
            .field("provider_name", &self.config.provider_name)
            .field("base_url", &self.config.base_url)
            .field("api_key", &"[redacted]")
            .field("model_name", &self.selected_model.model_name)
            .field("api_protocol", &self.selected_model.protocol)
            .finish_non_exhaustive()
    }
}

impl ModelClient {
    /// 读取当前模型配置并构造本轮使用的协议客户端。
    pub fn new(
        models: &ModelConfigManager,
        selection: &protocol::ModelSelection,
    ) -> Result<Self, ProviderError> {
        let (config, selected_model) = models.resolve(selection)?;
        let client = provider_client()?;
        Ok(Self { config, selected_model, client })
    }

    /// 本次所选模型的上下文窗口和最大输出，用于输出预算和轮换判断。
    pub fn model_configuration(&self) -> ModelConfigurationSnapshot {
        ModelConfigurationSnapshot {
            max_context_tokens: self.selected_model.max_context_tokens,
            max_output_tokens: self.selected_model.max_output_tokens,
        }
    }

    /// 提供方名称，写进请求记录与诊断日志。
    pub fn provider_name(&self) -> &str {
        &self.config.provider_name
    }

    /// 模型名称，写进请求记录与诊断日志。
    pub fn model_name(&self) -> &str {
        &self.selected_model.model_name
    }

    /// 发送一次流式请求并返回已校验的回复和实际用量；请求记录、计时和重试由调用方负责。
    pub async fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    ) -> ModelTurnResponse {
        let selection = &self.selected_model;
        let provider_name = &self.config.provider_name;
        let encoded = match selection.protocol {
            ProviderApiProtocol::Messages => {
                let endpoint = messages_endpoint(&self.config.base_url);
                messages_stream_request_payload(request, selection, provider_name, &endpoint)
                    .map(|payload| (endpoint, payload))
            }
            ProviderApiProtocol::Responses => {
                let endpoint = responses_endpoint(&self.config.base_url);
                openai_responses_stream_request_payload(request, selection, provider_name, &endpoint)
                    .map(|payload| (endpoint, payload))
            }
        };
        let (endpoint, payload) = match encoded {
            Ok(pair) => pair,
            Err(error) => {
                return ModelTurnResponse::failed(
                    error,
                    crate::Message::assistant(String::new(), String::new(), Vec::new(), None),
                );
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
        match selection.protocol {
            ProviderApiProtocol::Messages => {
                read_messages_sse_stream(cancellation, response, on_event, &self.config, selection, &endpoint)
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
        match self.selected_model.protocol {
            ProviderApiProtocol::Messages => {
                request = request.header("anthropic-version", ANTHROPIC_VERSION);
                if !self.config.api_key.is_empty() {
                    request = request.header("x-api-key", sensitive_header_value(&self.config.api_key)?);
                }
            }
            ProviderApiProtocol::Responses => {
                if !self.config.api_key.is_empty() {
                    request = request.bearer_auth(&self.config.api_key);
                }
            }
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
        let body = match read_bounded_error_body(response, MAX_HTTP_ERROR_BODY_BYTES, cancellation).await {
            Ok(body) => body,
            Err(BoundedBodyReadError::Cancelled) => return provider_cancelled_error(),
            Err(BoundedBodyReadError::Transport(read_error)) => {
                let _ = write!(
                    error.message,
                    " Could not read provider error response: {}",
                    provider_transport_error(read_error).message
                );
                return error;
            }
        };
        let fields = parse_provider_error_body(&body);
        // 已知错误码或错误类型比 HTTP 状态更准确，例如 400 里的上下文超限需要触发轮换。
        if let Some(kind) = provider_error_kind_for_fields(&fields) {
            error.kind = kind;
        }
        append_provider_http_diagnostic(&mut error.message, &fields, &body);
        error
    }
}
