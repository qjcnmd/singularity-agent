//! OpenAI Chat Completions/Responses 的请求投影、响应解码和 envelope 校验。

pub mod chat;
pub mod responses;
pub mod wire;

pub use chat::*;
pub use responses::*;
pub use wire::*;

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
