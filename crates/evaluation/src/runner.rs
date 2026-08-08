//! 开发期 Evaluation runner 的任务投影、Agent stage、验证证据与安全产物协调。
//!
//! 本模块只把 manifest 的可信内部命令和模型可见 command string 分开投影，
//! 并在固定 gate、sandbox 与 evidence 合同下汇总结果。

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use crate::{
    AgentStagePlan, AgentTaskProjection, BlockerKind, CacheMetrics, CommandExpectation,
    CommandSpec, ControlLoopMetrics, EvaluationBlocker, EvaluationDimensions,
    EvaluationEvidenceSummary, EvaluationManifest, EvaluationMetrics, EvaluationPromptStructure,
    EvaluationProviderEvidence, EvaluationReport, EvaluationReportSchemaVersion, EvaluationResult,
    EvaluationSandboxPreflight, EvaluationSandboxPreflightFact, EvaluationSandboxPreflightOutcome,
    EvaluationStageResults, EvaluationStatus, EvaluationSystemResult, EvaluationTaskResult,
    EvaluationTrialResult, FailureAttribution, FailureOwner, FailureStage, HarnessMetrics,
    MetricRatio, MetricStatistics, MetricUnavailableReason, MetricValue, PatchFormat,
    PlannedWorkspaceSource, ProviderUsageMetrics, RelativePath, RunId, StageResult, StageStatus,
    TaskId, TimingMetrics, VerificationStagePlan, WorkspacePlan, failure_owner_for_blocker,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use singularity_agent::{
    AgentLoop, AgentLoopEventSinkError, AgentLoopInput, AgentRecoveryMetrics, AgentStatus,
    ToolResultOccurrence,
};
use singularity_app_server::TraceProjector;
use singularity_core::{
    CancellationToken, Timestamp, bounded_stable_code, contains_sensitive_text,
    load_project_instructions,
};
use singularity_model::{
    ModelErrorCategory, ModelUsage, OpenAiProvider, Provider, ProviderApiProtocol,
    ProviderAttemptMetadata, ProviderCapabilityCacheLookupResult, ProviderCapabilityMetadata,
    ProviderCapabilityProfile, ProviderConfigSnapshot, ProviderDiagnostic, ProviderError,
    ProviderErrorStage, ProviderProtocolContract, ProviderProtocolNegotiation,
};
use singularity_policy::{ApprovalPolicy, PermissionProfileName, workspace_policy};
use singularity_protocol::{
    TraceEvent, TraceMetricAvailability, TraceMetricName, TraceMetricUnavailableReason,
    TraceMetrics, TraceProviderProtocol, TraceSpanKind, TraceSpanPhase, TraceSpanProjection,
    TraceSpanStatus, TraceToolStatus,
};
#[cfg(windows)]
use singularity_sandbox::{TrustedWorkspaceError, TrustedWorkspaceLease};
use singularity_tools::{
    COMMAND_TOOL as TOOL_COMMAND, CommandEnvironmentPolicy, CommandExecutionStatus, CommandRequest,
    CommandResult, CommandScriptRequest, CommandSemanticStatus, ExecutableAvailability, PATCH_TOOL,
    SandboxBackend, SandboxCapabilities, SandboxNetworkMode, SandboxPreflightFact,
    SandboxPreflightOutcome, SandboxPreflightReport, ToolBroker, ToolFailureKind, ToolRegistry,
    ToolResult, WorkspaceChangeSummary, WorkspaceMutation, WorkspaceTools, workspace_tool_entries,
};

mod command;
mod evidence;
mod recovery;
mod source_cache;
mod workspace;

use command::{
    CommandDiagnostic, command_blocker, command_is_strictly_sandboxed, command_succeeded,
    infrastructure_blocker, run_command_spec, run_raw_command,
    run_task_workspace_preflight_command, run_workspace_preparation_command,
    run_workspace_preparation_read_only_command, unchanged_command_succeeded,
};
use evidence::{
    agent_command_projection, build_evaluation_evidence, build_zero_sampling_evidence,
    canonical_json_digest, content_digest,
};
use recovery::{RecoveryAttempt, run_recovery_trial};
use source_cache::{SourceTemplateCache, SourceTemplateCacheStatus, SourceTemplatePreparation};
use workspace::{
    WorkspaceChangeEvidence, WorkspaceSnapshot, copy_tree_checked, copy_tree_for_preparation,
    evaluation_changed_paths, materialize_prepared_workspace, patch_evidence_digest,
    snapshot_workspace, workspace_change_evidence, workspace_root_identity,
    workspace_snapshot_digest,
};
const RUNNER_NAME: &str = "agent_loop";

const OUTPUT_ROOT_ENV: &str = "SINGULARITY_EVAL_OUTPUT_DIR";
const DEFAULT_AGENT_MAX_TURNS: u32 = 24;
const DEFAULT_COMMAND_TIMEOUT_SECONDS: u64 = 300;
const DEFAULT_SETUP_TIMEOUT_SECONDS: u64 = 900;
const GIT_TIMEOUT_SECONDS: u64 = 900;
const SOURCE_DIR: &str = "source";
const AGENT_DIR: &str = "agent";
const EVALUATOR_PATCH_FILE: &str = ".singularity-evaluator.patch";
const RESULT_FILE: &str = "result.json";
const REPORT_FILE: &str = "report.json";
const EVIDENCE_FILE: &str = "evidence.json";
const FAILURE_FILE: &str = "failure.json";
const PUBLICATION_DIR: &str = "publication";
const PUBLICATION_MANIFEST_FILE: &str = "publication.json";
const PUBLICATION_SCHEMA_VERSION: &str = "evaluation.publication/v1";
const FAILURE_SCHEMA_VERSION: &str = "evaluation.failure/v1";
const AGENT_TRACE_FILE: &str = "agent-trace.json";
const PATCH_EVIDENCE_FILE: &str = "patch-evidence.json";
const ARTIFACT_TEMP_FILE_ATTEMPTS: usize = 64;
const WINDOWS_MAX_PATH_CHARS: usize = 260;
const GIT_PACK_HEX: &str = "0123456789012345678901234567890123456789";
const CARGO_DEP_HEX: &str = "0123456789012345678901234567890123456789012345678901234567890123";

static ARTIFACT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 一次开发期 Evaluation 运行的输入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationRunParams {
    pub manifest: String,
    pub run_id: String,
    pub output_root: Option<String>,
    /// Number of independent trials that may execute at once.
    pub max_workers: usize,
    /// Inject a process-restart recovery into every Nth trial when explicitly enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_every: Option<NonZeroUsize>,
}

/// 一次开发期 Evaluation 运行的有限结果摘要。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationRunResult {
    pub run_id: String,
    pub manifest: String,
    pub runner: String,
    pub max_workers: usize,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
    pub tasks: Vec<EvaluationTaskResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_path: Option<String>,
    pub evaluation_passed: bool,
    /// Whether the published three-dimensional gate determines this run's CLI success.
    pub gate_applicable: bool,
}

/// Evaluation runner 绑定的严格 sandbox backend。
pub type SharedSandboxBackend = Arc<dyn SandboxBackend + Send + Sync>;

/// Selects the development runner's execution scope without changing the strict runtime path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvaluationRunMode {
    /// Execute every manifest task for the configured trial count and publish the gate artifacts.
    Full,
    /// Execute the manifest-declared feedback tasks once each without a pass gate or evidence.
    Feedback,
}

/// 将 Evaluation 自身的停止令牌绑定到既有 sandbox 执行边界。
///
/// 固定 evaluator command 仍复用 `command` 模块的请求构造和策略校验；这里只把其
/// `execute` 入口投影为 cancellable backend，避免 shutdown 只能取消 AgentLoop 而留下
/// baseline、setup 或 verification 命令继续运行。
struct CancellationAwareSandboxBackend {
    backend: SharedSandboxBackend,
    cancellation: CancellationToken,
}

impl SandboxBackend for CancellationAwareSandboxBackend {
    fn name(&self) -> &'static str {
        self.backend.name()
    }

    fn capabilities(&self) -> SandboxCapabilities {
        self.backend.capabilities()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        self.backend
            .execute_cancellable(request, &self.cancellation)
    }

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        self.backend
            .execute_script_cancellable(request, &self.cancellation)
    }

    fn execute_cancellable(
        &self,
        request: &CommandRequest,
        cancellation: &CancellationToken,
    ) -> CommandResult {
        self.backend.execute_cancellable(request, cancellation)
    }

    fn execute_script_cancellable(
        &self,
        request: &CommandScriptRequest,
        cancellation: &CancellationToken,
    ) -> CommandResult {
        self.backend
            .execute_script_cancellable(request, cancellation)
    }

    fn preflight(
        &self,
        workspace: &Path,
        cancellation: &CancellationToken,
    ) -> SandboxPreflightReport {
        self.backend.preflight(workspace, cancellation)
    }

    fn release_workspace_observation(&self, workspace: &Path) -> Result<(), String> {
        self.backend.release_workspace_observation(workspace)
    }

    fn probe_executable(
        &self,
        workspace: &Path,
        executable: &str,
        environment: &CommandEnvironmentPolicy,
    ) -> ExecutableAvailability {
        self.backend
            .probe_executable(workspace, executable, environment)
    }
}

fn cancellation_aware_sandbox_backend(
    backend: &SharedSandboxBackend,
    cancellation: &CancellationToken,
) -> SharedSandboxBackend {
    Arc::new(CancellationAwareSandboxBackend {
        backend: Arc::clone(backend),
        cancellation: cancellation.clone(),
    })
}

#[derive(Debug, Clone, Default)]
struct StageDiagnostics {
    message: Option<String>,
    commands: Vec<CommandDiagnostic>,
}

#[derive(Debug, Clone, Default)]
struct TaskDiagnostics {
    source_commands: Vec<CommandDiagnostic>,
    source_preparation_duration_ms: u64,
    source_tree_digest: Option<String>,
    source_template_expected: bool,
    source_template_cache_status: Option<SourceTemplateCacheStatus>,
    source_template_materialization_ms: u64,
    baseline: StageDiagnostics,
    agent: StageDiagnostics,
    public: StageDiagnostics,
    hidden: StageDiagnostics,
    trial_duration_ms: u64,
    baseline_duration_ms: u64,
    public_duration_ms: u64,
    hidden_duration_ms: u64,
    agent_setup_ms: Option<u64>,
    changed_files: Vec<String>,
    patch_evidence: Vec<WorkspaceChangeEvidence>,
    patch_digest: Option<String>,
    patch_evidence_path: Option<String>,
    model_turns: u32,
    tool_calls: u32,
    approval_count: u32,
    invalid_tool_call_count: u32,
    repeated_tool_call_count: u32,
    repair_attempt_count: u32,
    completion_rejection_count: u32,
    compaction_count: u32,
    verification_required_command_count: u32,
    verification_satisfied_command_count: u32,
    verification_observed: bool,
    provider_attempt_count: u32,
    provider_retry_count: u32,
    probe_attempt_count: u32,
    probe_retry_count: u32,
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    reasoning_tokens: u64,
    total_tokens: u64,
    provider_latency_ms: u64,
    probe_latency_ms: u64,
    provider_usage_available: bool,
    capability_cache_hit_count: u32,
    capability_cache_miss_count: u32,
    /// Number of explicit capability-cache lookup observations emitted by the provider trace.
    capability_cache_observation_count: u32,
    strict_sandbox_command_count: usize,
    /// AgentLoop/provider elapsed time between the before/after workspace snapshots.
    agent_duration_ms: u64,
    local_process_fallback_count: usize,
    local_process_fallback_unknown_count: usize,
    trace_path: Option<String>,
    error: Option<String>,
    provider_diagnostic: Option<ProviderDiagnostic>,
    prompt_structure: Option<EvaluationPromptStructure>,
    prompt_fingerprint: Option<String>,
    tool_schema_fingerprint: Option<String>,
    provider_evidence: Option<EvaluationProviderEvidence>,
    /// Count of model tool occurrences that changed evaluator integrity paths.
    /// `None` means the control-plane paths or an execution summary were not fully observed.
    verification_bypass_count: Option<u64>,
    /// Whether this trial was selected for explicit process-restart recovery injection.
    recovery_injected: bool,
    /// A conclusion is recorded only when the durable marker was observed and the resumed
    /// turn reached a terminal state. Marker-unavailable attempts remain outside the ratio.
    recovery_completed: Option<bool>,
}

struct StageExecution {
    result: StageResult,
    diagnostics: StageDiagnostics,
}

impl StageExecution {
    fn passed(commands: Vec<CommandDiagnostic>) -> Self {
        Self {
            result: stage_result(StageStatus::Passed, None),
            diagnostics: StageDiagnostics {
                message: None,
                commands,
            },
        }
    }

    fn failed(message: impl Into<String>, commands: Vec<CommandDiagnostic>) -> Self {
        let message = safe_text(message.into());
        Self {
            result: stage_result(StageStatus::Failed, None),
            diagnostics: StageDiagnostics {
                message: Some(message),
                commands,
            },
        }
    }

    fn blocked(blocker: EvaluationBlocker, commands: Vec<CommandDiagnostic>) -> Self {
        Self {
            result: stage_result(StageStatus::Blocked, Some(blocker.clone())),
            diagnostics: StageDiagnostics {
                message: Some(blocker.message),
                commands,
            },
        }
    }

    fn skipped(message: impl Into<String>) -> Self {
        Self {
            result: stage_result(StageStatus::Skipped, None),
            diagnostics: StageDiagnostics {
                message: Some(safe_text(message.into())),
                commands: Vec::new(),
            },
        }
    }
}

struct AgentStageExecution {
    stage: StageExecution,
    workspace: Option<PathBuf>,
    changed_files: Vec<String>,
    patch_evidence: Vec<WorkspaceChangeEvidence>,
    patch_digest: Option<String>,
    patch_evidence_path: Option<String>,
    model_turns: u32,
    tool_calls: u32,
    approval_count: u32,
    recovery_metrics: AgentRecoveryMetrics,
    compaction_count: u32,
    verification_required_command_count: u32,
    verification_satisfied_command_count: u32,
    model_usage: ModelUsage,
    provider_attempts: ProviderAttemptMetadata,
    agent_duration_ms: u64,
    local_process_fallback_unknown_count: usize,
    trace_path: Option<String>,
    error: Option<String>,
    provider_diagnostic: Option<ProviderDiagnostic>,
    prompt_structure: Option<EvaluationPromptStructure>,
    prompt_fingerprint: Option<String>,
    tool_schema_fingerprint: Option<String>,
    provider_evidence: Option<EvaluationProviderEvidence>,
    verification_bypass_count: Option<u64>,
}

struct TaskExecution {
    result: EvaluationTrialResult,
    diagnostics: TaskDiagnostics,
}

struct TaskEvaluation {
    result: EvaluationTaskResult,
    trials: Vec<TaskExecution>,
}

/// Git source preparation path selected from the sandboxed Git capability probe.
///
/// `clone --revision` is available starting with Git 2.49. Older Git releases keep the
/// same fixed-commit contract through an explicit no-checkout clone followed by a detached
/// checkout and controller-owned verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteGitPreparationStrategy {
    RevisionBound,
    CloneThenCheckout,
}

struct MaterializedSource {
    commands: Vec<CommandDiagnostic>,
    snapshot: WorkspaceSnapshot,
    strict_sandbox_command_count: usize,
    local_process_fallback_count: usize,
    source_template: Option<SourceTemplatePreparation>,
}

/// The one Evaluation trace Store is shared by trial workers only for short SQLite operations.
///
/// `SessionStore` owns a rusqlite connection (`Send` but not `Sync`), so workers never hold this
/// guard while running a provider, sandbox command, or workspace operation.
type SharedEvaluationTraceStore<'a> = Arc<Mutex<&'a mut singularity_store::SessionStore>>;

/// Materialized source state prepared before any provider trial in the run.
struct PreparedTaskSource {
    task_root: PathBuf,
    source_dir: PathBuf,
    source_snapshot: Option<WorkspaceSnapshot>,
    strict_sandbox_command_count: usize,
    local_process_fallback_count: usize,
    source_template_expected: bool,
    source_template: Option<SourceTemplatePreparation>,
    source_commands: Vec<CommandDiagnostic>,
    duration_ms: u64,
    blocker: Option<EvaluationBlocker>,
}

/// 同一 prepared source 派生全部隔离 trial 时共享的只读任务上下文。
struct PreparedTaskContext<'store, 'ctx> {
    run_id: &'ctx RunId,
    task_root: &'ctx Path,
    source_dir: &'ctx Path,
    source_snapshot: &'ctx WorkspaceSnapshot,
    strict_sandbox_command_count: usize,
    local_process_fallback_count: usize,
    source_template_expected: bool,
    source_template: Option<&'ctx SourceTemplatePreparation>,
    source_commands: &'ctx [CommandDiagnostic],
    source_preparation_duration_ms: u64,
    plan: &'ctx WorkspacePlan,
    sandbox_backend: &'ctx SharedSandboxBackend,
    provider_snapshot: &'ctx ProviderConfigSnapshot,
    cancellation: &'ctx CancellationToken,
    trace_store: SharedEvaluationTraceStore<'store>,
    trace_failures: &'ctx Arc<Mutex<Vec<String>>>,
}

/// 一个 Evaluation run 内所有 task trial 共享的只读执行上下文。
struct EvaluationRunContext<'store, 'ctx> {
    run_id: &'ctx RunId,
    run_dir: &'ctx Path,
    sandbox_backend: &'ctx SharedSandboxBackend,
    provider_snapshot: &'ctx ProviderConfigSnapshot,
    cancellation: &'ctx CancellationToken,
    trace_store: SharedEvaluationTraceStore<'store>,
    trace_failures: Arc<Mutex<Vec<String>>>,
    sandbox_preflight: &'ctx SandboxPreflightReport,
    source_cache: &'ctx SourceTemplateCache,
}

#[derive(Debug)]
struct SandboxPreflightFailure {
    report: SandboxPreflightReport,
    blocker: EvaluationBlocker,
}

fn record_trace_failure(failures: &Arc<Mutex<Vec<String>>>, error: impl Into<String>) {
    if let Ok(mut failures) = failures.lock() {
        failures.push(error.into());
    }
}

fn ensure_trace_failures_empty(failures: &Arc<Mutex<Vec<String>>>) -> Result<(), String> {
    let failures = failures
        .lock()
        .map_err(|_| "evaluation trace failure mutex poisoned".to_string())?;
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "evaluation SQLite trace projection failed: {}",
            failures.join("; ")
        ))
    }
}

enum TrialWorkerError {
    Cancelled(Vec<TaskEvaluation>),
    Failed(String),
}

#[derive(Debug)]
enum IndexedWorkerError<T> {
    Cancelled(Vec<T>),
    Failed(String),
}

/// Run indexed work items with a bounded dynamic worker set while preserving index order.
///
/// A cancellation only exposes the completed task/trial work prefix in its in-memory partial
/// result; later completed work remains on disk but is intentionally not presented as resumable.
fn run_bounded_indexed_workers<T, F>(
    item_count: usize,
    max_workers: usize,
    cancellation: &CancellationToken,
    worker: F,
) -> Result<Vec<T>, IndexedWorkerError<T>>
where
    T: Send,
    F: Fn(usize) -> T + Send + Sync,
{
    debug_assert!((1..=8).contains(&max_workers));
    if max_workers == 1 {
        let mut results = Vec::with_capacity(item_count);
        for index in 0..item_count {
            if cancellation.is_cancelled() {
                return Err(IndexedWorkerError::Cancelled(results));
            }
            let result =
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker(index))) {
                    Ok(result) => result,
                    Err(_) => {
                        cancellation.cancel();
                        return Err(IndexedWorkerError::Failed(
                            "evaluation trial worker panicked".to_string(),
                        ));
                    }
                };
            results.push(result);
        }
        if cancellation.is_cancelled() {
            return Err(IndexedWorkerError::Cancelled(results));
        }
        return Ok(results);
    }

    let next_index = Arc::new(AtomicUsize::new(0));
    let slots = Arc::new(Mutex::new(
        (0..item_count).map(|_| None::<T>).collect::<Vec<_>>(),
    ));
    let worker_count = max_workers.min(item_count);
    let mut worker_panicked = false;
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let next_index = Arc::clone(&next_index);
            let slots = Arc::clone(&slots);
            let worker = &worker;
            handles.push(scope.spawn(move || {
                let mut panicked = false;
                loop {
                    if cancellation.is_cancelled() {
                        break;
                    }
                    let index = next_index.fetch_add(1, Ordering::Relaxed);
                    if index >= item_count {
                        break;
                    }
                    let result =
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            worker(index)
                        })) {
                            Ok(result) => result,
                            Err(_) => {
                                panicked = true;
                                cancellation.cancel();
                                break;
                            }
                        };
                    let Ok(mut slots) = slots.lock() else {
                        break;
                    };
                    slots[index] = Some(result);
                }
                panicked
            }));
        }
        for handle in handles {
            match handle.join() {
                Ok(panicked) => worker_panicked |= panicked,
                Err(_) => worker_panicked = true,
            }
        }
    });

    let slots = match Arc::try_unwrap(slots) {
        Ok(slots) => match slots.into_inner() {
            Ok(slots) => slots,
            Err(_) => {
                return Err(IndexedWorkerError::Failed(
                    "evaluation trial result mutex poisoned".to_string(),
                ));
            }
        },
        Err(_) => {
            return Err(IndexedWorkerError::Failed(
                "evaluation trial workers did not join".to_string(),
            ));
        }
    };
    if worker_panicked {
        return Err(IndexedWorkerError::Failed(
            "evaluation trial worker panicked".to_string(),
        ));
    }
    let mut results = Vec::with_capacity(slots.len());
    let mut complete_prefix = true;
    for slot in slots {
        match slot {
            Some(result) if complete_prefix => results.push(result),
            Some(_) => {}
            None => complete_prefix = false,
        }
    }
    if cancellation.is_cancelled() {
        return Err(IndexedWorkerError::Cancelled(results));
    }
    if !complete_prefix {
        return Err(IndexedWorkerError::Failed(
            "evaluation trial worker stopped before all trials completed".to_string(),
        ));
    }
    Ok(results)
}

/// Run a flattened task/trial work list with a bounded worker set.
///
/// The flattened index is task-major so the indexed worker collector can restore the
/// manifest task order and each task's trial ordinal after concurrent execution.
fn run_bounded_trial_workers<T, F>(
    task_count: usize,
    trial_ordinals: &[u32],
    max_workers: usize,
    cancellation: &CancellationToken,
    worker: F,
) -> Result<Vec<T>, IndexedWorkerError<T>>
where
    T: Send,
    F: Fn(usize, u32) -> T + Send + Sync,
{
    if task_count == 0 || trial_ordinals.is_empty() {
        return Ok(Vec::new());
    }
    let trial_count = trial_ordinals.len();
    let item_count = task_count.checked_mul(trial_count).ok_or_else(|| {
        IndexedWorkerError::Failed("evaluation trial work item count overflowed".to_string())
    })?;
    let trial_ordinals = trial_ordinals.to_vec();
    run_bounded_indexed_workers(item_count, max_workers, cancellation, move |index| {
        let task_index = index / trial_count;
        let trial = trial_ordinals[index % trial_count];
        worker(task_index, trial)
    })
}

fn run_task_workers(
    context: &EvaluationRunContext<'_, '_>,
    plans: &[WorkspacePlan],
    prepared_sources: &[PreparedTaskSource],
    trials_per_task: u32,
    max_workers: usize,
    selected_trial: Option<u32>,
    recovery_every: Option<NonZeroUsize>,
) -> Result<Vec<TaskEvaluation>, TrialWorkerError> {
    if context.cancellation.is_cancelled() {
        return Err(TrialWorkerError::Cancelled(Vec::new()));
    }
    let trial_ordinals = selected_trial
        .map(|trial| vec![trial])
        .unwrap_or_else(|| (1..=trials_per_task).collect::<Vec<_>>());
    let trial_count = trial_ordinals.len();
    let task_traces = plans
        .iter()
        .map(|plan| EvaluationTaskTrace::start(context, plan))
        .collect::<Vec<_>>();
    let result = run_bounded_trial_workers(
        plans.len(),
        &trial_ordinals,
        max_workers,
        context.cancellation,
        move |task_index, trial| {
            let recovery_injected = recovery_every.is_some_and(|every| {
                let ordinal = task_index
                    .saturating_mul(trial_count)
                    .saturating_add(usize::try_from(trial.saturating_sub(1)).unwrap_or(usize::MAX))
                    .saturating_add(1);
                ordinal.is_multiple_of(every.get())
            });
            run_task_trial_with_prepared_source(
                context,
                &plans[task_index],
                &prepared_sources[task_index],
                trial,
                recovery_injected,
            )
        },
    );
    match result {
        Ok(trials) => {
            let task_executions = task_evaluations_from_trials(plans, &trial_ordinals, trials);
            end_task_traces(task_traces, &task_executions);
            refresh_trial_trace_artifacts(context, &task_executions);
            Ok(task_executions)
        }
        Err(IndexedWorkerError::Cancelled(trials)) => {
            let task_executions = task_evaluations_from_trials(plans, &trial_ordinals, trials);
            end_task_traces(task_traces, &task_executions);
            refresh_trial_trace_artifacts(context, &task_executions);
            Err(TrialWorkerError::Cancelled(task_executions))
        }
        Err(IndexedWorkerError::Failed(message)) => {
            end_task_traces(task_traces, &[]);
            Err(TrialWorkerError::Failed(message))
        }
    }
}

fn task_evaluations_from_trials(
    plans: &[WorkspacePlan],
    trial_ordinals: &[u32],
    trials: Vec<TaskExecution>,
) -> Vec<TaskEvaluation> {
    if trial_ordinals.is_empty() {
        return Vec::new();
    }
    let mut grouped = Vec::<Vec<TaskExecution>>::new();
    for (index, trial) in trials.into_iter().enumerate() {
        let task_index = index / trial_ordinals.len();
        if grouped.len() <= task_index {
            grouped.push(Vec::new());
        }
        grouped[task_index].push(trial);
    }
    grouped
        .into_iter()
        .enumerate()
        .map(|(task_index, trials)| task_evaluation_from_trials(&plans[task_index], trials))
        .collect()
}

fn end_task_traces(task_traces: Vec<EvaluationTaskTrace<'_>>, task_executions: &[TaskEvaluation]) {
    for (index, trace) in task_traces.into_iter().enumerate() {
        let status = task_executions
            .get(index)
            .map(|execution| execution.result.status)
            .unwrap_or(EvaluationStatus::Failed);
        trace.end(evaluation_status_trace_status(status));
    }
}

/// Refresh each persisted trial trace after both its trial and parent task spans have closed.
fn refresh_trial_trace_artifacts(
    context: &EvaluationRunContext<'_, '_>,
    task_executions: &[TaskEvaluation],
) {
    for execution in task_executions {
        let task_span_id = evaluation_span_id(
            context.run_id,
            &format!("task:{}", execution.result.task_id.as_str()),
            TraceSpanKind::Task,
        );
        for trial in &execution.trials {
            let Some(trace_path) = trial.diagnostics.trace_path.as_deref() else {
                continue;
            };
            let session_id = format!(
                "trial:{}:{}",
                execution.result.task_id.as_str(),
                trial.result.trial
            );
            let trace = match evaluation_agent_trace_shared(
                &context.trace_store,
                context.run_id.as_str(),
                &session_id,
                &task_span_id,
            ) {
                Ok(trace) => trace,
                Err(error) => {
                    record_trace_failure(
                        &context.trace_failures,
                        format!("failed to refresh evaluation trace artifact: {error}"),
                    );
                    continue;
                }
            };
            if let Err(error) = write_json_atomic(Path::new(trace_path), &trace) {
                record_trace_failure(
                    &context.trace_failures,
                    format!("failed to refresh evaluation trace artifact: {error}"),
                );
            }
        }
    }
}

fn evaluation_span_id(run_id: &RunId, scope: &str, kind: TraceSpanKind) -> String {
    format!(
        "eval_span:{}",
        content_digest(
            format!(
                "{}\u{0}{}\u{0}{}",
                run_id.as_str(),
                scope,
                kind.as_storage_text()
            )
            .as_bytes()
        )
    )
}

struct EvaluationTraceSink<'store> {
    store: SharedEvaluationTraceStore<'store>,
    run_id: String,
    failures: Arc<Mutex<Vec<String>>>,
}

impl<'store> EvaluationTraceSink<'store> {
    fn new(
        store: SharedEvaluationTraceStore<'store>,
        run_id: &RunId,
        failures: &Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            store,
            run_id: run_id.as_str().to_string(),
            failures: Arc::clone(failures),
        }
    }

    fn start(
        &self,
        session_id: &str,
        span_id: &str,
        parent_span_id: Option<&str>,
        kind: TraceSpanKind,
        summary: &str,
    ) {
        let mut event = TraceEvent::new(
            format!("{span_id}:start"),
            &self.run_id,
            session_id,
            "evaluation",
            summary,
        );
        event.timestamp = Some(Timestamp::now_utc().to_string());
        event.span_id = Some(span_id.to_string());
        event.parent_span_id = parent_span_id.map(str::to_string);
        event.span_kind = Some(kind);
        event.span_phase = Some(TraceSpanPhase::Start);
        event.span_projection = Some(TraceSpanProjection::default());
        event.payload = json!({"evaluation_span": kind.as_storage_text()});
        match self.store.lock() {
            Ok(store) => {
                if let Err(error) = store.append_trace_idempotent(&event) {
                    record_trace_failure(&self.failures, format!("{summary} start: {error}"));
                }
            }
            Err(_) => record_trace_failure(
                &self.failures,
                format!("{summary} start: evaluation trace store mutex poisoned"),
            ),
        }
    }

    fn end(
        &self,
        session_id: &str,
        span_id: &str,
        parent_span_id: Option<&str>,
        kind: TraceSpanKind,
        status: TraceSpanStatus,
        started: Instant,
    ) {
        let mut event = TraceEvent::new(
            format!("{span_id}:end"),
            &self.run_id,
            session_id,
            "evaluation",
            match kind {
                TraceSpanKind::Task => "evaluation task",
                TraceSpanKind::Turn => "evaluation trial",
                _ => "evaluation span",
            },
        );
        event.timestamp = Some(Timestamp::now_utc().to_string());
        event.span_id = Some(span_id.to_string());
        event.parent_span_id = parent_span_id.map(str::to_string);
        event.span_kind = Some(kind);
        event.span_phase = Some(TraceSpanPhase::End);
        event.span_status = Some(status);
        event.duration_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        event.span_projection = Some(TraceSpanProjection::default());
        event.payload = json!({"evaluation_span": kind.as_storage_text()});
        match self.store.lock() {
            Ok(store) => {
                if let Err(error) = store.append_trace(&event) {
                    record_trace_failure(
                        &self.failures,
                        format!(
                            "{} end: {error}",
                            match kind {
                                TraceSpanKind::Task => "evaluation task",
                                TraceSpanKind::Turn => "evaluation trial",
                                _ => "evaluation span",
                            }
                        ),
                    );
                }
            }
            Err(_) => record_trace_failure(
                &self.failures,
                format!(
                    "{} end: evaluation trace store mutex poisoned",
                    match kind {
                        TraceSpanKind::Task => "evaluation task",
                        TraceSpanKind::Turn => "evaluation trial",
                        _ => "evaluation span",
                    }
                ),
            ),
        }
    }
}

struct EvaluationTaskTrace<'store> {
    sink: EvaluationTraceSink<'store>,
    session_id: String,
    span_id: String,
    started: Instant,
}

impl<'store> EvaluationTaskTrace<'store> {
    fn start<'ctx>(context: &EvaluationRunContext<'store, 'ctx>, plan: &WorkspacePlan) -> Self {
        let session_id = format!("task:{}", plan.task_id.as_str());
        let span_id = evaluation_span_id(context.run_id, &session_id, TraceSpanKind::Task);
        let started = Instant::now();
        let sink = EvaluationTraceSink::new(
            Arc::clone(&context.trace_store),
            context.run_id,
            &context.trace_failures,
        );
        sink.start(
            &session_id,
            &span_id,
            None,
            TraceSpanKind::Task,
            "evaluation task",
        );
        Self {
            sink,
            session_id,
            span_id,
            started,
        }
    }

    fn end(self, status: TraceSpanStatus) {
        self.sink.end(
            &self.session_id,
            &self.span_id,
            None,
            TraceSpanKind::Task,
            status,
            self.started,
        );
    }
}

struct EvaluationTrialTrace<'store> {
    sink: EvaluationTraceSink<'store>,
    session_id: String,
    turn_span_id: String,
    task_span_id: String,
}

impl<'store> EvaluationTrialTrace<'store> {
    fn new(
        store: SharedEvaluationTraceStore<'store>,
        run_id: &RunId,
        failures: &Arc<Mutex<Vec<String>>>,
        session_id: String,
        turn_span_id: String,
        task_span_id: String,
    ) -> Self {
        Self {
            sink: EvaluationTraceSink {
                store,
                run_id: run_id.as_str().to_string(),
                failures: Arc::clone(failures),
            },
            session_id,
            turn_span_id,
            task_span_id,
        }
    }
}

fn evaluation_status_trace_status(status: EvaluationStatus) -> TraceSpanStatus {
    match status {
        EvaluationStatus::Completed => TraceSpanStatus::Ok,
        EvaluationStatus::Failed | EvaluationStatus::Blocked => TraceSpanStatus::Error,
    }
}

#[derive(Debug)]
struct ResolvedEvaluationTools {
    registry: ToolRegistry,
    names: Vec<String>,
    schema_fingerprint: String,
}

#[derive(Debug, Serialize)]
struct PublicationArtifact {
    path: String,
    digest: String,
}

#[derive(Debug, Serialize)]
struct EvaluationPublicationManifest {
    schema_version: &'static str,
    run_id: String,
    artifact_set_digest: String,
    result: PublicationArtifact,
    report: PublicationArtifact,
    evidence: PublicationArtifact,
}

#[derive(Debug)]
struct PublishedEvaluationArtifacts {
    result_path: PathBuf,
    report_path: PathBuf,
    evidence_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EvaluationRunErrorKind {
    Input,
    Publication,
    Infrastructure,
    Cancelled,
}

impl EvaluationRunErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Publication => "publication",
            Self::Infrastructure => "infrastructure",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone)]
pub struct EvaluationRunError {
    kind: EvaluationRunErrorKind,
    message: String,
    partial_result: Option<Box<EvaluationRunResult>>,
}

impl EvaluationRunError {
    fn input(message: impl Into<String>) -> Self {
        Self {
            kind: EvaluationRunErrorKind::Input,
            message: message.into(),
            partial_result: None,
        }
    }

    fn publication(message: impl Into<String>) -> Self {
        Self {
            kind: EvaluationRunErrorKind::Publication,
            message: message.into(),
            partial_result: None,
        }
    }

    fn infrastructure(message: impl Into<String>) -> Self {
        Self {
            kind: EvaluationRunErrorKind::Infrastructure,
            message: message.into(),
            partial_result: None,
        }
    }

    fn cancelled(message: impl Into<String>, partial_result: Option<EvaluationRunResult>) -> Self {
        Self {
            kind: EvaluationRunErrorKind::Cancelled,
            message: message.into(),
            partial_result: partial_result.map(Box::new),
        }
    }

    pub fn partial_result(&self) -> Option<&EvaluationRunResult> {
        self.partial_result.as_deref()
    }
}

impl std::fmt::Display for EvaluationRunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EvaluationRunError {}

#[derive(Serialize)]
struct EvaluationFailureEvidence<'a> {
    schema_version: &'static str,
    kind: &'static str,
    message: &'a str,
}

/// Run the complete manifest evaluation (the stable v9/v4 path).
pub fn run_evaluation(
    params: &EvaluationRunParams,
    sandbox_backend: SharedSandboxBackend,
    provider_snapshot: &ProviderConfigSnapshot,
    cancellation: &CancellationToken,
    trace_store: &mut singularity_store::SessionStore,
) -> Result<EvaluationRunResult, EvaluationRunError> {
    run_evaluation_with_mode(
        params,
        sandbox_backend,
        provider_snapshot,
        cancellation,
        trace_store,
        EvaluationRunMode::Full,
    )
}

/// Run a full or feedback Evaluation with shared strict orchestration.
pub fn run_evaluation_with_mode(
    params: &EvaluationRunParams,
    sandbox_backend: SharedSandboxBackend,
    provider_snapshot: &ProviderConfigSnapshot,
    cancellation: &CancellationToken,
    trace_store: &mut singularity_store::SessionStore,
    mode: EvaluationRunMode,
) -> Result<EvaluationRunResult, EvaluationRunError> {
    let feedback_mode = matches!(mode, EvaluationRunMode::Feedback);
    let run_started = Instant::now();
    if !(1..=8).contains(&params.max_workers) {
        return Err(EvaluationRunError::input(
            "evaluation max_workers must be between 1 and 8",
        ));
    }
    if params.recovery_every.is_some() && params.max_workers != 1 {
        return Err(EvaluationRunError::input(
            "evaluation recovery mode requires max_workers=1",
        ));
    }
    if cancellation.is_cancelled() {
        let partial = RunId::new(params.run_id.clone())
            .ok()
            .map(|run_id| partial_evaluation_result(params, &run_id, &[]));
        return Err(EvaluationRunError::cancelled(
            "evaluation cancelled",
            partial,
        ));
    }
    let manifest_path = Path::new(&params.manifest);
    let manifest_json = fs::read_to_string(manifest_path).map_err(|error| {
        EvaluationRunError::input(format!(
            "invalid eval manifest: failed to read {}: {error}",
            manifest_path.display()
        ))
    })?;
    let manifest_digest = content_digest(manifest_json.as_bytes());
    let manifest_root = manifest_path.parent().ok_or_else(|| {
        EvaluationRunError::input(format!(
            "invalid eval manifest: manifest path has no parent: {}",
            manifest_path.display()
        ))
    })?;
    let manifest = EvaluationManifest::from_json_str(&manifest_json, manifest_root)
        .map_err(|error| EvaluationRunError::input(format!("invalid eval manifest: {error}")))?;
    let run_id = RunId::new(params.run_id.clone())
        .map_err(|error| EvaluationRunError::input(format!("invalid eval run id: {error}")))?;
    let output_root = evaluation_output_root(params.output_root.as_deref());
    let all_plans = manifest
        .task_set()
        .tasks
        .iter()
        .map(|task| {
            manifest
                .workspace_plan(&task.task_id)
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(EvaluationRunError::input)?;
    let manifest_trial_count = manifest.task_set().trial_count;
    let plans = if feedback_mode {
        let feedback_index = all_plans
            .iter()
            .position(|plan| matches!(&plan.source, PlannedWorkspaceSource::Local { .. }))
            .unwrap_or(0);
        vec![
            all_plans
                .into_iter()
                .nth(feedback_index)
                .expect("validated manifest has at least one task"),
        ]
    } else {
        all_plans
    };
    let task_ids = plans
        .iter()
        .map(|plan| plan.task_id.clone())
        .collect::<Vec<_>>();
    let trials_per_task = if feedback_mode {
        1
    } else {
        manifest_trial_count
    };
    preflight_evaluation_path_budget(&output_root, &run_id, &task_ids, trials_per_task)
        .map_err(EvaluationRunError::input)?;

    fs::create_dir_all(&output_root).map_err(|error| {
        EvaluationRunError::infrastructure(format!(
            "failed to create evaluation output root {}: {error}",
            output_root.display()
        ))
    })?;
    let run_dir = output_root.join(run_id.as_str());
    fs::create_dir(&run_dir).map_err(|error| {
        EvaluationRunError::infrastructure(format!(
            "failed to create new evaluation run directory {}: {error}",
            run_dir.display()
        ))
    })?;

    let source_cache = SourceTemplateCache::new(source_template_cache_root(&output_root));
    let cached_remote_repositories =
        match plans
            .iter()
            .try_fold(BTreeSet::new(), |mut repositories, plan| {
                let PlannedWorkspaceSource::RemoteGit { repository, .. } = &plan.source else {
                    return Ok(repositories);
                };
                match source_cache.entry_available(
                    plan.task_id.as_str(),
                    &redacted_remote_repository(repository.as_str()),
                ) {
                    Ok(true) => {
                        repositories.insert(repository.as_str().to_string());
                        Ok(repositories)
                    }
                    Ok(false) => Ok(repositories),
                    Err(error) => Err(EvaluationRunError::infrastructure(format!(
                        "{}: source-template cache lookup failed for task {}: {error}",
                        error.stable_code(),
                        plan.task_id.as_str()
                    ))),
                }
            }) {
            Ok(repositories) => repositories,
            Err(error) => return Err(preserve_incomplete_run(&run_dir, error)),
        };

    let cancellable_sandbox_backend =
        cancellation_aware_sandbox_backend(&sandbox_backend, cancellation);
    let preflight = match run_sandbox_preflight(
        &run_dir,
        &plans,
        &cancellable_sandbox_backend,
        cancellation,
        &cached_remote_repositories,
    ) {
        Ok(report) => report,
        Err(failure) => {
            let preflight = sandbox_preflight_evidence(&failure.report);
            let result = EvaluationResult::blocked_by_sandbox_preflight(
                run_id.clone(),
                u32::try_from(plans.len()).unwrap_or(u32::MAX),
                trials_per_task,
                failure.blocker,
                preflight.clone(),
            );
            if feedback_mode {
                return finish_feedback_run(
                    params,
                    &run_dir,
                    result,
                    &[],
                    None,
                    elapsed_ms(run_started),
                );
            }
            return publish_zero_sampling_blocked_run(
                params,
                &run_dir,
                manifest_digest,
                &plans,
                result,
                preflight,
                elapsed_ms(run_started),
            );
        }
    };

    let shared_trace_store = Arc::new(Mutex::new(trace_store));
    let run_context = EvaluationRunContext {
        run_id: &run_id,
        run_dir: &run_dir,
        sandbox_backend: &cancellable_sandbox_backend,
        provider_snapshot,
        cancellation,
        trace_store: Arc::clone(&shared_trace_store),
        trace_failures: Arc::new(Mutex::new(Vec::new())),
        sandbox_preflight: &preflight,
        source_cache: &source_cache,
    };
    if cancellation.is_cancelled() {
        let partial = partial_evaluation_result(params, &run_id, &[]);
        return Err(preserve_incomplete_run(
            &run_dir,
            EvaluationRunError::cancelled("evaluation cancelled", Some(partial)),
        ));
    }
    if let Err(error) = provider_snapshot.provider() {
        let blocker = run_level_blocker(provider_configuration_blocker(&error));
        let result = EvaluationResult::blocked_before_sampling(
            run_id.clone(),
            u32::try_from(plans.len()).unwrap_or(u32::MAX),
            trials_per_task,
            blocker,
            sandbox_preflight_evidence(&preflight),
        );
        if feedback_mode {
            return finish_feedback_run(
                params,
                &run_dir,
                result,
                &[],
                None,
                elapsed_ms(run_started),
            );
        }
        return publish_zero_sampling_blocked_run(
            params,
            &run_dir,
            manifest_digest,
            &plans,
            result,
            sandbox_preflight_evidence(&preflight),
            elapsed_ms(run_started),
        );
    }
    // Materialize every task source before entering the first provider trial. This keeps
    // source preparation failures deterministic and lets the run-level barrier reject the
    // entire run before any task can sample the provider.
    let prepared_sources = plans
        .iter()
        .map(|plan| prepare_task_source(&run_context, plan))
        .collect::<Vec<_>>();
    if cancellation.is_cancelled() {
        let partial = partial_evaluation_result(params, &run_id, &[]);
        return Err(preserve_incomplete_run(
            &run_dir,
            EvaluationRunError::cancelled("evaluation cancelled", Some(partial)),
        ));
    }
    if let Some(blocker) = prepared_sources
        .iter()
        .find_map(|prepared_source| prepared_source.blocker.clone())
    {
        let result = EvaluationResult::blocked_before_sampling(
            run_id.clone(),
            u32::try_from(plans.len()).unwrap_or(u32::MAX),
            trials_per_task,
            run_level_blocker(blocker),
            sandbox_preflight_evidence(&preflight),
        );
        if feedback_mode {
            return finish_feedback_run(
                params,
                &run_dir,
                result,
                &[],
                None,
                elapsed_ms(run_started),
            );
        }
        return publish_zero_sampling_blocked_run(
            params,
            &run_dir,
            manifest_digest,
            &plans,
            result,
            sandbox_preflight_evidence(&preflight),
            elapsed_ms(run_started),
        );
    }
    let task_executions = match run_task_workers(
        &run_context,
        &plans,
        &prepared_sources,
        trials_per_task,
        params.max_workers,
        None,
        params.recovery_every,
    ) {
        Ok(task_executions) => task_executions,
        Err(TrialWorkerError::Cancelled(task_executions)) => {
            if let Err(error) = ensure_trace_failures_empty(&run_context.trace_failures) {
                return Err(preserve_incomplete_run(
                    &run_dir,
                    EvaluationRunError::infrastructure(error),
                ));
            }
            let partial = partial_evaluation_result(params, &run_id, &task_executions);
            return Err(preserve_incomplete_run(
                &run_dir,
                EvaluationRunError::cancelled("evaluation cancelled", Some(partial)),
            ));
        }
        Err(TrialWorkerError::Failed(message)) => {
            return Err(preserve_incomplete_run(
                &run_dir,
                EvaluationRunError::infrastructure(message),
            ));
        }
    };

    if let Err(error) = ensure_trace_failures_empty(&run_context.trace_failures) {
        return Err(preserve_incomplete_run(
            &run_dir,
            EvaluationRunError::infrastructure(error),
        ));
    }

    let tasks = task_executions
        .iter()
        .map(|execution| execution.result.clone())
        .collect::<Vec<_>>();
    let mut result = EvaluationResult::from_tasks(run_id.clone(), trials_per_task, tasks);
    result.sandbox_preflight = Some(sandbox_preflight_evidence(&preflight));
    if let Err(error) = result.validate() {
        return Err(preserve_incomplete_run(
            &run_dir,
            EvaluationRunError::infrastructure(format!("invalid evaluation result: {error}")),
        ));
    }

    if feedback_mode {
        let trace_metrics =
            trace_metrics_shared(&shared_trace_store, run_id.as_str()).map_err(|error| {
                preserve_incomplete_run(
                    &run_dir,
                    EvaluationRunError::infrastructure(format!(
                        "failed to query evaluation trace metrics: {error}"
                    )),
                )
            })?;
        return finish_feedback_run(
            params,
            &run_dir,
            result,
            &task_executions,
            Some(&trace_metrics),
            elapsed_ms(run_started),
        );
    }

    let evidence = match build_evaluation_evidence(
        &run_id,
        manifest_digest,
        &plans,
        &task_executions,
        &run_dir,
        sandbox_preflight_evidence(&preflight),
    ) {
        Ok(evidence) => evidence,
        Err(error) => {
            return Err(preserve_incomplete_run(
                &run_dir,
                EvaluationRunError::infrastructure(error),
            ));
        }
    };
    if let Err(error) = evidence.validate_against_result(&result) {
        return Err(preserve_incomplete_run(
            &run_dir,
            EvaluationRunError::infrastructure(format!(
                "evaluation evidence/result mismatch: {error}"
            )),
        ));
    }
    let status_string = match enum_string(result.status) {
        Ok(status) => status,
        Err(error) => {
            return Err(preserve_incomplete_run(
                &run_dir,
                EvaluationRunError::infrastructure(error),
            ));
        }
    };
    let blocker = match result.blocker.as_ref().map(blocker_code).transpose() {
        Ok(blocker) => blocker,
        Err(error) => {
            return Err(preserve_incomplete_run(
                &run_dir,
                EvaluationRunError::infrastructure(error),
            ));
        }
    };
    let trace_metrics =
        trace_metrics_shared(&shared_trace_store, run_id.as_str()).map_err(|error| {
            preserve_incomplete_run(
                &run_dir,
                EvaluationRunError::infrastructure(format!(
                    "failed to query evaluation trace metrics: {error}"
                )),
            )
        })?;
    let report = build_evaluation_report(
        params,
        &result,
        &task_executions,
        Some(&trace_metrics),
        elapsed_ms(run_started),
    )
    .map_err(|error| {
        preserve_incomplete_run(
            &run_dir,
            EvaluationRunError::infrastructure(format!("invalid evaluation report: {error}")),
        )
    })?;
    let task_reports = report.tasks.clone();
    let published =
        match publish_evaluation_artifacts(&run_dir, &run_id, &result, &report, &evidence) {
            Ok(published) => published,
            Err(error) => {
                return Err(preserve_incomplete_run(
                    &run_dir,
                    EvaluationRunError::publication(error),
                ));
            }
        };

    Ok(EvaluationRunResult {
        run_id: run_id.as_str().to_string(),
        manifest: params.manifest.clone(),
        runner: RUNNER_NAME.to_string(),
        max_workers: params.max_workers,
        status: status_string,
        blocker,
        tasks: task_reports,
        result_path: Some(published.result_path.to_string_lossy().into_owned()),
        report_path: Some(published.report_path.to_string_lossy().into_owned()),
        evidence_path: Some(published.evidence_path.to_string_lossy().into_owned()),
        evaluation_passed: result.evaluation_passed,
        gate_applicable: true,
    })
}

fn partial_evaluation_result(
    params: &EvaluationRunParams,
    run_id: &RunId,
    task_executions: &[TaskEvaluation],
) -> EvaluationRunResult {
    let status = if task_executions
        .iter()
        .any(|execution| execution.result.status == EvaluationStatus::Failed)
    {
        "failed"
    } else {
        "blocked"
    };
    EvaluationRunResult {
        run_id: run_id.as_str().to_string(),
        manifest: safe_text(&params.manifest),
        runner: RUNNER_NAME.to_string(),
        max_workers: params.max_workers,
        status: status.to_string(),
        blocker: Some("evaluation_cancelled".to_string()),
        tasks: task_executions
            .iter()
            .map(|execution| execution.result.clone())
            .collect(),
        result_path: None,
        report_path: None,
        evidence_path: None,
        evaluation_passed: false,
        gate_applicable: true,
    }
}

fn preserve_incomplete_run(run_dir: &Path, mut error: EvaluationRunError) -> EvaluationRunError {
    let safe_message = safe_text(&error.message);
    let failure = EvaluationFailureEvidence {
        schema_version: FAILURE_SCHEMA_VERSION,
        kind: error.kind.as_str(),
        message: &safe_message,
    };
    if let Err(write_error) = write_json_atomic(&run_dir.join(FAILURE_FILE), &failure) {
        error.message = format!(
            "{}; failed to preserve incomplete evaluation evidence in {}: {write_error}",
            error.message,
            run_dir.display()
        );
    }
    error
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn trace_metrics_shared(
    store: &SharedEvaluationTraceStore<'_>,
    run_id: &str,
) -> Result<TraceMetrics, String> {
    let store = store
        .lock()
        .map_err(|_| "evaluation trace store mutex poisoned".to_string())?;
    store
        .trace_metrics(run_id)
        .map_err(|error| format!("trace metric query failed: {error}"))
}

fn build_evaluation_report(
    params: &EvaluationRunParams,
    result: &EvaluationResult,
    executions: &[TaskEvaluation],
    trace_metrics: Option<&TraceMetrics>,
    run_duration_ms: u64,
) -> Result<EvaluationReport, String> {
    let tasks = executions
        .iter()
        .map(|execution| execution.result.clone())
        .collect();
    let report = EvaluationReport {
        schema: EvaluationReportSchemaVersion::V2,
        run_id: result.run_id.clone(),
        manifest: safe_text(&params.manifest),
        runner: RUNNER_NAME.to_string(),
        max_workers: params.max_workers,
        dimensions: report_dimensions(result),
        system_result: EvaluationSystemResult {
            status: result.status,
            evaluation_passed: result.evaluation_passed,
            blocker: result.blocker.clone(),
        },
        metrics: build_evaluation_metrics(executions, trace_metrics, run_duration_ms),
        failure_attribution: build_failure_attributions(result, executions),
        tasks,
    };
    report.validate().map_err(|error| error.to_string())?;
    Ok(report)
}

fn report_dimensions(result: &EvaluationResult) -> EvaluationDimensions {
    EvaluationDimensions {
        functional_task_success: result.summary.meets_functional_task_success_threshold,
        functional_task_success_count: result.summary.functional_task_success_count,
        functional_task_count: result.summary.task_count,
        functional_task_success_rate_basis_points: result
            .summary
            .functional_task_success_rate_basis_points,
        agent_protocol_success: result.summary.meets_agent_protocol_success_threshold,
        agent_protocol_success_count: result.summary.agent_protocol_success_count,
        agent_protocol_task_count: result.summary.task_count,
        agent_protocol_success_rate_basis_points: result
            .summary
            .agent_protocol_success_rate_basis_points,
        sandbox_security_success: result.summary.meets_sandbox_security_success_threshold,
        sandbox_security_success_count: result.summary.sandbox_security_success_count,
        sandbox_security_task_count: result.summary.task_count,
        sandbox_security_success_rate_basis_points: result
            .summary
            .sandbox_security_success_rate_basis_points,
    }
}

fn metric_unavailable_from_trace(reason: TraceMetricUnavailableReason) -> MetricUnavailableReason {
    match reason {
        TraceMetricUnavailableReason::NoProducer => MetricUnavailableReason::NoProducer,
        _ => MetricUnavailableReason::NotObserved,
    }
}

fn trace_metric_statistics(
    trace_metrics: Option<&TraceMetrics>,
    name: TraceMetricName,
) -> MetricValue<MetricStatistics> {
    let Some(trace_metrics) = trace_metrics else {
        return MetricValue::unavailable(MetricUnavailableReason::NoProducer);
    };
    let Some(metric) = trace_metrics.metric(name.as_storage_text()) else {
        return MetricValue::unavailable(MetricUnavailableReason::NoProducer);
    };
    match (&metric.availability, &metric.distribution) {
        (TraceMetricAvailability::Available, Some(distribution)) => {
            MetricValue::available(MetricStatistics {
                count: distribution.count,
                sum: distribution.sum,
                min: distribution.min.unwrap_or(0),
                max: distribution.max.unwrap_or(0),
                mean: distribution.mean.unwrap_or(0.0),
                p50: distribution.p50.unwrap_or(0),
                p95: distribution.p95.unwrap_or(0),
            })
        }
        (TraceMetricAvailability::Available, None) => {
            MetricValue::unavailable(MetricUnavailableReason::NotObserved)
        }
        (TraceMetricAvailability::Unavailable { reason }, _) => {
            MetricValue::unavailable(metric_unavailable_from_trace(*reason))
        }
    }
}

fn metric_values<T>(
    samples: &[T],
    producer: impl Fn(&T) -> bool,
    value: impl Fn(&T) -> Option<u64>,
) -> Result<Vec<u64>, MetricUnavailableReason> {
    let producers = samples
        .iter()
        .filter(|sample| producer(sample))
        .collect::<Vec<_>>();
    if producers.is_empty() {
        return Err(MetricUnavailableReason::NoProducer);
    }
    let values = producers.into_iter().filter_map(value).collect::<Vec<_>>();
    if values.is_empty() {
        Err(MetricUnavailableReason::NotObserved)
    } else {
        Ok(values)
    }
}

fn metric_sum<T>(
    samples: &[T],
    producer: impl Fn(&T) -> bool,
    value: impl Fn(&T) -> Option<u64>,
) -> MetricValue<u64> {
    match metric_values(samples, producer, value) {
        Ok(values) => MetricValue::available(values.into_iter().fold(0u64, u64::saturating_add)),
        Err(reason) => MetricValue::unavailable(reason),
    }
}

fn metric_statistics<T>(
    samples: &[T],
    producer: impl Fn(&T) -> bool,
    value: impl Fn(&T) -> Option<u64>,
) -> MetricValue<MetricStatistics> {
    match metric_values(samples, producer, value) {
        Ok(values) => MetricStatistics::from_values(&values)
            .map(MetricValue::available)
            .unwrap_or_else(|| MetricValue::unavailable(MetricUnavailableReason::NotObserved)),
        Err(reason) => MetricValue::unavailable(reason),
    }
}

/// Project the provider/tool samples for one successful functional trial.
///
/// Functional success is the only denominator for the per-success cost metrics.  A successful
/// trial without a provider usage observation is retained as `NotObserved` rather than turning
/// its default token count into a zero sample.
fn functional_success_statistics(
    samples: &[&TaskExecution],
    value: impl Fn(&TaskExecution) -> Option<u64>,
) -> MetricValue<MetricStatistics> {
    let successful = samples
        .iter()
        .copied()
        .filter(|execution| execution.result.functional_task_success)
        .collect::<Vec<_>>();
    if successful.is_empty() {
        return MetricValue::unavailable(MetricUnavailableReason::NoProducer);
    }
    let values = successful.into_iter().filter_map(value).collect::<Vec<_>>();
    if values.is_empty() {
        return MetricValue::unavailable(MetricUnavailableReason::NotObserved);
    }
    MetricStatistics::from_values(&values)
        .map(MetricValue::available)
        .unwrap_or_else(|| MetricValue::unavailable(MetricUnavailableReason::NotObserved))
}

/// Read a ratio emitted by the typed trace producer.
///
/// `Store` publishes rate metrics as a one-sample distribution whose value is already in basis
/// points.  The report ratio keeps that exact value with a 10,000-point denominator; it does not
/// reconstruct a ratio from invalid/repeated/repair counters.
fn trace_metric_ratio(
    trace_metrics: Option<&TraceMetrics>,
    name: TraceMetricName,
) -> MetricValue<MetricRatio> {
    let Some(trace_metrics) = trace_metrics else {
        return MetricValue::unavailable(MetricUnavailableReason::NoProducer);
    };
    let Some(metric) = trace_metrics.metric(name.as_storage_text()) else {
        return MetricValue::unavailable(MetricUnavailableReason::NoProducer);
    };
    match (&metric.availability, &metric.distribution) {
        (TraceMetricAvailability::Available, Some(distribution)) if distribution.count == 1 => {
            MetricRatio::new(distribution.sum, 10_000)
                .map(MetricValue::available)
                .unwrap_or_else(|| MetricValue::unavailable(MetricUnavailableReason::NotObserved))
        }
        (TraceMetricAvailability::Available, Some(_)) => {
            MetricValue::unavailable(MetricUnavailableReason::NotObserved)
        }
        (TraceMetricAvailability::Available, None) => {
            MetricValue::unavailable(MetricUnavailableReason::NotObserved)
        }
        (TraceMetricAvailability::Unavailable { reason }, _) => {
            MetricValue::unavailable(metric_unavailable_from_trace(*reason))
        }
    }
}

fn compaction_performance_decay(samples: &[&TaskExecution]) -> MetricValue<i32> {
    if samples.is_empty() {
        return MetricValue::unavailable(MetricUnavailableReason::NoProducer);
    }
    let mut non_compacted_total = 0_u64;
    let mut non_compacted_successes = 0_u64;
    let mut compacted_total = 0_u64;
    let mut compacted_successes = 0_u64;
    for execution in samples {
        if execution.diagnostics.compaction_count == 0 {
            non_compacted_total = non_compacted_total.saturating_add(1);
            if execution.result.functional_task_success {
                non_compacted_successes = non_compacted_successes.saturating_add(1);
            }
        } else {
            compacted_total = compacted_total.saturating_add(1);
            if execution.result.functional_task_success {
                compacted_successes = compacted_successes.saturating_add(1);
            }
        }
    }
    if non_compacted_total == 0 || compacted_total == 0 {
        return MetricValue::unavailable(MetricUnavailableReason::NotObserved);
    }
    let non_compacted_rate = non_compacted_successes
        .saturating_mul(10_000)
        .checked_div(non_compacted_total)
        .and_then(|value| i32::try_from(value).ok());
    let compacted_rate = compacted_successes
        .saturating_mul(10_000)
        .checked_div(compacted_total)
        .and_then(|value| i32::try_from(value).ok());
    match (non_compacted_rate, compacted_rate) {
        (Some(non_compacted_rate), Some(compacted_rate)) => {
            MetricValue::available(non_compacted_rate - compacted_rate)
        }
        _ => MetricValue::unavailable(MetricUnavailableReason::NotObserved),
    }
}

fn verification_bypass_metric(samples: &[&TaskExecution]) -> MetricValue<u64> {
    if samples.is_empty() {
        return MetricValue::unavailable(MetricUnavailableReason::NoProducer);
    }
    let mut total = 0_u64;
    for execution in samples {
        let Some(count) = execution.diagnostics.verification_bypass_count else {
            return MetricValue::unavailable(MetricUnavailableReason::NotObserved);
        };
        total = total.saturating_add(count);
    }
    MetricValue::available(total)
}

fn recovery_completion_metric(samples: &[&TaskExecution]) -> MetricValue<MetricRatio> {
    let injected = samples
        .iter()
        .copied()
        .filter(|execution| execution.diagnostics.recovery_injected)
        .collect::<Vec<_>>();
    if injected.is_empty() {
        return MetricValue::unavailable(MetricUnavailableReason::NoProducer);
    }
    let conclusions = injected
        .iter()
        .filter_map(|execution| execution.diagnostics.recovery_completed)
        .collect::<Vec<_>>();
    if conclusions.is_empty() {
        return MetricValue::unavailable(MetricUnavailableReason::NotObserved);
    }
    let successes = conclusions.iter().filter(|completed| **completed).count();
    MetricRatio::new(
        u64::try_from(successes).unwrap_or(u64::MAX),
        u64::try_from(conclusions.len()).unwrap_or(u64::MAX),
    )
    .map(MetricValue::available)
    .unwrap_or_else(|| MetricValue::unavailable(MetricUnavailableReason::NotObserved))
}

fn build_harness_metrics(
    samples: &[&TaskExecution],
    trace_metrics: Option<&TraceMetrics>,
) -> HarnessMetrics {
    HarnessMetrics {
        tokens_per_functional_success: functional_success_statistics(samples, |execution| {
            execution
                .diagnostics
                .provider_usage_available
                .then_some(execution.diagnostics.total_tokens)
        }),
        time_per_functional_success: functional_success_statistics(samples, |execution| {
            Some(execution.diagnostics.trial_duration_ms)
        }),
        tool_first_attempt_success_rate: trace_metric_ratio(
            trace_metrics,
            TraceMetricName::ToolFirstAttemptSuccessRateBps,
        ),
        compaction_performance_decay: compaction_performance_decay(samples),
        recovery_completion_rate: recovery_completion_metric(samples),
        verification_bypass_count: verification_bypass_metric(samples),
    }
}

fn build_evaluation_metrics(
    executions: &[TaskEvaluation],
    trace_metrics: Option<&TraceMetrics>,
    run_duration_ms: u64,
) -> EvaluationMetrics {
    let samples = executions
        .iter()
        .flat_map(|execution| execution.trials.iter())
        .collect::<Vec<_>>();
    let diagnostics = samples
        .iter()
        .map(|execution| &execution.diagnostics)
        .collect::<Vec<_>>();
    // Source preparation is a task-level producer shared by all of that task's trials.
    let source_samples = executions
        .iter()
        .filter_map(|execution| execution.trials.first())
        .collect::<Vec<_>>();
    let all = |value: fn(&TaskDiagnostics) -> u64| {
        metric_statistics(
            &diagnostics,
            |_| true,
            |diagnostics| Some(value(diagnostics)),
        )
    };
    let provider_producer = |diagnostics: &&TaskDiagnostics| diagnostics.provider_attempt_count > 0;
    let probe_producer = |diagnostics: &&TaskDiagnostics| diagnostics.probe_attempt_count > 0;
    let usage = |value: fn(&TaskDiagnostics) -> u64| {
        metric_sum(&diagnostics, provider_producer, |diagnostics| {
            diagnostics
                .provider_usage_available
                .then(|| value(diagnostics))
        })
    };
    let completion_attempts = metric_sum(&diagnostics, provider_producer, |diagnostics| {
        Some(u64::from(diagnostics.provider_attempt_count))
    });
    let completion_retries = metric_sum(&diagnostics, provider_producer, |diagnostics| {
        Some(u64::from(diagnostics.provider_retry_count))
    });
    let completion_latency = metric_statistics(&diagnostics, provider_producer, |diagnostics| {
        Some(diagnostics.provider_latency_ms)
    });
    let probe_attempts = metric_sum(&diagnostics, probe_producer, |diagnostics| {
        Some(u64::from(diagnostics.probe_attempt_count))
    });
    let probe_retries = metric_sum(&diagnostics, probe_producer, |diagnostics| {
        Some(u64::from(diagnostics.probe_retry_count))
    });
    let probe_latency = metric_statistics(&diagnostics, probe_producer, |diagnostics| {
        Some(diagnostics.probe_latency_ms)
    });
    let input_tokens = usage(|diagnostics| diagnostics.input_tokens);
    let cached_input_tokens = usage(|diagnostics| diagnostics.cached_input_tokens);
    let noncached_input_tokens = match (&input_tokens, &cached_input_tokens) {
        (MetricValue::Available { value: input }, MetricValue::Available { value: cached })
            if cached <= input =>
        {
            MetricValue::available(input.saturating_sub(*cached))
        }
        (MetricValue::Available { .. }, MetricValue::Available { .. }) => {
            MetricValue::unavailable(MetricUnavailableReason::NotObserved)
        }
        (MetricValue::Unavailable { reason }, _) => MetricValue::unavailable(*reason),
        (_, MetricValue::Unavailable { reason }) => MetricValue::unavailable(*reason),
    };
    let capability_attempt_observed = diagnostics
        .iter()
        .any(|diagnostics| diagnostics.provider_attempt_count > 0);
    let capability_observations = diagnostics
        .iter()
        .filter(|diagnostics| diagnostics.capability_cache_observation_count > 0)
        .collect::<Vec<_>>();
    let unavailable_capability_cache = |reason| {
        (
            MetricValue::unavailable(reason),
            MetricValue::unavailable(reason),
            MetricValue::unavailable(reason),
        )
    };
    let (capability_hits, capability_misses, capability_hit_ratio) = if capability_observations
        .is_empty()
    {
        unavailable_capability_cache(if capability_attempt_observed {
            MetricUnavailableReason::NotObserved
        } else {
            MetricUnavailableReason::NoProducer
        })
    } else {
        let hits = capability_observations
            .iter()
            .map(|diagnostics| u64::from(diagnostics.capability_cache_hit_count))
            .fold(0u64, u64::saturating_add);
        let misses = capability_observations
            .iter()
            .map(|diagnostics| u64::from(diagnostics.capability_cache_miss_count))
            .fold(0u64, u64::saturating_add);
        let hit_ratio = if hits.saturating_add(misses) == 0 {
            MetricValue::unavailable(MetricUnavailableReason::NotObserved)
        } else {
            MetricRatio::new(hits, hits + misses)
                .map(MetricValue::available)
                .unwrap_or_else(|| MetricValue::unavailable(MetricUnavailableReason::NotObserved))
        };
        (
            MetricValue::available(hits),
            MetricValue::available(misses),
            hit_ratio,
        )
    };
    let (source_template_hits, source_template_misses, source_template_materialization_latency_ms) =
        source_template_cache_metrics(
            &source_samples
                .iter()
                .map(|execution| &execution.diagnostics)
                .collect::<Vec<_>>(),
        );
    let local_overhead = metric_statistics(
        &samples,
        |execution| {
            let stage = &execution.result.stages.agent;
            stage.status != StageStatus::Skipped
                && stage.status != StageStatus::NotRun
                && (stage.status != StageStatus::Blocked
                    || !execution.diagnostics.agent.commands.is_empty()
                    || execution.diagnostics.provider_attempt_count > 0
                    || execution.diagnostics.model_turns > 0)
        },
        |execution| {
            let provider_wall = execution
                .diagnostics
                .provider_latency_ms
                .saturating_add(execution.diagnostics.probe_latency_ms);
            execution
                .diagnostics
                .agent_duration_ms
                .checked_sub(provider_wall)
        },
    );
    let control = ControlLoopMetrics {
        model_turns: metric_sum(&diagnostics, |_| true, |d| Some(u64::from(d.model_turns))),
        tool_calls: metric_sum(&diagnostics, |_| true, |d| Some(u64::from(d.tool_calls))),
        invalid_tool_calls: metric_sum(
            &diagnostics,
            |_| true,
            |d| Some(u64::from(d.invalid_tool_call_count)),
        ),
        repeated_tool_calls: metric_sum(
            &diagnostics,
            |_| true,
            |d| Some(u64::from(d.repeated_tool_call_count)),
        ),
        repair_attempts: metric_sum(
            &diagnostics,
            |_| true,
            |d| Some(u64::from(d.repair_attempt_count)),
        ),
        completion_rejections: metric_sum(
            &diagnostics,
            |_| true,
            |d| Some(u64::from(d.completion_rejection_count)),
        ),
        compactions: metric_sum(
            &diagnostics,
            |_| true,
            |d| Some(u64::from(d.compaction_count)),
        ),
        approval_count: metric_sum(
            &diagnostics,
            |_| true,
            |d| Some(u64::from(d.approval_count)),
        ),
        verification_required_commands: metric_sum(
            &diagnostics,
            |d| d.verification_observed,
            |d| Some(u64::from(d.verification_required_command_count)),
        ),
        verification_satisfied_commands: metric_sum(
            &diagnostics,
            |d| d.verification_observed,
            |d| Some(u64::from(d.verification_satisfied_command_count)),
        ),
    };
    let harness = build_harness_metrics(&samples, trace_metrics);
    EvaluationMetrics {
        timing: TimingMetrics {
            run_duration_ms: MetricValue::available(run_duration_ms),
            trial_duration_ms: all(|diagnostics| diagnostics.trial_duration_ms),
            source_preparation_duration_ms: metric_statistics(
                &source_samples,
                |execution| execution.diagnostics.source_tree_digest.is_some(),
                |execution| Some(execution.diagnostics.source_preparation_duration_ms),
            ),
            setup_duration_ms: metric_statistics(
                &samples,
                |execution| execution.diagnostics.agent_setup_ms.is_some(),
                |execution| execution.diagnostics.agent_setup_ms,
            ),
            baseline_duration_ms: metric_statistics(
                &samples,
                |execution| {
                    let stage = &execution.result.stages.baseline;
                    stage.status != StageStatus::Skipped
                        && stage.status != StageStatus::NotRun
                        && (stage.status != StageStatus::Blocked
                            || !execution.diagnostics.baseline.commands.is_empty())
                },
                |execution| Some(execution.diagnostics.baseline_duration_ms),
            ),
            agent_duration_ms: metric_statistics(
                &samples,
                |execution| {
                    let stage = &execution.result.stages.agent;
                    stage.status != StageStatus::Skipped
                        && stage.status != StageStatus::NotRun
                        && (stage.status != StageStatus::Blocked
                            || !execution.diagnostics.agent.commands.is_empty()
                            || execution.diagnostics.provider_attempt_count > 0
                            || execution.diagnostics.model_turns > 0)
                },
                |execution| Some(execution.diagnostics.agent_duration_ms),
            ),
            local_overhead_duration_ms: local_overhead,
            public_duration_ms: metric_statistics(
                &samples,
                |execution| {
                    let stage = &execution.result.stages.public;
                    stage.status != StageStatus::Skipped
                        && stage.status != StageStatus::NotRun
                        && (stage.status != StageStatus::Blocked
                            || !execution.diagnostics.public.commands.is_empty())
                },
                |execution| Some(execution.diagnostics.public_duration_ms),
            ),
            hidden_duration_ms: metric_statistics(
                &samples,
                |execution| {
                    let stage = &execution.result.stages.hidden;
                    stage.status != StageStatus::Skipped
                        && stage.status != StageStatus::NotRun
                        && (stage.status != StageStatus::Blocked
                            || !execution.diagnostics.hidden.commands.is_empty())
                },
                |execution| Some(execution.diagnostics.hidden_duration_ms),
            ),
            turn_duration_ms: trace_metric_statistics(
                trace_metrics,
                TraceMetricName::TurnDurationMs,
            ),
            tool_duration_ms: trace_metric_statistics(
                trace_metrics,
                TraceMetricName::ToolDurationMs,
            ),
        },
        provider_usage: ProviderUsageMetrics {
            completion_attempts,
            completion_retries,
            completion_latency_ms: completion_latency,
            probe_attempts,
            probe_retries,
            probe_latency_ms: probe_latency,
            time_to_first_token_ms: trace_metric_statistics(
                trace_metrics,
                TraceMetricName::ProviderTimeToFirstTokenMs,
            ),
            input_tokens,
            noncached_input_tokens,
            cached_input_tokens,
            output_tokens: usage(|diagnostics| diagnostics.output_tokens),
            reasoning_tokens: usage(|diagnostics| diagnostics.reasoning_tokens),
            total_tokens: usage(|diagnostics| diagnostics.total_tokens),
        },
        cache: CacheMetrics {
            capability_hits,
            capability_misses,
            capability_hit_ratio,
            source_template_hits,
            source_template_misses,
            source_template_materialization_latency_ms,
        },
        control_loop: control,
        harness,
    }
}

fn source_template_cache_metrics(
    diagnostics: &[&TaskDiagnostics],
) -> (
    MetricValue<u64>,
    MetricValue<u64>,
    MetricValue<MetricStatistics>,
) {
    let unavailable = |reason: MetricUnavailableReason| {
        (
            MetricValue::unavailable(reason),
            MetricValue::unavailable(reason),
            MetricValue::unavailable(reason),
        )
    };
    let expected = diagnostics
        .iter()
        .filter(|diagnostics| diagnostics.source_template_expected)
        .copied()
        .collect::<Vec<_>>();
    if expected.is_empty() {
        return unavailable(MetricUnavailableReason::NoProducer);
    }
    let observed = expected
        .into_iter()
        .filter(|diagnostics| diagnostics.source_template_cache_status.is_some())
        .collect::<Vec<_>>();
    if observed.is_empty() {
        return unavailable(MetricUnavailableReason::NotObserved);
    }
    let count = |status: SourceTemplateCacheStatus| {
        MetricValue::available(
            u64::try_from(
                observed
                    .iter()
                    .filter(|diagnostics| diagnostics.source_template_cache_status == Some(status))
                    .count(),
            )
            .unwrap_or(u64::MAX),
        )
    };
    (
        count(SourceTemplateCacheStatus::Hit),
        count(SourceTemplateCacheStatus::Miss),
        metric_statistics(
            &observed,
            |_| true,
            |diagnostics| Some(diagnostics.source_template_materialization_ms),
        ),
    )
}

/// Source-template cache failures are owned by Evaluation, not the model or the environment.
///
/// The stable `source_cache_*` blocker code overrides the generic kind-based attribution without
/// extending Result/v9 solely for this report projection.
fn source_cache_attribution_override(code: Option<&str>) -> Option<(FailureOwner, FailureStage)> {
    code.filter(|code| code.starts_with("source_cache_"))
        .map(|_| (FailureOwner::Evaluation, FailureStage::Evaluation))
}

fn build_failure_attributions(
    result: &EvaluationResult,
    executions: &[TaskEvaluation],
) -> Vec<FailureAttribution> {
    let mut failures = Vec::new();
    if executions.is_empty()
        && let Some(blocker) = &result.blocker
    {
        let (owner, stage) = source_cache_attribution_override(blocker.code.as_deref())
            .unwrap_or_else(|| {
                (
                    failure_owner_for_blocker(blocker.kind),
                    FailureStage::Evaluation,
                )
            });
        failures.push(FailureAttribution {
            owner,
            stage,
            task_id: blocker.task_id.clone(),
            trial: None,
            code: blocker.code.clone(),
            message: blocker.message.clone(),
        });
    }
    for execution in executions {
        for trial in &execution.trials {
            let result = &trial.result;
            if let Some(blocker) = &result.blocker {
                let (owner, stage) = source_cache_attribution_override(blocker.code.as_deref())
                    .unwrap_or_else(|| {
                        (
                            failure_owner_for_blocker(blocker.kind),
                            failure_stage_for_result(result),
                        )
                    });
                failures.push(FailureAttribution {
                    owner,
                    stage,
                    task_id: Some(execution.result.task_id.clone()),
                    trial: Some(result.trial),
                    code: blocker.code.clone(),
                    message: blocker.message.clone(),
                });
                continue;
            }
            if result.status != EvaluationStatus::Failed {
                continue;
            }
            let (owner, stage) = if !result.sandbox_security_success {
                (FailureOwner::Sandbox, FailureStage::Tool)
            } else if !result.agent_protocol_success {
                (FailureOwner::Harness, FailureStage::Agent)
            } else if !result.functional_task_success {
                (FailureOwner::Model, failure_stage_for_result(result))
            } else {
                (FailureOwner::Evaluation, FailureStage::Evaluation)
            };
            failures.push(FailureAttribution {
                owner,
                stage,
                task_id: Some(execution.result.task_id.clone()),
                trial: Some(result.trial),
                code: trial
                    .diagnostics
                    .provider_diagnostic
                    .as_ref()
                    .and_then(provider_diagnostic_code),
                message: trial
                    .diagnostics
                    .error
                    .as_deref()
                    .map(safe_text)
                    .unwrap_or_else(|| "evaluation trial failed".to_string()),
            });
        }
    }
    failures
}

fn failure_stage_for_result(result: &EvaluationTrialResult) -> FailureStage {
    if result.stages.baseline.status == StageStatus::Failed {
        FailureStage::Baseline
    } else if result.stages.agent.status == StageStatus::Failed {
        FailureStage::Agent
    } else if result.stages.public.status == StageStatus::Failed {
        FailureStage::Public
    } else if result.stages.hidden.status == StageStatus::Failed {
        FailureStage::Hidden
    } else {
        FailureStage::Evaluation
    }
}

fn finish_feedback_run(
    params: &EvaluationRunParams,
    run_dir: &Path,
    result: EvaluationResult,
    executions: &[TaskEvaluation],
    trace_metrics: Option<&TraceMetrics>,
    run_duration_ms: u64,
) -> Result<EvaluationRunResult, EvaluationRunError> {
    if let Err(error) = result.validate() {
        return Err(preserve_incomplete_run(
            run_dir,
            EvaluationRunError::infrastructure(format!(
                "invalid feedback evaluation result: {error}"
            )),
        ));
    }
    let report =
        build_evaluation_report(params, &result, executions, trace_metrics, run_duration_ms)
            .map_err(|error| {
                preserve_incomplete_run(
                    run_dir,
                    EvaluationRunError::infrastructure(format!(
                        "invalid feedback evaluation report: {error}"
                    )),
                )
            })?;
    let report_path = run_dir.join(REPORT_FILE);
    write_json_atomic(&report_path, &report).map_err(|error| {
        preserve_incomplete_run(
            run_dir,
            EvaluationRunError::publication(format!(
                "failed to write feedback evaluation report: {error}"
            )),
        )
    })?;
    let status = enum_string(result.status).map_err(|error| {
        preserve_incomplete_run(run_dir, EvaluationRunError::infrastructure(error))
    })?;
    let blocker = result
        .blocker
        .as_ref()
        .map(blocker_code)
        .transpose()
        .map_err(|error| {
            preserve_incomplete_run(run_dir, EvaluationRunError::infrastructure(error))
        })?;
    let task_reports = report.tasks.clone();
    Ok(EvaluationRunResult {
        run_id: result.run_id.as_str().to_string(),
        manifest: params.manifest.clone(),
        runner: RUNNER_NAME.to_string(),
        max_workers: params.max_workers,
        status,
        blocker,
        tasks: task_reports,
        result_path: None,
        report_path: Some(report_path.to_string_lossy().into_owned()),
        evidence_path: None,
        evaluation_passed: false,
        gate_applicable: false,
    })
}

fn publish_zero_sampling_blocked_run(
    params: &EvaluationRunParams,
    run_dir: &Path,
    manifest_digest: String,
    plans: &[WorkspacePlan],
    result: EvaluationResult,
    preflight: EvaluationSandboxPreflight,
    run_duration_ms: u64,
) -> Result<EvaluationRunResult, EvaluationRunError> {
    let run_id = &result.run_id;
    if let Err(error) = result.validate() {
        return Err(preserve_incomplete_run(
            run_dir,
            EvaluationRunError::infrastructure(format!(
                "invalid zero-sampling blocked evaluation result: {error}"
            )),
        ));
    }
    let evidence = match build_zero_sampling_evidence(
        run_id,
        manifest_digest,
        plans,
        result.summary.trials_per_task,
        preflight.clone(),
        &result,
    ) {
        Ok(evidence) => evidence,
        Err(error) => {
            return Err(preserve_incomplete_run(
                run_dir,
                EvaluationRunError::infrastructure(error),
            ));
        }
    };
    if let Err(error) = evidence.validate_against_result(&result) {
        return Err(preserve_incomplete_run(
            run_dir,
            EvaluationRunError::infrastructure(format!(
                "zero-sampling evidence/result mismatch: {error}"
            )),
        ));
    }
    let status = enum_string(result.status).map_err(|error| {
        preserve_incomplete_run(run_dir, EvaluationRunError::infrastructure(error))
    })?;
    let blocker = result
        .blocker
        .as_ref()
        .map(blocker_code)
        .transpose()
        .map_err(|error| {
            preserve_incomplete_run(run_dir, EvaluationRunError::infrastructure(error))
        })?;
    let report =
        build_evaluation_report(params, &result, &[], None, run_duration_ms).map_err(|error| {
            preserve_incomplete_run(
                run_dir,
                EvaluationRunError::infrastructure(format!("invalid evaluation report: {error}")),
            )
        })?;
    let published = publish_evaluation_artifacts(run_dir, run_id, &result, &report, &evidence)
        .map_err(|error| {
            preserve_incomplete_run(run_dir, EvaluationRunError::publication(error))
        })?;
    Ok(EvaluationRunResult {
        run_id: run_id.as_str().to_string(),
        manifest: params.manifest.clone(),
        runner: RUNNER_NAME.to_string(),
        max_workers: params.max_workers,
        status,
        blocker,
        tasks: Vec::new(),
        result_path: Some(published.result_path.to_string_lossy().into_owned()),
        report_path: Some(published.report_path.to_string_lossy().into_owned()),
        evidence_path: Some(published.evidence_path.to_string_lossy().into_owned()),
        evaluation_passed: false,
        gate_applicable: true,
    })
}

fn prepare_task_source(
    context: &EvaluationRunContext<'_, '_>,
    plan: &WorkspacePlan,
) -> PreparedTaskSource {
    let started = Instant::now();
    let task_root = context.run_dir.join(plan.task_id.as_str());
    let source_dir = task_root.join(SOURCE_DIR);
    let source_template_expected = matches!(plan.source, PlannedWorkspaceSource::RemoteGit { .. });
    let mut prepared = PreparedTaskSource {
        task_root,
        source_dir,
        source_snapshot: None,
        strict_sandbox_command_count: 0,
        local_process_fallback_count: 0,
        source_template_expected,
        source_template: None,
        source_commands: Vec::new(),
        duration_ms: 0,
        blocker: None,
    };
    if context.cancellation.is_cancelled() {
        prepared.blocker = Some(evaluation_blocker(
            BlockerKind::AgentRuntime,
            "evaluation cancelled",
        ));
        prepared.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        return prepared;
    }
    if let Err(error) = fs::create_dir(&prepared.task_root) {
        prepared.blocker = Some(evaluation_blocker(
            BlockerKind::WorkspacePreparation,
            format!(
                "failed to create task directory {}: {error}",
                prepared.task_root.display()
            ),
        ));
        prepared.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        return prepared;
    }
    if context.sandbox_preflight.outcome != SandboxPreflightOutcome::Supported {
        prepared.blocker = Some(sandbox_preflight_blocker(
            context
                .sandbox_preflight
                .error_code
                .clone()
                .unwrap_or_else(|| "sandbox_preflight_unavailable".to_string()),
            "validated sandbox preflight contract is not supported",
        ));
        prepared.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        return prepared;
    }
    match prepare_source(
        &plan.source,
        &plan.task_id,
        &prepared.task_root,
        &prepared.source_dir,
        Arc::clone(context.sandbox_backend),
        context.source_cache,
        context.cancellation,
    ) {
        Ok(MaterializedSource {
            commands,
            snapshot,
            strict_sandbox_command_count,
            local_process_fallback_count,
            source_template,
        }) => {
            prepared.source_commands = commands;
            prepared.source_snapshot = Some(snapshot);
            prepared.strict_sandbox_command_count = strict_sandbox_command_count;
            prepared.local_process_fallback_count = local_process_fallback_count;
            prepared.source_template = source_template;
        }
        Err((blocker, commands, strict_sandbox_command_count, local_process_fallback_count)) => {
            prepared.source_commands = commands;
            prepared.strict_sandbox_command_count = strict_sandbox_command_count;
            prepared.local_process_fallback_count = local_process_fallback_count;
            prepared.blocker = Some(blocker);
        }
    }
    prepared.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    prepared
}

fn run_task_trial_with_prepared_source(
    context: &EvaluationRunContext<'_, '_>,
    plan: &WorkspacePlan,
    prepared_source: &PreparedTaskSource,
    trial: u32,
    recovery_injected: bool,
) -> TaskExecution {
    if let Some(blocker) = &prepared_source.blocker {
        return blocked_task_trials(
            plan,
            1,
            blocker.clone(),
            prepared_source.source_commands.clone(),
            prepared_source.strict_sandbox_command_count,
            prepared_source.local_process_fallback_count,
            prepared_source.duration_ms,
            prepared_source.source_template_expected,
            prepared_source.source_template.as_ref(),
            matches!(blocker.kind, BlockerKind::WorkspacePreparation),
            Some(trial),
        )
        .trials
        .into_iter()
        .next()
        .expect("blocked trial projection must contain one trial");
    }
    if context.cancellation.is_cancelled() {
        return blocked_task_trials(
            plan,
            1,
            evaluation_blocker(BlockerKind::AgentRuntime, "evaluation cancelled"),
            prepared_source.source_commands.clone(),
            prepared_source.strict_sandbox_command_count,
            prepared_source.local_process_fallback_count,
            prepared_source.duration_ms,
            prepared_source.source_template_expected,
            prepared_source.source_template.as_ref(),
            false,
            Some(trial),
        )
        .trials
        .into_iter()
        .next()
        .expect("cancelled trial projection must contain one trial");
    }
    let Some(source_snapshot) = prepared_source.source_snapshot.as_ref() else {
        return blocked_task_trials(
            plan,
            1,
            evaluation_blocker(
                BlockerKind::WorkspacePreparation,
                "prepared source snapshot is unavailable",
            ),
            prepared_source.source_commands.clone(),
            prepared_source.strict_sandbox_command_count,
            prepared_source.local_process_fallback_count,
            prepared_source.duration_ms,
            prepared_source.source_template_expected,
            prepared_source.source_template.as_ref(),
            true,
            Some(trial),
        )
        .trials
        .into_iter()
        .next()
        .expect("missing source trial projection must contain one trial");
    };
    let prepared = PreparedTaskContext {
        run_id: context.run_id,
        task_root: &prepared_source.task_root,
        source_dir: &prepared_source.source_dir,
        source_snapshot,
        strict_sandbox_command_count: prepared_source.strict_sandbox_command_count,
        local_process_fallback_count: prepared_source.local_process_fallback_count,
        source_template_expected: prepared_source.source_template_expected,
        source_template: prepared_source.source_template.as_ref(),
        source_commands: &prepared_source.source_commands,
        source_preparation_duration_ms: prepared_source.duration_ms,
        plan,
        sandbox_backend: context.sandbox_backend,
        provider_snapshot: context.provider_snapshot,
        cancellation: context.cancellation,
        trace_store: Arc::clone(&context.trace_store),
        trace_failures: &context.trace_failures,
    };
    run_task(&prepared, trial, recovery_injected)
}

#[allow(clippy::too_many_arguments)]
fn blocked_task_trials(
    plan: &WorkspacePlan,
    trials_per_task: u32,
    blocker: EvaluationBlocker,
    source_commands: Vec<CommandDiagnostic>,
    source_strict_sandbox_command_count: usize,
    source_local_process_fallback_count: usize,
    source_preparation_duration_ms: u64,
    source_template_expected: bool,
    source_template: Option<&SourceTemplatePreparation>,
    source_preparation_failed: bool,
    selected_trial: Option<u32>,
) -> TaskEvaluation {
    let trial_ordinals = selected_trial
        .map(|trial| vec![trial])
        .unwrap_or_else(|| (1..=trials_per_task).collect());
    let trials = trial_ordinals
        .into_iter()
        .map(|trial| {
            let mut diagnostics = TaskDiagnostics {
                source_commands: source_commands.clone(),
                strict_sandbox_command_count: source_strict_sandbox_command_count,
                local_process_fallback_count: source_local_process_fallback_count,
                source_preparation_duration_ms,
                source_template_expected,
                source_template_cache_status: source_template.map(|source| source.status),
                source_template_materialization_ms: source_template
                    .map_or(0, |source| source.materialization_ms),
                error: Some(blocker.message.clone()),
                ..TaskDiagnostics::default()
            };
            if source_preparation_failed {
                diagnostics.baseline.message = Some(blocker.message.clone());
                finish_task(
                    trial,
                    StageExecution::blocked(blocker.clone(), Vec::new()),
                    StageExecution::skipped(
                        "agent stage skipped because source preparation failed",
                    ),
                    StageExecution::skipped(
                        "public stage skipped because source preparation failed",
                    ),
                    StageExecution::skipped(
                        "hidden stage skipped because source preparation failed",
                    ),
                    diagnostics,
                )
            } else {
                blocked_task_before_workspace(trial, blocker.clone(), diagnostics)
            }
        })
        .collect();
    task_evaluation_from_trials(plan, trials)
}

fn task_evaluation_from_trials(plan: &WorkspacePlan, trials: Vec<TaskExecution>) -> TaskEvaluation {
    let result = EvaluationTaskResult::from_trials(
        plan.task_id.clone(),
        plan.capabilities.clone(),
        trials.iter().map(|trial| trial.result.clone()).collect(),
    );
    TaskEvaluation { result, trials }
}

fn run_task(
    prepared: &PreparedTaskContext<'_, '_>,
    trial: u32,
    recovery_injected: bool,
) -> TaskExecution {
    let scope = format!("trial:{}:{}", prepared.plan.task_id.as_str(), trial);
    let session_id = scope.clone();
    let span_id = evaluation_span_id(prepared.run_id, &scope, TraceSpanKind::Turn);
    let task_span_id = evaluation_span_id(
        prepared.run_id,
        &format!("task:{}", prepared.plan.task_id.as_str()),
        TraceSpanKind::Task,
    );
    let started = Instant::now();
    let trace = EvaluationTrialTrace::new(
        Arc::clone(&prepared.trace_store),
        prepared.run_id,
        prepared.trace_failures,
        session_id,
        span_id,
        task_span_id,
    );
    trace.sink.start(
        &trace.session_id,
        &trace.turn_span_id,
        Some(&trace.task_span_id),
        TraceSpanKind::Turn,
        "evaluation trial",
    );
    let mut execution = run_task_inner(prepared, trial, recovery_injected, &trace);
    execution.diagnostics.trial_duration_ms =
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    trace.sink.end(
        &trace.session_id,
        &trace.turn_span_id,
        Some(&trace.task_span_id),
        TraceSpanKind::Turn,
        evaluation_status_trace_status(execution.result.status),
        started,
    );
    execution
}

fn run_task_inner(
    prepared: &PreparedTaskContext<'_, '_>,
    trial: u32,
    recovery_injected: bool,
    trace: &EvaluationTrialTrace<'_>,
) -> TaskExecution {
    let task_dir = prepared.task_root.join(format!("trial-{trial:04}"));
    let mut diagnostics = TaskDiagnostics {
        source_commands: prepared.source_commands.to_vec(),
        strict_sandbox_command_count: prepared.strict_sandbox_command_count,
        local_process_fallback_count: prepared.local_process_fallback_count,
        source_preparation_duration_ms: prepared.source_preparation_duration_ms,
        source_tree_digest: Some(
            workspace_snapshot_digest(prepared.source_snapshot)
                .expect("prepared workspace snapshot serializes"),
        ),
        source_template_expected: prepared.source_template_expected,
        source_template_cache_status: prepared.source_template.map(|source| source.status),
        source_template_materialization_ms: prepared
            .source_template
            .map_or(0, |source| source.materialization_ms),
        ..TaskDiagnostics::default()
    };
    if let Err(error) = fs::create_dir(&task_dir) {
        let blocker = evaluation_blocker(
            BlockerKind::WorkspacePreparation,
            format!(
                "failed to create task directory {}: {error}",
                task_dir.display()
            ),
        );
        return blocked_task_before_workspace(trial, blocker, diagnostics);
    }

    let provider = match prepared.provider_snapshot.provider() {
        Ok(provider) => provider,
        Err(error) => {
            let blocker = provider_blocker(&error);
            diagnostics.error = Some(safe_text(error.message));
            return blocked_task_before_workspace(trial, blocker, diagnostics);
        }
    };
    let agent_dir = task_dir.join(AGENT_DIR);
    match materialize_prepared_workspace(
        prepared.source_dir,
        &agent_dir,
        prepared.source_snapshot,
        prepared.cancellation,
    ) {
        Ok(()) => {}
        Err(error) => {
            let baseline = StageExecution::blocked(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                Vec::new(),
            );
            diagnostics.baseline = baseline.diagnostics.clone();
            return finish_task(
                trial,
                baseline,
                StageExecution::skipped(
                    "agent stage skipped because trial workspace is unavailable",
                ),
                StageExecution::skipped(
                    "public stage skipped because trial workspace is unavailable",
                ),
                StageExecution::skipped(
                    "hidden stage skipped because trial workspace is unavailable",
                ),
                diagnostics,
            );
        }
    }
    let mut setup_diagnostics = Vec::new();
    let setup_started = Instant::now();
    let setup_result = run_setup_commands(
        &agent_dir,
        &prepared.plan.setup_commands,
        Arc::clone(prepared.sandbox_backend),
        &mut setup_diagnostics,
        &mut diagnostics,
    );
    diagnostics.agent_setup_ms =
        Some(u64::try_from(setup_started.elapsed().as_millis()).unwrap_or(u64::MAX));
    let baseline_started = Instant::now();
    let baseline = match setup_result {
        Ok(()) => run_verification_after_setup(
            &agent_dir,
            prepared.plan.baseline.test_patch.as_ref(),
            &prepared.plan.baseline.commands,
            prepared.plan.baseline.expectation,
            Arc::clone(prepared.sandbox_backend),
            setup_diagnostics,
            &mut diagnostics,
        ),
        Err(blocker) => StageExecution::blocked(blocker, setup_diagnostics),
    };
    diagnostics.baseline_duration_ms =
        u64::try_from(baseline_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    diagnostics.baseline = baseline.diagnostics.clone();
    if baseline.result.status != StageStatus::Passed {
        let agent = StageExecution::skipped(
            "agent stage skipped because baseline did not fail as expected",
        );
        let public = StageExecution::skipped("public stage skipped because baseline did not pass");
        let hidden = StageExecution::skipped("hidden stage skipped because baseline did not pass");
        return finish_task(trial, baseline, agent, public, hidden, diagnostics);
    }

    let agent_execution = if recovery_injected {
        run_recovery_agent_stage(
            prepared,
            &task_dir,
            &agent_dir,
            &prepared.plan.agent,
            trial,
            trace,
            &mut diagnostics,
        )
    } else {
        run_agent_stage(
            prepared,
            &task_dir,
            &agent_dir,
            &prepared.plan.agent,
            trial,
            provider,
            trace,
            &mut diagnostics,
        )
    };
    diagnostics.agent = agent_execution.stage.diagnostics.clone();
    diagnostics.changed_files = agent_execution.changed_files.clone();
    diagnostics.patch_evidence = agent_execution.patch_evidence.clone();
    diagnostics.patch_digest = agent_execution.patch_digest.clone();
    diagnostics.patch_evidence_path = agent_execution.patch_evidence_path.clone();
    diagnostics.model_turns = agent_execution.model_turns;
    diagnostics.tool_calls = agent_execution.tool_calls;
    diagnostics.approval_count = agent_execution.approval_count;
    diagnostics.invalid_tool_call_count = agent_execution.recovery_metrics.invalid_tool_call_count;
    diagnostics.repeated_tool_call_count =
        agent_execution.recovery_metrics.repeated_tool_call_count;
    diagnostics.repair_attempt_count = agent_execution.recovery_metrics.repair_attempt_count;
    diagnostics.completion_rejection_count =
        agent_execution.recovery_metrics.completion_rejection_count;
    diagnostics.compaction_count = agent_execution.compaction_count;
    diagnostics.verification_required_command_count =
        agent_execution.verification_required_command_count;
    diagnostics.verification_satisfied_command_count =
        agent_execution.verification_satisfied_command_count;
    diagnostics.input_tokens = agent_execution.model_usage.input_tokens;
    diagnostics.output_tokens = agent_execution.model_usage.output_tokens;
    diagnostics.cached_input_tokens = agent_execution.model_usage.cached_input_tokens;
    diagnostics.reasoning_tokens = agent_execution.model_usage.reasoning_tokens;
    diagnostics.total_tokens = agent_execution.model_usage.total_tokens;
    diagnostics.provider_attempt_count = agent_execution.provider_attempts.attempt_count;
    diagnostics.provider_retry_count = agent_execution.provider_attempts.retry_count;
    diagnostics.provider_latency_ms = agent_execution.provider_attempts.latency_ms;
    diagnostics.agent_duration_ms = agent_execution.agent_duration_ms;
    diagnostics.local_process_fallback_unknown_count =
        agent_execution.local_process_fallback_unknown_count;
    diagnostics.trace_path = agent_execution.trace_path.clone();
    diagnostics.error = agent_execution.error.clone();
    diagnostics.provider_diagnostic = agent_execution.provider_diagnostic.clone();
    diagnostics.prompt_structure = agent_execution.prompt_structure.clone();
    diagnostics.prompt_fingerprint = agent_execution.prompt_fingerprint.clone();
    diagnostics.tool_schema_fingerprint = agent_execution.tool_schema_fingerprint.clone();
    diagnostics.provider_evidence = agent_execution.provider_evidence.clone();
    diagnostics.verification_bypass_count = agent_execution.verification_bypass_count;
    if !recovery_injected {
        diagnostics.verification_observed = true;
    }
    if agent_execution.stage.result.status == StageStatus::Blocked
        && (agent_execution.workspace.is_none() || agent_execution.patch_evidence.is_empty())
    {
        return finish_task(
            trial,
            baseline,
            agent_execution.stage,
            StageExecution::skipped("public stage skipped because agent execution was blocked"),
            StageExecution::skipped("hidden stage skipped because agent execution was blocked"),
            diagnostics,
        );
    }

    let Some(agent_workspace) = agent_execution.workspace.as_deref() else {
        let public =
            StageExecution::skipped("public stage skipped because agent workspace is unavailable");
        let hidden =
            StageExecution::skipped("hidden stage skipped because agent workspace is unavailable");
        return finish_task(
            trial,
            baseline,
            agent_execution.stage,
            public,
            hidden,
            diagnostics,
        );
    };

    if prepared.cancellation.is_cancelled() {
        return finish_task(
            trial,
            baseline,
            agent_execution.stage,
            StageExecution::skipped("public stage skipped because evaluation was cancelled"),
            StageExecution::skipped("hidden stage skipped because evaluation was cancelled"),
            diagnostics,
        );
    }

    let ((public, public_duration_ms), (hidden, hidden_duration_ms)) =
        run_post_agent_verification_stages(
            agent_workspace,
            &prepared.plan.public,
            &prepared.plan.hidden,
            prepared.sandbox_backend,
            &mut diagnostics,
        );
    diagnostics.public_duration_ms = public_duration_ms;
    diagnostics.hidden_duration_ms = hidden_duration_ms;
    diagnostics.public = public.diagnostics.clone();
    diagnostics.hidden = hidden.diagnostics.clone();
    finish_task(
        trial,
        baseline,
        agent_execution.stage,
        public,
        hidden,
        diagnostics,
    )
}

fn run_post_agent_verification_stages(
    workspace: &Path,
    public_plan: &VerificationStagePlan,
    hidden_plan: &VerificationStagePlan,
    sandbox_backend: &SharedSandboxBackend,
    diagnostics: &mut TaskDiagnostics,
) -> ((StageExecution, u64), (StageExecution, u64)) {
    let public_started = Instant::now();
    let public = run_verification_after_setup(
        workspace,
        public_plan.test_patch.as_ref(),
        &public_plan.commands,
        public_plan.expectation,
        Arc::clone(sandbox_backend),
        Vec::new(),
        diagnostics,
    );
    let public_duration = u64::try_from(public_started.elapsed().as_millis()).unwrap_or(u64::MAX);

    if public.result.status == StageStatus::Blocked {
        return (
            (public, public_duration),
            (
                StageExecution::skipped(
                    "hidden stage skipped because the shared trial workspace is unavailable",
                ),
                0,
            ),
        );
    }

    let hidden_started = Instant::now();
    let hidden = run_verification_after_setup(
        workspace,
        hidden_plan.test_patch.as_ref(),
        &hidden_plan.commands,
        hidden_plan.expectation,
        Arc::clone(sandbox_backend),
        Vec::new(),
        diagnostics,
    );
    let hidden_duration = u64::try_from(hidden_started.elapsed().as_millis()).unwrap_or(u64::MAX);

    ((public, public_duration), (hidden, hidden_duration))
}

fn redacted_remote_repository(repository: &str) -> String {
    let Some((scheme, remainder)) = repository.split_once("://") else {
        return "[redacted]".to_string();
    };
    let without_query = remainder.split(['?', '#']).next().unwrap_or(remainder);
    let without_userinfo = without_query
        .rsplit_once('@')
        .map_or(without_query, |(_, host_and_path)| host_and_path);
    safe_text(format!("{scheme}://{without_userinfo}"))
}

fn blocked_task_before_workspace(
    trial: u32,
    blocker: EvaluationBlocker,
    mut diagnostics: TaskDiagnostics,
) -> TaskExecution {
    diagnostics.agent.message = Some(blocker.message.clone());
    diagnostics.error = Some(blocker.message.clone());
    finish_task(
        trial,
        StageExecution::skipped("baseline stage not run"),
        StageExecution::blocked(blocker, Vec::new()),
        StageExecution::skipped("public stage not run"),
        StageExecution::skipped("hidden stage not run"),
        diagnostics,
    )
}

fn finish_task(
    trial: u32,
    baseline: StageExecution,
    agent: StageExecution,
    public: StageExecution,
    hidden: StageExecution,
    diagnostics: TaskDiagnostics,
) -> TaskExecution {
    let stages = EvaluationStageResults {
        baseline: baseline.result,
        agent: agent.result,
        public: public.result,
        hidden: hidden.result,
    };
    let blocker = [
        &stages.baseline,
        &stages.agent,
        &stages.public,
        &stages.hidden,
    ]
    .into_iter()
    .find_map(|stage| stage.blocker.clone());
    let agent_completed = stages.agent.status == StageStatus::Passed;
    let tests_passed =
        stages.public.status == StageStatus::Passed && stages.hidden.status == StageStatus::Passed;
    let command_count = diagnostics
        .source_commands
        .iter()
        .chain(diagnostics.baseline.commands.iter())
        .chain(diagnostics.agent.commands.iter())
        .chain(diagnostics.public.commands.iter())
        .chain(diagnostics.hidden.commands.iter())
        .count();
    let strict_sandbox_command_count = diagnostics.strict_sandbox_command_count;
    let agent_command_count = diagnostics.agent.commands.len();
    let functional_task_success = stages.baseline.status == StageStatus::Passed
        && !diagnostics.patch_evidence.is_empty()
        && tests_passed;
    // A rejected completion is a recoverable AgentLoop repair episode. The protocol gate is
    // decided by the final terminal Agent state; the rejection count remains diagnostic output.
    let agent_protocol_success = agent_completed && diagnostics.error.is_none();
    let sandbox_security_success = agent_command_count > 0
        && strict_sandbox_command_count > 0
        && strict_sandbox_command_count == command_count
        && diagnostics.local_process_fallback_count == 0
        && diagnostics.local_process_fallback_unknown_count == 0;
    let evaluation_passed =
        functional_task_success && agent_protocol_success && sandbox_security_success;
    let status = if blocker.is_some() {
        EvaluationStatus::Blocked
    } else if evaluation_passed {
        EvaluationStatus::Completed
    } else {
        EvaluationStatus::Failed
    };
    let result = EvaluationTrialResult {
        trial,
        status,
        blocker,
        stages,
        agent_completed,
        tests_passed,
        functional_task_success,
        agent_protocol_success,
        sandbox_security_success,
        evaluation_passed,
        evidence: EvaluationEvidenceSummary {
            workspace_change_count: u32::try_from(diagnostics.patch_evidence.len())
                .unwrap_or(u32::MAX),
            patch_digest: diagnostics.patch_digest.clone(),
            tool_calls: diagnostics.tool_calls,
            model_turns: diagnostics.model_turns,
            approval_count: diagnostics.approval_count,
            invalid_tool_call_count: diagnostics.invalid_tool_call_count,
            repeated_tool_call_count: diagnostics.repeated_tool_call_count,
            repair_attempt_count: diagnostics.repair_attempt_count,
            completion_rejection_count: diagnostics.completion_rejection_count,
            compaction_count: diagnostics.compaction_count,
            verification_required_command_count: diagnostics.verification_required_command_count,
            verification_satisfied_command_count: diagnostics.verification_satisfied_command_count,
            provider_attempt_count: diagnostics.provider_attempt_count,
            provider_retry_count: diagnostics.provider_retry_count,
            input_tokens: diagnostics.input_tokens,
            output_tokens: diagnostics.output_tokens,
            cached_input_tokens: diagnostics.cached_input_tokens,
            reasoning_tokens: diagnostics.reasoning_tokens,
            total_tokens: diagnostics.total_tokens,
            provider_latency_ms: diagnostics.provider_latency_ms,
            agent_duration_ms: diagnostics.agent_duration_ms,
            strict_sandbox_command_count: u32::try_from(strict_sandbox_command_count)
                .unwrap_or(u32::MAX),
            local_process_fallback_count: u32::try_from(diagnostics.local_process_fallback_count)
                .unwrap_or(u32::MAX),
            local_process_fallback_unknown_count: u32::try_from(
                diagnostics.local_process_fallback_unknown_count,
            )
            .unwrap_or(u32::MAX),
        },
    };
    TaskExecution {
        result,
        diagnostics,
    }
}

fn record_command_security(
    result: &CommandResult,
    strict_sandbox_command_count: &mut usize,
    local_process_fallback_count: &mut usize,
) {
    if command_is_strictly_sandboxed(result) {
        *strict_sandbox_command_count = strict_sandbox_command_count.saturating_add(1);
    }
    if result.sandbox.local_process_fallback {
        *local_process_fallback_count = local_process_fallback_count.saturating_add(1);
    }
}

fn prepare_source(
    source: &PlannedWorkspaceSource,
    task_id: &TaskId,
    task_dir: &Path,
    source_dir: &Path,
    sandbox_backend: SharedSandboxBackend,
    source_cache: &SourceTemplateCache,
    cancellation: &CancellationToken,
) -> Result<MaterializedSource, (EvaluationBlocker, Vec<CommandDiagnostic>, usize, usize)> {
    let mut commands = Vec::new();
    let mut strict_sandbox_command_count = 0;
    let mut local_process_fallback_count = 0;
    match source {
        PlannedWorkspaceSource::Local { path } => {
            copy_tree_for_preparation(path, source_dir).map_err(|error| {
                (
                    evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                    commands.clone(),
                    strict_sandbox_command_count,
                    local_process_fallback_count,
                )
            })?;
        }
        PlannedWorkspaceSource::RemoteGit { repository, commit } => {
            let repository_identity = redacted_remote_repository(repository.as_str());
            let preparation = source_cache.prepare_remote(
                task_id.as_str(),
                &repository_identity,
                source_dir,
                cancellation,
                |staging_dir| {
                    let strategy = probe_remote_git_preparation_strategy(
                        task_dir,
                        Arc::clone(&sandbox_backend),
                        &mut commands,
                        &mut strict_sandbox_command_count,
                        &mut local_process_fallback_count,
                    )
                    .map_err(|blocker| blocker.message)?;
                    let clone_argv = match strategy {
                        RemoteGitPreparationStrategy::RevisionBound => vec![
                            "git".to_string(),
                            "clone".to_string(),
                            "--quiet".to_string(),
                            "--revision".to_string(),
                            commit.as_str().to_string(),
                            repository.as_str().to_string(),
                            SOURCE_DIR.to_string(),
                        ],
                        RemoteGitPreparationStrategy::CloneThenCheckout => vec![
                            "git".to_string(),
                            "clone".to_string(),
                            "--quiet".to_string(),
                            "--no-checkout".to_string(),
                            repository.as_str().to_string(),
                            SOURCE_DIR.to_string(),
                        ],
                    };
                    let clone = run_workspace_preparation_command(
                        task_dir,
                        task_dir,
                        clone_argv,
                        GIT_TIMEOUT_SECONDS,
                        SandboxNetworkMode::Allowed,
                        Arc::clone(&sandbox_backend),
                    );
                    record_command_security(
                        &clone,
                        &mut strict_sandbox_command_count,
                        &mut local_process_fallback_count,
                    );
                    commands.push(CommandDiagnostic::new("source.git_clone", &clone));
                    if !command_succeeded(&clone) {
                        return Err(command_blocker(
                            &clone,
                            BlockerKind::WorkspacePreparation,
                            "git clone failed",
                        )
                        .message);
                    }
                    if matches!(strategy, RemoteGitPreparationStrategy::CloneThenCheckout) {
                        let checkout = run_workspace_preparation_command(
                            task_dir,
                            task_dir,
                            vec![
                                "git".to_string(),
                                "-C".to_string(),
                                SOURCE_DIR.to_string(),
                                "checkout".to_string(),
                                "--quiet".to_string(),
                                "--detach".to_string(),
                                commit.as_str().to_string(),
                            ],
                            GIT_TIMEOUT_SECONDS,
                            SandboxNetworkMode::Denied,
                            Arc::clone(&sandbox_backend),
                        );
                        record_command_security(
                            &checkout,
                            &mut strict_sandbox_command_count,
                            &mut local_process_fallback_count,
                        );
                        commands.push(CommandDiagnostic::new("source.git_checkout", &checkout));
                        if !command_succeeded(&checkout) {
                            return Err(command_blocker(
                                &checkout,
                                BlockerKind::WorkspacePreparation,
                                "git checkout failed",
                            )
                            .message);
                        }
                    }
                    verify_remote_git_checkout(
                        task_dir,
                        commit,
                        Arc::clone(&sandbox_backend),
                        &mut commands,
                        &mut strict_sandbox_command_count,
                        &mut local_process_fallback_count,
                    )
                    .map_err(|blocker| blocker.message)?;
                    // The published template must not carry Git metadata; removal happens after
                    // the controller-owned checkout verification, on the run-owned task tree.
                    let prepared_source = task_dir.join(SOURCE_DIR);
                    strip_source_git_metadata(&prepared_source).map_err(|error| {
                        format!("failed to strip .git metadata from cloned source: {error}")
                    })?;
                    // 把本次 run 已校验的干净代码树放入缓存 staging，由 prepare_remote 快照校验后
                    // 原子发布为固定模板；克隆本身只写入 task_dir（sandbox 写边界内）。
                    copy_tree_checked(&prepared_source, staging_dir).map_err(|error| {
                        format!("failed to stage prepared source into the source-template cache: {error}")
                    })?;
                    // 任务侧 source 目录随后由 prepare_remote 从发布后的模板统一物化，
                    // 这里移除本 run 的临时副本，避免与物化目标冲突。
                    fs::remove_dir_all(&prepared_source).map_err(|error| {
                        format!("failed to remove run-local source preparation copy: {error}")
                    })?;
                    Ok(())
                },
            );
            let preparation = match preparation {
                Ok(preparation) => preparation,
                Err(error) => {
                    // Source-cache failures are Evaluation-owned infrastructure failures with a
                    // stable code; the code overrides the generic workspace-preparation
                    // attribution downstream without extending Result/v9.
                    let mut blocker =
                        evaluation_blocker(BlockerKind::WorkspacePreparation, error.to_string());
                    blocker.code = Some(error.stable_code().to_string());
                    blocker.task_id = Some(task_id.clone());
                    return Err((
                        blocker,
                        commands,
                        strict_sandbox_command_count,
                        local_process_fallback_count,
                    ));
                }
            };
            let source_template = Some(preparation);
            let snapshot = snapshot_workspace(source_dir).map_err(|error| {
                (
                    evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                    commands.clone(),
                    strict_sandbox_command_count,
                    local_process_fallback_count,
                )
            })?;
            return Ok(MaterializedSource {
                commands,
                snapshot,
                strict_sandbox_command_count,
                local_process_fallback_count,
                source_template,
            });
        }
    };
    let snapshot = snapshot_workspace(source_dir).map_err(|error| {
        (
            evaluation_blocker(BlockerKind::WorkspacePreparation, error),
            commands.clone(),
            strict_sandbox_command_count,
            local_process_fallback_count,
        )
    })?;
    Ok(MaterializedSource {
        commands,
        snapshot,
        strict_sandbox_command_count,
        local_process_fallback_count,
        source_template: None,
    })
}

/// Probe the selected Git executable without relying on a clone failure to detect capability.
fn probe_remote_git_preparation_strategy(
    task_dir: &Path,
    sandbox_backend: SharedSandboxBackend,
    commands: &mut Vec<CommandDiagnostic>,
    strict_sandbox_command_count: &mut usize,
    local_process_fallback_count: &mut usize,
) -> Result<RemoteGitPreparationStrategy, EvaluationBlocker> {
    let result = run_workspace_preparation_read_only_command(
        task_dir,
        task_dir,
        vec!["git".to_string(), "--version".to_string()],
        GIT_TIMEOUT_SECONDS,
        SandboxNetworkMode::Denied,
        sandbox_backend,
    );
    record_command_security(
        &result,
        strict_sandbox_command_count,
        local_process_fallback_count,
    );
    commands.push(CommandDiagnostic::new("source.git_version", &result));
    if !remote_source_probe_succeeded(&result) {
        return Err(evaluation_blocker(
            BlockerKind::Environment,
            format!(
                "git capability probe failed: {}",
                result.stderr_preview.trim()
            ),
        ));
    }
    let Some((major, minor)) = parse_git_version(&result.stdout_preview) else {
        return Err(evaluation_blocker(
            BlockerKind::Environment,
            "git capability probe returned an unrecognized version",
        ));
    };
    if major > 2 || (major == 2 && minor >= 49) {
        Ok(RemoteGitPreparationStrategy::RevisionBound)
    } else {
        Ok(RemoteGitPreparationStrategy::CloneThenCheckout)
    }
}

/// Parse the stable numeric prefix from `git --version` output.
fn parse_git_version(output: &str) -> Option<(u32, u32)> {
    output.split_whitespace().find_map(|token| {
        let mut components = token.split('.');
        let major = components.next()?.parse::<u32>().ok()?;
        let minor = components.next()?.parse::<u32>().ok()?;
        Some((major, minor))
    })
}

/// Remove the `.git` directory from a freshly verified clone so the published template and every
/// materialized run-owned source stay free of Git metadata.
///
/// A real clone or checkout always produces `.git`; its absence is tolerated here because
/// `source_facts` rejects any tree that still carries Git metadata before publication.
fn strip_source_git_metadata(source_dir: &Path) -> Result<(), String> {
    let git_dir = source_dir.join(".git");
    if !git_dir.exists() {
        return Ok(());
    }
    fs::remove_dir_all(&git_dir).map_err(|error| error.to_string())
}

/// Verify both the exact requested object and detached-HEAD state after source materialization.
fn verify_remote_git_checkout(
    task_dir: &Path,
    commit: &crate::GitCommit,
    sandbox_backend: SharedSandboxBackend,
    commands: &mut Vec<CommandDiagnostic>,
    strict_sandbox_command_count: &mut usize,
    local_process_fallback_count: &mut usize,
) -> Result<String, EvaluationBlocker> {
    let revision = run_workspace_preparation_read_only_command(
        task_dir,
        task_dir,
        vec![
            "git".to_string(),
            "-C".to_string(),
            SOURCE_DIR.to_string(),
            "rev-parse".to_string(),
            "--verify".to_string(),
            "HEAD".to_string(),
        ],
        GIT_TIMEOUT_SECONDS,
        SandboxNetworkMode::Denied,
        Arc::clone(&sandbox_backend),
    );
    record_command_security(
        &revision,
        strict_sandbox_command_count,
        local_process_fallback_count,
    );
    commands.push(CommandDiagnostic::new(
        "source.git_verify_commit",
        &revision,
    ));
    if !remote_source_probe_succeeded(&revision) {
        return Err(command_blocker(
            &revision,
            BlockerKind::WorkspacePreparation,
            "git commit verification failed",
        ));
    }
    if revision.output_truncated || revision.stdout_preview.trim() != commit.as_str() {
        return Err(evaluation_blocker(
            BlockerKind::WorkspacePreparation,
            "git checkout resolved an unexpected commit",
        ));
    }

    let head = run_workspace_preparation_read_only_command(
        task_dir,
        task_dir,
        vec![
            "git".to_string(),
            "-C".to_string(),
            SOURCE_DIR.to_string(),
            "symbolic-ref".to_string(),
            "--quiet".to_string(),
            "--short".to_string(),
            "HEAD".to_string(),
        ],
        GIT_TIMEOUT_SECONDS,
        SandboxNetworkMode::Denied,
        sandbox_backend,
    );
    record_command_security(
        &head,
        strict_sandbox_command_count,
        local_process_fallback_count,
    );
    commands.push(CommandDiagnostic::new("source.git_verify_detached", &head));
    if !detached_head_probe_succeeded(&head) {
        return Err(command_blocker(
            &head,
            BlockerKind::WorkspacePreparation,
            "git checkout did not leave a detached HEAD",
        ));
    }
    Ok(revision.stdout_preview.trim().to_string())
}

/// The fixed symbolic-ref probe succeeds only when Git reports the expected detached state.
fn detached_head_probe_succeeded(result: &CommandResult) -> bool {
    result.execution_status == CommandExecutionStatus::Completed
        && result.semantic_status == CommandSemanticStatus::ExitNonzero
        && result.exit_code == Some(1)
        && result.stderr_preview.trim().is_empty()
        && matches!(
            result.workspace_mutation,
            WorkspaceMutation::Unknown | WorkspaceMutation::Unchanged
        )
        && !result.sandbox.local_process_fallback
        && result.sandbox.enforcement == singularity_tools::SandboxBackendEnforcement::Strict
}
fn run_verification_after_setup(
    workspace: &Path,
    test_patch: Option<&crate::EvaluatorTestPatch>,
    commands: &[CommandSpec],
    expectation: CommandExpectation,
    sandbox_backend: SharedSandboxBackend,
    mut diagnostics: Vec<CommandDiagnostic>,
    task_diagnostics: &mut TaskDiagnostics,
) -> StageExecution {
    let patch_path = match test_patch {
        Some(test_patch) => match apply_evaluator_patch(
            workspace,
            test_patch,
            Arc::clone(&sandbox_backend),
            &mut diagnostics,
            task_diagnostics,
        ) {
            Ok(path) => Some(path),
            Err(blocker) => return StageExecution::blocked(blocker, diagnostics),
        },
        None => None,
    };

    let command_result = (|| -> Result<(usize, usize), EvaluationBlocker> {
        let mut successes = 0usize;
        let mut failures = 0usize;
        for (index, command) in commands.iter().enumerate() {
            let result = run_command_spec(
                workspace,
                command,
                DEFAULT_COMMAND_TIMEOUT_SECONDS,
                Arc::clone(&sandbox_backend),
            )
            .map_err(|error| evaluation_blocker(BlockerKind::WorkspacePreparation, error))?;
            record_command_security(
                &result,
                &mut task_diagnostics.strict_sandbox_command_count,
                &mut task_diagnostics.local_process_fallback_count,
            );
            diagnostics.push(CommandDiagnostic::for_spec(
                format!("verification.command.{index}"),
                &result,
            ));
            if let Some(blocker) = infrastructure_blocker(&result, "verification command failed") {
                return Err(blocker);
            }
            if result.workspace_mutation == WorkspaceMutation::Changed
                && result
                    .workspace_change_summary
                    .as_ref()
                    .is_none_or(|summary| !summary.is_trusted_artifact_only())
            {
                return Err(evaluation_blocker(
                    BlockerKind::Sandbox,
                    "verification command modified the revision-bound trial workspace",
                ));
            }
            // A nonzero result is ordinary verification evidence when success was expected, but it
            // becomes the baseline's accepted outcome when failure was expected. Accepting that
            // outcome still requires a proven workspace observation.
            if expectation == CommandExpectation::Failure
                && result.execution_status == CommandExecutionStatus::Completed
                && result.semantic_status != CommandSemanticStatus::Succeeded
                && result.workspace_mutation == WorkspaceMutation::Unknown
            {
                return Err(evaluation_blocker(
                    BlockerKind::Sandbox,
                    "verification command failed: workspace mutation could not be verified",
                ));
            }
            if command_succeeded(&result) {
                successes += 1;
            } else {
                failures += 1;
            }
        }
        Ok((successes, failures))
    })();

    let revert_result = match patch_path {
        Some(path) => revert_evaluator_patch(
            workspace,
            &path,
            Arc::clone(&sandbox_backend),
            &mut diagnostics,
            task_diagnostics,
        ),
        None => Ok(()),
    };
    let (successes, failures) = match (command_result, revert_result) {
        (Ok(counts), Ok(())) => counts,
        (Err(blocker), Ok(())) | (Ok(_), Err(blocker)) => {
            return StageExecution::blocked(blocker, diagnostics);
        }
        (Err(primary), Err(cleanup)) => {
            return StageExecution::blocked(
                evaluation_blocker(
                    primary.kind,
                    format!("{}; {}", primary.message, cleanup.message),
                ),
                diagnostics,
            );
        }
    };

    match expectation {
        CommandExpectation::Success if successes == commands.len() => {
            StageExecution::passed(diagnostics)
        }
        CommandExpectation::Failure if failures > 0 => StageExecution::passed(diagnostics),
        CommandExpectation::Success => StageExecution::failed(
            format!("{failures} verification command(s) failed"),
            diagnostics,
        ),
        CommandExpectation::Failure => {
            StageExecution::failed("baseline commands unexpectedly succeeded", diagnostics)
        }
    }
}

fn run_setup_commands(
    workspace: &Path,
    commands: &[CommandSpec],
    sandbox_backend: SharedSandboxBackend,
    diagnostics: &mut Vec<CommandDiagnostic>,
    task_diagnostics: &mut TaskDiagnostics,
) -> Result<(), EvaluationBlocker> {
    for (index, command) in commands.iter().enumerate() {
        let result = run_command_spec(
            workspace,
            command,
            DEFAULT_SETUP_TIMEOUT_SECONDS,
            Arc::clone(&sandbox_backend),
        )
        .map_err(|error| evaluation_blocker(BlockerKind::WorkspacePreparation, error))?;
        record_command_security(
            &result,
            &mut task_diagnostics.strict_sandbox_command_count,
            &mut task_diagnostics.local_process_fallback_count,
        );
        diagnostics.push(CommandDiagnostic::for_spec(
            format!("setup.command.{index}"),
            &result,
        ));
        if !command_succeeded(&result) {
            return Err(command_blocker(
                &result,
                BlockerKind::WorkspacePreparation,
                "setup command failed",
            ));
        }
    }
    Ok(())
}

fn apply_evaluator_patch(
    workspace: &Path,
    patch: &crate::EvaluatorTestPatch,
    sandbox_backend: SharedSandboxBackend,
    diagnostics: &mut Vec<CommandDiagnostic>,
    task_diagnostics: &mut TaskDiagnostics,
) -> Result<PathBuf, EvaluationBlocker> {
    if patch.format != PatchFormat::UnifiedDiff {
        return Err(evaluation_blocker(
            BlockerKind::WorkspacePreparation,
            "unsupported evaluator patch format",
        ));
    }
    let patch_path = workspace.join(EVALUATOR_PATCH_FILE);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&patch_path)
        .map_err(|error| {
            evaluation_blocker(
                BlockerKind::WorkspacePreparation,
                format!("failed to create evaluator patch: {error}"),
            )
        })?;
    file.write_all(patch.content().as_bytes())
        .map_err(|error| {
            evaluation_blocker(
                BlockerKind::WorkspacePreparation,
                format!("failed to write evaluator patch: {error}"),
            )
        })?;
    file.sync_all().map_err(|error| {
        evaluation_blocker(
            BlockerKind::WorkspacePreparation,
            format!("failed to sync evaluator patch: {error}"),
        )
    })?;
    drop(file);

    let operation = (|| {
        let result = run_raw_command(
            workspace,
            workspace,
            evaluator_apply_argv(&["--whitespace=nowarn", EVALUATOR_PATCH_FILE]),
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
            SandboxNetworkMode::Denied,
            sandbox_backend,
        );
        record_command_security(
            &result,
            &mut task_diagnostics.strict_sandbox_command_count,
            &mut task_diagnostics.local_process_fallback_count,
        );
        diagnostics.push(CommandDiagnostic::new("evaluator.apply_patch", &result));
        if !command_succeeded(&result) {
            return Err(command_blocker(
                &result,
                BlockerKind::WorkspacePreparation,
                "failed to apply evaluator patch",
            ));
        }
        Ok(())
    })();
    match operation {
        Ok(()) => Ok(patch_path),
        Err(primary) => match cleanup_evaluator_patch(&patch_path) {
            Ok(()) => Err(primary),
            Err(cleanup) => Err(evaluation_blocker(
                primary.kind,
                format!("{}; {}", primary.message, cleanup.message),
            )),
        },
    }
}

fn revert_evaluator_patch(
    workspace: &Path,
    patch_path: &Path,
    sandbox_backend: SharedSandboxBackend,
    diagnostics: &mut Vec<CommandDiagnostic>,
    task_diagnostics: &mut TaskDiagnostics,
) -> Result<(), EvaluationBlocker> {
    let operation = (|| {
        let reverse = run_raw_command(
            workspace,
            workspace,
            evaluator_apply_argv(&["--reverse", "--whitespace=nowarn", EVALUATOR_PATCH_FILE]),
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
            SandboxNetworkMode::Denied,
            sandbox_backend,
        );
        record_command_security(
            &reverse,
            &mut task_diagnostics.strict_sandbox_command_count,
            &mut task_diagnostics.local_process_fallback_count,
        );
        diagnostics.push(CommandDiagnostic::new("evaluator.revert_patch", &reverse));
        if !command_succeeded(&reverse) {
            return Err(command_blocker(
                &reverse,
                BlockerKind::WorkspacePreparation,
                "failed to revert evaluator patch",
            ));
        }
        Ok(())
    })();
    let cleanup = cleanup_evaluator_patch(patch_path);
    match (operation, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(blocker), Ok(())) => Err(blocker),
        (Ok(()), Err(blocker)) => Err(blocker),
        (Err(primary), Err(cleanup)) => Err(evaluation_blocker(
            primary.kind,
            format!("{}; {}", primary.message, cleanup.message),
        )),
    }
}

fn evaluator_apply_argv(arguments: &[&str]) -> Vec<String> {
    let mut argv = vec![
        "git".to_string(),
        if cfg!(windows) {
            "--git-dir=NUL".to_string()
        } else {
            "--git-dir=/dev/null".to_string()
        },
        "--work-tree=.".to_string(),
        "-c".to_string(),
        "core.autocrlf=false".to_string(),
        "apply".to_string(),
        "--no-index".to_string(),
    ];
    argv.extend(arguments.iter().map(|argument| (*argument).to_string()));
    argv
}

fn cleanup_evaluator_patch(patch_path: &Path) -> Result<(), EvaluationBlocker> {
    if let Err(error) = fs::remove_file(patch_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(evaluation_blocker(
            BlockerKind::WorkspacePreparation,
            format!("failed to remove evaluator patch file: {error}"),
        ));
    }
    Ok(())
}

/// Run a selected trial through an independent AppServer process pair.
///
/// The ordinary AgentLoop path is intentionally replaced for this trial. The child owns the
/// same trial workspace and its own file-backed Store; recovery evidence is read back from that
/// Store, while the existing evaluator still owns snapshots and verification stages.
#[allow(clippy::too_many_arguments)]
fn run_recovery_agent_stage(
    prepared: &PreparedTaskContext<'_, '_>,
    task_dir: &Path,
    agent_dir: &Path,
    plan: &AgentStagePlan,
    _trial: u32,
    _trace: &EvaluationTrialTrace<'_>,
    diagnostics: &mut TaskDiagnostics,
) -> AgentStageExecution {
    diagnostics.recovery_injected = true;
    let before_identity = match workspace_root_identity(agent_dir) {
        Ok(identity) => identity,
        Err(error) => {
            diagnostics.recovery_completed = None;
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                Vec::new(),
            );
        }
    };
    let before = match snapshot_workspace(agent_dir) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            diagnostics.recovery_completed = None;
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                Vec::new(),
            );
        }
    };
    let resolved_tools = match evaluation_registry() {
        Ok(tools) => tools,
        Err(error) => {
            diagnostics.recovery_completed = None;
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::AgentRuntime, error),
                Vec::new(),
            );
        }
    };
    let project_instructions = match load_project_instructions(agent_dir, agent_dir) {
        Ok(instructions) => instructions,
        Err(error) => {
            diagnostics.recovery_completed = None;
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error.to_string()),
                Vec::new(),
            );
        }
    };
    let prompt = agent_prompt(&plan.projection, &resolved_tools.names);
    let project_instructions_fingerprint = project_instructions
        .as_ref()
        .map(|instructions| instructions.aggregate_digest().to_string());
    let prompt_structure = EvaluationPromptStructure {
        contract: "evaluation.agent_prompt/v1".to_string(),
        model_message_roles: vec!["developer".to_string(), "user".to_string()],
        section_kinds: vec![
            "task_instructions".to_string(),
            "resolved_tools".to_string(),
            "completion_instruction".to_string(),
        ],
        resolved_tool_count: u32::try_from(resolved_tools.names.len()).unwrap_or(u32::MAX),
        project_instructions_fingerprint: project_instructions_fingerprint.clone(),
    };
    let tool_schema_fingerprint = resolved_tools.schema_fingerprint.clone();
    let prompt_fingerprint = canonical_json_digest(&json!({
        "prompt_structure": &prompt_structure,
        "user_prompt_fingerprint": content_digest(prompt.as_bytes()),
        "project_instructions_fingerprint": project_instructions_fingerprint,
        "tool_schema_fingerprint": &tool_schema_fingerprint,
    }))
    .expect("recovery prompt fingerprint inputs serialize canonically");

    let recovery_db = task_dir.join("recovery.sqlite3");
    let model_selector = prepared.provider_snapshot.resolved_default_selector();
    let child_provider = match prepared
        .provider_snapshot
        .provider_for_selector(model_selector.as_deref())
    {
        Ok(provider) => provider,
        Err(error) => {
            diagnostics.recovery_completed = None;
            return blocked_agent_stage(provider_blocker(&error), Vec::new());
        }
    };
    let started = Instant::now();
    let recovery = run_recovery_trial(
        agent_dir,
        &recovery_db,
        &prompt,
        model_selector.as_deref(),
        prepared.cancellation,
    );
    let agent_duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    diagnostics.agent_duration_ms = agent_duration_ms;
    diagnostics.recovery_completed = match recovery.attempt {
        RecoveryAttempt::Completed => Some(true),
        RecoveryAttempt::Failed => Some(false),
        RecoveryAttempt::NotObserved => None,
    };

    let trace_path = task_dir.join("recovery-trace.json");
    let _ = write_json_atomic(
        &trace_path,
        &json!({
            "schema": "evaluation.recovery-trace/v1",
            "thread_id": recovery.thread_id,
            "turn_id": recovery.turn_id,
            "attempt": format!("{:?}", recovery.attempt),
            "events": &recovery.trace,
        }),
    );

    let (command_diagnostics, strict_sandbox_count, unknown_sandbox_count) =
        recovery_command_diagnostics(&recovery.trace);
    diagnostics.strict_sandbox_command_count = diagnostics
        .strict_sandbox_command_count
        .saturating_add(strict_sandbox_count);
    diagnostics.local_process_fallback_unknown_count = diagnostics
        .local_process_fallback_unknown_count
        .saturating_add(unknown_sandbox_count);
    let (
        provider_attempts,
        model_usage,
        model_turns,
        tool_calls,
        verification_required,
        verification_satisfied,
    ) = recovery_trace_metrics(&recovery.trace);
    let verification_counts =
        recovery_verification_counts(verification_required, verification_satisfied);
    diagnostics.verification_observed = verification_counts.is_ok();
    let (verification_required_command_count, verification_satisfied_command_count) =
        match &verification_counts {
            Ok((required, satisfied)) => (*required, *satisfied),
            Err(_) => (0, 0),
        };
    diagnostics.verification_required_command_count = verification_required_command_count;
    diagnostics.verification_satisfied_command_count = verification_satisfied_command_count;
    let child_provider_binding_observed = model_selector.as_deref().is_some_and(|selector| {
        recovery_provider_binding_matches(
            &recovery.trace,
            &recovery.turn_id,
            selector,
            prepared
                .provider_snapshot
                .redacted_config()
                .provider_name
                .as_deref(),
            child_provider.selected_api_protocol(),
        )
    });
    if recovery.attempt == RecoveryAttempt::Completed && !child_provider_binding_observed {
        diagnostics.recovery_completed = None;
    }
    let provider_evidence = child_provider_binding_observed
        .then(|| recovery_provider_protocol(&recovery.trace, &recovery.turn_id))
        .flatten()
        .map(|protocol| {
            let metadata = ProviderCapabilityMetadata {
                api_protocol: protocol,
                profile: ProviderCapabilityProfile::Declared,
                cache_hit: false,
                profile_attempts: 0,
                fallback_count: 0,
                probe_usage: ModelUsage::default(),
                probe_attempt_metadata: ProviderAttemptMetadata::default(),
                cache_observations: Vec::new(),
            };
            let contract = child_provider.protocol_contract();
            provider_evidence(&child_provider, Some(&contract), Some(&metadata))
        });
    diagnostics.provider_usage_available = provider_attempts.attempt_count > 0;
    diagnostics.provider_attempt_count = provider_attempts.attempt_count;
    diagnostics.provider_retry_count = provider_attempts.retry_count;
    diagnostics.provider_latency_ms = provider_attempts.latency_ms;
    diagnostics.model_turns = model_turns;
    diagnostics.tool_calls = tool_calls;
    diagnostics.input_tokens = model_usage.input_tokens;
    diagnostics.output_tokens = model_usage.output_tokens;
    diagnostics.cached_input_tokens = model_usage.cached_input_tokens;
    diagnostics.reasoning_tokens = model_usage.reasoning_tokens;
    diagnostics.total_tokens = model_usage.total_tokens;
    diagnostics.prompt_structure = Some(prompt_structure.clone());
    diagnostics.prompt_fingerprint = Some(prompt_fingerprint.clone());
    diagnostics.tool_schema_fingerprint = Some(tool_schema_fingerprint.clone());
    diagnostics.provider_evidence = provider_evidence.clone();
    let integrity_paths = verification_integrity_paths(prepared.plan, &before);
    diagnostics.verification_bypass_count = (recovery.attempt == RecoveryAttempt::Completed)
        .then(|| {
            recovery_verification_bypass_count(
                &recovery_db,
                &recovery.trace,
                &recovery.thread_id,
                &recovery.turn_id,
                integrity_paths.as_ref(),
            )
        })
        .flatten();

    let after = match snapshot_workspace(agent_dir) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return AgentStageExecution {
                stage: StageExecution::blocked(
                    evaluation_blocker(BlockerKind::WorkspacePreparation, error.clone()),
                    command_diagnostics,
                ),
                workspace: Some(agent_dir.to_path_buf()),
                changed_files: Vec::new(),
                patch_evidence: Vec::new(),
                patch_digest: None,
                patch_evidence_path: None,
                model_turns,
                tool_calls,
                approval_count: 0,
                recovery_metrics: AgentRecoveryMetrics::default(),
                compaction_count: 0,
                verification_required_command_count,
                verification_satisfied_command_count,
                model_usage,
                provider_attempts,
                agent_duration_ms,
                local_process_fallback_unknown_count: unknown_sandbox_count,
                trace_path: Some(trace_path.to_string_lossy().into_owned()),
                error: Some(safe_text(error)),
                provider_diagnostic: None,
                prompt_structure: Some(prompt_structure),
                prompt_fingerprint: Some(prompt_fingerprint),
                tool_schema_fingerprint: Some(tool_schema_fingerprint),
                provider_evidence,
                verification_bypass_count: None,
            };
        }
    };
    let changed_files = evaluation_changed_paths(&before, &after, prepared.source_snapshot);
    let patch_evidence = workspace_change_evidence(&before, &after, prepared.source_snapshot);
    let patch_digest = patch_evidence_digest(&patch_evidence);
    let patch_evidence_path = task_dir.join(PATCH_EVIDENCE_FILE);
    let patch_evidence_path = write_json_atomic(&patch_evidence_path, &patch_evidence)
        .ok()
        .map(|_| patch_evidence_path.to_string_lossy().into_owned());
    if workspace_root_identity(agent_dir).ok() != Some(before_identity) {
        return AgentStageExecution {
            stage: StageExecution::blocked(
                evaluation_blocker(
                    BlockerKind::WorkspacePreparation,
                    "agent workspace root identity changed during recovery trial",
                ),
                command_diagnostics,
            ),
            workspace: Some(agent_dir.to_path_buf()),
            changed_files,
            patch_evidence,
            patch_digest,
            patch_evidence_path,
            model_turns,
            tool_calls,
            approval_count: 0,
            recovery_metrics: AgentRecoveryMetrics::default(),
            compaction_count: 0,
            verification_required_command_count,
            verification_satisfied_command_count,
            model_usage,
            provider_attempts,
            agent_duration_ms,
            local_process_fallback_unknown_count: unknown_sandbox_count,
            trace_path: Some(trace_path.to_string_lossy().into_owned()),
            error: Some("agent workspace root identity changed during recovery trial".to_string()),
            provider_diagnostic: None,
            prompt_structure: Some(prompt_structure),
            prompt_fingerprint: Some(prompt_fingerprint),
            tool_schema_fingerprint: Some(tool_schema_fingerprint),
            provider_evidence,
            verification_bypass_count: None,
        };
    }
    let stage = match recovery.attempt {
        RecoveryAttempt::Completed if !child_provider_binding_observed => {
            StageExecution::blocked(recovery_provider_binding_blocker(), command_diagnostics)
        }
        RecoveryAttempt::Completed if diagnostics.verification_observed => {
            StageExecution::passed(command_diagnostics)
        }
        RecoveryAttempt::Completed => {
            StageExecution::blocked(recovery_verification_blocker(), command_diagnostics)
        }
        RecoveryAttempt::Failed | RecoveryAttempt::NotObserved => StageExecution::failed(
            recovery
                .reason
                .clone()
                .unwrap_or_else(|| "recovery trial did not complete".to_string()),
            command_diagnostics,
        ),
    };
    AgentStageExecution {
        stage,
        workspace: Some(agent_dir.to_path_buf()),
        changed_files,
        patch_evidence,
        patch_digest,
        patch_evidence_path,
        model_turns,
        tool_calls,
        approval_count: 0,
        recovery_metrics: AgentRecoveryMetrics::default(),
        compaction_count: 0,
        verification_required_command_count,
        verification_satisfied_command_count,
        model_usage,
        provider_attempts,
        agent_duration_ms,
        local_process_fallback_unknown_count: unknown_sandbox_count,
        trace_path: Some(trace_path.to_string_lossy().into_owned()),
        error: (recovery.attempt != RecoveryAttempt::Completed).then(|| {
            recovery
                .reason
                .unwrap_or_else(|| "recovery trial did not complete".to_string())
        }),
        provider_diagnostic: None,
        prompt_structure: Some(prompt_structure),
        prompt_fingerprint: Some(prompt_fingerprint),
        tool_schema_fingerprint: Some(tool_schema_fingerprint),
        provider_evidence,
        verification_bypass_count: None,
    }
}

fn recovery_command_diagnostics(trace: &[TraceEvent]) -> (Vec<CommandDiagnostic>, usize, usize) {
    let mut commands = Vec::new();
    let mut strict = 0usize;
    let mut unknown = 0usize;
    for start in trace.iter().filter(|event| {
        event.span_kind == Some(TraceSpanKind::SandboxExecution)
            && event.span_phase == Some(TraceSpanPhase::Start)
    }) {
        let end = trace.iter().find(|event| {
            event.span_id == start.span_id
                && event.span_kind == Some(TraceSpanKind::SandboxExecution)
                && event.span_phase == Some(TraceSpanPhase::End)
        });
        commands.push(CommandDiagnostic {
            phase: "recovery.command".to_string(),
            exit_code: None,
            duration_ms: end.and_then(|event| event.duration_ms),
        });
        let sandbox = end
            .and_then(|event| event.span_projection.as_ref())
            .and_then(|projection| projection.sandbox.as_ref());
        if sandbox.is_some_and(|sandbox| {
            sandbox.enforcement == Some(singularity_protocol::TraceSandboxEnforcement::Strict)
                && sandbox.command_id_binding_valid == Some(true)
        }) {
            strict = strict.saturating_add(1);
        } else {
            unknown = unknown.saturating_add(1);
        }
    }
    (commands, strict, unknown)
}

fn recovery_trace_metrics(
    trace: &[TraceEvent],
) -> (
    ProviderAttemptMetadata,
    ModelUsage,
    u32,
    u32,
    Option<u32>,
    Option<u32>,
) {
    let attempts = trace
        .iter()
        .filter(|event| {
            event.span_kind == Some(TraceSpanKind::ProviderAttempt)
                && event.span_phase == Some(TraceSpanPhase::Start)
        })
        .count();
    let latency_ms = trace
        .iter()
        .filter(|event| {
            event.span_kind == Some(TraceSpanKind::ProviderAttempt)
                && event.span_phase == Some(TraceSpanPhase::End)
        })
        .filter_map(|event| event.duration_ms)
        .fold(0u64, u64::saturating_add);
    let mut usage = ModelUsage::default();
    let mut model_turns = 0u32;
    let mut verification_required = None;
    let mut verification_satisfied = None;
    for event in trace.iter().filter(|event| {
        event.span_kind == Some(TraceSpanKind::ProviderAttempt)
            && event.span_phase == Some(TraceSpanPhase::End)
    }) {
        if let Some(value) = event
            .span_projection
            .as_ref()
            .and_then(|projection| projection.usage.as_ref())
        {
            usage.input_tokens = usage.input_tokens.saturating_add(value.input_tokens);
            usage.output_tokens = usage.output_tokens.saturating_add(value.output_tokens);
            usage.total_tokens = usage.total_tokens.saturating_add(value.total_tokens);
            usage.cached_input_tokens = usage
                .cached_input_tokens
                .saturating_add(value.cached_input_tokens);
            usage.reasoning_tokens = usage
                .reasoning_tokens
                .saturating_add(value.reasoning_tokens);
        }
        if let Some(turn) = event
            .span_projection
            .as_ref()
            .and_then(|projection| projection.model_turn_ordinal)
        {
            model_turns =
                model_turns.max(u32::try_from(turn.saturating_add(1)).unwrap_or(u32::MAX));
        }
    }
    for event in trace.iter().filter(|event| {
        event.span_kind == Some(TraceSpanKind::Verification)
            && event.span_phase == Some(TraceSpanPhase::End)
    }) {
        if let Some(verification) = event
            .span_projection
            .as_ref()
            .and_then(|projection| projection.verification.as_ref())
        {
            if let Some(required) = verification.required_command_count {
                verification_required = Some(
                    verification_required
                        .unwrap_or(0)
                        .max(u32::try_from(required).unwrap_or(u32::MAX)),
                );
            }
            if let Some(satisfied) = verification.satisfied_command_count {
                verification_satisfied = Some(
                    verification_satisfied
                        .unwrap_or(0)
                        .max(u32::try_from(satisfied).unwrap_or(u32::MAX)),
                );
            }
        }
    }
    let tool_calls = u32::try_from(
        trace
            .iter()
            .filter(|event| {
                event.span_kind == Some(TraceSpanKind::ToolCall)
                    && event.span_phase == Some(TraceSpanPhase::Start)
            })
            .count(),
    )
    .unwrap_or(u32::MAX);
    (
        ProviderAttemptMetadata {
            attempt_count: u32::try_from(attempts).unwrap_or(u32::MAX),
            retry_count: u32::try_from(attempts.saturating_sub(1)).unwrap_or(u32::MAX),
            latency_ms,
            occurrences: Vec::new(),
        },
        usage,
        model_turns,
        tool_calls,
        verification_required,
        verification_satisfied,
    )
}

/// Read the recovery trial's canonical ToolResult occurrences from the private trace envelope.
///
/// Recovery uses a separate SQLite Store, so its public trace projection cannot be fed to the
/// normal reducer directly.  The Store verifies the row envelope before returning the private
/// payload; this function then checks the public binding and hands the recovered results to the
/// same bypass reducer used by the ordinary AgentLoop path.
fn recovery_verification_bypass_count(
    recovery_db: &Path,
    trace: &[TraceEvent],
    thread_id: &str,
    turn_id: &str,
    integrity_paths: Option<&BTreeSet<String>>,
) -> Option<u64> {
    let integrity_paths = integrity_paths?;
    let store = singularity_store::SessionStore::open(recovery_db).ok()?;
    let results = recovery_tool_results(&store, trace, thread_id, turn_id)?;
    verification_bypass_count_for_results(&results, integrity_paths)
}

fn recovery_tool_results(
    store: &singularity_store::SessionStore,
    trace: &[TraceEvent],
    thread_id: &str,
    turn_id: &str,
) -> Option<Vec<ToolResult>> {
    let tool_result_events = trace
        .iter()
        .filter(|event| {
            event.payload.get("observation").and_then(Value::as_str) == Some("tool_result")
        })
        .collect::<Vec<_>>();
    let has_tool_calls = trace.iter().any(|event| {
        event.session_id == turn_id
            && event.span_kind == Some(TraceSpanKind::ToolCall)
            && event.span_phase == Some(TraceSpanPhase::Start)
    });
    if tool_result_events.is_empty() {
        // A completed recovery with no tool calls has a valid zero sample.  A tool-call span with
        // no paired private result is incomplete and must remain unavailable.
        return (!has_tool_calls).then_some(Vec::new());
    }

    let mut event_ids = BTreeSet::new();
    let mut results = Vec::with_capacity(tool_result_events.len());
    for event in tool_result_events {
        if event.validate_turn_binding(thread_id, turn_id).is_err()
            || !event_ids.insert(event.event_id.clone())
        {
            return None;
        }
        let internal_payload = store
            .get_trace_internal_payload(&event.event_id)
            .ok()
            .flatten()?;
        let occurrence =
            serde_json::from_value::<ToolResultOccurrence>(internal_payload.clone()).ok()?;
        if !recovery_tool_result_binding(event, &internal_payload, &occurrence, trace, turn_id) {
            return None;
        }
        results.push(occurrence.result().clone());
    }
    Some(results)
}

fn recovery_tool_result_binding(
    event: &TraceEvent,
    internal_payload: &Value,
    occurrence: &ToolResultOccurrence,
    trace: &[TraceEvent],
    turn_id: &str,
) -> bool {
    let Some(public) = event
        .payload
        .as_object()
        .and_then(|payload| payload.get("tool_result"))
        .and_then(Value::as_object)
    else {
        return false;
    };
    let Some(tool_name) = public.get("tool_name").and_then(Value::as_str) else {
        return false;
    };
    let Some(tool_call_id_digest) = public.get("tool_call_id_digest").and_then(Value::as_str)
    else {
        return false;
    };
    let Some(tool_call_ordinal) = public.get("tool_call_ordinal").and_then(Value::as_u64) else {
        return false;
    };
    let Some(first_attempt) = public.get("first_attempt").and_then(Value::as_bool) else {
        return false;
    };
    let Ok(status) = serde_json::from_value::<TraceToolStatus>(
        public.get("status").cloned().unwrap_or(Value::Null),
    ) else {
        return false;
    };
    let Ok(visibility) = serde_json::from_value::<singularity_agent::ToolResultVisibility>(
        public.get("visibility").cloned().unwrap_or(Value::Null),
    ) else {
        return false;
    };
    let result = occurrence.result();
    if result.tool_name != tool_name
        || content_digest(result.tool_call_id.as_bytes()) != tool_call_id_digest
        || result.ok != (status == TraceToolStatus::Succeeded)
        || occurrence.visibility() != visibility
        || public.get("ok").and_then(Value::as_bool) != Some(result.ok)
        || public.get("error_code").and_then(Value::as_str)
            != result
                .error_code
                .as_deref()
                .and_then(bounded_stable_code)
                .as_deref()
        || public.get("result_id_digest").and_then(Value::as_str)
            != result
                .result_id
                .as_deref()
                .map(|result_id| content_digest(result_id.as_bytes()))
                .as_deref()
    {
        return false;
    }

    let matching_tool_calls = trace
        .iter()
        .filter(|candidate| {
            candidate.session_id == turn_id
                && candidate.span_kind == Some(TraceSpanKind::ToolCall)
                && candidate.span_phase == Some(TraceSpanPhase::End)
        })
        .filter_map(|candidate| {
            let projection = candidate.span_projection.as_ref()?.tool.as_ref()?;
            (projection.tool_name.as_deref() == Some(tool_name)
                && projection.tool_call_id_digest.as_deref() == Some(tool_call_id_digest)
                && projection.tool_call_ordinal == Some(tool_call_ordinal)
                && projection.first_attempt == Some(first_attempt)
                && projection.status == Some(status)
                && candidate.span_id.as_deref().is_some_and(|span_id| {
                    trace.iter().any(|start| {
                        start.session_id == turn_id
                            && start.span_id.as_deref() == Some(span_id)
                            && start.span_kind == Some(TraceSpanKind::ToolCall)
                            && start.span_phase == Some(TraceSpanPhase::Start)
                    })
                }))
            .then_some(candidate)
        })
        .count();
    if matching_tool_calls != 1 {
        return false;
    }

    // The private payload itself is envelope-authenticated by Store.  Keep this argument in the
    // binding helper so callers cannot accidentally validate a result against a different row.
    internal_payload.is_object()
}

fn recovery_verification_counts(
    required: Option<u32>,
    satisfied: Option<u32>,
) -> Result<(u32, u32), EvaluationBlocker> {
    match (required, satisfied) {
        (Some(required), Some(satisfied)) => Ok((required, satisfied)),
        _ => Err(recovery_verification_blocker()),
    }
}

fn recovery_verification_blocker() -> EvaluationBlocker {
    evaluation_blocker_with_code(
        BlockerKind::AgentRuntime,
        Some("recovery_verification_evidence_unobserved".to_string()),
        "recovery verification evidence was not observed in AppServer trace",
    )
}

fn recovery_provider_protocol(trace: &[TraceEvent], turn_id: &str) -> Option<ProviderApiProtocol> {
    trace
        .iter()
        .filter(|event| {
            event.session_id == turn_id
                && event.span_kind == Some(TraceSpanKind::ProviderAttempt)
                && event.span_phase == Some(TraceSpanPhase::End)
                && event
                    .span_projection
                    .as_ref()
                    .and_then(|projection| projection.operation_phase)
                    == Some(singularity_protocol::TraceProviderOperationPhase::Completion)
        })
        .find_map(|event| {
            event
                .span_projection
                .as_ref()
                .and_then(|projection| projection.protocol)
                .map(trace_provider_protocol)
        })
}

fn trace_provider_protocol(protocol: TraceProviderProtocol) -> ProviderApiProtocol {
    match protocol {
        TraceProviderProtocol::Declared => ProviderApiProtocol::Declared,
        TraceProviderProtocol::OpenAiResponses => ProviderApiProtocol::OpenAiResponses,
        TraceProviderProtocol::OpenAiChatCompletions => ProviderApiProtocol::OpenAiChatCompletions,
    }
}

fn recovery_provider_binding_matches(
    trace: &[TraceEvent],
    turn_id: &str,
    selector: &str,
    fallback_provider_name: Option<&str>,
    expected_protocol: Option<ProviderApiProtocol>,
) -> bool {
    let Some((expected_provider, expected_model)) =
        recovery_selector_parts(selector, fallback_provider_name)
    else {
        return false;
    };
    let mut observed_completion = false;
    let mut observed_protocol = None;
    for event in trace.iter().filter(|event| {
        event.session_id == turn_id
            && event.span_kind == Some(TraceSpanKind::ProviderAttempt)
            && event.span_phase == Some(TraceSpanPhase::End)
    }) {
        let Some(projection) = event.span_projection.as_ref() else {
            return false;
        };
        let Some(operation_phase) = projection.operation_phase else {
            return false;
        };
        // A capability probe is part of the same frozen provider selection, but its declared
        // protocol is not the completion wire protocol.  Bind the recovery result only to the
        // actual completion attempts.
        if operation_phase != singularity_protocol::TraceProviderOperationPhase::Completion {
            continue;
        }
        let Some(provider_name) = projection
            .provider_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
        else {
            return false;
        };
        let Some(model_name) = projection
            .model_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
        else {
            return false;
        };
        let Some(protocol) = projection.protocol.map(trace_provider_protocol) else {
            return false;
        };
        observed_completion = true;
        if provider_name != expected_provider
            || model_name != expected_model
            || expected_protocol.is_some_and(|expected| expected != protocol)
        {
            return false;
        }
        if observed_protocol.is_some_and(|observed| observed != protocol) {
            return false;
        }
        observed_protocol = Some(protocol);
    }
    observed_completion
}

fn recovery_selector_parts(
    selector: &str,
    fallback_provider_name: Option<&str>,
) -> Option<(String, String)> {
    let (provider_name, model_and_effort) = selector
        .split_once('/')
        .map_or((fallback_provider_name?, selector), |(provider, rest)| {
            (provider, rest)
        });
    let model_name = model_and_effort
        .split_once('#')
        .map_or(model_and_effort, |(model, _)| model);
    (!provider_name.is_empty() && !model_name.is_empty())
        .then(|| (provider_name.to_string(), model_name.to_string()))
}

fn recovery_provider_binding_blocker() -> EvaluationBlocker {
    evaluation_blocker_with_code(
        BlockerKind::AgentRuntime,
        Some("recovery_provider_binding_unobserved".to_string()),
        "recovery child provider/model/protocol did not match the frozen selector",
    )
}

#[allow(clippy::too_many_arguments)]
fn run_agent_stage(
    prepared: &PreparedTaskContext<'_, '_>,
    task_dir: &Path,
    agent_dir: &Path,
    plan: &AgentStagePlan,
    trial: u32,
    provider: OpenAiProvider,
    trace: &EvaluationTrialTrace<'_>,
    diagnostics: &mut TaskDiagnostics,
) -> AgentStageExecution {
    if prepared.cancellation.is_cancelled() {
        return blocked_agent_stage(
            evaluation_blocker(BlockerKind::AgentRuntime, "evaluation cancelled"),
            Vec::new(),
        );
    }
    let pristine_source = prepared.source_snapshot;
    let mut command_diagnostics = Vec::new();
    let projection = &plan.projection;
    let before_identity = match workspace_root_identity(agent_dir) {
        Ok(identity) => identity,
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                command_diagnostics,
            );
        }
    };
    let before = match snapshot_workspace(agent_dir) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                command_diagnostics,
            );
        }
    };
    // The integrity target set is control-plane data.  Keep it in memory only; hidden
    // evaluator paths never enter the prompt, trace, or published diagnostics.
    let integrity_paths = verification_integrity_paths(prepared.plan, &before);
    match workspace_root_identity(agent_dir) {
        Ok(identity) if identity == before_identity => {}
        Ok(_) => {
            return blocked_agent_stage(
                evaluation_blocker(
                    BlockerKind::WorkspacePreparation,
                    "agent workspace root identity changed while capturing baseline",
                ),
                command_diagnostics,
            );
        }
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                command_diagnostics,
            );
        }
    }
    let project_instructions = match load_project_instructions(agent_dir, agent_dir) {
        Ok(instructions) => instructions,
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error.to_string()),
                command_diagnostics,
            );
        }
    };

    let resolved_tools = match evaluation_registry() {
        Ok(resolved_tools) => resolved_tools,
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::AgentRuntime, error),
                command_diagnostics,
            );
        }
    };
    let policy = workspace_policy(PermissionProfileName::WorkspaceWrite, ApprovalPolicy::Never);
    let prompt = agent_prompt(projection, &resolved_tools.names);
    let project_instructions_fingerprint = project_instructions
        .as_ref()
        .map(|instructions| instructions.aggregate_digest().to_string());
    let prompt_structure = EvaluationPromptStructure {
        contract: "evaluation.agent_prompt/v1".to_string(),
        model_message_roles: vec!["developer".to_string(), "user".to_string()],
        section_kinds: vec![
            "task_instructions".to_string(),
            "resolved_tools".to_string(),
            "completion_instruction".to_string(),
        ],
        resolved_tool_count: u32::try_from(resolved_tools.names.len()).unwrap_or(u32::MAX),
        project_instructions_fingerprint: project_instructions_fingerprint.clone(),
    };
    let tool_schema_fingerprint = resolved_tools.schema_fingerprint.clone();
    let prompt_fingerprint = canonical_json_digest(&json!({
        "prompt_structure": &prompt_structure,
        "user_prompt_fingerprint": content_digest(prompt.as_bytes()),
        "project_instructions_fingerprint": project_instructions_fingerprint,
        "tool_schema_fingerprint": &tool_schema_fingerprint,
    }))
    .expect("evaluation prompt fingerprint inputs serialize canonically");
    let mut input = AgentLoopInput::new(
        projection.task_id.as_str(),
        format!(
            "eval_{}_{}_trial_{trial}",
            prepared.run_id.as_str(),
            projection.task_id.as_str()
        ),
        prompt,
    )
    .with_max_turns(DEFAULT_AGENT_MAX_TURNS);
    if let Some(instructions) = project_instructions {
        input = input.with_project_instructions(instructions);
    }
    let command_runtime_executables = Vec::new();
    let workspace_tools = match WorkspaceTools::new(agent_dir) {
        Ok(tools) => tools
            .with_shared_sandbox_backend(Arc::clone(prepared.sandbox_backend))
            .with_command_environment(CommandEnvironmentPolicy::Isolated)
            .with_command_runtime_executables(command_runtime_executables),
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error.to_string()),
                command_diagnostics,
            );
        }
    };
    let agent_started = Instant::now();
    let provider_identity = provider.clone();
    let trace_store = Arc::clone(&prepared.trace_store);
    let trace_run_id = prepared.run_id.as_str().to_string();
    let trace_session_id = trace.session_id.clone();
    let trace_turn_span_id = trace.turn_span_id.clone();
    let trace_failures = Arc::clone(prepared.trace_failures);
    let mut on_event = |event| {
        let store = match trace_store.lock() {
            Ok(store) => store,
            Err(_) => {
                record_trace_failure(
                    &trace_failures,
                    "agent event projection: evaluation trace store mutex poisoned",
                );
                return Err(AgentLoopEventSinkError);
            }
        };
        let mut projector = TraceProjector::new_external(
            &store,
            &trace_run_id,
            &trace_session_id,
            &trace_turn_span_id,
        );
        match projector.project_event(event) {
            Ok(()) => Ok(()),
            Err(error) => {
                record_trace_failure(&trace_failures, format!("agent event projection: {error}"));
                Err(AgentLoopEventSinkError)
            }
        }
    };
    let result = AgentLoop::new(provider, ToolBroker::new(resolved_tools.registry), policy)
        .with_workspace_tools(workspace_tools)
        .with_cancellation_token(prepared.cancellation.clone())
        .run_with_events(&input, &mut on_event);
    let verification_bypass_count = integrity_paths
        .as_ref()
        .and_then(|paths| verification_bypass_count_for_results(&result.tool_results, paths));
    let agent_duration_ms = u64::try_from(agent_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    diagnostics.agent_duration_ms = agent_duration_ms;
    let run_status = result.to_run_status();
    let agent_command_projection = agent_command_projection(&result);
    command_diagnostics = agent_command_projection.diagnostics.clone();
    diagnostics.strict_sandbox_command_count = diagnostics
        .strict_sandbox_command_count
        .saturating_add(agent_command_projection.strict_sandbox_command_count);
    diagnostics.local_process_fallback_count = diagnostics
        .local_process_fallback_count
        .saturating_add(agent_command_projection.local_process_fallback_count);
    match trace_store.lock() {
        Ok(store) => {
            let mut projector = TraceProjector::new_external(
                &store,
                &trace_run_id,
                &trace_session_id,
                &trace_turn_span_id,
            );
            if let Err(error) = projector.project_result(&run_status) {
                record_trace_failure(
                    prepared.trace_failures,
                    format!("agent result projection: {error}"),
                );
            }
        }
        Err(_) => record_trace_failure(
            prepared.trace_failures,
            "agent result projection: evaluation trace store mutex poisoned",
        ),
    }
    let provider_evidence = provider_evidence(
        &provider_identity,
        run_status.provider_protocol_contract.as_ref(),
        run_status.provider_capability_metadata.as_ref(),
    );
    diagnostics.provider_usage_available = result
        .provider_attempts
        .occurrences
        .iter()
        .any(|occurrence| occurrence.usage.is_some());
    if let Some(metadata) = run_status.provider_capability_metadata.as_ref() {
        diagnostics.probe_attempt_count = metadata.probe_attempt_metadata.attempt_count;
        diagnostics.probe_retry_count = metadata.probe_attempt_metadata.retry_count;
        diagnostics.probe_latency_ms = metadata.probe_attempt_metadata.latency_ms;
        diagnostics.capability_cache_observation_count =
            u32::try_from(metadata.cache_observations.len()).unwrap_or(u32::MAX);
        for observation in &metadata.cache_observations {
            match observation.outcome {
                ProviderCapabilityCacheLookupResult::Hit => {
                    diagnostics.capability_cache_hit_count =
                        diagnostics.capability_cache_hit_count.saturating_add(1);
                }
                ProviderCapabilityCacheLookupResult::Miss => {
                    diagnostics.capability_cache_miss_count =
                        diagnostics.capability_cache_miss_count.saturating_add(1);
                }
            }
        }
    }
    let local_process_fallback_unknown_count = agent_command_projection.unknown_count;
    let trace_path = task_dir.join(AGENT_TRACE_FILE);
    let trace = match evaluation_agent_trace_shared(
        &prepared.trace_store,
        prepared.run_id.as_str(),
        &trace.session_id,
        &trace.task_span_id,
    ) {
        Ok(trace) => trace,
        Err(error) => {
            record_trace_failure(prepared.trace_failures, error.clone());
            return AgentStageExecution {
                stage: StageExecution::blocked(
                    evaluation_blocker(BlockerKind::WorkspacePreparation, error.clone()),
                    command_diagnostics,
                ),
                workspace: None,
                changed_files: Vec::new(),
                patch_evidence: Vec::new(),
                patch_digest: None,
                patch_evidence_path: None,
                model_turns: result.model_turns,
                tool_calls: result.tool_calls,
                approval_count: result.approval_count,
                recovery_metrics: result.recovery_metrics.clone(),
                compaction_count: result
                    .context_trace
                    .as_ref()
                    .map_or(0, |trace| trace.compaction_count),
                verification_required_command_count: result.verification.required_command_count,
                verification_satisfied_command_count: result.verification.satisfied_command_count,
                model_usage: result.model_usage.clone(),
                provider_attempts: result.provider_attempts.clone(),
                agent_duration_ms,
                local_process_fallback_unknown_count,
                trace_path: None,
                error: Some(safe_text(error)),
                provider_diagnostic: run_status.provider_diagnostic,
                prompt_structure: Some(prompt_structure.clone()),
                prompt_fingerprint: Some(prompt_fingerprint.clone()),
                tool_schema_fingerprint: Some(tool_schema_fingerprint.clone()),
                provider_evidence: Some(provider_evidence.clone()),
                verification_bypass_count: None,
            };
        }
    };
    let trace_path_string = match write_json_atomic(&trace_path, &trace) {
        Ok(()) => Some(trace_path.to_string_lossy().into_owned()),
        Err(error) => {
            return AgentStageExecution {
                stage: StageExecution::blocked(
                    evaluation_blocker(BlockerKind::WorkspacePreparation, error.clone()),
                    command_diagnostics,
                ),
                workspace: None,
                changed_files: Vec::new(),
                patch_evidence: Vec::new(),
                patch_digest: None,
                patch_evidence_path: None,
                model_turns: result.model_turns,
                tool_calls: result.tool_calls,
                approval_count: result.approval_count,
                recovery_metrics: result.recovery_metrics.clone(),
                compaction_count: result
                    .context_trace
                    .as_ref()
                    .map_or(0, |trace| trace.compaction_count),
                verification_required_command_count: result.verification.required_command_count,
                verification_satisfied_command_count: result.verification.satisfied_command_count,
                model_usage: result.model_usage.clone(),
                provider_attempts: result.provider_attempts.clone(),
                agent_duration_ms,
                local_process_fallback_unknown_count,
                trace_path: None,
                error: Some(safe_text(error)),
                provider_diagnostic: run_status.provider_diagnostic,
                prompt_structure: Some(prompt_structure.clone()),
                prompt_fingerprint: Some(prompt_fingerprint.clone()),
                tool_schema_fingerprint: Some(tool_schema_fingerprint.clone()),
                provider_evidence: Some(provider_evidence.clone()),
                verification_bypass_count: None,
            };
        }
    };

    let after = match snapshot_workspace(agent_dir) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return AgentStageExecution {
                stage: StageExecution::blocked(
                    evaluation_blocker(BlockerKind::WorkspacePreparation, error.clone()),
                    command_diagnostics,
                ),
                workspace: Some(agent_dir.to_path_buf()),
                changed_files: Vec::new(),
                patch_evidence: Vec::new(),
                patch_digest: None,
                patch_evidence_path: None,
                model_turns: result.model_turns,
                tool_calls: result.tool_calls,
                approval_count: result.approval_count,
                recovery_metrics: result.recovery_metrics.clone(),
                compaction_count: result
                    .context_trace
                    .as_ref()
                    .map_or(0, |trace| trace.compaction_count),
                verification_required_command_count: result.verification.required_command_count,
                verification_satisfied_command_count: result.verification.satisfied_command_count,
                model_usage: result.model_usage.clone(),
                provider_attempts: result.provider_attempts.clone(),
                agent_duration_ms,
                local_process_fallback_unknown_count,
                trace_path: trace_path_string,
                error: Some(safe_text(error)),
                provider_diagnostic: run_status.provider_diagnostic,
                prompt_structure: Some(prompt_structure.clone()),
                prompt_fingerprint: Some(prompt_fingerprint.clone()),
                tool_schema_fingerprint: Some(tool_schema_fingerprint.clone()),
                provider_evidence: Some(provider_evidence.clone()),
                verification_bypass_count: None,
            };
        }
    };
    match workspace_root_identity(agent_dir) {
        Ok(identity) if identity == before_identity => {}
        Ok(_) => {
            return AgentStageExecution {
                stage: StageExecution::blocked(
                    evaluation_blocker(
                        BlockerKind::WorkspacePreparation,
                        "agent workspace root identity changed while capturing final snapshot",
                    ),
                    command_diagnostics,
                ),
                workspace: Some(agent_dir.to_path_buf()),
                changed_files: Vec::new(),
                patch_evidence: Vec::new(),
                patch_digest: None,
                patch_evidence_path: None,
                model_turns: result.model_turns,
                tool_calls: result.tool_calls,
                approval_count: result.approval_count,
                recovery_metrics: result.recovery_metrics.clone(),
                compaction_count: result
                    .context_trace
                    .as_ref()
                    .map_or(0, |trace| trace.compaction_count),
                verification_required_command_count: result.verification.required_command_count,
                verification_satisfied_command_count: result.verification.satisfied_command_count,
                model_usage: result.model_usage.clone(),
                provider_attempts: result.provider_attempts.clone(),
                agent_duration_ms,
                local_process_fallback_unknown_count,
                trace_path: trace_path_string,
                error: Some(
                    "agent workspace root identity changed while capturing final snapshot"
                        .to_string(),
                ),
                provider_diagnostic: run_status.provider_diagnostic,
                prompt_structure: Some(prompt_structure.clone()),
                prompt_fingerprint: Some(prompt_fingerprint.clone()),
                tool_schema_fingerprint: Some(tool_schema_fingerprint.clone()),
                provider_evidence: Some(provider_evidence.clone()),
                verification_bypass_count: None,
            };
        }
        Err(error) => {
            return AgentStageExecution {
                stage: StageExecution::blocked(
                    evaluation_blocker(BlockerKind::WorkspacePreparation, error.clone()),
                    command_diagnostics,
                ),
                workspace: Some(agent_dir.to_path_buf()),
                changed_files: Vec::new(),
                patch_evidence: Vec::new(),
                patch_digest: None,
                patch_evidence_path: None,
                model_turns: result.model_turns,
                tool_calls: result.tool_calls,
                approval_count: result.approval_count,
                recovery_metrics: result.recovery_metrics.clone(),
                compaction_count: result
                    .context_trace
                    .as_ref()
                    .map_or(0, |trace| trace.compaction_count),
                verification_required_command_count: result.verification.required_command_count,
                verification_satisfied_command_count: result.verification.satisfied_command_count,
                model_usage: result.model_usage.clone(),
                provider_attempts: result.provider_attempts.clone(),
                agent_duration_ms,
                local_process_fallback_unknown_count,
                trace_path: trace_path_string,
                error: Some(safe_text(error)),
                provider_diagnostic: run_status.provider_diagnostic,
                prompt_structure: Some(prompt_structure.clone()),
                prompt_fingerprint: Some(prompt_fingerprint.clone()),
                tool_schema_fingerprint: Some(tool_schema_fingerprint.clone()),
                provider_evidence: Some(provider_evidence.clone()),
                verification_bypass_count: None,
            };
        }
    }
    let changed_files = evaluation_changed_paths(&before, &after, pristine_source);
    let patch_evidence = workspace_change_evidence(&before, &after, pristine_source);
    let patch_digest = patch_evidence_digest(&patch_evidence);
    let patch_evidence_path = task_dir.join(PATCH_EVIDENCE_FILE);
    let patch_evidence_path = match write_json_atomic(&patch_evidence_path, &patch_evidence) {
        Ok(()) => Some(patch_evidence_path.to_string_lossy().into_owned()),
        Err(error) => {
            return AgentStageExecution {
                stage: StageExecution::blocked(
                    evaluation_blocker(BlockerKind::WorkspacePreparation, error.clone()),
                    command_diagnostics,
                ),
                workspace: Some(agent_dir.to_path_buf()),
                changed_files,
                patch_evidence,
                patch_digest,
                patch_evidence_path: None,
                model_turns: result.model_turns,
                tool_calls: result.tool_calls,
                approval_count: result.approval_count,
                recovery_metrics: result.recovery_metrics.clone(),
                compaction_count: result
                    .context_trace
                    .as_ref()
                    .map_or(0, |trace| trace.compaction_count),
                verification_required_command_count: result.verification.required_command_count,
                verification_satisfied_command_count: result.verification.satisfied_command_count,
                model_usage: result.model_usage.clone(),
                provider_attempts: result.provider_attempts.clone(),
                agent_duration_ms,
                local_process_fallback_unknown_count,
                trace_path: trace_path_string,
                error: Some(safe_text(error)),
                provider_diagnostic: run_status.provider_diagnostic,
                prompt_structure: Some(prompt_structure.clone()),
                prompt_fingerprint: Some(prompt_fingerprint.clone()),
                tool_schema_fingerprint: Some(tool_schema_fingerprint.clone()),
                provider_evidence: Some(provider_evidence.clone()),
                verification_bypass_count: None,
            };
        }
    };
    let loop_completed = result.completed && result.status == AgentStatus::Completed;
    let error = result.error.clone().map(safe_text);
    let sandbox_blocker = agent_sandbox_blocker(&agent_command_projection);
    let stage = if let Some(blocker) = sandbox_blocker {
        StageExecution::blocked(blocker, command_diagnostics)
    } else if let Some(kind) = agent_blocker_kind(
        result.error_category.as_ref(),
        result.provider_diagnostic.as_ref(),
    ) {
        StageExecution::blocked(
            evaluation_blocker_with_code(
                kind,
                result
                    .provider_diagnostic
                    .as_ref()
                    .and_then(provider_diagnostic_code),
                error
                    .clone()
                    .unwrap_or_else(|| "provider request failed".to_string()),
            ),
            command_diagnostics,
        )
    } else if result.status == AgentStatus::Blocked {
        StageExecution::blocked(
            evaluation_blocker(
                BlockerKind::AgentRuntime,
                error
                    .clone()
                    .unwrap_or_else(|| "agent loop blocked".to_string()),
            ),
            command_diagnostics,
        )
    } else if !loop_completed {
        StageExecution::failed(
            error
                .clone()
                .unwrap_or_else(|| format!("agent loop ended as {}", result.status.as_str())),
            command_diagnostics,
        )
    } else {
        StageExecution::passed(command_diagnostics)
    };

    AgentStageExecution {
        stage,
        workspace: Some(agent_dir.to_path_buf()),
        changed_files,
        patch_evidence,
        patch_digest,
        patch_evidence_path,
        model_turns: result.model_turns,
        tool_calls: result.tool_calls,
        approval_count: result.approval_count,
        recovery_metrics: result.recovery_metrics.clone(),
        compaction_count: result
            .context_trace
            .as_ref()
            .map_or(0, |trace| trace.compaction_count),
        verification_required_command_count: result.verification.required_command_count,
        verification_satisfied_command_count: result.verification.satisfied_command_count,
        model_usage: result.model_usage.clone(),
        provider_attempts: result.provider_attempts.clone(),
        agent_duration_ms,
        local_process_fallback_unknown_count,
        trace_path: trace_path_string,
        error,
        provider_diagnostic: run_status.provider_diagnostic,
        prompt_structure: Some(prompt_structure),
        prompt_fingerprint: Some(prompt_fingerprint),
        tool_schema_fingerprint: Some(tool_schema_fingerprint),
        provider_evidence: Some(provider_evidence),
        verification_bypass_count,
    }
}

/// Resolve the evaluator-owned integrity target set without exposing its paths to the model.
///
/// A missing or ambiguous patch/command path makes the producer unavailable.  We deliberately
/// keep this parser narrower than a shell parser: manifest commands are direct argv, and only
/// path arguments that can be bound to a controlled workspace entry are admitted.
fn verification_integrity_paths(
    plan: &WorkspacePlan,
    workspace: &WorkspaceSnapshot,
) -> Option<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    for patch in [
        plan.baseline.test_patch.as_ref(),
        plan.public.test_patch.as_ref(),
        plan.hidden.test_patch.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        paths.extend(parse_unified_diff_paths(patch.content())?);
    }

    for commands in [
        &plan.baseline.commands,
        &plan.public.commands,
        &plan.hidden.commands,
    ] {
        paths.extend(parse_verification_command_paths(
            commands, &paths, workspace,
        )?);
    }
    (!paths.is_empty()).then_some(paths)
}

/// Parse the two-line unified-diff file headers documented by Git's diff format.
fn parse_unified_diff_paths(content: &str) -> Option<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    let mut old_path = None;
    for line in content.lines().map(|line| line.trim_end_matches('\r')) {
        if let Some(value) = line.strip_prefix("--- ") {
            if old_path.is_some() {
                return None;
            }
            old_path = Some(parse_patch_header_path(value)?);
        } else if let Some(value) = line.strip_prefix("+++ ") {
            let old_path = old_path.take()?;
            let new_path = parse_patch_header_path(value)?;
            let path = old_path.or(new_path)?;
            paths.insert(path);
        }
    }
    old_path
        .is_none()
        .then_some(paths)
        .filter(|paths| !paths.is_empty())
}

/// Parse an unquoted or Git C-quoted unified-diff path, retaining only a workspace-relative path.
fn parse_patch_header_path(value: &str) -> Option<Option<String>> {
    let value = value.trim_end();
    let value = if value.starts_with('"') {
        decode_git_quoted_path(value)?
    } else {
        let value = value.split_once('\t').map_or(value, |(path, _)| path);
        if value.contains('"') || value.contains('\\') {
            return None;
        }
        value.to_string()
    };
    if value == "/dev/null" {
        return Some(None);
    }
    let value = value
        .strip_prefix("a/")
        .or_else(|| value.strip_prefix("b/"))
        .unwrap_or(&value);
    Some(Some(
        RelativePath::new(value.to_string())
            .ok()?
            .as_str()
            .to_string(),
    ))
}

/// Decode Git's quoted pathname form without accepting malformed or partially decoded bytes.
fn decode_git_quoted_path(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.first().copied() != Some(b'"') {
        return None;
    }
    let mut output = Vec::new();
    let mut index = 1;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                if !bytes[index + 1..].iter().all(u8::is_ascii_whitespace) {
                    return None;
                }
                return String::from_utf8(output).ok();
            }
            b'\\' => {
                index += 1;
                let byte = *bytes.get(index)?;
                let decoded = match byte {
                    b'"' | b'\\' => byte,
                    b'a' => 0x07,
                    b'b' => 0x08,
                    b't' => b'\t',
                    b'n' => b'\n',
                    b'v' => 0x0b,
                    b'f' => 0x0c,
                    b'r' => b'\r',
                    b'0'..=b'7' => {
                        let mut value = u32::from(byte - b'0');
                        for _ in 0..2 {
                            index += 1;
                            let digit = *bytes.get(index)?;
                            if !(b'0'..=b'7').contains(&digit) {
                                return None;
                            }
                            value = value * 8 + u32::from(digit - b'0');
                        }
                        u8::try_from(value).ok()?
                    }
                    _ => return None,
                };
                output.push(decoded);
            }
            byte => output.push(byte),
        }
        index += 1;
    }
    None
}

/// Select only direct argv path operands that can be proven to belong to the trial workspace.
fn parse_verification_command_paths(
    commands: &[CommandSpec],
    known_paths: &BTreeSet<String>,
    workspace: &WorkspaceSnapshot,
) -> Option<BTreeSet<String>> {
    const PATH_FLAGS: &[&str] = &["-s", "--start-directory", "--rootdir", "--file", "--path"];
    const NON_PATH_VALUE_FLAGS: &[&str] = &[
        "-p",
        "--pattern",
        "-k",
        "--keyword",
        "-m",
        "--module",
        "--test",
    ];
    let mut paths = BTreeSet::new();
    for command in commands {
        let cwd = command.cwd.as_ref().map_or(".", RelativePath::as_str);
        if cwd != "." && !workspace.contains_key(cwd) && !path_overlaps_any(cwd, known_paths) {
            return None;
        }
        let mut expected_path = false;
        let mut skip_value = false;
        for argument in command.argv.as_slice().iter().skip(1) {
            if skip_value {
                skip_value = false;
                continue;
            }
            if expected_path {
                expected_path = false;
                if argument.starts_with('-') {
                    return None;
                }
                let path = command_dependency_path(cwd, argument)?;
                if !path_is_controlled(&path, workspace, known_paths) {
                    return None;
                }
                paths.insert(path);
                continue;
            }
            if PATH_FLAGS.contains(&argument.as_str()) {
                expected_path = true;
                continue;
            }
            if NON_PATH_VALUE_FLAGS.contains(&argument.as_str()) {
                // The following value is a pattern, expression, module, or cargo target rather
                // than an unambiguous workspace path.
                skip_value = true;
                continue;
            }
            if argument.starts_with('-') {
                continue;
            }
            let Some(path_argument) = path_argument_without_selector(argument) else {
                continue;
            };
            if !looks_like_workspace_path(path_argument) {
                continue;
            }
            let path = command_dependency_path(cwd, path_argument)?;
            if !path_is_controlled(&path, workspace, known_paths) {
                return None;
            }
            paths.insert(path);
        }
        if expected_path || skip_value {
            return None;
        }
    }
    Some(paths)
}

fn path_argument_without_selector(argument: &str) -> Option<&str> {
    let path = argument.split_once("::").map_or(argument, |(path, _)| path);
    (!path.is_empty()).then_some(path)
}

fn looks_like_workspace_path(argument: &str) -> bool {
    argument == "."
        || argument.contains('/')
        || argument.contains('\\')
        || argument.starts_with('.')
        || Path::new(argument).extension().is_some_and(|extension| {
            matches!(
                extension.to_str(),
                Some(
                    "c" | "cc"
                        | "cpp"
                        | "go"
                        | "java"
                        | "js"
                        | "json"
                        | "mjs"
                        | "py"
                        | "rs"
                        | "sh"
                        | "toml"
                        | "ts"
                        | "tsx"
                        | "yaml"
                        | "yml"
                )
            )
        })
}

fn command_dependency_path(cwd: &str, argument: &str) -> Option<String> {
    if argument.contains('*') || argument.contains('?') || argument.contains('[') {
        return None;
    }
    let argument = argument.strip_prefix("./").unwrap_or(argument);
    if argument == "." {
        return Some(cwd.to_string());
    }
    let value = if cwd == "." {
        argument.to_string()
    } else {
        format!("{cwd}/{argument}")
    };
    Some(RelativePath::new(value).ok()?.as_str().to_string())
}

fn path_is_controlled(
    path: &str,
    workspace: &WorkspaceSnapshot,
    known_paths: &BTreeSet<String>,
) -> bool {
    workspace.contains_key(path) || path_overlaps_any(path, known_paths)
}

fn path_overlaps_any(path: &str, paths: &BTreeSet<String>) -> bool {
    paths
        .iter()
        .any(|candidate| workspace_paths_overlap(path, candidate))
}

fn workspace_paths_overlap(left: &str, right: &str) -> bool {
    left == "."
        || right == "."
        || left == right
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

/// Count terminal, executed mutations whose trusted summary intersects the opaque target set.
fn verification_bypass_count_for_results(
    results: &[ToolResult],
    integrity_paths: &BTreeSet<String>,
) -> Option<u64> {
    if integrity_paths.is_empty() {
        return None;
    }
    let mut count = 0u64;
    for result in results {
        if result.tool_name != TOOL_COMMAND && result.tool_name != PATCH_TOOL {
            continue;
        }
        let observation = result.workspace_observation();
        let summary = result.workspace_change_summary();
        let Some(observation) = observation else {
            if is_known_not_executed_tool_result(result) {
                continue;
            }
            return None;
        };
        match observation.mutation() {
            WorkspaceMutation::Unknown => return None,
            WorkspaceMutation::Unchanged => {
                if let Some(summary) = summary
                    && !trusted_summary_paths(summary)
                {
                    return None;
                }
            }
            WorkspaceMutation::Changed => {
                let summary = summary?;
                if !trusted_summary_paths(summary) {
                    return None;
                }
                if result.tool_name == TOOL_COMMAND && !result.ok {
                    continue;
                }
                if summary
                    .changed_files
                    .iter()
                    .any(|path| path_overlaps_any(path, integrity_paths))
                {
                    count = count.saturating_add(1);
                }
            }
        }
    }
    Some(count)
}

fn trusted_summary_paths(summary: &WorkspaceChangeSummary) -> bool {
    summary.validate().is_ok()
        && summary
            .changed_files
            .iter()
            .all(|path| path != "." && RelativePath::new(path.clone()).is_ok())
}

fn is_known_not_executed_tool_result(result: &ToolResult) -> bool {
    if matches!(
        result.failure_kind.as_ref(),
        Some(
            ToolFailureKind::Input
                | ToolFailureKind::Visibility
                | ToolFailureKind::Capability
                | ToolFailureKind::Policy
                | ToolFailureKind::PermissionProfile
                | ToolFailureKind::WorkspaceBoundary
                | ToolFailureKind::ProtectedPath
                | ToolFailureKind::Approval
                | ToolFailureKind::Cancelled
        )
    ) {
        return true;
    }
    result
        .audit_metadata()
        .and_then(Value::as_object)
        .is_some_and(|audit| {
            audit.get("executor_started").and_then(Value::as_bool) == Some(false)
                || audit.get("sandbox_backend").and_then(Value::as_str) == Some("not_executed")
        })
}

fn blocked_agent_stage(
    blocker: EvaluationBlocker,
    commands: Vec<CommandDiagnostic>,
) -> AgentStageExecution {
    AgentStageExecution {
        stage: StageExecution::blocked(blocker.clone(), commands),
        workspace: None,
        changed_files: Vec::new(),
        patch_evidence: Vec::new(),
        patch_digest: None,
        patch_evidence_path: None,
        model_turns: 0,
        tool_calls: 0,
        approval_count: 0,
        recovery_metrics: AgentRecoveryMetrics::default(),
        compaction_count: 0,
        verification_required_command_count: 0,
        verification_satisfied_command_count: 0,
        model_usage: ModelUsage::default(),
        provider_attempts: ProviderAttemptMetadata::default(),
        agent_duration_ms: 0,
        local_process_fallback_unknown_count: 0,
        trace_path: None,
        error: Some(blocker.message),
        provider_diagnostic: None,
        prompt_structure: None,
        prompt_fingerprint: None,
        tool_schema_fingerprint: None,
        provider_evidence: None,
        verification_bypass_count: None,
    }
}

fn provider_evidence(
    provider: &OpenAiProvider,
    contract: Option<&ProviderProtocolContract>,
    metadata: Option<&ProviderCapabilityMetadata>,
) -> EvaluationProviderEvidence {
    let base = provider.runtime_fingerprint(None);
    let Some((contract, metadata)) = contract.zip(metadata) else {
        return EvaluationProviderEvidence {
            provider_fingerprint: base.provider_fingerprint,
            model_fingerprint: base.model_fingerprint,
            negotiation_fingerprint: None,
            api_protocol: None,
            protocol_contract_fingerprint: None,
            capability_metadata_fingerprint: None,
        };
    };
    let negotiation = ProviderProtocolNegotiation {
        contract: contract.clone(),
        metadata: metadata.clone(),
    };
    let runtime = provider.runtime_fingerprint_for_negotiation(None, &negotiation);
    EvaluationProviderEvidence {
        provider_fingerprint: runtime.provider_fingerprint,
        model_fingerprint: runtime.model_fingerprint,
        negotiation_fingerprint: runtime.negotiation_fingerprint,
        api_protocol: Some(
            enum_string(metadata.api_protocol)
                .expect("provider API protocol serializes to a string"),
        ),
        protocol_contract_fingerprint: Some(
            canonical_json_digest(contract).expect("provider contract serializes canonically"),
        ),
        capability_metadata_fingerprint: Some(
            canonical_json_digest(metadata)
                .expect("provider capability metadata serializes canonically"),
        ),
    }
}

fn evaluation_registry() -> Result<ResolvedEvaluationTools, String> {
    let mut registry = ToolRegistry::default();
    for entry in workspace_tool_entries() {
        registry.register(entry)?;
    }
    let names = registry
        .schema_payloads()
        .iter()
        .filter_map(|schema| schema.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<Vec<_>>();
    let schema_fingerprint = canonical_json_digest(&registry.schema_payloads())?;
    Ok(ResolvedEvaluationTools {
        registry,
        names,
        schema_fingerprint,
    })
}

fn agent_prompt(projection: &AgentTaskProjection, resolved_tools: &[String]) -> String {
    let allowed_tools = resolved_tools.join(", ");
    let mut sections = vec![
        projection.instructions.clone(),
        format!("Only these tools are available: {allowed_tools}."),
    ];
    sections.push(
        "Finish with a concise answer describing the change and the verification actually run."
            .to_string(),
    );
    sections.join("\n\n")
}

fn evaluation_agent_trace_events(
    store: &singularity_store::SessionStore,
    run_id: &str,
    session_id: &str,
    task_span_id: &str,
) -> Result<Vec<TraceEvent>, String> {
    let events = store
        .list_trace(run_id)
        .map_err(|error| format!("failed to query evaluation SQLite trace: {error}"))?
        .into_iter()
        .filter(|event| {
            event.session_id == session_id || event.span_id.as_deref() == Some(task_span_id)
        })
        .collect::<Vec<TraceEvent>>();
    if events.is_empty() {
        return Err("evaluation SQLite trace contains no events for the trial".to_string());
    }
    Ok(events)
}

fn evaluation_agent_trace_value(
    run_id: &str,
    session_id: &str,
    events: Vec<TraceEvent>,
) -> Result<Value, String> {
    serde_json::to_value(json!({
        "schema": "evaluation.agent-trace/v2",
        "run_id": run_id,
        "session_id": session_id,
        "events": events,
    }))
    .map_err(|error| format!("failed to serialize evaluation SQLite trace: {error}"))
}

fn evaluation_agent_trace_shared(
    store: &SharedEvaluationTraceStore<'_>,
    run_id: &str,
    session_id: &str,
    task_span_id: &str,
) -> Result<Value, String> {
    let store = store
        .lock()
        .map_err(|_| "evaluation trace store mutex poisoned while querying trace".to_string())?;
    let events = evaluation_agent_trace_events(&store, run_id, session_id, task_span_id)?;
    drop(store);
    evaluation_agent_trace_value(run_id, session_id, events)
}

fn agent_sandbox_blocker(
    projection: &evidence::AgentCommandProjection,
) -> Option<EvaluationBlocker> {
    if projection.unknown_count > 0 {
        return Some(evaluation_blocker(
            BlockerKind::Sandbox,
            "agent command sandbox evidence was incomplete or unbound",
        ));
    }
    if projection.strict_sandbox_command_count != projection.diagnostics.len()
        || projection.local_process_fallback_count > 0
    {
        return Some(evaluation_blocker(
            BlockerKind::Sandbox,
            "agent command sandbox enforcement was not strict",
        ));
    }
    None
}

fn provider_blocker(error: &ProviderError) -> EvaluationBlocker {
    let category = error.error.category();
    let diagnostic = error.error.provider_diagnostic();
    let kind = match category {
        ModelErrorCategory::Authentication => BlockerKind::ProviderAuthentication,
        ModelErrorCategory::Network | ModelErrorCategory::ProviderUnavailable => {
            BlockerKind::Network
        }
        ModelErrorCategory::ModelConfiguration => BlockerKind::ProviderConfiguration,
        _ if provider_response_stage(&diagnostic) => BlockerKind::ProviderResponse,
        _ => BlockerKind::AgentRuntime,
    };
    evaluation_blocker_with_code(
        kind,
        provider_diagnostic_code(&diagnostic),
        error.message.clone(),
    )
}

fn provider_configuration_blocker(error: &ProviderError) -> EvaluationBlocker {
    let diagnostic = error.error.provider_diagnostic();
    EvaluationBlocker {
        code: provider_diagnostic_code(&diagnostic)
            .or_else(|| Some("provider_configuration_invalid".to_string())),
        kind: BlockerKind::ProviderConfiguration,
        message: safe_text(&error.message),
        task_id: None,
    }
}

fn run_level_blocker(mut blocker: EvaluationBlocker) -> EvaluationBlocker {
    if blocker
        .code
        .as_deref()
        .is_none_or(|code| code.trim().is_empty())
    {
        blocker.code = Some(
            match blocker.kind {
                BlockerKind::Environment => "environment_preparation_failed",
                BlockerKind::WorkspacePreparation => "workspace_preparation_failed",
                BlockerKind::ProviderConfiguration => "provider_configuration_invalid",
                BlockerKind::Network => "network_unavailable",
                BlockerKind::Sandbox => "sandbox_unavailable",
                BlockerKind::ProviderResponse
                | BlockerKind::ProviderAuthentication
                | BlockerKind::AgentRuntime => "evaluation_blocked_before_sampling",
            }
            .to_string(),
        );
    }
    blocker
}

fn agent_blocker_kind(
    category: Option<&ModelErrorCategory>,
    diagnostic: Option<&ProviderDiagnostic>,
) -> Option<BlockerKind> {
    match category {
        Some(ModelErrorCategory::Authentication) => Some(BlockerKind::ProviderAuthentication),
        Some(ModelErrorCategory::Network | ModelErrorCategory::ProviderUnavailable) => {
            Some(BlockerKind::Network)
        }
        Some(ModelErrorCategory::ModelConfiguration) => Some(BlockerKind::ProviderConfiguration),
        Some(_) if diagnostic.is_some_and(provider_response_stage) => {
            Some(BlockerKind::ProviderResponse)
        }
        Some(
            ModelErrorCategory::Cancelled
            | ModelErrorCategory::InvalidRequest
            | ModelErrorCategory::ContextLengthExceeded
            | ModelErrorCategory::BudgetExceeded,
        )
        | None => None,
        Some(
            ModelErrorCategory::UnsupportedCapability
            | ModelErrorCategory::ToolCallParse
            | ModelErrorCategory::JsonSchema
            | ModelErrorCategory::ContentFilter
            | ModelErrorCategory::UnknownProviderError,
        ) => None,
    }
}

fn provider_response_stage(diagnostic: &ProviderDiagnostic) -> bool {
    matches!(
        diagnostic.stage,
        Some(
            ProviderErrorStage::ResponseStatus
                | ProviderErrorStage::ResponseBodyRead
                | ProviderErrorStage::ResponseJsonDecode
                | ProviderErrorStage::ResponseValidation
        )
    )
}

fn evaluation_blocker(kind: BlockerKind, message: impl Into<String>) -> EvaluationBlocker {
    evaluation_blocker_with_code(kind, None, message)
}

fn evaluation_blocker_with_code(
    kind: BlockerKind,
    code: Option<String>,
    message: impl Into<String>,
) -> EvaluationBlocker {
    EvaluationBlocker {
        code,
        kind,
        message: safe_text(message.into()),
        task_id: None,
    }
}

fn provider_diagnostic_code(diagnostic: &ProviderDiagnostic) -> Option<String> {
    diagnostic.code.as_deref().and_then(bounded_stable_code)
}

fn sandbox_preflight_blocker(
    code: impl Into<String>,
    message: impl Into<String>,
) -> EvaluationBlocker {
    EvaluationBlocker {
        code: Some(code.into()),
        kind: BlockerKind::Environment,
        message: safe_text(message.into()),
        task_id: None,
    }
}

fn sandbox_preflight_evidence(report: &SandboxPreflightReport) -> EvaluationSandboxPreflight {
    EvaluationSandboxPreflight {
        outcome: match report.outcome {
            SandboxPreflightOutcome::Supported => EvaluationSandboxPreflightOutcome::Supported,
            SandboxPreflightOutcome::Unsupported => EvaluationSandboxPreflightOutcome::Unsupported,
        },
        error_code: report.error_code.clone(),
        profile: report.profile.clone(),
        backend: report.backend.clone(),
        missing_capabilities: report.missing_capabilities.clone(),
        os: report.os.clone(),
        arch: report.arch.clone(),
        kernel: report.kernel.clone(),
        filesystem: report.filesystem.clone(),
        overlayfs: sandbox_preflight_fact(report.overlayfs),
        user_namespace: sandbox_preflight_fact(report.user_namespace),
        mount_namespace: sandbox_preflight_fact(report.mount_namespace),
        pid_namespace: sandbox_preflight_fact(report.pid_namespace),
        network_namespace: sandbox_preflight_fact(report.network_namespace),
        no_new_privs: sandbox_preflight_fact(report.no_new_privs),
        seccomp: sandbox_preflight_fact(report.seccomp),
        landlock: sandbox_preflight_fact(report.landlock),
        transactional_workspace: sandbox_preflight_fact(report.transactional_workspace),
        network_denied: sandbox_preflight_fact(report.network_denied),
        protected_paths: sandbox_preflight_fact(report.protected_paths),
    }
}

fn sandbox_preflight_fact(fact: SandboxPreflightFact) -> EvaluationSandboxPreflightFact {
    match fact {
        SandboxPreflightFact::Passed => EvaluationSandboxPreflightFact::Passed,
        SandboxPreflightFact::Failed => EvaluationSandboxPreflightFact::Failed,
        SandboxPreflightFact::NotApplicable => EvaluationSandboxPreflightFact::NotApplicable,
        SandboxPreflightFact::Unknown => EvaluationSandboxPreflightFact::Unknown,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteSourcePreflightFailure {
    Cancelled,
    Probe,
}

fn remote_git_repositories(plans: &[WorkspacePlan]) -> Vec<String> {
    let mut repositories = Vec::new();
    for plan in plans {
        let PlannedWorkspaceSource::RemoteGit { repository, .. } = &plan.source else {
            continue;
        };
        let repository = repository.as_str().to_string();
        if !repositories.contains(&repository) {
            repositories.push(repository);
        }
    }
    repositories
}

/// Verify each remote repository transport before sampling; `ls-remote` has no workspace side effect.
fn preflight_remote_sources(
    scratch: &Path,
    plans: &[WorkspacePlan],
    sandbox_backend: &SharedSandboxBackend,
    cancellation: &CancellationToken,
    cached_remote_repositories: &BTreeSet<String>,
) -> Result<(), RemoteSourcePreflightFailure> {
    for repository in remote_git_repositories(plans) {
        if cached_remote_repositories.contains(&repository) {
            continue;
        }
        if cancellation.is_cancelled() {
            return Err(RemoteSourcePreflightFailure::Cancelled);
        }
        let result = run_workspace_preparation_read_only_command(
            scratch,
            scratch,
            vec![
                "git".to_string(),
                "ls-remote".to_string(),
                "--exit-code".to_string(),
                "--no-tags".to_string(),
                repository,
            ],
            GIT_TIMEOUT_SECONDS,
            SandboxNetworkMode::Allowed,
            Arc::clone(sandbox_backend),
        );
        if !remote_source_probe_succeeded(&result) {
            return Err(RemoteSourcePreflightFailure::Probe);
        }
    }
    Ok(())
}

fn required_host_executables(plans: &[WorkspacePlan]) -> std::collections::BTreeSet<String> {
    let mut executables = std::collections::BTreeSet::new();
    for plan in plans {
        let commands = plan
            .setup_commands
            .iter()
            .chain(&plan.baseline.commands)
            .chain(&plan.public.commands)
            .chain(&plan.hidden.commands);
        for command in commands {
            let Some(executable) = command.argv.as_slice().first() else {
                continue;
            };
            let path = Path::new(executable);
            if path.is_absolute() || (!executable.contains('/') && !executable.contains('\\')) {
                executables.insert(executable.clone());
            }
        }
    }
    executables
}

fn preflight_task_executables(
    scratch: &Path,
    plans: &[WorkspacePlan],
    sandbox_backend: &SharedSandboxBackend,
    cancellation: &CancellationToken,
) -> Result<(), (&'static str, Vec<String>)> {
    let mut unavailable = Vec::new();
    let mut unknown = false;
    for executable in required_host_executables(plans) {
        if cancellation.is_cancelled() {
            return Err((
                "sandbox_preflight_cancelled",
                vec!["cancellation".to_string()],
            ));
        }
        match sandbox_backend.probe_executable(
            scratch,
            &executable,
            &CommandEnvironmentPolicy::Isolated,
        ) {
            ExecutableAvailability::Available => {}
            ExecutableAvailability::Unavailable => {
                unavailable.push(format!("task_executable:{executable}"));
            }
            ExecutableAvailability::Unknown => unknown = true,
        }
    }
    if !unavailable.is_empty() {
        return Err(("sandbox_preflight_task_executable_unavailable", unavailable));
    }
    if unknown {
        return Err((
            "sandbox_preflight_task_executable_unverified",
            vec!["task_executable_probe".to_string()],
        ));
    }
    Ok(())
}

/// A read-only remote source probe has no workspace mutation to observe.
///
/// Mutation-capable commands keep using `command_succeeded`; this narrower predicate only accepts
/// the fixed `git ls-remote` contract when completion and strict sandbox enforcement are proven.
fn remote_source_probe_succeeded(result: &CommandResult) -> bool {
    result.execution_status == CommandExecutionStatus::Completed
        && result.semantic_status == CommandSemanticStatus::Succeeded
        && result.exit_code == Some(0)
        && matches!(
            result.workspace_mutation,
            WorkspaceMutation::Unknown | WorkspaceMutation::Unchanged
        )
        && !result.sandbox.local_process_fallback
        && result.sandbox.enforcement == singularity_tools::SandboxBackendEnforcement::Strict
}

/// Create the sibling capability and task-like workspaces used by the runner preflight.
///
/// The capability workspace is intentionally separate because platform adapters may create
/// protected metadata there while probing. The ordinary command must instead run from the same
/// `task/trial-0001/agent` shape that a materialized Evaluation trial receives.
fn create_preflight_task_layout(scratch: &Path) -> Result<(PathBuf, PathBuf), String> {
    let capability_workspace = scratch.join("capability");
    let task_root = scratch.join("task");
    let task_workspace = task_root.join("trial-0001").join(AGENT_DIR);

    fs::create_dir(&capability_workspace).map_err(|error| error.to_string())?;
    fs::create_dir_all(task_root.join(SOURCE_DIR)).map_err(|error| error.to_string())?;
    fs::create_dir_all(&task_workspace).map_err(|error| error.to_string())?;

    let capability_workspace =
        fs::canonicalize(&capability_workspace).map_err(|error| error.to_string())?;
    let task_workspace = fs::canonicalize(&task_workspace).map_err(|error| error.to_string())?;
    Ok((capability_workspace, task_workspace))
}

fn run_sandbox_preflight(
    run_dir: &Path,
    plans: &[WorkspacePlan],
    sandbox_backend: &SharedSandboxBackend,
    cancellation: &CancellationToken,
    cached_remote_repositories: &BTreeSet<String>,
) -> Result<SandboxPreflightReport, Box<SandboxPreflightFailure>> {
    if plans.is_empty() {
        let mut report = SandboxPreflightReport::unverified_for_backend(sandbox_backend.as_ref());
        report.outcome = SandboxPreflightOutcome::Unsupported;
        report.error_code = Some("sandbox_preflight_empty_task_set".to_string());
        report.missing_capabilities.push("task_set".to_string());
        return Err(Box::new(SandboxPreflightFailure {
            report,
            blocker: sandbox_preflight_blocker(
                "sandbox_preflight_empty_task_set",
                "sandbox preflight requires at least one task",
            ),
        }));
    }
    if cancellation.is_cancelled() {
        let mut report = SandboxPreflightReport::unverified_for_backend(sandbox_backend.as_ref());
        report.outcome = SandboxPreflightOutcome::Unsupported;
        report.error_code = Some("sandbox_preflight_cancelled".to_string());
        report.missing_capabilities.push("cancellation".to_string());
        return Err(Box::new(SandboxPreflightFailure {
            report,
            blocker: sandbox_preflight_blocker(
                "sandbox_preflight_cancelled",
                "evaluation sandbox preflight was cancelled",
            ),
        }));
    }
    // The run-owned scratch directory is on the same filesystem as task roots. The capability
    // probe plus the ordinary task-layout no-op establish the backend/profile contract for the
    // entire task set without touching any task source or starting a provider trial.
    let scratch_path = run_dir.join(".sandbox-preflight");
    #[cfg(windows)]
    let (scratch, scratch_identity, capability_workspace, task_workspace) = {
        let mut lease = match TrustedWorkspaceLease::create(&scratch_path) {
            Ok(lease) => lease,
            Err(error) => {
                let mut report =
                    SandboxPreflightReport::unverified_for_backend(sandbox_backend.as_ref());
                report.outcome = SandboxPreflightOutcome::Unsupported;
                report.error_code = Some("sandbox_preflight_scratch_unavailable".to_string());
                report
                    .missing_capabilities
                    .push("scratch_workspace".to_string());
                return Err(Box::new(SandboxPreflightFailure {
                    report,
                    blocker: sandbox_preflight_blocker(
                        "sandbox_preflight_scratch_unavailable",
                        format!(
                            "sandbox preflight scratch workspace unavailable: {}",
                            error.code()
                        ),
                    ),
                }));
            }
        };
        let scratch_identity = lease.identity_fingerprint();
        let scratch = match fs::canonicalize(&scratch_path) {
            Ok(scratch) => scratch,
            Err(error) => {
                let cleanup_error = lease.rollback().err();
                let mut report =
                    SandboxPreflightReport::unverified_for_backend(sandbox_backend.as_ref());
                report.outcome = SandboxPreflightOutcome::Unsupported;
                let error_code = cleanup_error.as_ref().map_or(
                    "sandbox_preflight_scratch_unavailable",
                    |_| "sandbox_preflight_scratch_cleanup",
                );
                report.error_code = Some(error_code.to_string());
                report.missing_capabilities.push(
                    if cleanup_error.is_some() {
                        "scratch_cleanup"
                    } else {
                        "scratch_workspace"
                    }
                    .to_string(),
                );
                return Err(Box::new(SandboxPreflightFailure {
                    report,
                    blocker: sandbox_preflight_blocker(
                        error_code,
                        match cleanup_error {
                            Some(cleanup_error) => format!(
                                "sandbox preflight scratch canonicalization failed: {error}; cleanup failed: {}",
                                cleanup_error.code()
                            ),
                            None => {
                                format!("sandbox preflight scratch workspace unavailable: {error}")
                            }
                        },
                    ),
                }));
            }
        };
        let (capability_workspace, task_workspace) = match create_preflight_task_layout(&scratch) {
            Ok(layout) => layout,
            Err(_) => {
                let cleanup_error = lease.rollback().err();
                let mut report =
                    SandboxPreflightReport::unverified_for_backend(sandbox_backend.as_ref());
                report.outcome = SandboxPreflightOutcome::Unsupported;
                let error_code = cleanup_error.as_ref().map_or(
                    "sandbox_preflight_scratch_unavailable",
                    |_| "sandbox_preflight_scratch_cleanup",
                );
                report.error_code = Some(error_code.to_string());
                report.missing_capabilities.push(
                    if cleanup_error.is_some() {
                        "scratch_cleanup"
                    } else {
                        "scratch_workspace"
                    }
                    .to_string(),
                );
                return Err(Box::new(SandboxPreflightFailure {
                    report,
                    blocker: sandbox_preflight_blocker(
                        error_code,
                        if cleanup_error.is_some() {
                            "sandbox preflight scratch cleanup failed"
                        } else {
                            "sandbox preflight scratch workspace unavailable"
                        },
                    ),
                }));
            }
        };
        if let Err(error) = lease.commit() {
            let cleanup_error = lease.rollback().err();
            let mut report =
                SandboxPreflightReport::unverified_for_backend(sandbox_backend.as_ref());
            report.outcome = SandboxPreflightOutcome::Unsupported;
            let error_code = cleanup_error.as_ref().map_or(
                "sandbox_preflight_scratch_unavailable",
                |_| "sandbox_preflight_scratch_cleanup",
            );
            report.error_code = Some(error_code.to_string());
            report.missing_capabilities.push(
                if cleanup_error.is_some() {
                    "scratch_cleanup"
                } else {
                    "scratch_workspace"
                }
                .to_string(),
            );
            return Err(Box::new(SandboxPreflightFailure {
                report,
                blocker: sandbox_preflight_blocker(
                    error_code,
                    match cleanup_error {
                        Some(cleanup_error) => format!(
                            "sandbox preflight scratch identity changed: {}; cleanup failed: {}",
                            error.code(),
                            cleanup_error.code()
                        ),
                        None => format!(
                            "sandbox preflight scratch workspace identity changed: {}",
                            error.code()
                        ),
                    },
                ),
            }));
        }
        (
            scratch,
            scratch_identity,
            capability_workspace,
            task_workspace,
        )
    };
    #[cfg(not(windows))]
    let (scratch, capability_workspace, task_workspace) = {
        if let Err(error) = fs::create_dir_all(&scratch_path) {
            let mut report =
                SandboxPreflightReport::unverified_for_backend(sandbox_backend.as_ref());
            report.outcome = SandboxPreflightOutcome::Unsupported;
            report.error_code = Some("sandbox_preflight_scratch_unavailable".to_string());
            report
                .missing_capabilities
                .push("scratch_workspace".to_string());
            return Err(Box::new(SandboxPreflightFailure {
                report,
                blocker: sandbox_preflight_blocker(
                    "sandbox_preflight_scratch_unavailable",
                    format!("sandbox preflight scratch workspace unavailable: {error}"),
                ),
            }));
        }
        let scratch = match fs::canonicalize(&scratch_path) {
            Ok(scratch) => scratch,
            Err(_error) => {
                let cleanup_error = fs::remove_dir_all(&scratch_path).err();
                let mut report =
                    SandboxPreflightReport::unverified_for_backend(sandbox_backend.as_ref());
                report.outcome = SandboxPreflightOutcome::Unsupported;
                let error_code = cleanup_error.as_ref().map_or(
                    "sandbox_preflight_scratch_unavailable",
                    |_| "sandbox_preflight_scratch_cleanup",
                );
                report.error_code = Some(error_code.to_string());
                report.missing_capabilities.push(
                    if cleanup_error.is_some() {
                        "scratch_cleanup"
                    } else {
                        "scratch_workspace"
                    }
                    .to_string(),
                );
                return Err(Box::new(SandboxPreflightFailure {
                    report,
                    blocker: sandbox_preflight_blocker(
                        error_code,
                        if cleanup_error.is_some() {
                            "sandbox preflight scratch cleanup failed"
                        } else {
                            "sandbox preflight scratch workspace unavailable"
                        },
                    ),
                }));
            }
        };
        let (capability_workspace, task_workspace) = match create_preflight_task_layout(&scratch) {
            Ok(layout) => layout,
            Err(_) => {
                let cleanup_error = fs::remove_dir_all(&scratch).err();
                let mut report =
                    SandboxPreflightReport::unverified_for_backend(sandbox_backend.as_ref());
                report.outcome = SandboxPreflightOutcome::Unsupported;
                let error_code = cleanup_error.as_ref().map_or(
                    "sandbox_preflight_scratch_unavailable",
                    |_| "sandbox_preflight_scratch_cleanup",
                );
                report.error_code = Some(error_code.to_string());
                report.missing_capabilities.push(
                    if cleanup_error.is_some() {
                        "scratch_cleanup"
                    } else {
                        "scratch_workspace"
                    }
                    .to_string(),
                );
                return Err(Box::new(SandboxPreflightFailure {
                    report,
                    blocker: sandbox_preflight_blocker(
                        error_code,
                        if cleanup_error.is_some() {
                            "sandbox preflight scratch cleanup failed"
                        } else {
                            "sandbox preflight scratch workspace unavailable"
                        },
                    ),
                }));
            }
        };
        (scratch, capability_workspace, task_workspace)
    };
    let mut report = sandbox_backend.preflight(&capability_workspace, cancellation);
    let mut primary_detail = None;
    if report.outcome == SandboxPreflightOutcome::Supported {
        let result =
            run_task_workspace_preflight_command(&task_workspace, Arc::clone(sandbox_backend));
        if !unchanged_command_succeeded(&result) {
            primary_detail = Some(
                command_blocker(
                    &result,
                    BlockerKind::Sandbox,
                    "sandbox task workspace preflight failed",
                )
                .message,
            );
            report.outcome = SandboxPreflightOutcome::Unsupported;
            report.error_code = Some("sandbox_preflight_task_workspace_unavailable".to_string());
            report
                .missing_capabilities
                .push("strict_task_workspace".to_string());
        }
    }
    if report.outcome == SandboxPreflightOutcome::Supported
        && let Err((code, missing)) =
            preflight_task_executables(&capability_workspace, plans, sandbox_backend, cancellation)
    {
        report.outcome = SandboxPreflightOutcome::Unsupported;
        report.error_code = Some(code.to_string());
        report.missing_capabilities.extend(missing);
    }
    if report.outcome == SandboxPreflightOutcome::Supported {
        match preflight_remote_sources(
            &capability_workspace,
            plans,
            sandbox_backend,
            cancellation,
            cached_remote_repositories,
        ) {
            Ok(()) => {}
            Err(RemoteSourcePreflightFailure::Cancelled) => {
                report.outcome = SandboxPreflightOutcome::Unsupported;
                report.error_code = Some("sandbox_preflight_cancelled".to_string());
                report.missing_capabilities.push("cancellation".to_string());
            }
            Err(RemoteSourcePreflightFailure::Probe) => {
                report.outcome = SandboxPreflightOutcome::Unsupported;
                report.error_code = Some("sandbox_preflight_remote_source_unavailable".to_string());
                report
                    .missing_capabilities
                    .push("remote_git_source".to_string());
            }
        }
    }
    let trusted_git_required = plans.iter().any(|plan| {
        matches!(plan.source, PlannedWorkspaceSource::RemoteGit { .. })
            || plan.baseline.test_patch.is_some()
            || plan.public.test_patch.is_some()
            || plan.hidden.test_patch.is_some()
    });
    if report.outcome == SandboxPreflightOutcome::Supported && trusted_git_required {
        let preparation = run_workspace_preparation_command(
            &capability_workspace,
            &capability_workspace,
            vec![
                "git".to_string(),
                "init".to_string(),
                "--quiet".to_string(),
                SOURCE_DIR.to_string(),
            ],
            GIT_TIMEOUT_SECONDS,
            SandboxNetworkMode::Denied,
            Arc::clone(sandbox_backend),
        );
        if !command_succeeded(&preparation) {
            primary_detail = Some(
                command_blocker(
                    &preparation,
                    BlockerKind::Sandbox,
                    "sandbox trusted preparation preflight failed",
                )
                .message,
            );
            report.outcome = SandboxPreflightOutcome::Unsupported;
            report.error_code =
                Some("sandbox_preflight_trusted_preparation_unverified".to_string());
            report
                .missing_capabilities
                .push("trusted_workspace_preparation".to_string());
        }
    }
    let mut cleanup_errors = Vec::new();
    for (label, workspace) in [
        ("task workspace", task_workspace.as_path()),
        ("capability workspace", capability_workspace.as_path()),
    ] {
        if let Err(error) = sandbox_backend.release_workspace_observation(workspace) {
            cleanup_errors.push(format!("{label} observation release failed: {error}"));
        }
    }
    if !cleanup_errors.is_empty() {
        if report.outcome == SandboxPreflightOutcome::Supported {
            report.outcome = SandboxPreflightOutcome::Unsupported;
            report.error_code = Some("sandbox_preflight_observation_release_failed".to_string());
        }
        report
            .missing_capabilities
            .push("workspace_observation_release".to_string());
    }
    #[cfg(windows)]
    let scratch_cleanup = (|| {
        match fs::symlink_metadata(&scratch) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
        }
        let mut lease =
            TrustedWorkspaceLease::acquire(&scratch).map_err(|error| error.code().to_string())?;
        if !lease.matches_identity(scratch_identity) {
            return Err(TrustedWorkspaceError::RootDrift.code().to_string());
        }
        lease.rollback().map_err(|error| error.code().to_string())
    })();
    #[cfg(not(windows))]
    let scratch_cleanup = fs::remove_dir_all(&scratch).map_err(|error| error.to_string());
    if let Err(error) = scratch_cleanup {
        if report.outcome == SandboxPreflightOutcome::Supported {
            report.outcome = SandboxPreflightOutcome::Unsupported;
            report.error_code = Some("sandbox_preflight_scratch_cleanup".to_string());
        }
        report
            .missing_capabilities
            .push("scratch_cleanup".to_string());
        cleanup_errors.push(format!("sandbox preflight scratch cleanup failed: {error}"));
    }
    if report.outcome == SandboxPreflightOutcome::Supported
        && !report.proves_supported_contract_for(sandbox_backend.name())
    {
        report.outcome = SandboxPreflightOutcome::Unsupported;
        report.error_code = Some("sandbox_preflight_contract_invalid".to_string());
        report
            .missing_capabilities
            .push("validated_backend_contract".to_string());
    }
    if report.outcome == SandboxPreflightOutcome::Unsupported {
        let code = report
            .error_code
            .clone()
            .unwrap_or_else(|| "sandbox_preflight_unavailable".to_string());
        let mut message = format!("sandbox preflight unsupported: {code}");
        if let Some(detail) = primary_detail {
            message.push_str("; ");
            message.push_str(&detail);
        }
        if !cleanup_errors.is_empty() {
            message.push_str("; cleanup/resource errors: ");
            message.push_str(&cleanup_errors.join("; "));
        }
        return Err(Box::new(SandboxPreflightFailure {
            report,
            blocker: sandbox_preflight_blocker(code, message),
        }));
    }
    Ok(report)
}

fn stage_result(status: StageStatus, blocker: Option<EvaluationBlocker>) -> StageResult {
    StageResult { status, blocker }
}

fn evaluation_output_root(explicit: Option<&str>) -> PathBuf {
    let configured = std::env::var(OUTPUT_ROOT_ENV).ok();
    evaluation_output_root_for_sources(explicit, configured.as_deref(), &std::env::temp_dir())
}

fn source_template_cache_root(output_root: &Path) -> PathBuf {
    output_root
        .parent()
        .unwrap_or(output_root)
        .join("source-cache")
}

fn evaluation_output_root_for_sources(
    explicit: Option<&str>,
    configured: Option<&str>,
    system_temp: &Path,
) -> PathBuf {
    explicit
        .map(PathBuf::from)
        .or_else(|| configured.map(PathBuf::from))
        .unwrap_or_else(|| system_temp.join("singularity").join("evaluations"))
}

fn preflight_evaluation_path_budget(
    output_root: &Path,
    run_id: &RunId,
    task_ids: &[TaskId],
    trials_per_task: u32,
) -> Result<(), String> {
    if cfg!(windows) {
        preflight_evaluation_path_budget_with_limit(
            output_root,
            run_id,
            task_ids,
            trials_per_task,
            WINDOWS_MAX_PATH_CHARS,
        )
    } else {
        Ok(())
    }
}

fn preflight_evaluation_path_budget_with_limit(
    output_root: &Path,
    run_id: &RunId,
    task_ids: &[TaskId],
    trials_per_task: u32,
    max_path_chars: usize,
) -> Result<(), String> {
    let output_root = absolute_path_for_path_budget(output_root)?;
    let run_dir = output_root.join(run_id.as_str());
    for (context, path) in [
        (
            "evaluation result artifact",
            run_dir.join(PUBLICATION_DIR).join(RESULT_FILE),
        ),
        (
            "evaluation report artifact",
            run_dir.join(PUBLICATION_DIR).join(REPORT_FILE),
        ),
        (
            "evaluation evidence artifact",
            run_dir.join(PUBLICATION_DIR).join(EVIDENCE_FILE),
        ),
        (
            "evaluation publication manifest",
            run_dir
                .join(PUBLICATION_DIR)
                .join(PUBLICATION_MANIFEST_FILE),
        ),
        (
            "evaluation publication staging artifact",
            run_dir
                .join(format!(
                    ".{PUBLICATION_DIR}.4294967295.18446744073709551615.tmp"
                ))
                .join(EVIDENCE_FILE),
        ),
        (
            "evaluation publication staging manifest",
            run_dir
                .join(format!(
                    ".{PUBLICATION_DIR}.4294967295.18446744073709551615.tmp"
                ))
                .join(PUBLICATION_MANIFEST_FILE),
        ),
    ] {
        check_path_budget(context, &path, max_path_chars)?;
    }

    for task_id in task_ids {
        let task_dir = run_dir.join(task_id.as_str());
        let trial_dir = task_dir.join(format!("trial-{trials_per_task:04}"));
        for (context, path) in [
            (
                "remote git pack keep file",
                task_dir
                    .join(SOURCE_DIR)
                    .join(".git")
                    .join("objects")
                    .join("pack")
                    .join(format!("pack-{GIT_PACK_HEX}.keep")),
            ),
            ("agent trace artifact", trial_dir.join(AGENT_TRACE_FILE)),
            (
                "patch evidence artifact",
                trial_dir.join(PATCH_EVIDENCE_FILE),
            ),
        ] {
            check_path_budget(context, &path, max_path_chars)?;
        }

        check_path_budget(
            "Cargo target dependency artifact",
            &trial_dir
                .join(AGENT_DIR)
                .join("target")
                .join("debug")
                .join("deps")
                .join(format!("singularity_evaluation-{CARGO_DEP_HEX}.rlib")),
            max_path_chars,
        )?;
    }

    Ok(())
}

fn absolute_path_for_path_budget(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(|error| format!("failed to resolve evaluation output root: {error}"))
    }
}

fn check_path_budget(context: &str, path: &Path, max_path_chars: usize) -> Result<(), String> {
    let length = path_length_for_path_budget(path);
    if length >= max_path_chars {
        return Err(format!(
            "evaluation path budget exceeded for {context}: {} is {length} UTF-16 units; Windows legacy MAX_PATH is {max_path_chars}",
            path.display()
        ));
    }
    Ok(())
}

fn path_length_for_path_budget(path: &Path) -> usize {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        path.as_os_str().encode_wide().count()
    }

    #[cfg(not(windows))]
    {
        path.to_string_lossy().chars().count()
    }
}

fn publish_evaluation_artifacts(
    run_dir: &Path,
    run_id: &RunId,
    result: &impl Serialize,
    report: &impl Serialize,
    evidence: &impl Serialize,
) -> Result<PublishedEvaluationArtifacts, String> {
    let result_bytes = serialize_json_artifact(result)?;
    let report_bytes = serialize_json_artifact(report)?;
    let evidence_bytes = serialize_json_artifact(evidence)?;
    let relative_result = format!("{PUBLICATION_DIR}/{RESULT_FILE}");
    let relative_report = format!("{PUBLICATION_DIR}/{REPORT_FILE}");
    let relative_evidence = format!("{PUBLICATION_DIR}/{EVIDENCE_FILE}");
    let result_artifact = PublicationArtifact {
        path: relative_result,
        digest: content_digest(&result_bytes),
    };
    let report_artifact = PublicationArtifact {
        path: relative_report,
        digest: content_digest(&report_bytes),
    };
    let evidence_artifact = PublicationArtifact {
        path: relative_evidence,
        digest: content_digest(&evidence_bytes),
    };
    let artifact_set_digest = canonical_json_digest(&json!({
        "run_id": run_id.as_str(),
        "result": &result_artifact,
        "report": &report_artifact,
        "evidence": &evidence_artifact,
    }))?;
    let manifest = EvaluationPublicationManifest {
        schema_version: PUBLICATION_SCHEMA_VERSION,
        run_id: run_id.as_str().to_string(),
        artifact_set_digest,
        result: result_artifact,
        report: report_artifact,
        evidence: evidence_artifact,
    };
    let manifest_bytes = serialize_json_artifact(&manifest)?;

    let staging_dir = create_publication_staging_dir(run_dir)?;
    let write_result = (|| {
        write_synced_artifact(&staging_dir.join(RESULT_FILE), &result_bytes)?;
        write_synced_artifact(&staging_dir.join(REPORT_FILE), &report_bytes)?;
        write_synced_artifact(&staging_dir.join(EVIDENCE_FILE), &evidence_bytes)?;
        write_synced_artifact(
            &staging_dir.join(PUBLICATION_MANIFEST_FILE),
            &manifest_bytes,
        )
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_dir_all(&staging_dir);
        return Err(error);
    }

    let publication_dir = run_dir.join(PUBLICATION_DIR);
    if let Err(error) = fs::rename(&staging_dir, &publication_dir) {
        let _ = fs::remove_dir_all(&staging_dir);
        return Err(format!(
            "failed to publish evaluation artifact set {}: {error}",
            publication_dir.display()
        ));
    }
    Ok(PublishedEvaluationArtifacts {
        result_path: publication_dir.join(RESULT_FILE),
        report_path: publication_dir.join(REPORT_FILE),
        evidence_path: publication_dir.join(EVIDENCE_FILE),
    })
}

fn serialize_json_artifact(value: &impl Serialize) -> Result<Vec<u8>, String> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("failed to serialize artifact: {error}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn create_publication_staging_dir(run_dir: &Path) -> Result<PathBuf, String> {
    for _ in 0..ARTIFACT_TEMP_FILE_ATTEMPTS {
        let sequence = ARTIFACT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let staging_dir = run_dir.join(format!(
            ".{PUBLICATION_DIR}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        match fs::create_dir(&staging_dir) {
            Ok(()) => return Ok(staging_dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "failed to create publication staging directory {}: {error}",
                    staging_dir.display()
                ));
            }
        }
    }
    Err("failed to allocate a unique publication staging directory".to_string())
}

fn write_synced_artifact(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("failed to create artifact {}: {error}", path.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("failed to write artifact {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("failed to sync artifact {}: {error}", path.display()))
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let temp_path = write_json_temp(path, value)?;
    publish_json_temp(&temp_path, path)
}

fn write_json_temp(path: &Path, value: &impl Serialize) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("artifact path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "failed to create artifact directory {}: {error}",
            parent.display()
        )
    })?;
    let (temp_path, mut file) = create_artifact_temp_file(parent, path)?;
    let write_result = (|| {
        serde_json::to_writer_pretty(&mut file, value)
            .map_err(|error| format!("failed to serialize artifact: {error}"))?;
        file.write_all(b"\n")
            .map_err(|error| format!("failed to finalize artifact: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("failed to sync artifact: {error}"))
    })();
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    Ok(temp_path)
}

fn publish_json_temp(temp_path: &Path, path: &Path) -> Result<(), String> {
    fs::rename(temp_path, path).map_err(|error| {
        let _ = fs::remove_file(temp_path);
        format!(
            "failed to publish artifact {} from {}: {error}",
            path.display(),
            temp_path.display()
        )
    })
}

fn create_artifact_temp_file(parent: &Path, path: &Path) -> Result<(PathBuf, File), String> {
    let artifact_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
    for _ in 0..ARTIFACT_TEMP_FILE_ATTEMPTS {
        let sequence = ARTIFACT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".{artifact_name}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "failed to create artifact temp file {}: {error}",
                    temp_path.display()
                ));
            }
        }
    }
    Err(format!(
        "failed to allocate an artifact temp file in {} after {ARTIFACT_TEMP_FILE_ATTEMPTS} attempts",
        parent.display()
    ))
}

fn enum_string(value: impl Serialize) -> Result<String, String> {
    serde_json::to_value(value)
        .map_err(|error| error.to_string())?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "enum did not serialize as a string".to_string())
}

fn blocker_code(blocker: &EvaluationBlocker) -> Result<String, String> {
    enum_string(blocker.kind)
}

fn safe_text(text: impl AsRef<str>) -> String {
    let text = text.as_ref();
    if contains_sensitive_text(text) {
        "[redacted]".to_string()
    } else {
        text.chars().take(2_000).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use singularity_model::{ModelError, ModelErrorKind};
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn record_max(active: &AtomicUsize, maximum: &AtomicUsize) {
        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
        let mut observed = maximum.load(Ordering::SeqCst);
        while current > observed {
            match maximum.compare_exchange(observed, current, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                Err(next) => observed = next,
            }
        }
    }

    fn task_execution_with_status(task_index: usize, trial: u32) -> TaskExecution {
        let passed = !(task_index == 0 && trial == 1);
        let status = if passed {
            EvaluationStatus::Completed
        } else {
            EvaluationStatus::Failed
        };
        let agent_status = if passed {
            StageStatus::Passed
        } else {
            StageStatus::Failed
        };
        let evaluator_status = if passed {
            StageStatus::Passed
        } else {
            StageStatus::Skipped
        };
        TaskExecution {
            result: EvaluationTrialResult {
                trial,
                status,
                blocker: None,
                stages: EvaluationStageResults {
                    baseline: StageResult {
                        status: StageStatus::Passed,
                        blocker: None,
                    },
                    agent: StageResult {
                        status: agent_status,
                        blocker: None,
                    },
                    public: StageResult {
                        status: evaluator_status,
                        blocker: None,
                    },
                    hidden: StageResult {
                        status: evaluator_status,
                        blocker: None,
                    },
                },
                agent_completed: passed,
                tests_passed: passed,
                functional_task_success: passed,
                agent_protocol_success: passed,
                sandbox_security_success: true,
                evaluation_passed: passed,
                evidence: EvaluationEvidenceSummary::default(),
            },
            diagnostics: TaskDiagnostics::default(),
        }
    }

    #[test]
    fn bounded_workers_preserve_manifest_order() {
        let cancellation = CancellationToken::new();
        let results = run_bounded_indexed_workers(4, 2, &cancellation, |index| index * 2)
            .expect("workers complete");
        assert_eq!(results, vec![0, 2, 4, 6]);
        assert!(!cancellation.is_cancelled());
    }

    #[test]
    fn bounded_workers_fail_closed_after_panic() {
        let cancellation = CancellationToken::new();
        let result = run_bounded_indexed_workers(2, 2, &cancellation, |index| -> usize {
            if index == 0 {
                panic!("worker failure");
            }
            index
        });
        assert!(matches!(
            result,
            Err(IndexedWorkerError::Failed(message)) if message.contains("worker panicked")
        ));
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn bounded_workers_return_only_completed_prefix_after_cancellation() {
        let cancellation = CancellationToken::new();
        let canceller = cancellation.clone();
        let barrier = Arc::new(Barrier::new(2));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let result = run_bounded_indexed_workers(4, 2, &cancellation, {
            let barrier = Arc::clone(&barrier);
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            move |index| {
                record_max(&active, &maximum);
                if index < 2 {
                    barrier.wait();
                    if index == 1 {
                        canceller.cancel();
                    }
                    barrier.wait();
                }
                active.fetch_sub(1, Ordering::SeqCst);
                index
            }
        });
        assert!(matches!(
            result,
            Err(IndexedWorkerError::Cancelled(prefix)) if prefix == vec![0, 1]
        ));
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn trial_workers_overlap_all_trials_and_preserve_task_trial_order() {
        let cancellation = CancellationToken::new();
        let barrier = Arc::new(Barrier::new(6));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let results = run_bounded_trial_workers(3, &[1, 2], 6, &cancellation, {
            let barrier = Arc::clone(&barrier);
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            move |task_index, trial| {
                record_max(&active, &maximum);
                barrier.wait();
                active.fetch_sub(1, Ordering::SeqCst);
                (task_index, trial)
            }
        })
        .expect("trial workers complete");

        assert_eq!(maximum.load(Ordering::SeqCst), 6);
        assert_eq!(
            results,
            vec![(0, 1), (0, 2), (1, 1), (1, 2), (2, 1), (2, 2)]
        );
    }

    #[test]
    fn trial_workers_continue_after_failed_task_execution_without_failing_fast() {
        let cancellation = CancellationToken::new();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let results = match run_bounded_trial_workers(3, &[1, 2], 2, &cancellation, {
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            move |task_index, trial| {
                record_max(&active, &maximum);
                thread::sleep(Duration::from_millis(5));
                let execution = task_execution_with_status(task_index, trial);
                active.fetch_sub(1, Ordering::SeqCst);
                execution
            }
        }) {
            Ok(results) => results,
            Err(_) => panic!("a failed trial must not fail fast"),
        };

        assert!(maximum.load(Ordering::SeqCst) <= 2);
        assert_eq!(results.len(), 6);
        assert_eq!(results[0].result.status, EvaluationStatus::Failed);
        assert!(
            results
                .iter()
                .skip(1)
                .all(|execution| execution.result.status == EvaluationStatus::Completed)
        );
    }

    #[test]
    fn zero_sampling_blocker_builds_a_report_with_the_configured_task_denominator() {
        let sandbox_backend: SharedSandboxBackend = Arc::new(SourceSandboxBackend);
        let preflight = sandbox_preflight_evidence(
            &SandboxPreflightReport::unverified_for_backend(sandbox_backend.as_ref()),
        );
        let result = EvaluationResult::blocked_before_sampling(
            RunId::new("zero-sampling-report").expect("run id"),
            1,
            1,
            EvaluationBlocker {
                code: Some("workspace_preparation_failed".to_string()),
                kind: BlockerKind::WorkspacePreparation,
                message: "source preparation failed".to_string(),
                task_id: None,
            },
            preflight,
        );
        let params = EvaluationRunParams {
            manifest: "manifest.json".to_string(),
            run_id: result.run_id.as_str().to_string(),
            output_root: None,
            max_workers: 1,
            recovery_every: None,
        };

        let report = build_evaluation_report(&params, &result, &[], None, 1)
            .expect("zero-sampling blocker report");

        assert!(report.tasks.is_empty());
        assert_eq!(report.dimensions.functional_task_count, 1);
        assert_eq!(report.dimensions.functional_task_success_count, 0);
    }

    #[test]
    fn refresh_trial_trace_artifact_projects_terminal_spans_from_sqlite() {
        use singularity_store::SessionStore;

        let temp = tempfile::tempdir().expect("temp");
        let run_id = RunId::new("run").expect("run id");
        let task_id = TaskId::new("task").expect("task id");
        let trace_path = temp.path().join(AGENT_TRACE_FILE);
        let cancellation = CancellationToken::new();
        let trace_failures = Arc::new(Mutex::new(Vec::new()));
        let sandbox_backend: SharedSandboxBackend = Arc::new(SourceSandboxBackend);
        let sandbox_preflight =
            SandboxPreflightReport::unverified_for_backend(sandbox_backend.as_ref());
        let provider_snapshot = ProviderConfigSnapshot::capture(|_| None, None, None);
        let source_cache = SourceTemplateCache::new(temp.path().join("source-cache"));
        let mut store = SessionStore::open(":memory:").expect("trace store");
        let trace_store = Arc::new(Mutex::new(&mut store));
        let context = EvaluationRunContext {
            run_id: &run_id,
            run_dir: temp.path(),
            sandbox_backend: &sandbox_backend,
            provider_snapshot: &provider_snapshot,
            cancellation: &cancellation,
            trace_store: Arc::clone(&trace_store),
            trace_failures: Arc::clone(&trace_failures),
            sandbox_preflight: &sandbox_preflight,
            source_cache: &source_cache,
        };
        let sink = EvaluationTraceSink::new(Arc::clone(&trace_store), &run_id, &trace_failures);
        let task_session_id = format!("task:{}", task_id.as_str());
        let task_span_id = evaluation_span_id(&run_id, &task_session_id, TraceSpanKind::Task);
        let trial_session_id = format!("trial:{}:1", task_id.as_str());
        let trial_span_id = evaluation_span_id(&run_id, &trial_session_id, TraceSpanKind::Turn);
        let task_started = Instant::now();
        let trial_started = Instant::now();
        sink.start(
            &task_session_id,
            &task_span_id,
            None,
            TraceSpanKind::Task,
            "evaluation task",
        );
        sink.start(
            &trial_session_id,
            &trial_span_id,
            Some(&task_span_id),
            TraceSpanKind::Turn,
            "evaluation trial",
        );
        sink.end(
            &trial_session_id,
            &trial_span_id,
            Some(&task_span_id),
            TraceSpanKind::Turn,
            TraceSpanStatus::Ok,
            trial_started,
        );
        sink.end(
            &task_session_id,
            &task_span_id,
            None,
            TraceSpanKind::Task,
            TraceSpanStatus::Ok,
            task_started,
        );

        let mut execution = task_execution_with_status(0, 1);
        execution.diagnostics.trace_path = Some(trace_path.to_string_lossy().into_owned());
        let task_result =
            EvaluationTaskResult::from_trials(task_id, Vec::new(), vec![execution.result.clone()]);
        refresh_trial_trace_artifacts(
            &context,
            &[TaskEvaluation {
                result: task_result,
                trials: vec![execution],
            }],
        );

        assert!(ensure_trace_failures_empty(&trace_failures).is_ok());
        let artifact: Value =
            serde_json::from_str(&fs::read_to_string(&trace_path).expect("trace artifact"))
                .expect("trace artifact JSON");
        let events = artifact["events"].as_array().expect("trace events array");
        assert!(events.iter().any(|event| {
            event["span_id"] == task_span_id
                && event["span_phase"] == "end"
                && event["span_status"] == "ok"
        }));
        assert!(events.iter().any(|event| {
            event["span_id"] == trial_span_id
                && event["span_phase"] == "end"
                && event["span_status"] == "ok"
        }));
    }

    #[test]
    fn provider_blocker_keeps_bounded_code_and_redacts_message() {
        let error = ProviderError::from_model_error(
            ModelError::new(
                ModelErrorKind::UnknownProviderError,
                "provider_response_invalid: api_key=secret",
            )
            .with_provider_diagnostic(
                "provider_response_invalid",
                ProviderErrorStage::ResponseValidation,
            ),
        );

        let blocker = provider_blocker(&error);

        assert_eq!(blocker.kind, BlockerKind::ProviderResponse);
        assert_eq!(blocker.code.as_deref(), Some("provider_response_invalid"));
        assert_eq!(blocker.message, "[redacted]");
    }

    #[test]
    fn failure_attribution_keeps_bounded_provider_code_and_redacts_diagnostic_error() {
        let task_id = TaskId::new("task").expect("task id");
        let trial_result = EvaluationTrialResult {
            trial: 1,
            status: EvaluationStatus::Failed,
            blocker: None,
            stages: EvaluationStageResults {
                baseline: StageResult {
                    status: StageStatus::Passed,
                    blocker: None,
                },
                agent: StageResult {
                    status: StageStatus::Failed,
                    blocker: None,
                },
                public: StageResult {
                    status: StageStatus::Skipped,
                    blocker: None,
                },
                hidden: StageResult {
                    status: StageStatus::Skipped,
                    blocker: None,
                },
            },
            agent_completed: false,
            tests_passed: false,
            functional_task_success: false,
            agent_protocol_success: false,
            sandbox_security_success: true,
            evaluation_passed: false,
            evidence: EvaluationEvidenceSummary::default(),
        };
        let task_result = EvaluationTaskResult::from_trials(
            task_id.clone(),
            Vec::new(),
            vec![trial_result.clone()],
        );
        let run_result = EvaluationResult::from_tasks(
            RunId::new("run").expect("run id"),
            1,
            vec![task_result.clone()],
        );
        let execution = TaskEvaluation {
            result: task_result,
            trials: vec![TaskExecution {
                result: trial_result,
                diagnostics: TaskDiagnostics {
                    error: Some("provider_response_invalid: api_key=secret".to_string()),
                    provider_diagnostic: Some(ProviderDiagnostic {
                        code: Some("provider_response_invalid".to_string()),
                        stage: Some(ProviderErrorStage::ResponseValidation),
                        transport_category: None,
                        timeout_seconds: None,
                        http_status: None,
                        validation_errors: Vec::new(),
                    }),
                    ..TaskDiagnostics::default()
                },
            }],
        };

        let failures = build_failure_attributions(&run_result, &[execution]);

        assert_eq!(failures.len(), 1);
        assert_eq!(
            failures[0].code.as_deref(),
            Some("provider_response_invalid")
        );
        assert_eq!(failures[0].message, "[redacted]");
        assert_eq!(failures[0].task_id.as_ref(), Some(&task_id));
        assert_eq!(failures[0].trial, Some(1));
    }

    #[test]
    fn trace_projection_failures_are_preserved() {
        let failures = Arc::new(Mutex::new(Vec::new()));
        record_trace_failure(&failures, "trace write failed");
        let error = ensure_trace_failures_empty(&failures).expect_err("failure must propagate");
        assert!(error.contains("trace write failed"));
    }

    #[test]
    fn metric_values_keep_observed_producer_subset() {
        let samples = [(true, Some(7)), (true, None), (false, Some(11))];
        assert_eq!(
            metric_sum(&samples, |sample| sample.0, |sample| sample.1),
            MetricValue::available(7)
        );
    }

    #[test]
    fn recovery_trace_metrics_uses_typed_verification_end_counts() {
        let mut event = TraceEvent::for_turn(
            "verification-end",
            "thread",
            "turn",
            "app_server",
            "verification",
        );
        event.span_kind = Some(TraceSpanKind::Verification);
        event.span_phase = Some(TraceSpanPhase::End);
        event.span_status = Some(TraceSpanStatus::Ok);
        event.span_projection = Some(TraceSpanProjection {
            verification: Some(singularity_protocol::TraceVerificationProjection {
                required_command_count: Some(3),
                satisfied_command_count: Some(2),
                ..Default::default()
            }),
            ..Default::default()
        });

        let (_, _, _, _, required, satisfied) = recovery_trace_metrics(&[event]);
        assert_eq!(required, Some(3));
        assert_eq!(satisfied, Some(2));
    }

    #[test]
    fn recovery_verification_counts_rejects_missing_typed_evidence() {
        let blocker = recovery_verification_counts(None, None)
            .expect_err("required recovery verification must be observed");
        assert_eq!(blocker.kind, BlockerKind::AgentRuntime);
        assert_eq!(
            blocker.code.as_deref(),
            Some("recovery_verification_evidence_unobserved")
        );
    }

    #[test]
    fn harness_metrics_aggregate_successful_costs_and_compaction_decay() {
        let mut non_compacted = task_execution_with_status(1, 1);
        non_compacted.diagnostics.provider_usage_available = true;
        non_compacted.diagnostics.total_tokens = 100;
        non_compacted.diagnostics.trial_duration_ms = 30;
        non_compacted.diagnostics.compaction_count = 0;

        let mut compacted_success = task_execution_with_status(1, 2);
        compacted_success.diagnostics.provider_usage_available = true;
        compacted_success.diagnostics.total_tokens = 300;
        compacted_success.diagnostics.trial_duration_ms = 50;
        compacted_success.diagnostics.compaction_count = 1;

        let mut compacted_failure = task_execution_with_status(0, 1);
        compacted_failure.diagnostics.compaction_count = 2;

        let metrics = build_harness_metrics(
            &[&non_compacted, &compacted_success, &compacted_failure],
            None,
        );
        assert_eq!(
            metrics.tokens_per_functional_success,
            MetricValue::available(MetricStatistics {
                count: 2,
                sum: 400,
                min: 100,
                max: 300,
                mean: 200.0,
                p50: 100,
                p95: 300,
            })
        );
        assert_eq!(
            metrics.time_per_functional_success,
            MetricValue::available(MetricStatistics {
                count: 2,
                sum: 80,
                min: 30,
                max: 50,
                mean: 40.0,
                p50: 30,
                p95: 50,
            })
        );
        assert_eq!(
            metrics.compaction_performance_decay,
            MetricValue::available(5_000)
        );
        assert_eq!(
            metrics.recovery_completion_rate,
            MetricValue::unavailable(MetricUnavailableReason::NoProducer)
        );
    }

    #[test]
    fn harness_metrics_use_direct_tool_first_rate_and_require_bypass_observation() {
        let mut observed = task_execution_with_status(1, 1);
        observed.diagnostics.verification_bypass_count = Some(0);
        let mut second_observed = task_execution_with_status(1, 2);
        second_observed.diagnostics.verification_bypass_count = Some(2);
        let trace_metrics = TraceMetrics {
            run_id: "run".to_string(),
            metrics: vec![singularity_protocol::TraceMetric {
                name: TraceMetricName::ToolFirstAttemptSuccessRateBps,
                availability: TraceMetricAvailability::Available,
                distribution: Some(singularity_protocol::TraceMetricDistribution {
                    count: 1,
                    sum: 7_500,
                    min: Some(7_500),
                    max: Some(7_500),
                    mean: Some(7_500.0),
                    p50: Some(7_500),
                    p95: Some(7_500),
                }),
            }],
        };
        let metrics = build_harness_metrics(&[&observed, &second_observed], Some(&trace_metrics));
        assert!(matches!(
            metrics.tool_first_attempt_success_rate,
            MetricValue::Available {
                value: MetricRatio {
                    basis_points: 7_500,
                    ..
                }
            }
        ));
        assert_eq!(metrics.verification_bypass_count, MetricValue::available(2));

        second_observed.diagnostics.verification_bypass_count = None;
        let unavailable = build_harness_metrics(&[&observed, &second_observed], None);
        assert_eq!(
            unavailable.tool_first_attempt_success_rate,
            MetricValue::unavailable(MetricUnavailableReason::NoProducer)
        );
        assert_eq!(
            unavailable.verification_bypass_count,
            MetricValue::unavailable(MetricUnavailableReason::NotObserved)
        );
    }

    fn changed_tool_result(tool_name: &str, path: &str, ok: bool) -> ToolResult {
        ToolResult::summary("occurrence", tool_name, ok, "summary")
            .with_workspace_observation(singularity_tools::WorkspaceObservation::changed(
                singularity_tools::WorkspaceRevision::initial()
                    .next()
                    .expect("revision"),
            ))
            .with_workspace_change_summary(WorkspaceChangeSummary::new(
                vec![path.to_string()],
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            ))
    }

    #[test]
    fn verification_bypass_producer_counts_only_integrity_path_hits() {
        let integrity = BTreeSet::from(["tests/hidden.py".to_string()]);
        let ordinary = changed_tool_result(PATCH_TOOL, "src/module.py", true);
        assert_eq!(
            verification_bypass_count_for_results(&[ordinary], &integrity),
            Some(0)
        );

        let protected_overlap = changed_tool_result(PATCH_TOOL, "tests/hidden.py", true);
        assert_eq!(
            verification_bypass_count_for_results(&[protected_overlap], &integrity),
            Some(1)
        );

        let edit = changed_tool_result(PATCH_TOOL, "tests/hidden.py", true);
        let revert = changed_tool_result(PATCH_TOOL, "tests/hidden.py", true);
        assert_eq!(
            verification_bypass_count_for_results(&[edit, revert], &integrity),
            Some(2)
        );
    }

    #[test]
    fn verification_bypass_producer_excludes_denied_and_failed_commands() {
        let integrity = BTreeSet::from(["tests/hidden.py".to_string()]);
        let mut denied = ToolResult::summary("denied", PATCH_TOOL, false, "denied");
        denied.failure_kind = Some(ToolFailureKind::ProtectedPath);
        assert_eq!(
            verification_bypass_count_for_results(&[denied], &integrity),
            Some(0)
        );

        let failed_command = changed_tool_result(TOOL_COMMAND, "tests/hidden.py", false);
        assert_eq!(
            verification_bypass_count_for_results(&[failed_command], &integrity),
            Some(0)
        );
    }

    #[test]
    fn verification_bypass_producer_is_unavailable_for_unknown_or_incomplete_summary() {
        let integrity = BTreeSet::from(["tests/hidden.py".to_string()]);
        let unknown = ToolResult::summary("unknown", PATCH_TOOL, false, "unknown")
            .with_workspace_observation(singularity_tools::WorkspaceObservation::unknown());
        assert_eq!(
            verification_bypass_count_for_results(&[unknown], &integrity),
            None
        );

        let missing_summary = ToolResult::summary("missing", PATCH_TOOL, true, "changed")
            .with_workspace_observation(singularity_tools::WorkspaceObservation::changed(
                singularity_tools::WorkspaceRevision::initial()
                    .next()
                    .expect("revision"),
            ));
        assert_eq!(
            verification_bypass_count_for_results(&[missing_summary], &integrity),
            None
        );
    }

    #[test]
    fn hidden_integrity_paths_are_not_serialized_in_tool_results() {
        let integrity = BTreeSet::from(["tests/hidden.py".to_string()]);
        let result = changed_tool_result(PATCH_TOOL, "tests/hidden.py", true);
        assert_eq!(
            verification_bypass_count_for_results(std::slice::from_ref(&result), &integrity),
            Some(1)
        );
        let serialized = serde_json::to_string(&result).expect("tool result serializes");
        assert!(!serialized.contains("tests/hidden.py"));
    }

    fn recovery_occurrence_payload(
        result: &ToolResult,
        visibility: singularity_agent::ToolResultVisibility,
    ) -> Value {
        json!({
            "result": result,
            "visibility": visibility,
            "result_id": result.result_id,
            "context_token_count": result.context_token_count(),
            "audit_metadata": result.audit_metadata(),
            "workspace_observation": result.workspace_observation(),
            "workspace_change_summary": result.workspace_change_summary(),
        })
    }

    fn recovery_tool_call_span(
        event_id: &str,
        thread_id: &str,
        turn_id: &str,
        result: &ToolResult,
        phase: TraceSpanPhase,
        status: TraceToolStatus,
    ) -> TraceEvent {
        let mut event = TraceEvent::for_turn(event_id, thread_id, turn_id, "observability", "tool");
        event.span_id = Some("tool-span".to_string());
        event.parent_span_id = Some(format!("turn_span_{turn_id}"));
        event.span_kind = Some(TraceSpanKind::ToolCall);
        event.span_phase = Some(phase);
        event.span_projection = Some(TraceSpanProjection {
            tool: Some(singularity_protocol::TraceToolProjection {
                tool_name: Some(result.tool_name.clone()),
                tool_call_id_digest: Some(content_digest(result.tool_call_id.as_bytes())),
                tool_call_ordinal: Some(0),
                first_attempt: Some(true),
                status: (phase == TraceSpanPhase::End).then_some(status),
            }),
            ..TraceSpanProjection::default()
        });
        if phase == TraceSpanPhase::End {
            event.span_status = Some(if status == TraceToolStatus::Succeeded {
                TraceSpanStatus::Ok
            } else {
                TraceSpanStatus::Error
            });
            event.duration_ms = Some(1);
        }
        event
    }

    fn recovery_tool_result_event(
        event_id: &str,
        thread_id: &str,
        turn_id: &str,
        result: &ToolResult,
        visibility: singularity_agent::ToolResultVisibility,
        status: TraceToolStatus,
    ) -> TraceEvent {
        let mut event =
            TraceEvent::for_turn(event_id, thread_id, turn_id, "observability", "tool result");
        event.payload = json!({
            "observation": "tool_result",
            "tool_result": {
                "tool_name": result.tool_name,
                "tool_call_id_digest": content_digest(result.tool_call_id.as_bytes()),
                "tool_call_ordinal": 0,
                "first_attempt": true,
                "status": status,
                "visibility": visibility,
                "ok": result.ok,
            },
        });
        event
    }

    #[test]
    fn recovery_verification_bypass_reads_private_occurrence_payload() {
        let directory = tempfile::tempdir().expect("recovery trace directory");
        let db_path = directory.path().join("recovery.sqlite3");
        let store = singularity_store::SessionStore::open(&db_path).expect("open recovery store");
        let thread = store
            .create_thread(None, None)
            .expect("create recovery thread");
        let turn = store
            .create_turn(&thread.thread_id, "running")
            .expect("create recovery turn");
        let thread_id = thread.thread_id.as_str();
        let turn_id = turn.turn_id.as_str();
        let result = changed_tool_result(PATCH_TOOL, "tests/hidden.py", true);
        store
            .append_trace_idempotent(&recovery_tool_call_span(
                "tool-start",
                thread_id,
                turn_id,
                &result,
                TraceSpanPhase::Start,
                TraceToolStatus::Succeeded,
            ))
            .expect("append tool start");
        store
            .append_trace_idempotent(&recovery_tool_call_span(
                "tool-end",
                thread_id,
                turn_id,
                &result,
                TraceSpanPhase::End,
                TraceToolStatus::Succeeded,
            ))
            .expect("append tool end");
        let event = recovery_tool_result_event(
            "tool-result",
            thread_id,
            turn_id,
            &result,
            singularity_agent::ToolResultVisibility::Visible,
            TraceToolStatus::Succeeded,
        );
        store
            .append_trace_with_internal_payload_idempotent(
                &event,
                Some(&recovery_occurrence_payload(
                    &result,
                    singularity_agent::ToolResultVisibility::Visible,
                )),
            )
            .expect("append private tool result");
        let trace = store.list_trace(thread_id).expect("list recovery trace");
        let integrity = BTreeSet::from(["tests/hidden.py".to_string()]);
        assert_eq!(
            recovery_verification_bypass_count(
                &db_path,
                &trace,
                thread_id,
                turn_id,
                Some(&integrity),
            ),
            Some(1)
        );
    }

    #[test]
    fn recovery_verification_bypass_fails_closed_for_missing_or_unknown_payload() {
        let directory = tempfile::tempdir().expect("recovery trace directory");
        let db_path = directory.path().join("recovery.sqlite3");
        let store = singularity_store::SessionStore::open(&db_path).expect("open recovery store");
        let thread = store
            .create_thread(None, None)
            .expect("create recovery thread");
        let turn = store
            .create_turn(&thread.thread_id, "running")
            .expect("create recovery turn");
        let thread_id = thread.thread_id.as_str();
        let turn_id = turn.turn_id.as_str();
        let result = changed_tool_result(PATCH_TOOL, "tests/hidden.py", true);
        for event in [
            recovery_tool_call_span(
                "tool-start",
                thread_id,
                turn_id,
                &result,
                TraceSpanPhase::Start,
                TraceToolStatus::Succeeded,
            ),
            recovery_tool_call_span(
                "tool-end",
                thread_id,
                turn_id,
                &result,
                TraceSpanPhase::End,
                TraceToolStatus::Succeeded,
            ),
        ] {
            store
                .append_trace_idempotent(&event)
                .expect("append trace span");
        }
        let missing = recovery_tool_result_event(
            "missing-result",
            thread_id,
            turn_id,
            &result,
            singularity_agent::ToolResultVisibility::Visible,
            TraceToolStatus::Succeeded,
        );
        store
            .append_trace_idempotent(&missing)
            .expect("append missing private payload");
        let trace = store.list_trace(thread_id).expect("list missing trace");
        let integrity = BTreeSet::from(["tests/hidden.py".to_string()]);
        assert_eq!(
            recovery_verification_bypass_count(
                &db_path,
                &trace,
                thread_id,
                turn_id,
                Some(&integrity),
            ),
            None
        );

        let unknown = result
            .clone()
            .with_workspace_observation(singularity_tools::WorkspaceObservation::unknown());
        let unknown_event = recovery_tool_result_event(
            "unknown-result",
            thread_id,
            turn_id,
            &unknown,
            singularity_agent::ToolResultVisibility::Visible,
            TraceToolStatus::Succeeded,
        );
        store
            .append_trace_with_internal_payload_idempotent(
                &unknown_event,
                Some(&recovery_occurrence_payload(
                    &unknown,
                    singularity_agent::ToolResultVisibility::Visible,
                )),
            )
            .expect("append unknown private payload");
        let trace = store.list_trace(thread_id).expect("list unknown trace");
        assert_eq!(
            recovery_verification_bypass_count(
                &db_path,
                &trace,
                thread_id,
                turn_id,
                Some(&integrity),
            ),
            None
        );
    }

    #[test]
    fn unified_diff_integrity_parser_handles_new_and_deleted_files() {
        let paths = parse_unified_diff_paths(
            "diff --git a/tests/hidden.py b/tests/hidden.py\n--- /dev/null\n+++ b/tests/hidden.py\n@@ -0,0 +1 @@\n+pass\n\ndiff --git a/tests/old.py b/tests/old.py\n--- a/tests/old.py\n+++ /dev/null\n@@ -1 +0,0 @@\n-pass\n",
        )
        .expect("unified diff paths");
        assert_eq!(
            paths,
            BTreeSet::from(["tests/hidden.py".to_string(), "tests/old.py".to_string()])
        );
    }

    #[test]
    fn source_template_metrics_keep_observed_subset() {
        let observed = TaskDiagnostics {
            source_template_expected: true,
            source_template_cache_status: Some(SourceTemplateCacheStatus::Hit),
            source_template_materialization_ms: 4,
            ..TaskDiagnostics::default()
        };
        let not_observed = TaskDiagnostics {
            source_template_expected: true,
            ..TaskDiagnostics::default()
        };
        let (hits, misses, materialization) =
            source_template_cache_metrics(&[&observed, &not_observed]);
        assert_eq!(hits, MetricValue::available(1));
        assert_eq!(misses, MetricValue::available(0));
        assert!(matches!(materialization, MetricValue::Available { .. }));
    }

    /// 固定 git 能力与固定 commit 的 mock 后端，只服务于远程 git 源全路径测试。
    struct SourceSandboxBackend;

    impl SandboxBackend for SourceSandboxBackend {
        fn name(&self) -> &'static str {
            "source_test"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            if request.argv.as_slice() == ["git", "--version"] {
                return CommandResult::completed(&request.command_id, "git version 2.55.0")
                    .with_workspace_mutation(WorkspaceMutation::Unchanged)
                    .with_sandbox_execution(
                        self.name(),
                        singularity_tools::SandboxBackendEnforcement::Strict,
                    );
            }
            if request.argv.get(1).map(String::as_str) == Some("clone") {
                assert_eq!(request.argv.get(2).map(String::as_str), Some("--quiet"));
                assert_eq!(request.argv.get(3).map(String::as_str), Some("--revision"));
                assert_eq!(
                    request.argv.get(4).map(String::as_str),
                    Some(REMOTE_SOURCE_COMMIT)
                );
                // 克隆目标必须是 sandbox 写边界（task_dir）内的固定 source 目录。
                assert_eq!(
                    request.argv.last().map(String::as_str),
                    Some(SOURCE_DIR),
                    "clone target must stay inside the task workspace"
                );
                let source = Path::new(&request.cwd).join(SOURCE_DIR);
                fs::create_dir(&source).expect("source directory");
                fs::write(source.join("README.md"), "fixture").expect("source file");
                return CommandResult::completed(&request.command_id, "ok")
                    .with_workspace_mutation(WorkspaceMutation::Changed)
                    .with_sandbox_execution(
                        self.name(),
                        singularity_tools::SandboxBackendEnforcement::Strict,
                    );
            }
            if request.argv.get(3).map(String::as_str) == Some("rev-parse") {
                return CommandResult::completed(&request.command_id, REMOTE_SOURCE_COMMIT)
                    .with_workspace_mutation(WorkspaceMutation::Unchanged)
                    .with_sandbox_execution(
                        self.name(),
                        singularity_tools::SandboxBackendEnforcement::Strict,
                    );
            }
            if request.argv.get(3).map(String::as_str) == Some("symbolic-ref") {
                return CommandResult::executed(&request.command_id, 1, 0, "", "", false)
                    .with_workspace_mutation(WorkspaceMutation::Unchanged)
                    .with_sandbox_execution(
                        self.name(),
                        singularity_tools::SandboxBackendEnforcement::Strict,
                    );
            }
            panic!("unexpected source preparation command: {:?}", request.argv);
        }
    }

    const REMOTE_SOURCE_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn remote_git_source_prepares_task_tree_and_publishes_cache_template() {
        let temp = tempfile::tempdir().expect("temp");
        let task_id = TaskId::new("task").expect("task id");
        let task_dir = temp.path().join("task");
        fs::create_dir(&task_dir).expect("task directory");
        let source_dir = task_dir.join(SOURCE_DIR);
        let repository = "https://example.invalid/repo.git";
        let source = PlannedWorkspaceSource::RemoteGit {
            repository: crate::RemoteRepository::new(repository).expect("repository"),
            commit: crate::GitCommit::new(REMOTE_SOURCE_COMMIT).expect("commit"),
        };
        let cache = SourceTemplateCache::new(temp.path().join("source-cache"));

        let prepared = prepare_source(
            &source,
            &task_id,
            &task_dir,
            &source_dir,
            Arc::new(SourceSandboxBackend),
            &cache,
            &CancellationToken::new(),
        )
        .expect("first remote preparation must succeed");

        // 任务侧代码树就位、无 .git 元数据，且 verify 命令已真实执行。
        assert_eq!(
            fs::read_to_string(source_dir.join("README.md")).unwrap(),
            "fixture"
        );
        assert!(!source_dir.join(".git").exists());
        assert!(
            prepared
                .commands
                .iter()
                .any(|command| command.phase == "source.git_verify_commit")
        );

        // 首次 fetch 后缓存模板已发布，后续命中直接物化、不再 fetch。
        assert!(
            cache
                .entry_available(task_id.as_str(), repository)
                .expect("published cache entry")
        );
        let second_task_dir = temp.path().join("task-second");
        fs::create_dir(&second_task_dir).expect("second task directory");
        let second_source_dir = second_task_dir.join(SOURCE_DIR);
        prepare_source(
            &source,
            &task_id,
            &second_task_dir,
            &second_source_dir,
            Arc::new(SourceSandboxBackend),
            &cache,
            &CancellationToken::new(),
        )
        .expect("cached preparation must succeed");
        assert_eq!(
            fs::read_to_string(second_source_dir.join("README.md")).unwrap(),
            "fixture"
        );
    }
}
