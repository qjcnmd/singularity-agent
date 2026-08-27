//! OpenAI Chat Completions/Responses 的请求投影、响应解码和 envelope 校验。

pub(crate) mod chat;
pub(crate) mod responses;
pub(crate) mod wire;

pub(crate) use chat::*;
pub(crate) use responses::*;
pub(crate) use wire::OpenAiCompletion;
pub use wire::{chat_completions_endpoint, responses_endpoint};

use crate::provider::contract::{ProviderProtocolContract, request_uses_tool_protocol};
use crate::provider::runtime::WireRequestOptions;
use crate::types::{ModelTurnRequest, ProviderReasoningReplay, ProviderToolReasoningMode};

pub(crate) fn tool_choice_payload() -> serde_json::Value {
    serde_json::json!("auto")
}

pub(crate) struct ReasoningWireDecision<'a> {
    pub(crate) enabled: bool,
    pub(crate) disabled: bool,
    pub(crate) effort: Option<&'a str>,
    pub(crate) disabled_for_tool_calls: bool,
}

pub(crate) fn reasoning_wire_decision<'a>(
    request: &ModelTurnRequest,
    capabilities: &ProviderProtocolContract,
    wire: &'a WireRequestOptions,
) -> ReasoningWireDecision<'a> {
    ReasoningWireDecision {
        enabled: wire.reasoning_enabled,
        disabled: wire.reasoning_disabled,
        effort: wire.wire_reasoning_effort.as_deref(),
        disabled_for_tool_calls: request_uses_tool_protocol(request)
            && capabilities.tool_reasoning_mode == ProviderToolReasoningMode::DisabledForToolCalls,
    }
}

pub(crate) fn matching_reasoning_replay<'a>(
    history: &'a [ProviderReasoningReplay],
    call_ids: &[String],
) -> Option<&'a ProviderReasoningReplay> {
    history
        .iter()
        .find(|replay| replay.matches_tool_call_ids(call_ids))
}

#[cfg(test)]
mod tests {
    use super::responses::openai_responses_input;
    use crate::types::{ModelMessage, ModelRole};

    #[test]
    fn responses_projects_non_leading_developer_to_system() {
        let (instructions, input) = openai_responses_input(
            &[
                ModelMessage::text(ModelRole::User, "first"),
                ModelMessage::text(ModelRole::Developer, "late instruction"),
                ModelMessage::text(ModelRole::User, "last"),
            ],
            &[],
        );

        assert_eq!(instructions, None);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "system");
        assert_eq!(input[2]["role"], "user");
    }
}
