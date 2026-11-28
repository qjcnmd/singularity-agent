use app_core::skills::SkillCatalog;
use serde::Deserialize;
use serde_json::json;

use super::registry::{SKILL_TOOL_NAME, ToolExecution, error_result};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillArgs {
    pub(crate) name: String,
}

pub(crate) fn spec() -> app_core::ToolDefinition {
    app_core::ToolDefinition {
        name: SKILL_TOOL_NAME.to_string(),
        description: "Load the complete instructions of an available skill by name. The result includes the skill's base directory, from which its relative resources are resolved.".to_string(),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Skill name from the available skills list" },
            },
            "required": ["name"],
            "additionalProperties": false,
        }),
    }
}

pub(crate) fn execute(args: &SkillArgs, catalog: &SkillCatalog) -> ToolExecution {
    match catalog.model_skills().find(|skill| skill.name == args.name) {
        Some(skill) => skill.load_instructions().map_or_else(error_result, ToolExecution::text),
        None => error_result(format!("Unknown skill: {}", args.name)),
    }
}
