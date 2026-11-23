use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::config::selection::SelectedModel;
use crate::error::{ModelErrorKind, ProviderError};
use crate::types::{Message, ModelTurnResponse, ProviderMessageMetadata};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStreamEvent {
    ToolCallDelta,
    OutputTextDelta { delta: String },
    ReasoningTextDelta { delta: String },
}

pub(crate) fn canonical_base_url(value: &str) -> &str {
    value.trim().trim_end_matches('/')
}

pub(crate) fn endpoint(base_url: &str, path: &str) -> String {
    format!("{}{path}", canonical_base_url(base_url))
}

pub(crate) fn split_leading_instructions(messages: &[Message]) -> (Option<String>, &[Message]) {
    let instruction_count =
        messages.iter().take_while(|message| matches!(message, Message::Developer { .. })).count();
    let instructions = messages[..instruction_count]
        .iter()
        .map(Message::content_text)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    ((!instructions.is_empty()).then_some(instructions), &messages[instruction_count..])
}

pub(crate) fn message_metadata_for<'a>(
    message: &'a Message,
    selection: &SelectedModel,
    provider_name: &str,
    endpoint: &str,
) -> Option<&'a ProviderMessageMetadata> {
    message.provider_metadata().filter(|replay| {
        replay.is_for_model(provider_name, &selection.model_name, selection.protocol, endpoint)
    })
}

pub(crate) struct ReasoningWireDecision<'a> {
    pub(crate) enabled: Option<bool>,
    pub(crate) effort: Option<&'a str>,
}

pub(crate) fn reasoning_wire_decision(selection: &SelectedModel) -> ReasoningWireDecision<'_> {
    ReasoningWireDecision {
        enabled: selection.reasoning_variant.as_ref().map(|variant| variant != "off"),
        effort: selection.reasoning_variant.as_deref().filter(|variant| !matches!(*variant, "off" | "on")),
    }
}

pub(crate) fn image_data_url<'a>(
    images: &'a HashMap<String, String>,
    image_id: &str,
) -> Result<&'a str, ProviderError> {
    images.get(image_id).map(String::as_str).ok_or_else(|| {
        ProviderError::new(ModelErrorKind::Other, format!("missing image data for attachment {image_id}"))
    })
}

pub(crate) fn parse_tool_call_arguments(value: Option<&Value>) -> Result<Value, ProviderError> {
    let reason = match value {
        Some(Value::String(raw)) => return Ok(parse_tool_arguments(raw)),
        Some(value @ Value::Object(_)) => return Ok(value.clone()),
        None => "tool_call_arguments_missing",
        Some(_) => "tool_call_arguments_type_invalid",
    };
    Err(provider_response_validation_error(
        "provider tool arguments were missing or had an invalid type",
        &[reason],
    ))
}

pub(crate) fn parse_tool_arguments(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

pub(crate) fn serialize_tool_call_arguments(arguments: &Value) -> String {
    match arguments {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn provider_response_validation_error(message: &str, errors: &[impl AsRef<str>]) -> ProviderError {
    ProviderError::diagnostic(ModelErrorKind::Other, message, errors)
}

pub(crate) fn finalize_provider_response(mut response: ModelTurnResponse) -> ModelTurnResponse {
    let mut errors: Vec<&'static str> = Vec::new();
    if !response.is_length_truncated()
        && !response
            .assistant_message
            .content()
            .iter()
            .any(|block| matches!(block, crate::ContentBlock::Text { text, .. } if !text.trim().is_empty()))
        && response.assistant_message.tool_calls().next().is_none()
    {
        errors.push("empty_response");
    }

    let mut seen = HashSet::new();
    for call in response.assistant_message.tool_calls() {
        if call.tool_call_id.trim().is_empty() {
            errors.push("missing_tool_call_id");
        } else if !seen.insert(call.tool_call_id.as_str()) {
            errors.push("duplicate_tool_call_id");
        }
        if call.tool_name.trim().is_empty() {
            errors.push("missing_tool_name");
        }
    }

    errors.sort_unstable();
    errors.dedup();
    if !errors.is_empty() {
        response.stop_reason = crate::ModelStopReason::Failed(provider_response_validation_error(
            "provider response structure was invalid",
            &errors,
        ));
        response.usage = None;
        if let crate::Message::Assistant { content, provider_metadata } = &mut response.assistant_message {
            content.retain_mut(|block| match block {
                crate::ContentBlock::Text { text, metadata } => {
                    *metadata = None;
                    !text.is_empty()
                }
                crate::ContentBlock::Thinking { thinking, metadata } => {
                    *metadata = None;
                    !thinking.is_empty()
                }
                crate::ContentBlock::Image(_) | crate::ContentBlock::ToolCall(_) => false,
            });
            *provider_metadata = None;
        }
    }
    response
}
