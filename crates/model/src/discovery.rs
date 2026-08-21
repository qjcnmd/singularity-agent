//! 模型发现逻辑与 `/models` 端点查询。

use serde_json::Value;
use singularity_core::CancellationToken;
use std::collections::HashSet;

use crate::error::{ModelError, ModelErrorKind, ProviderError, ProviderErrorStage};
use crate::openai::models_endpoint;
use crate::provider::runtime::OpenAiProviderConfig;
use crate::transport::http::{
    block_on_provider_future, model_error_from_http_status, read_bounded_provider_response_body,
};
use crate::{MAX_DISCOVERED_MODEL_IDS, MAX_DISCOVERY_RESPONSE_BYTES, MAX_MODEL_ID_LENGTH};

/// Discover public model ids from the provider's standard `/models` endpoint using a client and runtime.
pub(crate) fn discover_provider_models(
    config: &OpenAiProviderConfig,
    client: &reqwest::Client,
    runtime: &tokio::runtime::Handle,
    request_timeout_seconds: u64,
) -> Result<Vec<String>, ProviderError> {
    let endpoint = models_endpoint(&config.base_url);
    let cancellation = CancellationToken::new();
    let response = block_on_provider_future(
        runtime,
        &cancellation,
        "provider_models_request_failed",
        ProviderErrorStage::RequestSend,
        request_timeout_seconds,
        || client.get(&endpoint).bearer_auth(&config.api_key).send(),
    )?;
    let status = response.status();
    if !status.is_success() {
        return Err(ProviderError::from_model_error(
            model_error_from_http_status(status.as_u16(), &config.provider_name, "models"),
        ));
    }
    let body = read_bounded_provider_response_body(
        runtime,
        &cancellation,
        request_timeout_seconds,
        response,
    )?;
    if body.len() > MAX_DISCOVERY_RESPONSE_BYTES {
        return Err(ProviderError::from_model_error(
            ModelError::new(
                ModelErrorKind::JsonSchemaViolation,
                "provider response body exceeded maximum byte limit",
            )
            .with_provider_diagnostic(
                "provider_response_body_too_large",
                ProviderErrorStage::ResponseBodyRead,
            ),
        ));
    }
    let payload: Value = serde_json::from_slice(&body).map_err(|_| {
        ProviderError::from_model_error(
            ModelError::new(
                ModelErrorKind::JsonSchemaViolation,
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
                ModelError::new(
                    ModelErrorKind::JsonSchemaViolation,
                    "provider models response did not contain a data array",
                )
                .with_provider_diagnostic(
                    "provider_models_schema_invalid",
                    ProviderErrorStage::ResponseValidation,
                ),
            )
        })?;
    if data.len() > MAX_DISCOVERED_MODEL_IDS {
        return Err(ProviderError::from_model_error(
            ModelError::new(
                ModelErrorKind::JsonSchemaViolation,
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
                ModelError::new(
                    ModelErrorKind::JsonSchemaViolation,
                    "provider models response contained an entry without a model id",
                )
                .with_provider_diagnostic(
                    "provider_models_schema_invalid",
                    ProviderErrorStage::ResponseValidation,
                ),
            )
        })?;
        if id.is_empty()
            || id.chars().count() > MAX_MODEL_ID_LENGTH
            || id
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(ProviderError::from_model_error(
                ModelError::new(
                    ModelErrorKind::JsonSchemaViolation,
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
                ModelError::new(
                    ModelErrorKind::JsonSchemaViolation,
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
            ModelError::new(
                ModelErrorKind::JsonSchemaViolation,
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
