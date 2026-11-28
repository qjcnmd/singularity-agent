use std::fmt::Write as _;

use protocol::ProviderApiProtocol;
use tokio_util::sync::CancellationToken;

use crate::config::selection::{ProviderConfig, SelectedModel};
use crate::config::{ModelConfigManager, ModelConfigurationSnapshot};
use crate::error::{ProviderError, apply_provider_error_body, provider_error_kind_for_http_status};
use crate::messages::{messages_stream_request_payload, read_messages_sse_stream};
use crate::provider::{ProviderStreamEvent, endpoint};
use crate::responses::{openai_responses_stream_request_payload, read_responses_sse_stream};
use crate::transport::http::{
    BoundedBodyReadError, MAX_HTTP_ERROR_BODY_BYTES, apply_provider_auth, provider_cancelled_error,
    provider_client, provider_future, provider_transport_error, read_bounded_body, retry_after_delay,
};
use crate::types::{ModelTurnRequest, ModelTurnResponse};

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
    pub fn new(
        models: &ModelConfigManager,
        selection: &protocol::ModelSelection,
    ) -> Result<Self, ProviderError> {
        let (config, selected_model) = models.resolve(selection)?;
        let client = provider_client()?;
        Ok(Self { config, selected_model, client })
    }

    pub fn model_configuration(&self) -> ModelConfigurationSnapshot {
        ModelConfigurationSnapshot {
            max_context_tokens: self.selected_model.max_context_tokens,
            max_output_tokens: self.selected_model.max_output_tokens,
        }
    }

    pub fn provider_name(&self) -> &str {
        &self.config.provider_name
    }

    pub fn model_name(&self) -> &str {
        &self.selected_model.model_name
    }

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
                let endpoint = endpoint(&self.config.base_url, "/messages");
                messages_stream_request_payload(request, selection, provider_name, &endpoint)
                    .map(|enc| (endpoint, enc.payload, Some(enc.prefix_digest)))
            }
            ProviderApiProtocol::Responses => {
                let endpoint = endpoint(&self.config.base_url, "/responses");
                openai_responses_stream_request_payload(request, selection, provider_name, &endpoint)
                    .map(|payload| (endpoint, payload, None))
            }
        };
        let (endpoint, payload, prefix_digest) = match encoded {
            Ok(encoded) => encoded,
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
                read_messages_sse_stream(
                    cancellation,
                    response,
                    on_event,
                    &self.config,
                    selection,
                    &endpoint,
                    prefix_digest,
                )
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
        let request = apply_provider_auth(
            self.client.post(endpoint).json(payload),
            self.selected_model.protocol,
            &self.config.api_key,
        )?;
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
        );
        error.retry_after = retry_after_delay(response.headers());
        let body = match read_bounded_body(response, MAX_HTTP_ERROR_BODY_BYTES, Some(cancellation)).await {
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
        apply_provider_error_body(&mut error, &body, &self.config.api_key);
        error
    }
}
