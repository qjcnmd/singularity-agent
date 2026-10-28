//! `skill` 工具：按名称加载一个技能的完整指令。技能目录在 Developer 指令里，由 Skills 模块提供。

use serde::Deserialize;
use serde_json::json;
use singularity_core::skills::SkillCatalog;

use super::registry::{ToolExecution, error_result};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillArgs {
    pub(crate) name: String,
}

pub(crate) fn spec() -> singularity_model::ModelToolSchema {
    singularity_model::ModelToolSchema {
        name: "skill".to_string(),
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
