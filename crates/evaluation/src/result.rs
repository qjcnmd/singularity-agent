use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{
    EvaluationError, RESULT_SCHEMA_VERSION, Result, RunId, TaskId, require_schema_version,
    validation_error,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvaluationResultSchemaVersion {
    #[serde(rename = "evaluation.result/v2")]
    V2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    NotRun,
    Passed,
    Failed,
    Blocked,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerKind {
    Environment,
    WorkspacePreparation,
    ProviderConfiguration,
    ProviderAuthentication,
    Network,
    Sandbox,
    AgentRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationBlocker {
    pub kind: BlockerKind,
    pub message: String,
}

impl EvaluationBlocker {
    fn validate(&self, context: &str) -> Result<()> {
        if self.message.trim().is_empty() {
            return Err(validation_error(format!(
                "{context} blocker.message must not be empty"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageResult {
    pub status: StageStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<EvaluationBlocker>,
}

impl StageResult {
    fn validate(&self, context: &str) -> Result<()> {
        validate_stage_blocker(self.status, self.blocker.as_ref(), context)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationStageResults {
    pub baseline: StageResult,
    pub agent: StageResult,
    pub public: StageResult,
    pub hidden: StageResult,
}

impl EvaluationStageResults {
    fn validate(&self, task_id: &TaskId) -> Result<()> {
        self.baseline
            .validate(&format!("evaluation task {task_id} baseline stage"))?;
        self.agent
            .validate(&format!("evaluation task {task_id} agent stage"))?;
        self.public
            .validate(&format!("evaluation task {task_id} public stage"))?;
        self.hidden
            .validate(&format!("evaluation task {task_id} hidden stage"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationTaskResult {
    pub task_id: TaskId,
    pub status: EvaluationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<EvaluationBlocker>,
    pub stages: EvaluationStageResults,
    pub agent_completed: bool,
    pub tests_passed: bool,
    pub evaluation_passed: bool,
}

impl EvaluationTaskResult {
    fn validate(&self) -> Result<()> {
        let context = format!("evaluation task {}", self.task_id);
        validate_evaluation_blocker(self.status, self.blocker.as_ref(), &context)?;
        self.stages.validate(&self.task_id)?;
        if self.agent_completed != (self.stages.agent.status == StageStatus::Passed) {
            return Err(validation_error(format!(
                "{context} agent_completed must match the agent stage status"
            )));
        }
        if self.tests_passed
            && (self.stages.public.status != StageStatus::Passed
                || self.stages.hidden.status != StageStatus::Passed)
        {
            return Err(validation_error(format!(
                "{context} tests_passed requires passed public and hidden stages"
            )));
        }
        if self.evaluation_passed && self.stages.baseline.status != StageStatus::Passed {
            return Err(validation_error(format!(
                "{context} evaluation_passed requires a passed baseline stage"
            )));
        }
        if self.evaluation_passed && (!self.agent_completed || !self.tests_passed) {
            return Err(validation_error(format!(
                "{context} evaluation_passed requires agent_completed and tests_passed"
            )));
        }
        if self.evaluation_passed && self.status != EvaluationStatus::Completed {
            return Err(validation_error(format!(
                "{context} evaluation_passed requires completed status"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationResult {
    pub schema_version: EvaluationResultSchemaVersion,
    pub run_id: RunId,
    pub status: EvaluationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<EvaluationBlocker>,
    pub evaluation_passed: bool,
    pub tasks: Vec<EvaluationTaskResult>,
}

impl EvaluationResult {
    pub fn from_json_str(json: &str) -> Result<Self> {
        require_schema_version(json, RESULT_SCHEMA_VERSION)?;
        let result: Self = serde_json::from_str(json)?;
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<()> {
        validate_evaluation_blocker(self.status, self.blocker.as_ref(), "evaluation run")?;
        if self.tasks.is_empty() {
            return Err(validation_error(
                "evaluation result requires at least one task result",
            ));
        }
        let mut task_ids = BTreeSet::new();
        for task in &self.tasks {
            task.validate()?;
            if !task_ids.insert(task.task_id.clone()) {
                return Err(EvaluationError::DuplicateTaskId(task.task_id.clone()));
            }
        }
        let all_tasks_passed = self.tasks.iter().all(|task| task.evaluation_passed);
        if self.evaluation_passed != all_tasks_passed {
            return Err(validation_error(
                "evaluation run evaluation_passed must equal all task evaluation_passed values",
            ));
        }
        if self.evaluation_passed && self.status != EvaluationStatus::Completed {
            return Err(validation_error(
                "evaluation run evaluation_passed requires completed status",
            ));
        }
        Ok(())
    }
}

fn validate_evaluation_blocker(
    status: EvaluationStatus,
    blocker: Option<&EvaluationBlocker>,
    context: &str,
) -> Result<()> {
    validate_blocker(status == EvaluationStatus::Blocked, blocker, context)
}

fn validate_stage_blocker(
    status: StageStatus,
    blocker: Option<&EvaluationBlocker>,
    context: &str,
) -> Result<()> {
    validate_blocker(status == StageStatus::Blocked, blocker, context)
}

fn validate_blocker(
    blocked: bool,
    blocker: Option<&EvaluationBlocker>,
    context: &str,
) -> Result<()> {
    match (blocked, blocker) {
        (true, Some(blocker)) => blocker.validate(context),
        (true, None) => Err(validation_error(format!(
            "{context} blocked status requires blocker"
        ))),
        (false, Some(_)) => Err(validation_error(format!(
            "{context} blocker is only valid for blocked status"
        ))),
        (false, None) => Ok(()),
    }
}
