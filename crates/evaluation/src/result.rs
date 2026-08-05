//! Evaluation trial/result 状态、有限统计、门禁汇总和 blocker 校验。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{
    EvaluationCapability, EvaluationError, RESULT_SCHEMA_VERSION, Result, RunId,
    TASK_DIMENSION_SUCCESS_THRESHOLD_BASIS_POINTS, TaskId, require_schema_version,
    validation_error,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Evaluation result 的 schema 版本。
pub enum EvaluationResultSchemaVersion {
    #[serde(rename = "evaluation.result/v9")]
    V9,
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
/// Evaluation run 或 trial 的稳定终态。
pub enum EvaluationStatus {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub kind: BlockerKind,
    pub message: String,
    /// Task identity when a run-level blocker was observed while preparing one task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
}

impl EvaluationBlocker {
    fn validate(&self, context: &str) -> Result<()> {
        if self
            .code
            .as_deref()
            .is_some_and(|code| code.trim().is_empty())
        {
            return Err(validation_error(format!(
                "{context} blocker.code must not be empty"
            )));
        }
        if self.message.trim().is_empty() {
            return Err(validation_error(format!(
                "{context} blocker.message must not be empty"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Stable result projection of the sandbox preflight outcome.
pub enum EvaluationSandboxPreflightOutcome {
    Supported,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Bounded fact for a platform-specific sandbox control.
pub enum EvaluationSandboxPreflightFact {
    Passed,
    Failed,
    NotApplicable,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Run-level sandbox facts captured before provider/trial sampling.
pub struct EvaluationSandboxPreflight {
    pub outcome: EvaluationSandboxPreflightOutcome,
    pub error_code: Option<String>,
    pub profile: String,
    pub backend: String,
    pub missing_capabilities: Vec<String>,
    pub os: String,
    pub arch: String,
    pub kernel: Option<String>,
    pub filesystem: Option<String>,
    pub overlayfs: EvaluationSandboxPreflightFact,
    pub user_namespace: EvaluationSandboxPreflightFact,
    pub mount_namespace: EvaluationSandboxPreflightFact,
    pub pid_namespace: EvaluationSandboxPreflightFact,
    pub network_namespace: EvaluationSandboxPreflightFact,
    pub no_new_privs: EvaluationSandboxPreflightFact,
    pub seccomp: EvaluationSandboxPreflightFact,
    pub landlock: EvaluationSandboxPreflightFact,
    pub transactional_workspace: EvaluationSandboxPreflightFact,
    pub network_denied: EvaluationSandboxPreflightFact,
    pub protected_paths: EvaluationSandboxPreflightFact,
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
    /// Public/hidden functional correctness, independent of AgentLoop protocol completion.
    pub functional_task_success: bool,
    /// AgentLoop lifecycle and terminal review contract outcome.
    pub agent_protocol_success: bool,
    /// Strict sandbox, network denial, evaluator protection, and no-fallback outcome.
    pub sandbox_security_success: bool,
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
        if self.functional_task_success
            && (self.stages.baseline.status != StageStatus::Passed || !self.tests_passed)
        {
            return Err(validation_error(format!(
                "{context} functional_task_success requires baseline and evaluator tests"
            )));
        }
        if self.agent_protocol_success
            && (!self.agent_completed || self.status == EvaluationStatus::Blocked)
        {
            return Err(validation_error(format!(
                "{context} agent_protocol_success requires a completed AgentLoop"
            )));
        }
        if self.sandbox_security_success
            && (self.evidence.strict_sandbox_command_count == 0
                || self.evidence.local_process_fallback_count != 0
                || self.evidence.local_process_fallback_unknown_count != 0)
        {
            return Err(validation_error(format!(
                "{context} sandbox_security_success requires strict sandbox and complete zero-fallback evidence"
            )));
        }
        if self.evaluation_passed
            != (self.functional_task_success
                && self.agent_protocol_success
                && self.sandbox_security_success)
        {
            return Err(validation_error(format!(
                "{context} evaluation_passed must equal the three success dimensions"
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
    pub functional_task_success_count: u32,
    pub functional_task_success_rate_basis_points: u32,
    pub agent_protocol_success_count: u32,
    pub agent_protocol_success_rate_basis_points: u32,
    pub sandbox_security_success_count: u32,
    pub sandbox_security_success_rate_basis_points: u32,
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
        // A blocked trial joins a dimension's denominator only with positive evidence for that
        // dimension; protocol success on a blocked trial is forbidden by trial validation.
        let functional_task_success_count =
            count_trials(trials, |trial| trial.functional_task_success);
        let functional_blocked_success_count = count_trials(trials, |trial| {
            trial.status == EvaluationStatus::Blocked && trial.functional_task_success
        });
        let functional_scored_trial_count =
            agent_scored_trial_count.saturating_add(functional_blocked_success_count);
        let agent_protocol_success_count =
            count_trials(trials, |trial| trial.agent_protocol_success);
        let sandbox_security_success_count =
            count_trials(trials, |trial| trial.sandbox_security_success);
        let sandbox_blocked_success_count = count_trials(trials, |trial| {
            trial.status == EvaluationStatus::Blocked && trial.sandbox_security_success
        });
        let sandbox_scored_trial_count =
            agent_scored_trial_count.saturating_add(sandbox_blocked_success_count);
        Self {
            trial_count,
            completed_trial_count,
            failed_trial_count,
            blocked_trial_count,
            agent_scored_trial_count,
            agent_completed_count,
            agent_failed_count: agent_scored_trial_count.saturating_sub(agent_completed_count),
            functional_task_success_count,
            functional_task_success_rate_basis_points: rate_basis_points(
                functional_task_success_count,
                functional_scored_trial_count,
            ),
            agent_protocol_success_count,
            agent_protocol_success_rate_basis_points: rate_basis_points(
                agent_protocol_success_count,
                agent_scored_trial_count,
            ),
            sandbox_security_success_count,
            sandbox_security_success_rate_basis_points: rate_basis_points(
                sandbox_security_success_count,
                sandbox_scored_trial_count,
            ),
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
    pub status: EvaluationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<EvaluationBlocker>,
    /// 任务成功：该任务的每个 trial 都满足任务定义的完成条件。
    pub evaluation_passed: bool,
    pub functional_task_success: bool,
    pub agent_protocol_success: bool,
    pub sandbox_security_success: bool,
    pub summary: EvaluationTaskSummary,
    pub stability: EvaluationStabilitySummary,
    pub trials: Vec<EvaluationTrialResult>,
}

impl EvaluationTaskResult {
    /// 从同一任务的独立 trial 构造不可手工覆盖的汇总。
    pub fn from_trials(
        task_id: TaskId,
        capabilities: Vec<EvaluationCapability>,
        trials: Vec<EvaluationTrialResult>,
    ) -> Self {
        let summary = EvaluationTaskSummary::from_trials(&trials);
        let status = aggregate_status(&trials.iter().map(|trial| trial.status).collect::<Vec<_>>());
        let blocker = (status == EvaluationStatus::Blocked)
            .then(|| trials.iter().find_map(|trial| trial.blocker.clone()))
            .flatten();
        let functional_task_success =
            !trials.is_empty() && trials.iter().all(|trial| trial.functional_task_success);
        let agent_protocol_success =
            !trials.is_empty() && trials.iter().all(|trial| trial.agent_protocol_success);
        let sandbox_security_success =
            !trials.is_empty() && trials.iter().all(|trial| trial.sandbox_security_success);
        let evaluation_passed =
            functional_task_success && agent_protocol_success && sandbox_security_success;
        let stability = EvaluationStabilitySummary::from_trials(&trials);
        Self {
            task_id,
            capabilities,
            status,
            blocker,
            evaluation_passed,
            functional_task_success,
            agent_protocol_success,
            sandbox_security_success,
            summary,
            stability,
            trials,
        }
    }

    pub(crate) fn validate(&self, trials_per_task: u32) -> Result<()> {
        let context = format!("evaluation task {}", self.task_id);
        validate_nonempty_unique(&self.capabilities, &format!("{context} capabilities"))?;
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
        if self.functional_task_success
            != self
                .trials
                .iter()
                .all(|trial| trial.functional_task_success)
            || self.agent_protocol_success
                != self.trials.iter().all(|trial| trial.agent_protocol_success)
            || self.sandbox_security_success
                != self
                    .trials
                    .iter()
                    .all(|trial| trial.sandbox_security_success)
            || self.evaluation_passed
                != (self.functional_task_success
                    && self.agent_protocol_success
                    && self.sandbox_security_success)
        {
            return Err(validation_error(format!(
                "{context} success dimensions must match all trials and their conjunction"
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
    /// Number of trials requested by the manifest, including runs blocked before sampling.
    pub configured_trial_count: u32,
    /// Number of trials that actually entered AgentLoop sampling.
    pub sampled_trial_count: u32,
    pub trial_count: u32,
    pub completed_trial_count: u32,
    pub failed_trial_count: u32,
    pub blocked_trial_count: u32,
    pub agent_scored_trial_count: u32,
    pub agent_completed_count: u32,
    pub agent_failed_count: u32,
    pub functional_task_success_count: u32,
    pub functional_task_success_rate_basis_points: u32,
    pub meets_functional_task_success_threshold: bool,
    pub agent_protocol_success_count: u32,
    pub agent_protocol_success_rate_basis_points: u32,
    pub meets_agent_protocol_success_threshold: bool,
    pub sandbox_security_success_count: u32,
    pub sandbox_security_success_rate_basis_points: u32,
    pub meets_sandbox_security_success_threshold: bool,
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
        let functional_task_success_count = count_tasks(tasks, |task| task.functional_task_success);
        let agent_protocol_success_count = count_tasks(tasks, |task| task.agent_protocol_success);
        let sandbox_security_success_count =
            count_tasks(tasks, |task| task.sandbox_security_success);
        Self {
            task_count,
            trials_per_task,
            configured_trial_count: task_count.saturating_mul(trials_per_task),
            sampled_trial_count: trial_count,
            trial_count,
            completed_trial_count,
            failed_trial_count,
            blocked_trial_count,
            agent_scored_trial_count,
            agent_completed_count,
            agent_failed_count,
            functional_task_success_count,
            functional_task_success_rate_basis_points: rate_basis_points(
                functional_task_success_count,
                task_count,
            ),
            meets_functional_task_success_threshold: rate_basis_points(
                functional_task_success_count,
                task_count,
            )
                >= TASK_DIMENSION_SUCCESS_THRESHOLD_BASIS_POINTS,
            agent_protocol_success_count,
            agent_protocol_success_rate_basis_points: rate_basis_points(
                agent_protocol_success_count,
                task_count,
            ),
            meets_agent_protocol_success_threshold: rate_basis_points(
                agent_protocol_success_count,
                task_count,
            )
                >= TASK_DIMENSION_SUCCESS_THRESHOLD_BASIS_POINTS,
            sandbox_security_success_count,
            sandbox_security_success_rate_basis_points: rate_basis_points(
                sandbox_security_success_count,
                task_count,
            ),
            meets_sandbox_security_success_threshold: task_count > 0
                && sandbox_security_success_count == task_count,
        }
    }

    /// Construct a run summary for a blocker observed before trial sampling.
    pub fn for_preflight_blocker(task_count: u32, trials_per_task: u32) -> Self {
        Self {
            task_count,
            trials_per_task,
            configured_trial_count: task_count.saturating_mul(trials_per_task),
            sampled_trial_count: 0,
            trial_count: 0,
            ..Self::default()
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_preflight: Option<EvaluationSandboxPreflight>,
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
        let summary = EvaluationRunSummary::from_tasks(&tasks, trials_per_task);
        let evaluation_passed = summary.meets_functional_task_success_threshold
            && summary.meets_agent_protocol_success_threshold
            && summary.meets_sandbox_security_success_threshold;
        Self {
            schema_version: EvaluationResultSchemaVersion::V9,
            run_id,
            status,
            blocker,
            evaluation_passed,
            summary,
            tasks,
            sandbox_preflight: None,
        }
    }

    /// Construct a run-level environment blocker without fabricating sampled trials.
    pub fn blocked_by_sandbox_preflight(
        run_id: RunId,
        task_count: u32,
        trials_per_task: u32,
        blocker: EvaluationBlocker,
        sandbox_preflight: EvaluationSandboxPreflight,
    ) -> Self {
        Self::blocked_before_sampling(
            run_id,
            task_count,
            trials_per_task,
            blocker,
            sandbox_preflight,
        )
    }

    /// Construct a run-level blocker observed before any trial enters AgentLoop.
    ///
    /// The selected task identities remain in the manifest/evidence projection, while the stable
    /// result intentionally keeps `tasks` empty because no task or trial was sampled.
    pub fn blocked_before_sampling(
        run_id: RunId,
        task_count: u32,
        trials_per_task: u32,
        blocker: EvaluationBlocker,
        sandbox_preflight: EvaluationSandboxPreflight,
    ) -> Self {
        Self {
            schema_version: EvaluationResultSchemaVersion::V9,
            run_id,
            status: EvaluationStatus::Blocked,
            blocker: Some(blocker),
            evaluation_passed: false,
            summary: EvaluationRunSummary::for_preflight_blocker(task_count, trials_per_task),
            tasks: Vec::new(),
            sandbox_preflight: Some(sandbox_preflight),
        }
    }

    /// Whether this result is a validated run-level zero-sampling projection.
    pub(crate) fn is_blocked_before_sampling(&self) -> bool {
        self.tasks.is_empty()
            && self.status == EvaluationStatus::Blocked
            && !self.evaluation_passed
            && self.summary.task_count > 0
            && self.summary.trials_per_task > 0
            && self.summary.configured_trial_count > 0
            && self.summary.sampled_trial_count == 0
            && self.summary.trial_count == 0
            && self
                .blocker
                .as_ref()
                .is_some_and(|blocker| is_pre_sampling_blocker_kind(blocker.kind))
    }

    pub fn from_json_str(json: &str) -> Result<Self> {
        require_schema_version(json, RESULT_SCHEMA_VERSION)?;
        let result: Self = serde_json::from_str(json)?;
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<()> {
        if self.summary.trials_per_task == 0 {
            return Err(validation_error(
                "evaluation result requires tasks and a positive trials_per_task",
            ));
        }
        let Some(preflight) = self.sandbox_preflight.as_ref() else {
            return Err(validation_error(
                "evaluation result requires sandbox preflight evidence",
            ));
        };
        validate_sandbox_preflight(preflight, "evaluation result sandbox_preflight")?;
        if self.tasks.is_empty() {
            let expected_summary = EvaluationRunSummary::for_preflight_blocker(
                self.summary.task_count,
                self.summary.trials_per_task,
            );
            let Some(blocker) = self.blocker.as_ref() else {
                return Err(validation_error(
                    "empty evaluation result requires one pre-sampling blocker with zero sampled trials",
                ));
            };
            if self.summary != expected_summary
                || self.status != EvaluationStatus::Blocked
                || self.evaluation_passed
                || self.summary.task_count == 0
                || !is_pre_sampling_blocker_kind(blocker.kind)
            {
                return Err(validation_error(
                    "empty evaluation result must be one pre-sampling blocker with zero sampled trials",
                ));
            }
            validate_pre_sampling_blocker(blocker, preflight)?;
            return Ok(());
        }
        if preflight.outcome != EvaluationSandboxPreflightOutcome::Supported {
            return Err(validation_error(
                "sampled evaluation result cannot carry an unsupported sandbox preflight",
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
        let expected_evaluation_passed = self.summary.meets_functional_task_success_threshold
            && self.summary.meets_agent_protocol_success_threshold
            && self.summary.meets_sandbox_security_success_threshold;
        if self.evaluation_passed != expected_evaluation_passed {
            return Err(validation_error(
                "evaluation run evaluation_passed must equal the functional, protocol, and sandbox gates",
            ));
        }
        Ok(())
    }
}

fn is_pre_sampling_blocker_kind(kind: BlockerKind) -> bool {
    matches!(
        kind,
        BlockerKind::Environment
            | BlockerKind::WorkspacePreparation
            | BlockerKind::ProviderConfiguration
            | BlockerKind::Network
            | BlockerKind::Sandbox
    )
}

fn validate_pre_sampling_blocker(
    blocker: &EvaluationBlocker,
    preflight: &EvaluationSandboxPreflight,
) -> Result<()> {
    blocker.validate("evaluation run")?;
    if blocker
        .code
        .as_deref()
        .is_none_or(|code| code.trim().is_empty())
    {
        return Err(validation_error(
            "evaluation run pre-sampling blocker requires a non-empty code",
        ));
    }
    if preflight.outcome == EvaluationSandboxPreflightOutcome::Unsupported
        && (blocker.kind != BlockerKind::Environment
            || !blocker
                .code
                .as_deref()
                .is_some_and(|code| code.starts_with("sandbox_preflight_"))
            || blocker.code.as_deref() != preflight.error_code.as_deref())
    {
        return Err(validation_error(
            "unsupported sandbox preflight must be the run-level blocker",
        ));
    }
    Ok(())
}

pub(crate) fn validate_sandbox_preflight(
    preflight: &EvaluationSandboxPreflight,
    context: &str,
) -> Result<()> {
    const MAX_FACT_CHARS: usize = 128;
    let valid_text =
        |value: &str| !value.trim().is_empty() && value.chars().count() <= MAX_FACT_CHARS;
    let missing = preflight
        .missing_capabilities
        .iter()
        .collect::<BTreeSet<_>>();
    if preflight.profile != "workspace_write_network_denied"
        || !valid_text(&preflight.backend)
        || !valid_text(&preflight.os)
        || !valid_text(&preflight.arch)
        || preflight
            .kernel
            .as_deref()
            .is_some_and(|value| !valid_text(value))
        || preflight
            .filesystem
            .as_deref()
            .is_some_and(|value| !valid_text(value))
        || preflight
            .error_code
            .as_deref()
            .is_some_and(|code| !valid_text(code) || !code.starts_with("sandbox_preflight_"))
        || missing.len() != preflight.missing_capabilities.len()
        || preflight
            .missing_capabilities
            .iter()
            .any(|capability| !valid_text(capability))
    {
        return Err(validation_error(format!(
            "{context} must contain bounded backend, platform, profile, and error fields"
        )));
    }
    match preflight.outcome {
        EvaluationSandboxPreflightOutcome::Supported
            if preflight.error_code.is_some()
                || !preflight.missing_capabilities.is_empty()
                || [
                    preflight.overlayfs,
                    preflight.user_namespace,
                    preflight.mount_namespace,
                    preflight.pid_namespace,
                    preflight.network_namespace,
                    preflight.no_new_privs,
                    preflight.seccomp,
                    preflight.landlock,
                    preflight.transactional_workspace,
                    preflight.network_denied,
                    preflight.protected_paths,
                ]
                .contains(&EvaluationSandboxPreflightFact::Unknown)
                || preflight.transactional_workspace != EvaluationSandboxPreflightFact::Passed
                || preflight.network_denied != EvaluationSandboxPreflightFact::Passed
                || preflight.protected_paths != EvaluationSandboxPreflightFact::Passed =>
        {
            return Err(validation_error(format!(
                "{context} supported outcome requires a fully verified strict contract"
            )));
        }
        EvaluationSandboxPreflightOutcome::Unsupported
            if preflight.error_code.is_none() || preflight.missing_capabilities.is_empty() =>
        {
            return Err(validation_error(format!(
                "{context} unsupported outcome requires error_code and missing capabilities"
            )));
        }
        _ => {}
    }
    Ok(())
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
