//! Typed Evaluation report projection.
//!
//! `EvaluationResult` and `EvaluationEvidence` remain the stable gate and evidence contracts.
//! This module owns only the human/development report projection. Every metric is either present
//! or explicitly absent because no producer exists or it was not observed.

use std::collections::BTreeSet;

use crate::{
    BlockerKind, EvaluationBlocker, EvaluationStatus, EvaluationTaskResult, Result, RunId,
    TASK_DIMENSION_SUCCESS_THRESHOLD_BASIS_POINTS, TaskId, validation_error,
};
use serde::{Deserialize, Serialize};

/// Current formal report schema identifier.
pub const REPORT_SCHEMA_VERSION: &str = "evaluation.report/v1";

/// Stable report schema version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvaluationReportSchemaVersion {
    #[serde(rename = "evaluation.report/v1")]
    V1,
}

/// A metric value with an explicit producer/availability state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MetricValue<T> {
    Available { value: T },
    Unavailable { reason: MetricUnavailableReason },
}

fn unavailable_metric_statistics() -> MetricValue<MetricStatistics> {
    MetricValue::unavailable(MetricUnavailableReason::NoProducer)
}

impl<T> MetricValue<T> {
    pub const fn available(value: T) -> Self {
        Self::Available { value }
    }

    pub const fn unavailable(reason: MetricUnavailableReason) -> Self {
        Self::Unavailable { reason }
    }

    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }
}

/// Why a report metric is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricUnavailableReason {
    NoProducer,
    NotObserved,
}

/// Deterministic distribution summary used by report-local aggregates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricStatistics {
    pub count: u64,
    pub sum: u64,
    pub min: u64,
    pub max: u64,
    pub mean: f64,
    pub p50: u64,
    pub p95: u64,
}

impl MetricStatistics {
    pub fn from_values(values: &[u64]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let count = u64::try_from(sorted.len()).ok()?;
        let sum = sorted.iter().copied().try_fold(0u64, u64::checked_add)?;
        let mean = sum as f64 / count as f64;
        Some(Self {
            count,
            sum,
            min: sorted[0],
            max: *sorted.last()?,
            mean,
            p50: nearest_rank(&sorted, 50),
            p95: nearest_rank(&sorted, 95),
        })
    }

    fn validate(&self, context: &str) -> Result<()> {
        if self.count == 0
            || self.min > self.max
            || self.p50 < self.min
            || self.p50 > self.max
            || self.p95 < self.min
            || self.p95 > self.max
            || !self.mean.is_finite()
            || self.mean < self.min as f64
            || self.mean > self.max as f64
        {
            return Err(validation_error(format!(
                "{context} must contain finite statistics for at least one sample"
            )));
        }
        Ok(())
    }
}

fn nearest_rank(sorted: &[u64], percentile: u64) -> u64 {
    let count = sorted.len() as u64;
    let rank = (count.saturating_mul(percentile).saturating_add(99)) / 100;
    let index = usize::try_from(rank.saturating_sub(1)).unwrap_or(sorted.len() - 1);
    sorted[index.min(sorted.len() - 1)]
}

/// Ratio represented without a floating point denominator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricRatio {
    pub numerator: u64,
    pub denominator: u64,
    pub basis_points: u32,
}

impl MetricRatio {
    pub fn new(numerator: u64, denominator: u64) -> Option<Self> {
        if denominator == 0 || numerator > denominator {
            return None;
        }
        let basis_points = numerator
            .checked_mul(10_000)
            .and_then(|value| value.checked_div(denominator))
            .and_then(|value| u32::try_from(value).ok())?;
        Some(Self {
            numerator,
            denominator,
            basis_points,
        })
    }

    fn validate(&self, context: &str) -> Result<()> {
        if self.denominator == 0
            || self.numerator > self.denominator
            || self.basis_points > 10_000
            || self.basis_points
                != self
                    .numerator
                    .checked_mul(10_000)
                    .and_then(|value| value.checked_div(self.denominator))
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or(u32::MAX)
        {
            return Err(validation_error(format!("{context} ratio is invalid")));
        }
        Ok(())
    }
}

/// Run-level dimension projection.  Values are copied from Result/v9; they are not re-reduced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationDimensions {
    /// End-to-end outcome of the selected model, Harness, and task; not a Harness-only score.
    pub functional_task_success: bool,
    pub functional_task_success_count: u32,
    pub functional_task_count: u32,
    pub functional_task_success_rate_basis_points: u32,
    pub agent_protocol_success: bool,
    pub agent_protocol_success_count: u32,
    pub agent_protocol_task_count: u32,
    pub agent_protocol_success_rate_basis_points: u32,
    pub sandbox_security_success: bool,
    pub sandbox_security_success_count: u32,
    pub sandbox_security_task_count: u32,
    pub sandbox_security_success_rate_basis_points: u32,
}

impl EvaluationDimensions {
    fn validate(&self) -> Result<()> {
        for (name, count, denominator, rate) in [
            (
                "functional_task_success",
                self.functional_task_success_count,
                self.functional_task_count,
                self.functional_task_success_rate_basis_points,
            ),
            (
                "agent_protocol_success",
                self.agent_protocol_success_count,
                self.agent_protocol_task_count,
                self.agent_protocol_success_rate_basis_points,
            ),
            (
                "sandbox_security_success",
                self.sandbox_security_success_count,
                self.sandbox_security_task_count,
                self.sandbox_security_success_rate_basis_points,
            ),
        ] {
            if denominator == 0 || count > denominator || rate > 10_000 {
                return Err(validation_error(format!(
                    "report dimension {name} is invalid"
                )));
            }
        }
        Ok(())
    }
}

/// Stable run status and gate projection copied from Result/v9.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationSystemResult {
    pub status: EvaluationStatus,
    pub evaluation_passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<EvaluationBlocker>,
}

/// Timing metrics that can be attributed to Evaluation or AgentLoop execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimingMetrics {
    pub run_duration_ms: MetricValue<u64>,
    pub trial_duration_ms: MetricValue<MetricStatistics>,
    pub source_preparation_duration_ms: MetricValue<MetricStatistics>,
    pub setup_duration_ms: MetricValue<MetricStatistics>,
    pub baseline_duration_ms: MetricValue<MetricStatistics>,
    pub agent_duration_ms: MetricValue<MetricStatistics>,
    pub local_overhead_duration_ms: MetricValue<MetricStatistics>,
    pub public_duration_ms: MetricValue<MetricStatistics>,
    pub hidden_duration_ms: MetricValue<MetricStatistics>,
    pub turn_duration_ms: MetricValue<MetricStatistics>,
    pub tool_duration_ms: MetricValue<MetricStatistics>,
}

/// Provider completion and capability-probe usage/attempt aggregates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderUsageMetrics {
    pub completion_attempts: MetricValue<u64>,
    pub completion_retries: MetricValue<u64>,
    pub completion_latency_ms: MetricValue<MetricStatistics>,
    pub probe_attempts: MetricValue<u64>,
    pub probe_retries: MetricValue<u64>,
    pub probe_latency_ms: MetricValue<MetricStatistics>,
    #[serde(default = "unavailable_metric_statistics")]
    pub time_to_first_token_ms: MetricValue<MetricStatistics>,
    pub input_tokens: MetricValue<u64>,
    pub noncached_input_tokens: MetricValue<u64>,
    pub cached_input_tokens: MetricValue<u64>,
    pub output_tokens: MetricValue<u64>,
    pub reasoning_tokens: MetricValue<u64>,
    pub total_tokens: MetricValue<u64>,
}

/// Cache observations remain explicitly named by producer without a nested DTO hierarchy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheMetrics {
    pub capability_hits: MetricValue<u64>,
    pub capability_misses: MetricValue<u64>,
    pub capability_hit_ratio: MetricValue<MetricRatio>,
    pub source_template_hits: MetricValue<u64>,
    pub source_template_misses: MetricValue<u64>,
    pub source_template_materialization_latency_ms: MetricValue<MetricStatistics>,
}

/// Control-loop counters that explain AgentLoop behavior without affecting the pass gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlLoopMetrics {
    pub model_turns: MetricValue<u64>,
    pub tool_calls: MetricValue<u64>,
    pub invalid_tool_calls: MetricValue<u64>,
    pub repeated_tool_calls: MetricValue<u64>,
    pub repair_attempts: MetricValue<u64>,
    pub completion_rejections: MetricValue<u64>,
    pub compactions: MetricValue<u64>,
    pub approval_count: MetricValue<u64>,
    pub verification_required_commands: MetricValue<u64>,
    pub verification_satisfied_commands: MetricValue<u64>,
}

/// All non-gating Harness efficiency metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationMetrics {
    pub timing: TimingMetrics,
    pub provider_usage: ProviderUsageMetrics,
    pub cache: CacheMetrics,
    pub control_loop: ControlLoopMetrics,
}

/// Typed owner of a failed trial or evaluation stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureOwner {
    Harness,
    Model,
    Provider,
    Environment,
    Sandbox,
    Evaluation,
}

/// Stable stage source for failure attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureStage {
    Setup,
    Baseline,
    Agent,
    Tool,
    Turn,
    Public,
    Hidden,
    Repair,
    Completion,
    Compaction,
    Evaluation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureAttribution {
    pub owner: FailureOwner,
    pub stage: FailureStage,
    pub task_id: Option<TaskId>,
    pub trial: Option<u32>,
    pub code: Option<String>,
    pub message: String,
}

/// Typed `evaluation.report/v1` artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationReport {
    pub schema: EvaluationReportSchemaVersion,
    pub run_id: RunId,
    pub manifest: String,
    pub runner: String,
    pub max_workers: usize,
    pub dimensions: EvaluationDimensions,
    pub system_result: EvaluationSystemResult,
    pub metrics: EvaluationMetrics,
    pub failure_attribution: Vec<FailureAttribution>,
    /// Task results are the authoritative Result/v9 projection; the report does not copy them
    /// into a second diagnostics DTO hierarchy.
    pub tasks: Vec<EvaluationTaskResult>,
}

impl EvaluationReport {
    pub fn from_json_str(json: &str) -> Result<Self> {
        let payload: serde_json::Value = serde_json::from_str(json)?;
        let actual = payload
            .get("schema")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if actual != REPORT_SCHEMA_VERSION {
            return Err(crate::EvaluationError::UnsupportedSchemaVersion {
                expected: REPORT_SCHEMA_VERSION,
                actual: actual.to_string(),
            });
        }
        let report: Self = serde_json::from_str(json)?;
        report.validate()?;
        Ok(report)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != EvaluationReportSchemaVersion::V1 {
            return Err(validation_error("unsupported evaluation report schema"));
        }
        self.dimensions.validate()?;
        if self.system_result.evaluation_passed
            != (self.dimensions.functional_task_success
                && self.dimensions.agent_protocol_success
                && self.dimensions.sandbox_security_success)
        {
            return Err(validation_error(
                "evaluation report system_result gate must equal dimension conjunction",
            ));
        }
        let mut task_ids = BTreeSet::new();
        for task in &self.tasks {
            task.validate(u32::try_from(task.trials.len()).unwrap_or(u32::MAX))?;
            if !task_ids.insert(task.task_id.clone()) {
                return Err(validation_error(format!(
                    "duplicate evaluation report task {}",
                    task.task_id
                )));
            }
        }
        let expected_task_count = u32::try_from(task_ids.len()).unwrap_or(u32::MAX);
        validate_report_dimensions(&self.dimensions, &self.tasks, expected_task_count)?;
        if self.tasks.is_empty() {
            if self.system_result.status != EvaluationStatus::Blocked
                || self.system_result.blocker.is_none()
            {
                return Err(validation_error(
                    "evaluation report without sampled tasks must be blocked with a blocker",
                ));
            }
        } else {
            let expected_status = aggregate_task_status(&self.tasks);
            if self.system_result.status != expected_status {
                return Err(validation_error(
                    "evaluation report system status must match task statuses",
                ));
            }
        }
        if (self.system_result.status == EvaluationStatus::Blocked)
            != self.system_result.blocker.is_some()
        {
            return Err(validation_error(
                "evaluation report system blocker must match blocked status",
            ));
        }
        for failure in &self.failure_attribution {
            if failure.message.trim().is_empty() {
                return Err(validation_error(
                    "evaluation report failure attribution message must not be empty",
                ));
            }
            if failure.trial == Some(0) {
                return Err(validation_error(
                    "evaluation report failure attribution trial must be positive",
                ));
            }
        }
        validate_metrics(&self.metrics)?;
        Ok(())
    }
}

fn validate_report_dimensions(
    dimensions: &EvaluationDimensions,
    tasks: &[EvaluationTaskResult],
    expected_task_count: u32,
) -> Result<()> {
    let functional_success_count = count_task_dimension(tasks, |task| task.functional_task_success);
    let agent_success_count = count_task_dimension(tasks, |task| task.agent_protocol_success);
    let sandbox_success_count = count_task_dimension(tasks, |task| task.sandbox_security_success);
    let expected_functional_rate = rate_basis_points(functional_success_count, expected_task_count);
    let expected_agent_rate = rate_basis_points(agent_success_count, expected_task_count);
    let expected_sandbox_rate = rate_basis_points(sandbox_success_count, expected_task_count);
    if dimensions.functional_task_count != expected_task_count
        || dimensions.agent_protocol_task_count != expected_task_count
        || dimensions.sandbox_security_task_count != expected_task_count
        || dimensions.functional_task_success_count != functional_success_count
        || dimensions.agent_protocol_success_count != agent_success_count
        || dimensions.sandbox_security_success_count != sandbox_success_count
        || dimensions.functional_task_success_rate_basis_points != expected_functional_rate
        || dimensions.agent_protocol_success_rate_basis_points != expected_agent_rate
        || dimensions.sandbox_security_success_rate_basis_points != expected_sandbox_rate
        || dimensions.functional_task_success
            != (expected_functional_rate >= TASK_DIMENSION_SUCCESS_THRESHOLD_BASIS_POINTS)
        || dimensions.agent_protocol_success
            != (expected_agent_rate >= TASK_DIMENSION_SUCCESS_THRESHOLD_BASIS_POINTS)
        || dimensions.sandbox_security_success
            != (expected_task_count > 0 && sandbox_success_count == expected_task_count)
    {
        return Err(validation_error(
            "evaluation report dimensions must match task aggregation",
        ));
    }
    Ok(())
}

fn count_task_dimension(
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

fn aggregate_task_status(tasks: &[EvaluationTaskResult]) -> EvaluationStatus {
    if tasks
        .iter()
        .all(|task| task.status == EvaluationStatus::Completed)
    {
        EvaluationStatus::Completed
    } else if tasks
        .iter()
        .any(|task| task.status == EvaluationStatus::Failed)
    {
        EvaluationStatus::Failed
    } else {
        EvaluationStatus::Blocked
    }
}

fn validate_metrics(metrics: &EvaluationMetrics) -> Result<()> {
    validate_metric_statistics(&metrics.timing.trial_duration_ms, "trial duration")?;
    validate_metric_statistics(
        &metrics.timing.source_preparation_duration_ms,
        "source preparation duration",
    )?;
    validate_metric_statistics(&metrics.timing.setup_duration_ms, "setup duration")?;
    validate_metric_statistics(&metrics.timing.baseline_duration_ms, "baseline duration")?;
    validate_metric_statistics(&metrics.timing.agent_duration_ms, "agent duration")?;
    validate_metric_statistics(
        &metrics.timing.local_overhead_duration_ms,
        "local overhead duration",
    )?;
    validate_metric_statistics(&metrics.timing.public_duration_ms, "public duration")?;
    validate_metric_statistics(&metrics.timing.hidden_duration_ms, "hidden duration")?;
    validate_metric_statistics(&metrics.timing.turn_duration_ms, "turn duration")?;
    validate_metric_statistics(&metrics.timing.tool_duration_ms, "tool duration")?;
    validate_metric_statistics(
        &metrics.provider_usage.completion_latency_ms,
        "completion latency",
    )?;
    validate_metric_statistics(&metrics.provider_usage.probe_latency_ms, "probe latency")?;
    validate_metric_statistics(
        &metrics.provider_usage.time_to_first_token_ms,
        "provider time to first token",
    )?;
    validate_ratio(
        &metrics.cache.capability_hit_ratio,
        "capability cache hit ratio",
    )?;
    Ok(())
}

fn validate_metric_statistics(metric: &MetricValue<MetricStatistics>, context: &str) -> Result<()> {
    if let MetricValue::Available { value } = metric {
        value.validate(context)?;
    }
    Ok(())
}

fn validate_ratio(metric: &MetricValue<MetricRatio>, context: &str) -> Result<()> {
    if let MetricValue::Available { value } = metric {
        value.validate(context)?;
    }
    Ok(())
}

/// Map a Result/v9 blocker to the report's typed owner.
pub fn failure_owner_for_blocker(kind: BlockerKind) -> FailureOwner {
    match kind {
        BlockerKind::Environment | BlockerKind::WorkspacePreparation => FailureOwner::Environment,
        BlockerKind::ProviderConfiguration
        | BlockerKind::ProviderResponse
        | BlockerKind::ProviderAuthentication
        | BlockerKind::Network => FailureOwner::Provider,
        BlockerKind::Sandbox => FailureOwner::Sandbox,
        BlockerKind::AgentRuntime => FailureOwner::Harness,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_metric_is_tagged_and_never_encoded_as_zero() {
        let metric: MetricValue<u64> =
            MetricValue::unavailable(MetricUnavailableReason::NotObserved);
        let value = serde_json::to_value(&metric).expect("metric serializes");
        assert_eq!(value["state"], "unavailable");
        assert_eq!(value["reason"], "not_observed");
        assert!(value.get("value").is_none());
    }

    #[test]
    fn report_reader_requires_the_formal_schema_key() {
        let error = EvaluationReport::from_json_str(r#"{"schema_version":"evaluation.report/v1"}"#)
            .expect_err("legacy schema key must be rejected");
        assert!(matches!(
            error,
            crate::EvaluationError::UnsupportedSchemaVersion {
                expected: REPORT_SCHEMA_VERSION,
                ..
            }
        ));
    }
}
