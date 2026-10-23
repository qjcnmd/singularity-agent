//! 解析提供方返回的模型能力；未知取值保持未知，不把 reasoning 布尔值当作档位列表。
use super::validate_identifier;
use serde_json::Value;
use singularity_protocol::DiscoveredModel;

pub(super) fn metadata(id: &str, entry: &Value) -> DiscoveredModel {
    let capacity = |paths: &[&str]| {
        paths
            .iter()
            .find_map(|path| u32::try_from(entry.pointer(path)?.as_u64()?).ok().filter(|value| *value > 0))
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
            && !reasoning_variants.iter().any(|variant: &String| variant == effort)
        {
            reasoning_variants.push(effort.to_string());
        }
    }
    DiscoveredModel {
        model_id: id.to_string(),
        max_context_tokens: capacity(&["/context_length", "/limit/context"]),
        max_output_tokens: capacity(&["/limit/output", "/top_provider/max_completion_tokens"]),
        reasoning_variants,
    }
}
