//! Provider/model selector parsing and selection seam.
//!
//! The implementation remains owned by the parent configuration module so
//! secret-bearing snapshots and validation share one authority. This module
//! exposes only the narrow selection-facing seam used by sibling code.

use super::*;

pub(crate) fn model_selector_error(
    message: impl Into<String>,
    code: &'static str,
) -> ProviderError {
    super::configuration_error(message, code)
}

pub(super) struct ParsedModelSelector<'a> {
    pub(super) provider_name: &'a str,
    pub(super) model_name: &'a str,
    pub(super) reasoning_effort: Option<&'a str>,
}

/// 模型选择器各段：`provider/model#effort`。宽松拆分时任一段都可能缺省，
/// 不在此处校验合法性（校验由 `parse_model_selector` 与上游配置层负责）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelectorParts<'a> {
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
}

/// 宽松拆分 `provider/model#effort` 选择器：分隔符为 `/` 与 `#`，`#` 优先于 `/`
/// 拆分 effort。缺省字段在对应位返回 `None`，空字符串视为缺省。
pub fn split_model_selector(selector: &str) -> ModelSelectorParts<'_> {
    let (without_effort, effort) = selector
        .rsplit_once('#')
        .map_or((selector, None), |(model, effort)| (model, Some(effort)));
    let (provider, model) = without_effort
        .split_once('/')
        .map_or((None, without_effort), |(provider, model)| {
            (Some(provider), model)
        });
    ModelSelectorParts {
        provider: provider.filter(|value| !value.is_empty()),
        model: Some(model).filter(|value| !value.is_empty()),
        effort: effort.filter(|value| !value.is_empty()),
    }
}

pub(super) fn parse_model_selector(
    selector: &str,
) -> Result<ParsedModelSelector<'_>, ProviderError> {
    let Some((provider_name, model_and_effort)) = selector.split_once('/') else {
        return Err(model_selector_error(
            "model selector must use provider_id/model_id[#variant]",
            "provider_selector_invalid",
        ));
    };
    let (model_name, reasoning_effort) = match model_and_effort.rsplit_once('#') {
        Some((model_name, reasoning_effort)) => (model_name, Some(reasoning_effort)),
        None => (model_and_effort, None),
    };
    super::validate_provider_identifier(provider_name, "provider id").map_err(|_| {
        model_selector_error(
            "model selector must contain a valid provider id",
            "provider_selector_invalid",
        )
    })?;
    super::validate_model_id(model_name, "model id").map_err(|_| {
        model_selector_error(
            "model selector must contain a valid model id",
            "provider_selector_invalid",
        )
    })?;
    if let Some(reasoning_effort) = reasoning_effort {
        super::validate_identifier(reasoning_effort, "reasoning variant").map_err(|_| {
            model_selector_error(
                "model selector must contain a valid reasoning variant",
                "provider_selector_invalid",
            )
        })?;
    }
    Ok(ParsedModelSelector {
        provider_name,
        model_name,
        reasoning_effort,
    })
}

pub(super) fn provider_for_selection(
    catalog: &ModelSelectionSnapshot,
    selector: Option<&str>,
) -> Result<OpenAiProvider, ProviderError> {
    let selector = selector.unwrap_or(&catalog.default_model);
    let parsed = parse_model_selector(selector)?;
    let provider = catalog.providers.get(parsed.provider_name).ok_or_else(|| {
        model_selector_error(
            "model selector references an unknown provider",
            "provider_selector_unknown_provider",
        )
    })?;
    let model = provider.models.get(parsed.model_name).ok_or_else(|| {
        model_selector_error(
            "model selector references an unknown or disallowed model",
            "provider_selector_unknown_model",
        )
    })?;
    let provider_instance = provider.provider.as_ref().ok_or_else(|| {
        provider.provider_error.clone().unwrap_or_else(|| {
            super::missing_provider_auth_error(Some(ProviderConfigSource::UserConfigFile))
        })
    })?;
    let requested_variant = parsed.reasoning_effort.or(model.default_variant.as_deref());
    let Some(requested_variant) = requested_variant else {
        return Ok(
            provider_instance.with_selected_model(super::super::SelectedModel {
                model_name: parsed.model_name.to_string(),
                api_protocol: model.protocol,
                max_context_tokens: model.max_context_tokens,
                max_output_tokens: model.max_output_tokens,
                reasoning_variant: None,
                reasoning_enabled: false,
                wire_reasoning_effort: None,
                thinking_wire_format: model.thinking_wire_format,
                tool_reasoning_mode: ProviderToolReasoningMode::Unspecified,
                supports_developer_role: model.supports_developer_role,
                supports_tool_choice: model.supports_tool_choice,
                requires_reasoning_content_for_tool_calls: false,
                requires_assistant_content_for_tool_calls: model
                    .requires_assistant_content_for_tool_calls,
            }),
        );
    };
    let variant = model
        .reasoning_variants
        .get(requested_variant)
        .ok_or_else(|| {
            model_selector_error(
                "model selector references an unknown or disallowed reasoning variant",
                "provider_selector_unknown_reasoning_variant",
            )
        })?;
    if !variant.enabled && requested_variant != "off" {
        return Err(model_selector_error(
            "only the explicitly disabled off variant may be selected",
            "provider_selector_unknown_reasoning_variant",
        ));
    }
    let reasoning_enabled = variant.enabled;
    let tool_reasoning_mode = if reasoning_enabled {
        model.tool_reasoning_mode
    } else {
        ProviderToolReasoningMode::DisabledForToolCalls
    };
    let requires_reasoning_content_for_tool_calls =
        model.requires_reasoning_content_for_tool_calls && reasoning_enabled;
    Ok(
        provider_instance.with_selected_model(super::super::SelectedModel {
            model_name: parsed.model_name.to_string(),
            api_protocol: model.protocol,
            max_context_tokens: model.max_context_tokens,
            max_output_tokens: model.max_output_tokens,
            reasoning_variant: Some(requested_variant.to_string()),
            reasoning_enabled,
            wire_reasoning_effort: variant.wire_effort.clone(),
            thinking_wire_format: model.thinking_wire_format,
            tool_reasoning_mode,
            supports_developer_role: model.supports_developer_role,
            supports_tool_choice: model.supports_tool_choice,
            requires_reasoning_content_for_tool_calls,
            requires_assistant_content_for_tool_calls: model
                .requires_assistant_content_for_tool_calls,
        }),
    )
}
