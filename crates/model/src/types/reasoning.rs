use super::message::{ModelMessage, ModelRole};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// tool 推理内容是否符合模型提供方的 tool call 历史契约。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderToolReasoningMode {
    #[default]
    Unspecified,
    DisabledForToolCalls,
    /// The adapter must preserve Chat Completions `reasoning_content` on every
    /// assistant tool-call continuation.
    ReplayReasoningContent,
    /// The adapter must preserve Responses reasoning output items verbatim.
    ReplayResponsesItems,
}

/// Provider-private reasoning state that is safe to replay at the adapter
/// boundary but must never be displayed or projected into public conversation,
/// trace, Evaluation, or error schemas. The Rust type is public only because
/// the harness owns the reasoning-replay boundary between turns.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderReasoningReplay {
    Chat {
        provider_name: String,
        model_name: String,
        reasoning_effort: String,
        tool_call_ids: Vec<String>,
        reasoning_content: String,
    },
    Responses {
        provider_name: String,
        model_name: String,
        reasoning_effort: String,
        tool_call_ids: Vec<String>,
        /// The complete provider output sequence is retained verbatim.  The
        /// adapter only appends later `function_call_output` items.
        items: Vec<Value>,
    },
}

impl fmt::Debug for ProviderReasoningReplay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("ProviderReasoningReplay");
        match self {
            Self::Chat {
                tool_call_ids,
                reasoning_content,
                ..
            } => {
                debug
                    .field("protocol", &"chat")
                    .field("tool_call_count", &tool_call_ids.len())
                    .field("reasoning_content_len", &reasoning_content.len());
            }
            Self::Responses {
                tool_call_ids,
                items,
                ..
            } => {
                debug
                    .field("protocol", &"responses")
                    .field("tool_call_count", &tool_call_ids.len())
                    .field("output_item_count", &items.len())
                    .field("reasoning_item_present", &true);
            }
        }
        debug.finish()
    }
}

impl ProviderReasoningReplay {
    /// Validate the opaque replay at the owning provider boundary without
    /// exposing its private payload in an error.
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Chat {
                provider_name,
                model_name,
                reasoning_effort,
                tool_call_ids,
                reasoning_content,
            } => {
                validate_replay_binding(provider_name, model_name, reasoning_effort)?;
                validate_replay_tool_call_ids(tool_call_ids)?;
                if reasoning_content.is_empty() {
                    return Err("provider reasoning replay content is empty");
                }
            }
            Self::Responses {
                provider_name,
                model_name,
                reasoning_effort,
                tool_call_ids,
                items,
            } => {
                validate_replay_binding(provider_name, model_name, reasoning_effort)?;
                validate_replay_tool_call_ids(tool_call_ids)?;
                validate_responses_replay_items(items, tool_call_ids)?;
            }
        }
        Ok(())
    }

    /// Return only the validation result at non-model boundaries.
    pub fn is_valid(&self) -> bool {
        self.validate().is_ok()
    }

    /// Check the private replay binding without exposing its opaque payload. The
    /// agent uses this gate when a persisted thread switches provider/model.
    pub fn is_compatible_with(
        &self,
        provider_name: &str,
        model_name: &str,
        reasoning_effort: &str,
        mode: ProviderToolReasoningMode,
    ) -> bool {
        self.validate_for(provider_name, model_name, reasoning_effort, mode)
            .is_ok()
    }

    /// Validate the replay against one selected provider/model/variant and mode.
    pub(crate) fn validate_for(
        &self,
        provider_name: &str,
        model_name: &str,
        reasoning_effort: &str,
        mode: ProviderToolReasoningMode,
    ) -> Result<(), &'static str> {
        self.validate()?;
        let (replay_provider, replay_model, replay_variant) = self.binding_internal();
        if replay_provider != provider_name
            || replay_model != model_name
            || replay_variant != reasoning_effort
            || self.mode_internal() != mode
        {
            return Err("provider reasoning replay binding does not match selected model");
        }
        Ok(())
    }

    fn binding_internal(&self) -> (&str, &str, &str) {
        match self {
            Self::Chat {
                provider_name,
                model_name,
                reasoning_effort,
                ..
            }
            | Self::Responses {
                provider_name,
                model_name,
                reasoning_effort,
                ..
            } => (provider_name, model_name, reasoning_effort),
        }
    }

    /// Returns whether the replay is bound to all supplied tool-call ids in order.
    pub fn matches_tool_call_ids(&self, ids: &[String]) -> bool {
        match self {
            Self::Chat { tool_call_ids, .. } | Self::Responses { tool_call_ids, .. } => {
                tool_call_ids == ids
            }
        }
    }

    /// Return true when the replay contains a tool-call id without exposing ids.
    pub fn has_tool_call_id(&self, id: &str) -> bool {
        match self {
            Self::Chat { tool_call_ids, .. } | Self::Responses { tool_call_ids, .. } => {
                tool_call_ids.iter().any(|candidate| candidate == id)
            }
        }
    }

    /// Return true when one assistant message in the supplied model history
    /// carries exactly this replay's ordered tool-call binding.
    pub fn is_bound_to_messages(&self, messages: &[ModelMessage]) -> bool {
        self.bound_assistant_count(messages) == 1
    }

    /// Count assistant tool-call messages with this exact ordered binding.
    pub fn bound_assistant_count(&self, messages: &[ModelMessage]) -> usize {
        messages
            .iter()
            .filter(|message| {
                message.role == ModelRole::Assistant
                    && self.matches_tool_call_ids(
                        &message
                            .tool_calls
                            .iter()
                            .map(|call| call.tool_call_id.clone())
                            .collect::<Vec<_>>(),
                    )
            })
            .count()
    }

    /// Returns the protocol-specific reasoning mode inside the model crate.
    pub(crate) fn mode_internal(&self) -> ProviderToolReasoningMode {
        match self {
            Self::Chat { .. } => ProviderToolReasoningMode::ReplayReasoningContent,
            Self::Responses { .. } => ProviderToolReasoningMode::ReplayResponsesItems,
        }
    }
}

fn validate_replay_binding(
    provider_name: &str,
    model_name: &str,
    reasoning_effort: &str,
) -> Result<(), &'static str> {
    for value in [provider_name, model_name, reasoning_effort] {
        if value.is_empty()
            || value
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err("provider reasoning replay binding is malformed");
        }
    }
    if reasoning_effort == "off" {
        return Err("provider reasoning replay cannot use disabled variant");
    }
    Ok(())
}

fn validate_replay_tool_call_ids(ids: &[String]) -> Result<(), &'static str> {
    if ids.is_empty()
        || ids.iter().any(|id| {
            id.is_empty()
                || id
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
        })
        || ids.iter().collect::<std::collections::BTreeSet<_>>().len() != ids.len()
    {
        return Err("provider reasoning replay tool-call identity is invalid");
    }
    Ok(())
}

fn validate_responses_replay_items(
    items: &[Value],
    tool_call_ids: &[String],
) -> Result<(), &'static str> {
    if items.is_empty() {
        return Err("Responses reasoning replay output is empty");
    }
    let mut reasoning_count = 0usize;
    let mut function_call_ids = Vec::new();
    for item in items {
        let object = item
            .as_object()
            .ok_or("Responses replay output item is not an object")?;
        let item_type = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or("Responses replay output item type is missing")?;
        match item_type {
            "reasoning" => {
                let id = object
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or("Responses reasoning item id is missing")?;
                if id.chars().any(|character| character.is_control()) {
                    return Err("Responses reasoning item id is invalid");
                }
                reasoning_count = reasoning_count.saturating_add(1);
            }
            "message" => {}
            "function_call" => {
                let call_id = object
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or("Responses function_call id is missing")?;
                if call_id.chars().any(|character| character.is_control()) {
                    return Err("Responses function_call id is invalid");
                }
                function_call_ids.push(call_id.to_string());
            }
            _ => return Err("Responses replay output item type is unsupported"),
        }
    }
    if reasoning_count == 0 {
        return Err("Responses reasoning replay item is missing");
    }
    if function_call_ids != tool_call_ids
        || function_call_ids
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != function_call_ids.len()
    {
        return Err("Responses replay function_call ids do not match tool calls");
    }
    Ok(())
}
