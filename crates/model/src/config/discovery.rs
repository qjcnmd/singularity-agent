//! Read-only model metadata for the configuration editor. Saved config remains authoritative.

use std::collections::BTreeMap;

use serde_json::Value;
use singularity_protocol::{DiscoveredModel, ReasoningVariantInput};

use super::{ProviderError, user_config_error, validate_identifier, validate_model_id};

pub(super) async fn discover(
    request: reqwest::RequestBuilder,
    base_url: &str,
) -> Result<Vec<DiscoveredModel>, ProviderError> {
    let response = request.send().await.map_err(|_| {
        user_config_error("无法连接模型目录，请检查 API 地址或稍后重试；仍可手动添加模型。")
    })?;
    if !response.status().is_success() {
        return Err(user_config_error(format!(
            "获取模型失败：HTTP {}。仍可手动添加模型。",
            response.status().as_u16()
        )));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| user_config_error("提供方未返回有效的模型目录。仍可手动添加模型。"))?;
    let mut models = read_listing(&body)?;
    if models.iter().any(|model| {
        model.max_context_tokens.is_none()
            || model.max_output_tokens.is_none()
            || model.reasoning_variants.is_empty()
    }) {
        // This public directory request carries neither the user's endpoint nor credentials.
        if let Ok(client) = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .build()
            && let Ok(response) = client.get("https://models.dev/api.json").send().await
            && response.status().is_success()
            && let Ok(directory) = response.json::<Value>().await
        {
            supplement(&mut models, base_url, &directory);
        }
    }
    Ok(models)
}

fn read_listing(body: &Value) -> Result<Vec<DiscoveredModel>, ProviderError> {
    let entries: Vec<(&str, &Value)> =
        if let Some(data) = body.get("data").and_then(Value::as_array) {
            data.iter()
                .filter_map(|entry| Some((entry.get("id")?.as_str()?, entry)))
                .collect()
        } else if let Some(models) = body.get("models").and_then(Value::as_object) {
            models
                .iter()
                .map(|(id, entry)| (id.as_str(), entry))
                .collect()
        } else {
            return Err(user_config_error(
                "提供方未返回 data 模型列表或 models 目录。仍可手动添加模型。",
            ));
        };
    let mut models = BTreeMap::new();
    for (id, entry) in entries {
        if entry.is_object() && validate_model_id(id, "model id").is_ok() {
            models
                .entry(id.to_string())
                .or_insert_with(|| metadata(id, entry));
        }
    }
    Ok(models.into_values().collect())
}

fn metadata(id: &str, entry: &Value) -> DiscoveredModel {
    let label = |paths: &[&str]| {
        paths.iter().find_map(|path| {
            entry
                .pointer(path)?
                .as_str()
                .filter(|text| !text.is_empty())
        })
    };
    let capacity = |paths: &[&str]| {
        paths.iter().find_map(|path| {
            u32::try_from(entry.pointer(path)?.as_u64()?)
                .ok()
                .filter(|value| *value > 0)
        })
    };
    let mut efforts = Vec::new();
    if let Some(values) = entry.get("reasoning_efforts").and_then(Value::as_array) {
        efforts.extend(values.iter().filter_map(Value::as_str));
    }
    if let Some(options) = entry.get("reasoning_options").and_then(Value::as_array) {
        for option in options {
            if option.get("type").and_then(Value::as_str) == Some("effort")
                && let Some(values) = option.get("values").and_then(Value::as_array)
            {
                efforts.extend(values.iter().filter_map(Value::as_str));
            }
        }
    }
    let mut reasoning_variants = Vec::new();
    for effort in efforts {
        if !matches!(effort, "default" | "off")
            && validate_identifier(effort, "reasoning effort").is_ok()
            && !reasoning_variants
                .iter()
                .any(|variant: &ReasoningVariantInput| variant.id == effort)
        {
            reasoning_variants.push(ReasoningVariantInput {
                id: effort.to_string(),
                enabled: true,
                wire_effort: Some(effort.to_string()),
            });
        }
    }
    // Prefer an advertised default, then medium; this is the imported configuration's default.
    let default_variant = label(&["/default_reasoning_effort"])
        .filter(|id| reasoning_variants.iter().any(|variant| variant.id == *id))
        .or_else(|| {
            reasoning_variants
                .iter()
                .find(|variant| variant.id == "medium")
                .map(|variant| variant.id.as_str())
        })
        .or_else(|| {
            reasoning_variants
                .first()
                .map(|variant| variant.id.as_str())
        })
        .map(str::to_string);
    let thinking_wire_format =
        (!reasoning_variants.is_empty()).then(|| "reasoning_effort".to_string());
    DiscoveredModel {
        model_id: id.to_string(),
        display_name: label(&["/name", "/display_name", "/displayName"]).map(str::to_string),
        max_context_tokens: capacity(&[
            "/context_window",
            "/context_length",
            "/contextWindow",
            "/max_input_tokens",
            "/limit/context",
        ]),
        max_output_tokens: capacity(&[
            "/max_output_tokens",
            "/maxOutputTokens",
            "/max_tokens",
            "/maxTokens",
            "/limit/output",
            "/top_provider/max_completion_tokens",
        ]),
        reasoning_variants,
        default_variant,
        thinking_wire_format,
    }
}

fn supplement(models: &mut [DiscoveredModel], base_url: &str, directory: &Value) {
    let Some(providers) = directory.as_object() else {
        return;
    };
    let endpoint = base_url.trim_end_matches('/');
    let Some((provider_id, provider)) = providers.iter().find(|(id, provider)| {
        let api = provider
            .get("api")
            .and_then(Value::as_str)
            .or(match id.as_str() {
                "openai" => Some(crate::catalog::OPENAI_BASE_URL),
                "deepseek" => Some(crate::catalog::DEEPSEEK_BASE_URL),
                _ => None,
            });
        api.is_some_and(|api| api.trim_end_matches('/') == endpoint)
    }) else {
        return;
    };
    for model in models {
        let Some(entry) = provider
            .get("models")
            .and_then(|models| models.get(&model.model_id))
        else {
            continue;
        };
        let known = metadata(&model.model_id, entry);
        model.display_name = model.display_name.take().or(known.display_name);
        model.max_context_tokens = model.max_context_tokens.or(known.max_context_tokens);
        model.max_output_tokens = model.max_output_tokens.or(known.max_output_tokens);
        if model.reasoning_variants.is_empty() && !known.reasoning_variants.is_empty() {
            model.reasoning_variants = known.reasoning_variants;
            model.default_variant = known.default_variant;
            model.thinking_wire_format = Some(
                if provider_id.starts_with("alibaba") {
                    "enable_thinking"
                } else if provider_id == "deepseek" {
                    "thinking_type"
                } else {
                    "reasoning_effort"
                }
                .to_string(),
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixture assertions follow the owning module's convention.
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn listing_preserves_aliases_and_only_advertised_efforts() {
        let models = read_listing(&json!({"models": {
            "gateway-alias": {"id": "canonical-id", "limit": {"context": 128000, "output": 8192}, "reasoning_options": [{"type": "effort", "values": ["low", "xhigh", "low", null, "default"]}]},
            "unknown": {"reasoning": true}, "version": 1
        }})).expect("valid model listing");
        assert_eq!(models[0].model_id, "gateway-alias");
        assert_eq!(models[0].max_context_tokens, Some(128000));
        assert_eq!(
            models[0]
                .reasoning_variants
                .iter()
                .map(|variant| variant.id.as_str())
                .collect::<Vec<_>>(),
            ["low", "xhigh"]
        );
        assert!(models[1].reasoning_variants.is_empty());
    }

    #[test]
    fn supplement_requires_exact_route_and_preserves_endpoint_facts() {
        let directory = json!({"alibaba-cn": {"api": "https://dashscope.aliyuncs.com/compatible-mode/v1", "models": {
            "example": {"limit": {"context": 1000000}, "reasoning_options": [{"type": "effort", "values": ["low", "medium", "xhigh"]}]}
        }}});
        let mut models = read_listing(
            &json!({"data": [{"id": "example", "context_length": 64000}, {"id": "example-alias"}]}),
        )
        .expect("valid model listing");
        supplement(&mut models, "https://proxy.invalid/v1", &directory);
        assert!(models[0].reasoning_variants.is_empty());
        supplement(
            &mut models,
            "https://dashscope.aliyuncs.com/compatible-mode/v1/",
            &directory,
        );
        assert_eq!(models[0].max_context_tokens, Some(64000));
        assert_eq!(models[0].default_variant.as_deref(), Some("medium"));
        assert_eq!(
            models[0].thinking_wire_format.as_deref(),
            Some("enable_thinking")
        );
        assert!(models[1].reasoning_variants.is_empty());
    }
}
