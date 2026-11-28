use serde::Deserialize;
use serde_json::json;

use super::registry::{TODO_TOOL_NAME, ToolExecution, error_result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TodoItem {
    pub(crate) content: String,
    pub(crate) status: TodoStatus,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TodoArgs {
    pub(crate) todos: Vec<TodoItem>,
}

impl TodoArgs {
    pub(crate) fn validate(self) -> Result<Self, ToolExecution> {
        let mut in_progress = 0usize;
        for item in &self.todos {
            if item.content.trim().is_empty() {
                return Err(error_result("invalid tool arguments: todo item content must not be empty"));
            }
            if item.status == TodoStatus::InProgress {
                in_progress += 1;
            }
        }
        if in_progress > 1 {
            return Err(error_result(
                "invalid tool arguments: at most one todo item may be in_progress at a time",
            ));
        }
        Ok(self)
    }
}

pub(crate) fn spec() -> app_core::ToolDefinition {
    app_core::ToolDefinition {
        name: TODO_TOOL_NAME.to_string(),
        description: "Maintain a concise checklist for complex multi-step tasks (3+ steps). Skip for simple or single-step work. Each call replaces the full list; pass an empty array [] to clear it. Keep at most one item in_progress at a time and update immediately when completing a step.".to_string(),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "Full updated list of todo items; pass [] to clear the list",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "Concise task description"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["todos"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn execute(args: &TodoArgs) -> ToolExecution {
    if args.todos.is_empty() {
        return ToolExecution::text("Cleared todo list.");
    }
    let total = args.todos.len();
    let completed = args.todos.iter().filter(|item| item.status == TodoStatus::Completed).count();
    let in_progress = args.todos.iter().filter(|item| item.status == TodoStatus::InProgress).count();
    ToolExecution::text(format!(
        "Updated todo list ({completed}/{total} completed, {in_progress} in progress)."
    ))
}
