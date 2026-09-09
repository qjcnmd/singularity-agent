//! OpenAI Chat Completions/Responses 的请求投影、响应解码和 envelope 校验。

pub(crate) mod chat;
pub(crate) mod responses;
pub(crate) mod wire;

pub(crate) use chat::*;
pub(crate) use responses::*;
pub(crate) use wire::OpenAiCompletion;
pub use wire::{chat_completions_endpoint, responses_endpoint};

use crate::provider::runtime::SelectedModel;

pub(crate) struct ReasoningWireDecision<'a> {
    pub(crate) enabled: Option<bool>,
    pub(crate) effort: Option<&'a str>,
}

pub(crate) fn reasoning_wire_decision(selection: &SelectedModel) -> ReasoningWireDecision<'_> {
    ReasoningWireDecision {
        enabled: selection
            .reasoning_variant
            .as_ref()
            .map(|_| selection.reasoning_enabled),
        effort: selection.wire_reasoning_effort.as_deref(),
    }
}

#[cfg(test)]
mod tests {
    use super::responses::openai_responses_input;
    use crate::types::{ModelMessage, ModelRole};

    #[test]
    fn responses_projects_non_leading_developer_to_system() {
        let (instructions, input) = openai_responses_input(&[
            ModelMessage::text(ModelRole::User, "first"),
            ModelMessage::text(ModelRole::Developer, "late instruction"),
            ModelMessage::text(ModelRole::User, "last"),
        ]);

        assert_eq!(instructions, None);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "system");
        assert_eq!(input[2]["role"], "user");
    }
}
