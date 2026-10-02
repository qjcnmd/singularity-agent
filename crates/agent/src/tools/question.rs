//! 同步提问：模型参数在准入时校验，实际等待由 Agent 的回合控制面执行。

use serde::Deserialize;
use serde_json::json;
use singularity_protocol::UserQuestion;

use super::registry::{ToolExecution, ToolSpec, error_result};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuestionArgs {
    pub questions: Vec<UserQuestion>,
}

impl QuestionArgs {
    pub(crate) fn validate(self) -> Result<Self, ToolExecution> {
        let mut ids = std::collections::HashSet::new();
        if self.questions.is_empty() {
            return Err(error_result("questions must not be empty"));
        }
        for question in &self.questions {
            if question.id.trim().is_empty()
                || question.question.trim().is_empty()
                || !ids.insert(&question.id)
            {
                return Err(error_result(
                    "Each question needs a unique, nonempty id and question text",
                ));
            }
            let mut labels = std::collections::HashSet::new();
            if question
                .options
                .iter()
                .any(|option| option.label.trim().is_empty() || !labels.insert(&option.label))
            {
                return Err(error_result(
                    "Option labels must be nonempty and unique within each question",
                ));
            }
        }
        Ok(self)
    }
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "ask_user_question",
        snippet: "Ask the user a question and wait for their answer",
        description: "Ask for missing information or a decision that only the user can provide. Execution waits until the user submits answers or stops the task. Ask concise, self-contained questions. Options are optional; users can always enter their own text. Use multiSelect for choices that can coexist. Do not use this tool for facts you can inspect yourself or routine implementation decisions. Put a recommended option first and mark its label when useful.",
        parameters: json!({
            "type": "object", "additionalProperties": false, "required": ["questions"],
            "properties": { "questions": { "type": "array", "minItems": 1, "items": {
                "type": "object", "additionalProperties": false, "required": ["id", "question"],
                "properties": {
                    "id": {"type": "string", "description": "Unique identifier within this request"},
                    "question": {"type": "string"},
                    "multiSelect": {"type": "boolean", "default": false},
                    "options": {"type": "array", "items": {
                        "type": "object", "additionalProperties": false, "required": ["label"],
                        "properties": {"label": {"type": "string"}, "description": {"type": "string"}}
                    }}
                }
            }}}
        }),
    }
}
