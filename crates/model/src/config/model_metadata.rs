//! 模型目录的能力投影与补全；未知取值保持未知，不把 reasoning 布尔值当作档位列表。
use super::validate_identifier;
use serde_json::Value;
use singularity_protocol::{DiscoveredModel, ReasoningVariant};

fn fill(model: &mut DiscoveredModel, known: DiscoveredModel) {
    let before = model.clone();
    model.display_name = model.display_name.take().or(known.display_name);
    model.max_context_tokens = model.max_context_tokens.or(known.max_context_tokens);
    model.max_output_tokens = model.max_output_tokens.or(known.max_output_tokens);
    model.requires_reasoning_content_for_tool_calls = model
        .requires_reasoning_content_for_tool_calls
        .or(known.requires_reasoning_content_for_tool_calls);
    if model.reasoning_variants.is_empty() {
        model.reasoning_variants = known.reasoning_variants;
        model.default_variant = known.default_variant;
    }
    if *model != before {
        model.metadata_source = Some("models.dev · 提供方目录".into());
    }
}

/// 公共目录只按实际端点与精确模型 ID 提供推荐资料。
pub(super) fn supplement(models: &mut [DiscoveredModel], base_url: &str, directory: &Value) {
    let Some(providers) = directory.as_object() else {
        return;
    };
    let endpoint = crate::openai::api_root(base_url);
    let Some(provider) = providers.values().find(|provider| {
        provider
            .get("api")
            .and_then(Value::as_str)
            .is_some_and(|api| crate::openai::api_root(api) == endpoint)
    }) else {
        return;
    };
    for model in models {
        if let Some(entry) = provider
            .get("models")
            .and_then(|models| models.get(&model.model_id))
        {
            fill(model, metadata(&model.model_id, entry));
        }
    }
}

pub(super) fn metadata(id: &str, entry: &Value) -> DiscoveredModel {
    let capacity = |paths: &[&str]| {
        paths.iter().find_map(|path| {
            u32::try_from(entry.pointer(path)?.as_u64()?)
                .ok()
                .filter(|value| *value > 0)
        })
    };
    let mut efforts = Vec::new();
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
        // default 只是占位值，off 是本仓约定的关闭档位，两者都不作为可选档位。
        if !matches!(effort, "default" | "off")
            && validate_identifier(effort, "reasoning effort").is_ok()
            && !reasoning_variants
                .iter()
                .any(|variant: &ReasoningVariant| variant.id == effort)
        {
            reasoning_variants.push(ReasoningVariant {
                id: effort.to_string(),
                wire_effort: Some(effort.to_string()),
            });
        }
    }
    let default_variant = reasoning_variants
        .iter()
        .find(|variant| variant.id == "medium")
        .or_else(|| reasoning_variants.first())
        .map(|variant| variant.id.clone());
    DiscoveredModel {
        model_id: id.to_string(),
        display_name: entry
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
        max_context_tokens: capacity(&["/context_length", "/limit/context"]),
        max_output_tokens: capacity(&["/limit/output", "/top_provider/max_completion_tokens"]),
        reasoning_variants,
        default_variant,
        requires_reasoning_content_for_tool_calls: (entry
            .pointer("/interleaved/field")
            .and_then(Value::as_str)
            == Some("reasoning_content"))
        .then_some(true),
        metadata_source: None,
    }
}
