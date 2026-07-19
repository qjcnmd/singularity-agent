//! Evaluation trial/result 状态、有限统计、门禁汇总和 blocker 校验。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    CORE_TASK_SUCCESS_THRESHOLD_BASIS_POINTS, EvaluationCapability, EvaluationError,
    PREVIOUS_RESULT_SCHEMA_VERSION, RESULT_SCHEMA_VERSION, Result, RunId, TaskId,
    ToolCapabilityRequirement, validation_error,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Evaluation result 的 schema 版本。
pub enum EvaluationResultSchemaVersion {
    #[serde(rename = "evaluation.result/v7")]
    V7,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 单个 trial 的执行证据计数和摘要。
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 非 blocked trial 样本的有限值统计。
pub struct FiniteStatistics {
    pub sample_count: u32,
    pub minimum: u64,
    pub maximum: u64,
    pub mean: f64,
    pub population_variance: f64,
}

impl FiniteStatistics {
    /// 从整数观测构造不会产生 NaN 或 infinity 的统计。
    pub fn from_values(values: &[u64]) -> Option<Self> {
        let (&minimum, &maximum) = (values.iter().min()?, values.iter().max()?);
        let sample_count = u32::try_from(values.len()).ok()?;
        let mean = values.iter().map(|value| *value as f64).sum::<f64>() / f64::from(sample_count);
        let population_variance = values
            .iter()
            .map(|value| {
                let delta = *value as f64 - mean;
                delta * delta
            })
            .sum::<f64>()
            / f64::from(sample_count);
        Some(Self {
            sample_count,
            minimum,
            maximum,
            mean,
            population_variance,
        })
    }

    fn validate(&self, context: &str) -> Result<()> {
        if self.sample_count == 0
            || self.minimum > self.maximum
            || !self.mean.is_finite()
            || !self.population_variance.is_finite()
            || self.population_variance < 0.0
            || self.mean < self.minimum as f64
            || self.mean > self.maximum as f64
        {
            return Err(validation_error(format!(
                "{context} must contain finite statistics for at least one sample"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Evaluation run 或 trial 的生命周期状态。
pub enum EvaluationStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// 单个阶段的执行状态。
pub enum StageStatus {
    NotRun,
    Passed,
    Failed,
    Blocked,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// 阻塞 Evaluation 的稳定原因类别。
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
/// Evaluation 或阶段的阻塞原因。
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
/// 单个阶段的状态与可选 blocker。
pub struct StageResult {
    pub status: StageStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<EvaluationBlocker>,
}

impl StageResult {
    fn validate(&self, context: &str) -> Result<()> {
        validate_blocker(
            self.status == StageStatus::Blocked,
            self.blocker.as_ref(),
            context,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 一个 trial 的 baseline、agent、public、hidden 阶段结果。
pub struct EvaluationStageResults {
    pub baseline: StageResult,
    pub agent: StageResult,
    pub public: StageResult,
    pub hidden: StageResult,
}

impl EvaluationStageResults {
    fn validate(&self, context: &str) -> Result<()> {
        self.baseline
            .validate(&format!("{context} baseline stage"))?;
        self.agent.validate(&format!("{context} agent stage"))?;
        self.public.validate(&format!("{context} public stage"))?;
        self.hidden.validate(&format!("{context} hidden stage"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 单个独立 trial 的最终结果和任务定义完成条件字段。
pub struct EvaluationTrialResult {
    pub trial: u32,
    pub status: EvaluationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<EvaluationBlocker>,
    pub stages: EvaluationStageResults,
    pub agent_completed: bool,
    pub tests_passed: bool,
    /// 该 trial 是否满足 manifest evaluator 定义的完成条件。
    pub evaluation_passed: bool,
    pub evidence: EvaluationEvidenceSummary,
}

impl EvaluationTrialResult {
    fn validate(&self, task_id: &TaskId) -> Result<()> {
        let context = format!("evaluation task {task_id} trial {}", self.trial);
        if self.trial == 0 {
            return Err(validation_error(format!(
                "{context} trial must be positive"
            )));
        }
        validate_evaluation_blocker(self.status, self.blocker.as_ref(), &context)?;
        self.stages.validate(&context)?;
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
        if self.evaluation_passed
            && (self.stages.baseline.status != StageStatus::Passed
                || !self.agent_completed
                || !self.tests_passed
                || self.status != EvaluationStatus::Completed)
        {
            return Err(validation_error(format!(
                "{context} evaluation_passed requires completed baseline, agent, and tests"
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 同一任务的 trial 分母、状态计数和 trial 成功数。
pub struct EvaluationTaskSummary {
    pub trial_count: u32,
    pub completed_trial_count: u32,
    pub failed_trial_count: u32,
    pub blocked_trial_count: u32,
    pub agent_scored_trial_count: u32,
    pub agent_completed_count: u32,
    pub agent_failed_count: u32,
    /// 满足任务定义完成条件的 trial 数；仅对非 blocked trial 作为 trial rate 的分子。
    pub trial_success_count: u32,
}

impl EvaluationTaskSummary {
    pub fn from_trials(trials: &[EvaluationTrialResult]) -> Self {
        let trial_count = count_trials(trials, |_| true);
        let completed_trial_count =
            count_trials(trials, |trial| trial.status == EvaluationStatus::Completed);
        let failed_trial_count =
            count_trials(trials, |trial| trial.status == EvaluationStatus::Failed);
        let blocked_trial_count =
            count_trials(trials, |trial| trial.status == EvaluationStatus::Blocked);
        let agent_scored_trial_count = completed_trial_count.saturating_add(failed_trial_count);
        let agent_completed_count = count_trials(trials, |trial| {
            trial.status != EvaluationStatus::Blocked && trial.agent_completed
        });
        Self {
            trial_count,
            completed_trial_count,
            failed_trial_count,
            blocked_trial_count,
            agent_scored_trial_count,
            agent_completed_count,
            agent_failed_count: agent_scored_trial_count.saturating_sub(agent_completed_count),
            trial_success_count: count_trials(trials, |trial| trial.evaluation_passed),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 多 trial 的稳定性结论和有限指标分布。
pub struct EvaluationStabilitySummary {
    pub stable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_turns: Option<FiniteStatistics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<FiniteStatistics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_duration_ms: Option<FiniteStatistics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_latency_ms: Option<FiniteStatistics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_retries: Option<FiniteStatistics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<FiniteStatistics>,
}

impl EvaluationStabilitySummary {
    pub fn from_trials(trials: &[EvaluationTrialResult]) -> Self {
        let scored = trials
            .iter()
            .filter(|trial| trial.status != EvaluationStatus::Blocked)
            .collect::<Vec<_>>();
        let stable = trials.len() > 1
            && scored.len() == trials.len()
            && scored.first().is_some_and(|first| {
                scored.iter().all(|trial| {
                    trial.status == first.status
                        && trial.agent_completed == first.agent_completed
                        && trial.tests_passed == first.tests_passed
                        && trial.evaluation_passed == first.evaluation_passed
                })
            });
        Self {
            stable,
            model_turns: FiniteStatistics::from_values(
                &scored
                    .iter()
                    .map(|trial| u64::from(trial.evidence.model_turns))
                    .collect::<Vec<_>>(),
            ),
            tool_calls: FiniteStatistics::from_values(
                &scored
                    .iter()
                    .map(|trial| u64::from(trial.evidence.tool_calls))
                    .collect::<Vec<_>>(),
            ),
            agent_duration_ms: FiniteStatistics::from_values(
                &scored
                    .iter()
                    .map(|trial| trial.evidence.agent_duration_ms)
                    .collect::<Vec<_>>(),
            ),
            provider_latency_ms: FiniteStatistics::from_values(
                &scored
                    .iter()
                    .map(|trial| trial.evidence.provider_latency_ms)
                    .collect::<Vec<_>>(),
            ),
            provider_retries: FiniteStatistics::from_values(
                &scored
                    .iter()
                    .map(|trial| u64::from(trial.evidence.provider_retry_count))
                    .collect::<Vec<_>>(),
            ),
            total_tokens: FiniteStatistics::from_values(
                &scored
                    .iter()
                    .map(|trial| trial.evidence.total_tokens)
                    .collect::<Vec<_>>(),
            ),
        }
    }

    fn validate(&self, context: &str, trial_count: u32) -> Result<()> {
        if trial_count == 1 && self.stable {
            return Err(validation_error(format!(
                "{context} single-trial result must report stable=false"
            )));
        }
        for (name, statistic) in [
            ("model_turns", self.model_turns.as_ref()),
            ("tool_calls", self.tool_calls.as_ref()),
            ("agent_duration_ms", self.agent_duration_ms.as_ref()),
            ("provider_latency_ms", self.provider_latency_ms.as_ref()),
            ("provider_retries", self.provider_retries.as_ref()),
            ("total_tokens", self.total_tokens.as_ref()),
        ] {
            if let Some(statistic) = statistic {
                statistic.validate(&format!("{context} {name}"))?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 一个任务的全部隔离 trial 与派生稳定性结论。
pub struct EvaluationTaskResult {
    pub task_id: TaskId,
    pub capabilities: Vec<EvaluationCapability>,
    pub required_tool_capabilities: Vec<ToolCapabilityRequirement>,
    pub status: EvaluationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<EvaluationBlocker>,
    /// 任务成功：该任务的每个 trial 都满足任务定义的完成条件。
    pub evaluation_passed: bool,
    pub summary: EvaluationTaskSummary,
    pub stability: EvaluationStabilitySummary,
    pub trials: Vec<EvaluationTrialResult>,
}

impl EvaluationTaskResult {
    /// 从同一任务的独立 trial 构造不可手工覆盖的汇总。
    pub fn from_trials(
        task_id: TaskId,
        capabilities: Vec<EvaluationCapability>,
        required_tool_capabilities: Vec<ToolCapabilityRequirement>,
        trials: Vec<EvaluationTrialResult>,
    ) -> Self {
        let summary = EvaluationTaskSummary::from_trials(&trials);
        let status = aggregate_status(&trials.iter().map(|trial| trial.status).collect::<Vec<_>>());
        let blocker = (status == EvaluationStatus::Blocked)
            .then(|| trials.iter().find_map(|trial| trial.blocker.clone()))
            .flatten();
        let evaluation_passed =
            !trials.is_empty() && trials.iter().all(|trial| trial.evaluation_passed);
        let stability = EvaluationStabilitySummary::from_trials(&trials);
        Self {
            task_id,
            capabilities,
            required_tool_capabilities,
            status,
            blocker,
            evaluation_passed,
            summary,
            stability,
            trials,
        }
    }

    fn validate(&self, trials_per_task: u32) -> Result<()> {
        let context = format!("evaluation task {}", self.task_id);
        validate_nonempty_unique(&self.capabilities, &format!("{context} capabilities"))?;
        validate_nonempty_unique(
            &self.required_tool_capabilities,
            &format!("{context} required_tool_capabilities"),
        )?;
        if self.trials.len() != usize::try_from(trials_per_task).unwrap_or(usize::MAX) {
            return Err(validation_error(format!(
                "{context} trial count must match trials_per_task"
            )));
        }
        for (index, trial) in self.trials.iter().enumerate() {
            trial.validate(&self.task_id)?;
            if trial.trial != u32::try_from(index + 1).unwrap_or(u32::MAX) {
                return Err(validation_error(format!(
                    "{context} trials must be ordered contiguously from one"
                )));
            }
        }
        let expected_summary = EvaluationTaskSummary::from_trials(&self.trials);
        if self.summary != expected_summary {
            return Err(validation_error(format!(
                "{context} summary must match trials"
            )));
        }
        let expected_status = aggregate_status(
            &self
                .trials
                .iter()
                .map(|trial| trial.status)
                .collect::<Vec<_>>(),
        );
        if self.status != expected_status {
            return Err(validation_error(format!(
                "{context} status must match trials"
            )));
        }
        validate_evaluation_blocker(self.status, self.blocker.as_ref(), &context)?;
        if self.evaluation_passed != self.trials.iter().all(|trial| trial.evaluation_passed) {
            return Err(validation_error(format!(
                "{context} evaluation_passed must match all trials"
            )));
        }
        let expected_stability = EvaluationStabilitySummary::from_trials(&self.trials);
        if self.stability != expected_stability {
            return Err(validation_error(format!(
                "{context} stability must match trials"
            )));
        }
        self.stability.validate(&context, trials_per_task)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 整个 Evaluation run 的任务/ trial 指标和门禁结果。
pub struct EvaluationRunSummary {
    pub task_count: u32,
    pub trials_per_task: u32,
    pub trial_count: u32,
    pub completed_trial_count: u32,
    pub failed_trial_count: u32,
    pub blocked_trial_count: u32,
    pub agent_scored_trial_count: u32,
    pub agent_completed_count: u32,
    pub agent_failed_count: u32,
    /// 满足任务定义完成条件的 task 数；task rate 的分母是所有选定 task。
    pub task_success_count: u32,
    /// 所有 task 的 trial success 数，仅用于稳定性/模型波动观测。
    pub trial_success_count: u32,
    /// 非 blocked trial 中满足完成条件的比例，仅用于稳定性/模型波动观测。
    pub trial_success_rate_basis_points: u32,
    /// 满足全部 trial 完成条件的 task 比例，唯一用于核心能力门禁。
    pub task_success_rate_basis_points: u32,
    /// 是否达到核心 task success rate 门槛。
    pub meets_core_task_success_threshold: bool,
}

impl EvaluationRunSummary {
    pub fn from_tasks(tasks: &[EvaluationTaskResult], trials_per_task: u32) -> Self {
        let task_count = u32::try_from(tasks.len()).unwrap_or(u32::MAX);
        let trial_count = tasks.iter().map(|task| task.summary.trial_count).sum();
        let completed_trial_count = tasks
            .iter()
            .map(|task| task.summary.completed_trial_count)
            .sum();
        let failed_trial_count = tasks
            .iter()
            .map(|task| task.summary.failed_trial_count)
            .sum();
        let blocked_trial_count = tasks
            .iter()
            .map(|task| task.summary.blocked_trial_count)
            .sum();
        let agent_scored_trial_count = tasks
            .iter()
            .map(|task| task.summary.agent_scored_trial_count)
            .sum();
        let agent_completed_count = tasks
            .iter()
            .map(|task| task.summary.agent_completed_count)
            .sum();
        let agent_failed_count = tasks
            .iter()
            .map(|task| task.summary.agent_failed_count)
            .sum();
        let task_success_count = count_tasks(tasks, |task| task.evaluation_passed);
        let trial_success_count: u32 = tasks
            .iter()
            .map(|task| task.summary.trial_success_count)
            .fold(0, u32::saturating_add);
        let trial_success_rate_basis_points =
            rate_basis_points(trial_success_count, agent_scored_trial_count);
        let task_success_rate_basis_points = rate_basis_points(task_success_count, task_count);
        Self {
            task_count,
            trials_per_task,
            trial_count,
            completed_trial_count,
            failed_trial_count,
            blocked_trial_count,
            agent_scored_trial_count,
            agent_completed_count,
            agent_failed_count,
            task_success_count,
            trial_success_count,
            trial_success_rate_basis_points,
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

fn rate_basis_points(success_count: u32, denominator: u32) -> u32 {
    if denominator == 0 {
        0
    } else {
        success_count
            .saturating_mul(10_000)
            .checked_div(denominator)
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 一个 Evaluation run 的稳定结果。
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
    /// 从已完成的 task 结果构造唯一的 run-level 状态和指标投影。
    pub fn from_tasks(
        run_id: RunId,
        trials_per_task: u32,
        tasks: Vec<EvaluationTaskResult>,
    ) -> Self {
        let status = aggregate_status(&tasks.iter().map(|task| task.status).collect::<Vec<_>>());
        let blocker = (status == EvaluationStatus::Blocked)
            .then(|| tasks.iter().find_map(|task| task.blocker.clone()))
            .flatten();
        let evaluation_passed =
            !tasks.is_empty() && tasks.iter().all(|task| task.evaluation_passed);
        Self {
            schema_version: EvaluationResultSchemaVersion::V7,
            run_id,
            status,
            blocker,
            evaluation_passed,
            summary: EvaluationRunSummary::from_tasks(&tasks, trials_per_task),
            tasks,
        }
    }

    pub fn from_json_str(json: &str) -> Result<Self> {
        let mut value: Value = serde_json::from_str(json)?;
        let actual = value
            .get("schema_version")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let migrated = actual == PREVIOUS_RESULT_SCHEMA_VERSION;
        match actual {
            RESULT_SCHEMA_VERSION => {}
            PREVIOUS_RESULT_SCHEMA_VERSION => migrate_result_v6(&mut value)?,
            _ => {
                return Err(EvaluationError::UnsupportedSchemaVersion {
                    expected: RESULT_SCHEMA_VERSION,
                    actual: actual.to_string(),
                });
            }
        }
        let result: Self = serde_json::from_value(value)?;
        let result = if migrated {
            let tasks = result
                .tasks
                .into_iter()
                .map(|task| {
                    EvaluationTaskResult::from_trials(
                        task.task_id,
                        task.capabilities,
                        task.required_tool_capabilities,
                        task.trials,
                    )
                })
                .collect::<Vec<_>>();
            EvaluationResult::from_tasks(result.run_id, result.summary.trials_per_task, tasks)
        } else {
            result
        };
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<()> {
        if self.tasks.is_empty() || self.summary.trials_per_task == 0 {
            return Err(validation_error(
                "evaluation result requires tasks and a positive trials_per_task",
            ));
        }
        let mut task_ids = BTreeSet::new();
        for task in &self.tasks {
            task.validate(self.summary.trials_per_task)?;
            if !task_ids.insert(task.task_id.clone()) {
                return Err(EvaluationError::DuplicateTaskId(task.task_id.clone()));
            }
        }
        let expected_summary =
            EvaluationRunSummary::from_tasks(&self.tasks, self.summary.trials_per_task);
        if self.summary != expected_summary {
            return Err(validation_error(
                "evaluation run summary must match trial results",
            ));
        }
        let expected_status = aggregate_status(
            &self
                .tasks
                .iter()
                .map(|task| task.status)
                .collect::<Vec<_>>(),
        );
        if self.status != expected_status {
            return Err(validation_error(
                "evaluation run status must match task results",
            ));
        }
        validate_evaluation_blocker(self.status, self.blocker.as_ref(), "evaluation run")?;
        if self.evaluation_passed != self.tasks.iter().all(|task| task.evaluation_passed) {
            return Err(validation_error(
                "evaluation run evaluation_passed must equal all task values",
            ));
        }
        Ok(())
    }
}

fn aggregate_status(statuses: &[EvaluationStatus]) -> EvaluationStatus {
    if statuses
        .iter()
        .all(|status| *status == EvaluationStatus::Completed)
    {
        EvaluationStatus::Completed
    } else if statuses.contains(&EvaluationStatus::Failed) {
        EvaluationStatus::Failed
    } else {
        EvaluationStatus::Blocked
    }
}

fn count_trials(
    trials: &[EvaluationTrialResult],
    predicate: impl Fn(&EvaluationTrialResult) -> bool,
) -> u32 {
    u32::try_from(trials.iter().filter(|trial| predicate(trial)).count()).unwrap_or(u32::MAX)
}

fn migrate_result_v6(value: &mut Value) -> Result<()> {
    let tasks = value
        .get_mut("tasks")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| validation_error("evaluation.result/v6 tasks must be an array"))?;
    for task in tasks {
        let task_summary = task
            .get_mut("summary")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                validation_error("evaluation.result/v6 task summary must be an object")
            })?;
        task_summary.remove("evaluation_passed_count");
        task_summary.insert("trial_success_count".to_string(), json!(0));
    }

    let summary = value
        .get_mut("summary")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| validation_error("evaluation.result/v6 summary must be an object"))?;
    summary.remove("evaluation_passed_count");
    summary.insert("task_success_count".to_string(), json!(0));
    summary.insert("trial_success_count".to_string(), json!(0));
    summary.insert("trial_success_rate_basis_points".to_string(), json!(0));
    value["schema_version"] = Value::String(RESULT_SCHEMA_VERSION.to_string());
    Ok(())
}

fn validate_nonempty_unique<T: Ord + Clone>(values: &[T], context: &str) -> Result<()> {
    if values.is_empty() || values.iter().cloned().collect::<BTreeSet<_>>().len() != values.len() {
        return Err(validation_error(format!(
            "{context} must be non-empty and unique"
        )));
    }
    Ok(())
}

fn validate_evaluation_blocker(
    status: EvaluationStatus,
    blocker: Option<&EvaluationBlocker>,
    context: &str,
) -> Result<()> {
    validate_blocker(status == EvaluationStatus::Blocked, blocker, context)
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
