//! OpenAI Chat Completions/Responses 的请求投影、响应解码和 envelope 校验。

pub(crate) mod chat;
pub(crate) mod responses;
pub(crate) mod wire;

pub(crate) use chat::*;
pub(crate) use responses::*;
pub(crate) use wire::OpenAiCompletion;
pub use wire::{chat_completions_endpoint, responses_endpoint};

pub(crate) fn tool_choice_payload() -> serde_json::Value {
    serde_json::json!("auto")
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
