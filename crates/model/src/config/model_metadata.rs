//! 模型目录的能力投影与补全；未知取值保持未知，不把 reasoning 布尔值当作档位列表。
use super::validate_identifier;
use serde_json::Value;
use singularity_protocol::{DiscoveredModel, ReasoningVariant};

fn modalities(value: Option<&Value>) -> Option<Vec<String>> {
    let values = value?.as_array()?;
    let parsed: Option<Vec<_>> = values
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect();
    parsed.filter(|values| {
        !values.is_empty()
            && values
                .iter()
                .all(|value| singularity_protocol::MODEL_MODALITIES.contains(&value.as_str()))
    })
}

fn fill(model: &mut DiscoveredModel, known: DiscoveredModel, source: &str) {
    let before = model.clone();
    model.display_name = model.display_name.take().or(known.display_name);
    model.max_context_tokens = model.max_context_tokens.or(known.max_context_tokens);
    model.max_output_tokens = model.max_output_tokens.or(known.max_output_tokens);
    model.input_modalities = model.input_modalities.take().or(known.input_modalities);
    model.output_modalities = model.output_modalities.take().or(known.output_modalities);
    model.thinking_wire_format = model
        .thinking_wire_format
        .take()
        .or(known.thinking_wire_format);
    model.chat_output_tokens_field = model
        .chat_output_tokens_field
        .take()
        .or(known.chat_output_tokens_field);
    model.requires_reasoning_content_for_tool_calls = model
        .requires_reasoning_content_for_tool_calls
        .or(known.requires_reasoning_content_for_tool_calls);
    if model.reasoning_variants.is_empty() {
        model.reasoning_variants = known.reasoning_variants;
        model.default_variant = known.default_variant;
    }
    if *model != before {
        model.metadata_source = Some(match model.metadata_source.take() {
            Some(previous) if previous != source => format!("{previous} + {source}"),
            _ => source.to_string(),
        });
    }
}

/// 先匹配实际端点；聚合网关没有目录条目时，按原厂及精确模型 ID 补充能力。
/// 不从其他网关同名模型复制档位：网关可能重新解释或限制推理参数。
pub(super) fn supplement(models: &mut [DiscoveredModel], base_url: &str, directory: &Value) {
    let Some(providers) = directory.as_object() else {
        return;
    };
    let endpoint = crate::openai::api_root(base_url);
    let matched = providers.values().find(|provider| {
        provider
            .get("api")
            .and_then(Value::as_str)
            .is_some_and(|api| crate::openai::api_root(api) == endpoint)
    });
    for model in models {
        let direct = matched.and_then(|provider| provider.get("models")?.get(&model.model_id));
        if let Some(entry) = direct {
            fill(
                model,
                metadata(&model.model_id, entry),
                "models.dev · 提供方目录",
            );
            continue;
        }
        let id = model.model_id.rsplit('/').next().unwrap_or(&model.model_id);
        let manufacturer = if id.starts_with("mimo-") {
            "xiaomi"
        } else if id.starts_with("deepseek-") {
            "deepseek"
        } else if id.starts_with("gpt-")
            || id.starts_with("o1")
            || id.starts_with("o3")
            || id.starts_with("o4")
        {
            "openai"
        } else if id.starts_with("qwen") {
            "alibaba"
        } else if id.starts_with("glm-") {
            "zai"
        } else if id.starts_with("kimi-") || id.starts_with("moonshot-") {
            "moonshotai"
        } else if id.starts_with("claude-") {
            "anthropic"
        } else if id.starts_with("gemini-") {
            "google"
        } else if id.to_lowercase().starts_with("minimax-") {
            "minimax"
        } else {
            continue;
        };
        let entry = providers
            .get(manufacturer)
            .and_then(|provider| provider.get("models")?.get(id));
        if let Some(entry) = entry {
            let mut known = metadata(&model.model_id, entry);
            known.reasoning_variants.clear();
            known.default_variant = None;
            known.thinking_wire_format = None;
            fill(model, known, "models.dev · 原厂能力");
        }
    }
}

/// 目录未表达的线上参数采用已核实的端点规则。
/// 来源：https://docs.b.ai/llmservice/models/mimo-v2.6-flash/
/// https://mimo.mi.com/docs/en-US/api/chat/openai-api
pub(super) fn supplement_documented(models: &mut [DiscoveredModel], base_url: &str) {
    let endpoint = crate::openai::api_root(base_url);
    for model in models {
        let bai = endpoint == "https://api.b.ai/v1";
        let mimo = endpoint == "https://api.xiaomimimo.com/v1" || bai;
        if bai && model.model_id == "deepseek-v4.1-flash" {
            // https://docs.b.ai/llmservice/models/deepseek-v4-1-flash/
            // https://api-docs.deepseek.com/guides/thinking_mode/
            let known = DiscoveredModel {
                model_id: model.model_id.clone(),
                display_name: Some("DeepSeek V4.1 Flash".into()),
                max_context_tokens: Some(1048576),
                max_output_tokens: Some(393216),
                input_modalities: Some(vec!["text".into(), "image".into()]),
                output_modalities: Some(vec!["text".into()]),
                reasoning_variants: ["off", "low", "high", "max"]
                    .map(|id| ReasoningVariant {
                        id: id.into(),
                        wire_effort: (id != "off").then(|| id.into()),
                    })
                    .into(),
                default_variant: Some("high".into()),
                thinking_wire_format: Some("thinking_type".into()),
                chat_output_tokens_field: Some("max_tokens".into()),
                requires_reasoning_content_for_tool_calls: Some(true),
                metadata_source: None,
            };
            fill(model, known, "DeepSeek / B.AI 官方文档");
            continue;
        }
        let dashscope = matches!(
            endpoint,
            "https://dashscope.aliyuncs.com/compatible-mode/v1"
                | "https://dashscope-intl.aliyuncs.com/compatible-mode/v1"
        );
        if (bai || dashscope) && model.model_id == "qwen3.8-flash" {
            // https://help.aliyun.com/zh/model-studio/deep-thinking
            // https://docs.b.ai/llmservice/models/qwen3-8-flash/
            let known = DiscoveredModel {
                model_id: model.model_id.clone(),
                display_name: Some("Qwen3.8 Flash".into()),
                max_context_tokens: Some(1000000),
                max_output_tokens: Some(131072),
                input_modalities: Some(vec!["text".into(), "image".into(), "video".into()]),
                output_modalities: Some(vec!["text".into()]),
                reasoning_variants: thinking_switch(),
                default_variant: Some("on".into()),
                thinking_wire_format: Some("enable_thinking".into()),
                chat_output_tokens_field: Some("max_completion_tokens".into()),
                requires_reasoning_content_for_tool_calls: None,
                metadata_source: None,
            };
            fill(model, known, "Qwen / B.AI 官方文档");
            continue;
        }
        if !mimo || !matches!(model.model_id.as_str(), "mimo-v2.6-flash" | "mimo-v2.6-pro") {
            continue;
        }
        let known = DiscoveredModel {
            model_id: model.model_id.clone(),
            display_name: Some(
                if model.model_id.ends_with("flash") {
                    "MiMo V2.6 Flash"
                } else {
                    "MiMo V2.6 Pro"
                }
                .into(),
            ),
            max_context_tokens: Some(1048576),
            max_output_tokens: Some(131072),
            input_modalities: Some(vec![
                "text".into(),
                "image".into(),
                "audio".into(),
                "video".into(),
            ]),
            output_modalities: Some(vec!["text".into()]),
            reasoning_variants: thinking_switch(),
            default_variant: Some("on".into()),
            thinking_wire_format: Some("thinking_type".into()),
            chat_output_tokens_field: Some("max_completion_tokens".into()),
            requires_reasoning_content_for_tool_calls: Some(true),
            metadata_source: None,
        };
        fill(model, known, "MiMo / B.AI 官方文档");
    }
}

fn thinking_switch() -> Vec<ReasoningVariant> {
    ["off", "on"]
        .map(|id| ReasoningVariant {
            id: id.into(),
            wire_effort: None,
        })
        .into()
}

pub(super) fn metadata(id: &str, entry: &Value) -> DiscoveredModel {
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
    // 优先用对方声明的 default，其次 medium；导入配置时也按这个默认值。
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
        thinking_wire_format: label(&["/thinking_wire_format"]).map(str::to_string),
        chat_output_tokens_field: label(&["/chat_output_tokens_field"]).map(str::to_string),
        input_modalities: modalities(entry.pointer("/modalities/input")),
        output_modalities: modalities(entry.pointer("/modalities/output")),
        requires_reasoning_content_for_tool_calls: entry
            .get("requires_reasoning_content_for_tool_calls")
            .and_then(Value::as_bool)
            .or_else(|| {
                (entry.pointer("/interleaved/field").and_then(Value::as_str)
                    == Some("reasoning_content"))
                .then_some(true)
            }),
        metadata_source: None,
    }
}
