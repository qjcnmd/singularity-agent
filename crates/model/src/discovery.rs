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
    parse_discovery_payload(&payload)
}

/// Parse a `/models` JSON payload into validated model ids.
///
/// 响应级缺陷（缺 `data` 数组、条目数超限、全部条目无效）fail closed；
/// 单个坏条目（缺 id、id 非法、重复 id）只被跳过，不拖垮整个发现结果。
pub(crate) fn parse_discovery_payload(payload: &Value) -> Result<Vec<String>, ProviderError> {
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
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty()
            || id.chars().count() > MAX_MODEL_ID_LENGTH
            || id
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
        {
            continue;
        }
        if !seen_ids.insert(id) {
            continue;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(entries: Value) -> Value {
        serde_json::json!({ "data": entries })
    }

    #[test]
    fn malformed_entries_are_skipped_without_failing_discovery() {
        let parsed = parse_discovery_payload(&payload(serde_json::json!([
            { "id": "gpt-valid" },
            {},
            { "id": 42 },
            { "id": "" },
            { "id": "has space" },
            { "id": "gpt-valid" },
            { "other": true },
            { "id": "gpt-also-valid" }
        ])))
        .expect("valid entries survive malformed siblings");

        assert_eq!(
            parsed,
            vec!["gpt-valid".to_string(), "gpt-also-valid".to_string()]
        );
    }

    #[test]
    fn response_level_defects_still_fail_closed() {
        let error = parse_discovery_payload(&serde_json::json!({ "models": [] }))
            .expect_err("missing data array must fail");
        assert_eq!(
            error.error.code.as_deref(),
            Some("provider_models_schema_invalid")
        );

        let all_malformed = parse_discovery_payload(&payload(serde_json::json!([
            {}, { "id": "" }
        ])))
        .expect_err("a response with no usable entry must fail");
        assert_eq!(
            all_malformed.error.code.as_deref(),
            Some("provider_models_empty")
        );
    }
}
