use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{
    CORE_TASK_SUCCESS_THRESHOLD_BASIS_POINTS, EvaluationCapability, EvaluationError,
    RESULT_SCHEMA_VERSION, Result, RunId, TaskId, require_schema_version, validation_error,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvaluationResultSchemaVersion {
    #[serde(rename = "evaluation.result/v5")]
    V5,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationEvidenceSummary {
    pub workspace_change_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_digest: Option<String>,
    pub tool_calls: u32,
    pub model_turns: u32,
    pub approval_count: u32,
    pub plan_update_count: u32,
    pub plan_completed: bool,
    pub invalid_tool_call_count: u32,
    pub repeated_tool_call_count: u32,
    pub repair_attempt_count: u32,
    pub completion_rejection_count: u32,
    pub compaction_count: u32,
    pub verification_required_command_count: u32,
    pub verification_satisfied_command_count: u32,
    pub provider_attempt_count: u32,
    pub provider_retry_count: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub provider_latency_ms: u64,
    pub agent_duration_ms: u64,
    pub smoke_command_satisfied: bool,
    pub strict_sandbox_command_count: u32,
    pub local_process_fallback_count: u32,
    pub local_process_fallback_unknown_count: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationRunSummary {
    pub task_count: u32,
    pub scored_task_count: u32,
    pub agent_completed_count: u32,
    pub tests_passed_count: u32,
    pub evaluation_passed_count: u32,
    pub blocked_count: u32,
    pub task_success_rate_basis_points: u32,
    pub meets_core_task_success_threshold: bool,
}

impl EvaluationRunSummary {
    pub fn from_tasks(tasks: &[EvaluationTaskResult]) -> Self {
        let task_count = u32::try_from(tasks.len()).unwrap_or(u32::MAX);
        let agent_completed_count = count_tasks(tasks, |task| task.agent_completed);
        let tests_passed_count = count_tasks(tasks, |task| task.tests_passed);
        let evaluation_passed_count = count_tasks(tasks, |task| task.evaluation_passed);
        let blocked_count = count_tasks(tasks, |task| task.status == EvaluationStatus::Blocked);
        let scored_task_count = task_count;
        let task_success_rate_basis_points = if scored_task_count == 0 {
            0
        } else {
            evaluation_passed_count
                .saturating_mul(10_000)
                .checked_div(scored_task_count)
                .unwrap_or(0)
        };
        Self {
            task_count,
            scored_task_count,
            agent_completed_count,
            tests_passed_count,
            evaluation_passed_count,
            blocked_count,
            task_success_rate_basis_points,
            meets_core_task_success_threshold: task_success_rate_basis_points
                >= CORE_TASK_SUCCESS_THRESHOLD_BASIS_POINTS,
        }
    }
}

fn count_tasks(
    tasks: &[EvaluationTaskResult],
    predicate: impl Fn(&EvaluationTaskResult) -> bool,
) -> u32 {
    u32::try_from(tasks.iter().filter(|task| predicate(task)).count()).unwrap_or(u32::MAX)
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
    ProviderResponse,
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
    pub capabilities: Vec<EvaluationCapability>,
    pub status: EvaluationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<EvaluationBlocker>,
    pub stages: EvaluationStageResults,
    pub agent_completed: bool,
    pub tests_passed: bool,
    pub evaluation_passed: bool,
    pub evidence: EvaluationEvidenceSummary,
}

impl EvaluationTaskResult {
    fn validate(&self) -> Result<()> {
        let context = format!("evaluation task {}", self.task_id);
        if self.capabilities.is_empty()
            || self
                .capabilities
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len()
                != self.capabilities.len()
        {
            return Err(validation_error(format!(
                "{context} capabilities must be non-empty and unique"
            )));
        }
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
        if self.evaluation_passed
            && (!self.evidence.smoke_command_satisfied
                || self.evidence.strict_sandbox_command_count == 0
                || self.evidence.local_process_fallback_count != 0
                || self.evidence.local_process_fallback_unknown_count != 0
                || self.evidence.patch_digest.is_none())
        {
            return Err(validation_error(format!(
                "{context} evaluation_passed requires patch, smoke, strict sandbox, and complete zero-fallback evidence"
            )));
        }
        if self.evidence.provider_retry_count > self.evidence.provider_attempt_count {
            return Err(validation_error(format!(
                "{context} provider_retry_count cannot exceed provider_attempt_count"
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
    pub summary: EvaluationRunSummary,
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
        let expected_summary = EvaluationRunSummary::from_tasks(&self.tasks);
        if self.summary != expected_summary {
            return Err(validation_error(
                "evaluation run summary must match the task results",
            ));
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
