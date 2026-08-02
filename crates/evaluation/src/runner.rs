//! 开发期 Evaluation runner 的任务投影、Agent stage、验证证据与安全产物协调。
//!
//! 本模块只把 manifest 的可信内部命令和模型可见 command string 分开投影，
//! 并在固定 gate、sandbox 与 evidence 合同下汇总结果。

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use crate::{
    AgentStagePlan, AgentTaskProjection, BlockerKind, CommandExpectation, CommandSpec,
    EvaluationBlocker, EvaluationEvidenceSummary, EvaluationManifest, EvaluationPromptStructure,
    EvaluationProviderEvidence, EvaluationResult, EvaluationSandboxPreflight,
    EvaluationSandboxPreflightFact, EvaluationSandboxPreflightOutcome, EvaluationSelection,
    EvaluationStageResults, EvaluationStatus, EvaluationTaskResult, EvaluationTrialResult,
    PatchFormat, PlannedWorkspaceSource, RunId, StageResult, StageStatus, TaskId,
    VerificationStagePlan, WorkspacePlan,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use singularity_agent::{
    AgentLoop, AgentLoopEventSinkError, AgentLoopInput, AgentLoopResult, AgentRecoveryMetrics,
    AgentStatus,
};
use singularity_app_server::TraceProjector;
use singularity_core::{
    CancellationToken, Timestamp, contains_sensitive_text, load_project_instructions,
};
use singularity_model::{
    ModelErrorCategory, ModelUsage, OpenAiProvider, ProviderAttemptMetadata,
    ProviderCapabilityMetadata, ProviderConfigSnapshot, ProviderDiagnostic, ProviderError,
    ProviderErrorStage, ProviderProtocolContract, ProviderProtocolNegotiation,
};
use singularity_policy::{ApprovalPolicy, PermissionProfileName, workspace_policy};
use singularity_protocol::{
    TraceEvent, TraceSpanKind, TraceSpanPhase, TraceSpanProjection, TraceSpanStatus,
};
use singularity_sandbox::PreparedWorkspaceObservation;
#[cfg(windows)]
use singularity_sandbox::{TrustedWorkspaceError, TrustedWorkspaceLease};
use singularity_tools::{
    COMMAND_TOOL as TOOL_COMMAND, CommandEnvironmentPolicy, CommandExecutionStatus, CommandRequest,
    CommandResult, CommandScriptRequest, CommandSemanticStatus, ExecutableAvailability,
    PATCH_TOOL as TOOL_PATCH, SandboxBackend, SandboxCapabilities, SandboxNetworkMode,
    SandboxPreflightFact, SandboxPreflightOutcome, SandboxPreflightReport, ToolBroker,
    ToolRegistry, WorkspaceChangeSummary, WorkspaceMutation, WorkspaceTools,
    workspace_tool_entries,
};
#[cfg(test)]
use singularity_tools::{READ_TOOL as TOOL_READ, SandboxFilesystemMode};

mod command;
mod evidence;
mod workspace;

use command::{
    CommandDiagnostic, command_blocker, command_succeeded, infrastructure_blocker,
    run_command_spec, run_raw_command, run_task_workspace_preflight_command,
    run_workspace_preparation_command, run_workspace_preparation_read_only_command,
    unchanged_command_succeeded,
};
use evidence::{
    agent_command_projection, build_evaluation_evidence, build_zero_sampling_evidence,
    canonical_json_digest, content_digest,
};
use workspace::{
    ObservedPreparedSource, WorkspaceChangeEvidence, WorkspaceObservationMetric, WorkspaceSnapshot,
    copy_tree_for_preparation, evaluation_changed_paths, materialize_prepared_workspace,
    patch_evidence_digest, snapshot_workspace_incremental, snapshot_workspace_with_work,
    workspace_change_evidence, workspace_root_identity, workspace_snapshot_digest,
};
#[cfg(test)]
use workspace::{copy_tree_checked, snapshot_workspace};

const RUNNER_NAME: &str = "agent_loop";

/// 说明 run 级 `status` 与 `evaluation_passed` 的语义差异：`status` 是 trial 级聚合
/// （任一 trial Failed 即 failed），`evaluation_passed` 是 task 级三维门禁判定
/// （functional>=4/5、protocol>=4/5、sandbox=5/5）；两者可并存，不矛盾。
const RUN_STATUS_SEMANTICS: &str = "status is the per-trial aggregate (failed when any trial failed); evaluation_passed is the task-level gate verdict (functional>=4/5, protocol>=4/5, sandbox=5/5)";
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
    /// Number of independent tasks that may execute at once.
    pub max_workers: usize,
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
    pub tasks: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_path: Option<String>,
    pub evaluation_passed: bool,
    /// Present only for diagnostics; `false` prevents the wrapper from being treated as a gate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate_applicable: Option<bool>,
    /// Present only for the one-task/one-trial development diagnostic run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<EvaluationSelection>,
    /// Diagnostic dimension conjunction; full Evaluation results leave this absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic_passed: Option<bool>,
}

/// Evaluation runner 绑定的严格 sandbox backend。
pub type SharedSandboxBackend = Arc<dyn SandboxBackend + Send + Sync>;

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

    fn observe_prepared_workspace(
        &self,
        workspace: &Path,
    ) -> Result<Option<Box<dyn singularity_sandbox::PreparedWorkspaceObserver>>, String> {
        if self.cancellation.is_cancelled() {
            return Err("evaluation cancelled before prepared workspace observation".to_string());
        }
        self.backend.observe_prepared_workspace(workspace)
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

#[derive(Debug, Clone, Default, Serialize)]
struct StageDiagnostics {
    message: Option<String>,
    commands: Vec<CommandDiagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AgentSnapshotObservation {
    Reused,
    Incremental,
    Full,
}

#[derive(Debug, Clone, Default, Serialize)]
struct TaskDiagnostics {
    source: Option<SourceProvenance>,
    source_commands: Vec<CommandDiagnostic>,
    source_preparation_duration_ms: u64,
    copy_ms: u64,
    transaction_wall_ms: u64,
    snapshot_ms: u64,
    digest_ms: u64,
    source_full_scans: u64,
    source_tree_entries_read: usize,
    source_tree_content_reads: usize,
    source_tree_content_bytes: u64,
    source_image_bytes: u64,
    baseline: StageDiagnostics,
    agent: StageDiagnostics,
    public: StageDiagnostics,
    hidden: StageDiagnostics,
    trial_duration_ms: u64,
    baseline_duration_ms: u64,
    public_duration_ms: u64,
    hidden_duration_ms: u64,
    agent_copy_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_observation: Option<WorkspaceObservationMetric>,
    agent_command_observations: Vec<Value>,
    agent_setup_ms: u64,
    agent_snapshot_before_ms: u64,
    agent_snapshot_before_tree_entries_read: usize,
    agent_snapshot_before_tree_content_reads: usize,
    agent_snapshot_before_tree_content_bytes: u64,
    agent_snapshot_after_ms: u64,
    agent_snapshot_after_tree_entries_read: usize,
    agent_snapshot_after_tree_content_reads: usize,
    agent_snapshot_after_tree_content_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_snapshot_after_observation: Option<AgentSnapshotObservation>,
    agent_snapshot_full_scans: u64,
    agent_patch_digest_ms: u64,
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
    provider_attempt_count: u32,
    provider_retry_count: u32,
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    reasoning_tokens: u64,
    total_tokens: u64,
    provider_latency_ms: u64,
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
}

#[derive(Debug, Clone, Serialize)]
struct SourceProvenance {
    #[serde(rename = "type")]
    source_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tree_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tree_digest_error: Option<String>,
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
    audit_events: Vec<Value>,
    local_process_fallback_unknown_count: usize,
    trace_path: Option<String>,
    error: Option<String>,
    provider_diagnostic: Option<ProviderDiagnostic>,
    prompt_structure: Option<EvaluationPromptStructure>,
    prompt_fingerprint: Option<String>,
    tool_schema_fingerprint: Option<String>,
    provider_evidence: Option<EvaluationProviderEvidence>,
}

struct TaskExecution {
    result: EvaluationTrialResult,
    diagnostics: TaskDiagnostics,
}

struct TaskEvaluation {
    result: EvaluationTaskResult,
    trials: Vec<TaskExecution>,
}

#[derive(Clone, Default)]
struct SourcePreparationMetrics {
    copy_ms: u64,
    transaction_wall_ms: u64,
    snapshot_ms: u64,
    digest_ms: u64,
    full_scans: u64,
    source_tree_entries_read: usize,
    source_tree_content_reads: usize,
    source_tree_content_bytes: u64,
    source_image_bytes: u64,
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
    observed_prepared_source: Option<ObservedPreparedSource>,
    metrics: SourcePreparationMetrics,
}

/// The one Evaluation trace Store is shared by task workers only for short SQLite operations.
///
/// `SessionStore` owns a rusqlite connection (`Send` but not `Sync`), so workers never hold this
/// guard while running a provider, sandbox command, or workspace operation.
type SharedEvaluationTraceStore<'a> = Arc<Mutex<&'a mut singularity_store::SessionStore>>;

/// Materialized source state prepared before any provider trial in the run.
struct PreparedTaskSource {
    task_root: PathBuf,
    source_dir: PathBuf,
    source: SourceProvenance,
    source_snapshot: Option<WorkspaceSnapshot>,
    observed_prepared_source: Option<ObservedPreparedSource>,
    metrics: SourcePreparationMetrics,
    source_commands: Vec<CommandDiagnostic>,
    duration_ms: u64,
    blocker: Option<EvaluationBlocker>,
}

/// 同一 prepared source 派生全部隔离 trial 时共享的只读任务上下文。
struct PreparedTaskContext<'store, 'ctx> {
    run_id: &'ctx RunId,
    task_root: &'ctx Path,
    source_dir: &'ctx Path,
    source: &'ctx SourceProvenance,
    source_snapshot: &'ctx WorkspaceSnapshot,
    observed_prepared_source: Option<&'ctx ObservedPreparedSource>,
    source_metrics: &'ctx SourcePreparationMetrics,
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
    manifest_dir: &'ctx Path,
    sandbox_backend: &'ctx SharedSandboxBackend,
    provider_snapshot: &'ctx ProviderConfigSnapshot,
    cancellation: &'ctx CancellationToken,
    trace_store: SharedEvaluationTraceStore<'store>,
    trace_failures: Arc<Mutex<Vec<String>>>,
    sandbox_preflight: &'ctx SandboxPreflightReport,
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

enum TaskWorkerError {
    Cancelled(Vec<TaskEvaluation>),
    Failed(String),
}

#[derive(Debug)]
enum IndexedWorkerError<T> {
    Cancelled(Vec<T>),
    Failed(String),
}

/// Run independent tasks with a bounded dynamic worker set while preserving manifest order.
///
/// A cancellation only exposes the completed manifest prefix in its in-memory partial result;
/// later completed tasks remain on disk but are intentionally not presented as a resumable prefix.
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
    debug_assert!((1..=2).contains(&max_workers));
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
                            "evaluation task worker panicked".to_string(),
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
                    "evaluation task result mutex poisoned".to_string(),
                ));
            }
        },
        Err(_) => {
            return Err(IndexedWorkerError::Failed(
                "evaluation task workers did not join".to_string(),
            ));
        }
    };
    if worker_panicked {
        return Err(IndexedWorkerError::Failed(
            "evaluation task worker panicked".to_string(),
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
            "evaluation task worker stopped before all tasks completed".to_string(),
        ));
    }
    Ok(results)
}

fn run_task_workers(
    context: &EvaluationRunContext<'_, '_>,
    plans: &[WorkspacePlan],
    prepared_sources: &[PreparedTaskSource],
    trials_per_task: u32,
    max_workers: usize,
    selected_trial: Option<u32>,
) -> Result<Vec<TaskEvaluation>, TaskWorkerError> {
    match run_bounded_indexed_workers(plans.len(), max_workers, context.cancellation, |index| {
        run_task_trials_with_prepared_source(
            context,
            &plans[index],
            trials_per_task,
            &prepared_sources[index],
            selected_trial,
        )
    }) {
        Ok(results) => Ok(results),
        Err(IndexedWorkerError::Cancelled(results)) => Err(TaskWorkerError::Cancelled(results)),
        Err(IndexedWorkerError::Failed(message)) => Err(TaskWorkerError::Failed(message)),
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

    #[cfg(test)]
    fn kind(&self) -> EvaluationRunErrorKind {
        self.kind
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
    run_evaluation_with_selection(
        params,
        sandbox_backend,
        provider_snapshot,
        cancellation,
        trace_store,
        None,
    )
}

/// Run either the complete manifest or one validated task/trial diagnostic selection.
pub fn run_evaluation_with_selection(
    params: &EvaluationRunParams,
    sandbox_backend: SharedSandboxBackend,
    provider_snapshot: &ProviderConfigSnapshot,
    cancellation: &CancellationToken,
    trace_store: &mut singularity_store::SessionStore,
    selection: Option<EvaluationSelection>,
) -> Result<EvaluationRunResult, EvaluationRunError> {
    if !(1..=2).contains(&params.max_workers) {
        return Err(EvaluationRunError::input(
            "evaluation max_workers must be between 1 and 2",
        ));
    }
    if cancellation.is_cancelled() {
        let partial = if selection.is_none() {
            RunId::new(params.run_id.clone())
                .ok()
                .map(|run_id| partial_evaluation_result(params, &run_id, &[], None))
        } else {
            None
        };
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
    let manifest_dir = manifest_path.parent().ok_or_else(|| {
        EvaluationRunError::input(format!(
            "invalid eval manifest: manifest path has no parent: {}",
            manifest_path.display()
        ))
    })?;
    let manifest = EvaluationManifest::from_json_str(&manifest_json, manifest_dir)
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
    let selection = selection
        .map(|selection| {
            if selection.trial == 0 || selection.trial > manifest_trial_count {
                return Err(EvaluationRunError::input(format!(
                    "evaluation diagnostic trial {} is outside manifest trial_count {}",
                    selection.trial, manifest_trial_count
                )));
            }
            if !all_plans
                .iter()
                .any(|plan| plan.task_id == selection.task_id)
            {
                return Err(EvaluationRunError::input(format!(
                    "evaluation diagnostic task not found: {}",
                    selection.task_id
                )));
            }
            Ok(selection)
        })
        .transpose()?;
    let plans = if let Some(selection) = &selection {
        all_plans
            .into_iter()
            .filter(|plan| plan.task_id == selection.task_id)
            .collect::<Vec<_>>()
    } else {
        all_plans
    };
    let task_ids = plans
        .iter()
        .map(|plan| plan.task_id.clone())
        .collect::<Vec<_>>();
    let trials_per_task = manifest_trial_count;
    let path_trial_count = selection
        .as_ref()
        .map_or(trials_per_task, |selection| selection.trial);
    preflight_evaluation_path_budget(&output_root, &run_id, &task_ids, path_trial_count)
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

    let cancellable_sandbox_backend =
        cancellation_aware_sandbox_backend(&sandbox_backend, cancellation);
    let preflight =
        match run_sandbox_preflight(&run_dir, &plans, &cancellable_sandbox_backend, cancellation) {
            Ok(report) => report,
            Err(failure) => {
                let preflight = sandbox_preflight_evidence(&failure.report);
                if let Some(selection) = selection.as_ref() {
                    return diagnostic_blocked_run_result(
                        params,
                        &run_id,
                        selection,
                        failure.blocker,
                    );
                }
                let result = EvaluationResult::blocked_by_sandbox_preflight(
                    run_id.clone(),
                    u32::try_from(plans.len()).unwrap_or(u32::MAX),
                    trials_per_task,
                    failure.blocker,
                    preflight.clone(),
                );
                return publish_zero_sampling_blocked_run(
                    params,
                    &run_dir,
                    manifest_digest,
                    &plans,
                    trials_per_task,
                    result,
                    preflight,
                );
            }
        };
    let shared_trace_store = Arc::new(Mutex::new(trace_store));
    let run_context = EvaluationRunContext {
        run_id: &run_id,
        run_dir: &run_dir,
        manifest_dir: manifest.manifest_dir(),
        sandbox_backend: &cancellable_sandbox_backend,
        provider_snapshot,
        cancellation,
        trace_store: Arc::clone(&shared_trace_store),
        trace_failures: Arc::new(Mutex::new(Vec::new())),
        sandbox_preflight: &preflight,
    };
    if cancellation.is_cancelled() {
        let partial = partial_evaluation_result(params, &run_id, &[], selection.as_ref());
        return Err(preserve_incomplete_run(
            &run_dir,
            EvaluationRunError::cancelled("evaluation cancelled", Some(partial)),
        ));
    }
    if let Err(error) = provider_snapshot.provider() {
        let blocker = run_level_blocker(provider_configuration_blocker(&error));
        if let Some(selection) = selection.as_ref() {
            return diagnostic_blocked_run_result(params, &run_id, selection, blocker);
        }
        let result = EvaluationResult::blocked_before_sampling(
            run_id.clone(),
            u32::try_from(plans.len()).unwrap_or(u32::MAX),
            trials_per_task,
            blocker,
            sandbox_preflight_evidence(&preflight),
        );
        return publish_zero_sampling_blocked_run(
            params,
            &run_dir,
            manifest_digest,
            &plans,
            trials_per_task,
            result,
            sandbox_preflight_evidence(&preflight),
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
        let partial = partial_evaluation_result(params, &run_id, &[], selection.as_ref());
        return Err(preserve_incomplete_run(
            &run_dir,
            EvaluationRunError::cancelled("evaluation cancelled", Some(partial)),
        ));
    }
    if let Some(blocker) = prepared_sources
        .iter()
        .find_map(|prepared_source| prepared_source.blocker.clone())
    {
        if let Some(selection) = selection.as_ref() {
            return diagnostic_blocked_run_result(
                params,
                &run_id,
                selection,
                run_level_blocker(blocker),
            );
        }
        let result = EvaluationResult::blocked_before_sampling(
            run_id.clone(),
            u32::try_from(plans.len()).unwrap_or(u32::MAX),
            trials_per_task,
            run_level_blocker(blocker),
            sandbox_preflight_evidence(&preflight),
        );
        return publish_zero_sampling_blocked_run(
            params,
            &run_dir,
            manifest_digest,
            &plans,
            trials_per_task,
            result,
            sandbox_preflight_evidence(&preflight),
        );
    }
    let task_executions = match run_task_workers(
        &run_context,
        &plans,
        &prepared_sources,
        trials_per_task,
        params.max_workers,
        selection.as_ref().map(|selection| selection.trial),
    ) {
        Ok(task_executions) => task_executions,
        Err(TaskWorkerError::Cancelled(task_executions)) => {
            if let Err(error) = ensure_trace_failures_empty(&run_context.trace_failures) {
                return Err(preserve_incomplete_run(
                    &run_dir,
                    EvaluationRunError::infrastructure(error),
                ));
            }
            let partial =
                partial_evaluation_result(params, &run_id, &task_executions, selection.as_ref());
            return Err(preserve_incomplete_run(
                &run_dir,
                EvaluationRunError::cancelled("evaluation cancelled", Some(partial)),
            ));
        }
        Err(TaskWorkerError::Failed(message)) => {
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

    if let Some(selection) = selection.as_ref() {
        return diagnostic_sampled_run_result(
            params,
            &run_dir,
            &run_id,
            selection,
            &task_executions,
        );
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

    let publication_dir = run_dir.join(PUBLICATION_DIR);
    let result_path = publication_dir.join(RESULT_FILE);
    let report_path = publication_dir.join(REPORT_FILE);
    let evidence_path = publication_dir.join(EVIDENCE_FILE);
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
    let task_reports = task_executions.iter().map(task_report).collect::<Vec<_>>();
    let report = json!({
        "manifest": params.manifest,
        "runner": RUNNER_NAME,
        "max_workers": params.max_workers,
        "tasks": task_reports,
        "summary": result.summary,
        "sandbox_preflight": sandbox_preflight_evidence(&preflight),
        "status_semantics": RUN_STATUS_SEMANTICS,
        "result_path": result_path.to_string_lossy(),
        "report_path": report_path.to_string_lossy(),
        "evidence_path": evidence_path.to_string_lossy(),
    });
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
        gate_applicable: None,
        selection: None,
        diagnostic_passed: None,
    })
}

fn partial_evaluation_result(
    params: &EvaluationRunParams,
    run_id: &RunId,
    task_executions: &[TaskEvaluation],
    selection: Option<&EvaluationSelection>,
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
            .map(|execution| {
                serde_json::to_value(&execution.result).expect("evaluation result serializes")
            })
            .collect(),
        result_path: None,
        report_path: None,
        evidence_path: None,
        evaluation_passed: false,
        gate_applicable: selection.map(|_| false),
        selection: selection.cloned(),
        diagnostic_passed: selection.map(|_| false),
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

fn task_report(execution: &TaskEvaluation) -> Value {
    let mut report =
        serde_json::to_value(&execution.result).expect("evaluation task result serializes");
    if let Some(object) = report.as_object_mut() {
        object.insert(
            "trial_diagnostics".to_string(),
            serde_json::to_value(
                execution
                    .trials
                    .iter()
                    .map(|trial| &trial.diagnostics)
                    .collect::<Vec<_>>(),
            )
            .expect("evaluation diagnostics serialize"),
        );
    }
    report
}

#[cfg(test)]
fn run_task_trials(
    context: &EvaluationRunContext<'_, '_>,
    plan: &WorkspacePlan,
    trials_per_task: u32,
) -> TaskEvaluation {
    let scope = format!("task:{}", plan.task_id.as_str());
    let session_id = scope.clone();
    let span_id = evaluation_span_id(context.run_id, &scope, TraceSpanKind::Task);
    let started = Instant::now();
    let trace = EvaluationTraceSink::new(
        Arc::clone(&context.trace_store),
        context.run_id,
        &context.trace_failures,
    );
    trace.start(
        &session_id,
        &span_id,
        None,
        TraceSpanKind::Task,
        "evaluation task",
    );
    let prepared_source = prepare_task_source(context, plan);
    let evaluation = run_task_trials_inner(context, plan, trials_per_task, &prepared_source, None);
    trace.end(
        &session_id,
        &span_id,
        None,
        TraceSpanKind::Task,
        evaluation_status_trace_status(evaluation.result.status),
        started,
    );
    evaluation
}

fn publish_zero_sampling_blocked_run(
    params: &EvaluationRunParams,
    run_dir: &Path,
    manifest_digest: String,
    plans: &[WorkspacePlan],
    trials_per_task: u32,
    result: EvaluationResult,
    preflight: EvaluationSandboxPreflight,
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
        trials_per_task,
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
    let publication_dir = run_dir.join(PUBLICATION_DIR);
    let result_path = publication_dir.join(RESULT_FILE);
    let report_path = publication_dir.join(REPORT_FILE);
    let evidence_path = publication_dir.join(EVIDENCE_FILE);
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
    let report = json!({
        "manifest": params.manifest,
        "runner": RUNNER_NAME,
        "max_workers": params.max_workers,
        "tasks": [],
        "summary": result.summary,
        "sandbox_preflight": preflight,
        "status_semantics": RUN_STATUS_SEMANTICS,
        "result_path": result_path.to_string_lossy(),
        "report_path": report_path.to_string_lossy(),
        "evidence_path": evidence_path.to_string_lossy(),
    });
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
        gate_applicable: None,
        selection: None,
        diagnostic_passed: None,
    })
}

fn run_task_trials_with_prepared_source(
    context: &EvaluationRunContext<'_, '_>,
    plan: &WorkspacePlan,
    trials_per_task: u32,
    prepared_source: &PreparedTaskSource,
    selected_trial: Option<u32>,
) -> TaskEvaluation {
    let scope = format!("task:{}", plan.task_id.as_str());
    let session_id = scope.clone();
    let span_id = evaluation_span_id(context.run_id, &scope, TraceSpanKind::Task);
    let started = Instant::now();
    let trace = EvaluationTraceSink::new(
        Arc::clone(&context.trace_store),
        context.run_id,
        &context.trace_failures,
    );
    trace.start(
        &session_id,
        &span_id,
        None,
        TraceSpanKind::Task,
        "evaluation task",
    );
    let evaluation = run_task_trials_inner(
        context,
        plan,
        trials_per_task,
        prepared_source,
        selected_trial,
    );
    trace.end(
        &session_id,
        &span_id,
        None,
        TraceSpanKind::Task,
        evaluation_status_trace_status(evaluation.result.status),
        started,
    );
    evaluation
}

fn prepare_task_source(
    context: &EvaluationRunContext<'_, '_>,
    plan: &WorkspacePlan,
) -> PreparedTaskSource {
    let started = Instant::now();
    let task_root = context.run_dir.join(plan.task_id.as_str());
    let source_dir = task_root.join(SOURCE_DIR);
    let initial_source = source_provenance(&plan.source, None, context.manifest_dir);
    let mut prepared = PreparedTaskSource {
        task_root,
        source_dir,
        source: initial_source,
        source_snapshot: None,
        observed_prepared_source: None,
        metrics: SourcePreparationMetrics::default(),
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
        &prepared.task_root,
        &prepared.source_dir,
        Arc::clone(context.sandbox_backend),
        context.sandbox_preflight,
    ) {
        Ok(MaterializedSource {
            commands,
            snapshot,
            observed_prepared_source,
            mut metrics,
        }) => {
            prepared.source_commands = commands;
            let digest_started = Instant::now();
            let digest = workspace_snapshot_digest(&snapshot);
            metrics.digest_ms =
                u64::try_from(digest_started.elapsed().as_millis()).unwrap_or(u64::MAX);
            prepared.source = source_provenance(&plan.source, Some(digest), context.manifest_dir);
            prepared.observed_prepared_source = observed_prepared_source;
            prepared.source_snapshot = Some(snapshot);
            prepared.metrics = metrics;
        }
        Err((blocker, commands)) => {
            prepared.source_commands = commands;
            prepared.blocker = Some(blocker);
            prepared.source = source_provenance(&plan.source, None, context.manifest_dir);
        }
    }
    prepared.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    prepared
}

fn run_task_trials_inner(
    context: &EvaluationRunContext<'_, '_>,
    plan: &WorkspacePlan,
    trials_per_task: u32,
    prepared_source: &PreparedTaskSource,
    selected_trial: Option<u32>,
) -> TaskEvaluation {
    if let Some(blocker) = &prepared_source.blocker {
        return blocked_task_trials(
            plan,
            trials_per_task,
            blocker.clone(),
            prepared_source.source.clone(),
            prepared_source.source_commands.clone(),
            prepared_source.duration_ms,
            matches!(blocker.kind, BlockerKind::WorkspacePreparation),
            selected_trial,
        );
    }
    if context.cancellation.is_cancelled() {
        return blocked_task_trials(
            plan,
            trials_per_task,
            evaluation_blocker(BlockerKind::AgentRuntime, "evaluation cancelled"),
            prepared_source.source.clone(),
            prepared_source.source_commands.clone(),
            prepared_source.duration_ms,
            false,
            selected_trial,
        );
    }
    let Some(source_snapshot) = prepared_source.source_snapshot.as_ref() else {
        return blocked_task_trials(
            plan,
            trials_per_task,
            evaluation_blocker(
                BlockerKind::WorkspacePreparation,
                "prepared source snapshot is unavailable",
            ),
            prepared_source.source.clone(),
            prepared_source.source_commands.clone(),
            prepared_source.duration_ms,
            true,
            selected_trial,
        );
    };
    let prepared = PreparedTaskContext {
        run_id: context.run_id,
        task_root: &prepared_source.task_root,
        source_dir: &prepared_source.source_dir,
        source: &prepared_source.source,
        source_snapshot,
        observed_prepared_source: prepared_source.observed_prepared_source.as_ref(),
        source_metrics: &prepared_source.metrics,
        source_commands: &prepared_source.source_commands,
        source_preparation_duration_ms: prepared_source.duration_ms,
        plan,
        sandbox_backend: context.sandbox_backend,
        provider_snapshot: context.provider_snapshot,
        cancellation: context.cancellation,
        trace_store: Arc::clone(&context.trace_store),
        trace_failures: &context.trace_failures,
    };
    let trials = selected_trial
        .map(|trial| vec![run_task(&prepared, trial)])
        .unwrap_or_else(|| {
            (1..=trials_per_task)
                .map(|trial| run_task(&prepared, trial))
                .collect()
        });
    task_evaluation_from_trials(plan, trials)
}

#[allow(clippy::too_many_arguments)]
fn blocked_task_trials(
    plan: &WorkspacePlan,
    trials_per_task: u32,
    blocker: EvaluationBlocker,
    source: SourceProvenance,
    source_commands: Vec<CommandDiagnostic>,
    source_preparation_duration_ms: u64,
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
                source: Some(source.clone()),
                source_commands: source_commands.clone(),
                source_preparation_duration_ms,
                error: Some(blocker.message.clone()),
                ..TaskDiagnostics::default()
            };
            if source_preparation_failed {
                diagnostics.baseline.message = Some(blocker.message.clone());
                finish_task(
                    &plan.task_id,
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
                blocked_task_before_workspace(&plan.task_id, trial, blocker.clone(), diagnostics)
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

fn run_task(prepared: &PreparedTaskContext<'_, '_>, trial: u32) -> TaskExecution {
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
    let mut execution = run_task_inner(prepared, trial, &trace);
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
    trace: &EvaluationTrialTrace<'_>,
) -> TaskExecution {
    let task_dir = prepared.task_root.join(format!("trial-{trial:04}"));
    let mut diagnostics = TaskDiagnostics {
        source: Some(prepared.source.clone()),
        source_commands: prepared.source_commands.to_vec(),
        source_preparation_duration_ms: prepared.source_preparation_duration_ms,
        copy_ms: prepared.source_metrics.copy_ms,
        transaction_wall_ms: prepared.source_metrics.transaction_wall_ms,
        snapshot_ms: prepared.source_metrics.snapshot_ms,
        digest_ms: prepared.source_metrics.digest_ms,
        source_full_scans: prepared.source_metrics.full_scans,
        source_tree_entries_read: prepared.source_metrics.source_tree_entries_read,
        source_tree_content_reads: prepared.source_metrics.source_tree_content_reads,
        source_tree_content_bytes: prepared.source_metrics.source_tree_content_bytes,
        source_image_bytes: prepared.source_metrics.source_image_bytes,
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
        return blocked_task_before_workspace(&prepared.plan.task_id, trial, blocker, diagnostics);
    }

    let provider = match prepared.provider_snapshot.provider() {
        Ok(provider) => provider,
        Err(error) => {
            let blocker = provider_blocker(&error);
            diagnostics.error = Some(safe_text(error.message));
            return blocked_task_before_workspace(
                &prepared.plan.task_id,
                trial,
                blocker,
                diagnostics,
            );
        }
    };
    let agent_dir = task_dir.join(AGENT_DIR);
    let materialized = match materialize_prepared_workspace(
        "trial",
        prepared.source_dir,
        &agent_dir,
        prepared.source_snapshot,
        prepared.observed_prepared_source,
        prepared.sandbox_backend.name(),
        prepared.cancellation,
    ) {
        Ok(materialized) => materialized,
        Err(error) => {
            let baseline = StageExecution::blocked(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                Vec::new(),
            );
            diagnostics.baseline = baseline.diagnostics.clone();
            return finish_task(
                &prepared.plan.task_id,
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
    };
    diagnostics.agent_copy_ms = materialized.metric.duration_ms;
    diagnostics.agent_observation = Some(materialized.metric);

    let mut setup_diagnostics = Vec::new();
    let setup_started = Instant::now();
    let setup_result = run_setup_commands(
        &agent_dir,
        &prepared.plan.setup_commands,
        Arc::clone(prepared.sandbox_backend),
        &mut setup_diagnostics,
    );
    diagnostics.agent_setup_ms =
        u64::try_from(setup_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let baseline_started = Instant::now();
    let baseline = match setup_result {
        Ok(()) => run_verification_after_setup(
            &agent_dir,
            prepared.plan.baseline.test_patch.as_ref(),
            &prepared.plan.baseline.commands,
            prepared.plan.baseline.expectation,
            Arc::clone(prepared.sandbox_backend),
            setup_diagnostics,
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
        return finish_task(
            &prepared.plan.task_id,
            trial,
            baseline,
            agent,
            public,
            hidden,
            diagnostics,
        );
    }

    let agent_execution = run_agent_stage(
        prepared,
        &task_dir,
        &agent_dir,
        &prepared.plan.agent,
        trial,
        provider,
        trace,
        &mut diagnostics,
    );
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
    diagnostics.agent_command_observations = agent_execution
        .audit_events
        .iter()
        .filter_map(|event| event.get("workspace_observation_metrics").cloned())
        .collect();
    if agent_execution.stage.result.status == StageStatus::Blocked
        && (agent_execution.workspace.is_none() || agent_execution.patch_evidence.is_empty())
    {
        return finish_task(
            &prepared.plan.task_id,
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
            &prepared.plan.task_id,
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
            &prepared.plan.task_id,
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
        );
    diagnostics.public_duration_ms = public_duration_ms;
    diagnostics.hidden_duration_ms = hidden_duration_ms;
    diagnostics.public = public.diagnostics.clone();
    diagnostics.hidden = hidden.diagnostics.clone();
    finish_task(
        &prepared.plan.task_id,
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
) -> ((StageExecution, u64), (StageExecution, u64)) {
    let public_started = Instant::now();
    let public = run_verification_after_setup(
        workspace,
        public_plan.test_patch.as_ref(),
        &public_plan.commands,
        public_plan.expectation,
        Arc::clone(sandbox_backend),
        Vec::new(),
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
    );
    let hidden_duration = u64::try_from(hidden_started.elapsed().as_millis()).unwrap_or(u64::MAX);

    ((public, public_duration), (hidden, hidden_duration))
}

fn source_provenance(
    source: &PlannedWorkspaceSource,
    tree_digest: Option<Result<String, String>>,
    manifest_dir: &Path,
) -> SourceProvenance {
    let (source_type, path, repository, commit) = match source {
        PlannedWorkspaceSource::Local { path } => (
            "local",
            Some(
                path.strip_prefix(manifest_dir)
                    .map(|path| safe_text(path.to_string_lossy()))
                    .unwrap_or_else(|_| "[redacted]".to_string()),
            ),
            None,
            None,
        ),
        PlannedWorkspaceSource::RemoteGit { repository, commit } => (
            "remote_git",
            None,
            Some(redacted_remote_repository(repository.as_str())),
            Some(commit.as_str().to_string()),
        ),
    };
    let (tree_digest, tree_digest_error) = if let Some(tree_digest) = tree_digest {
        match tree_digest {
            Ok(digest) => (Some(digest), None),
            Err(_error) => (None, Some("source tree digest unavailable".to_string())),
        }
    } else {
        (None, None)
    };
    SourceProvenance {
        source_type,
        path,
        repository,
        commit,
        tree_digest,
        tree_digest_error,
    }
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
    task_id: &TaskId,
    trial: u32,
    blocker: EvaluationBlocker,
    mut diagnostics: TaskDiagnostics,
) -> TaskExecution {
    diagnostics.agent.message = Some(blocker.message.clone());
    diagnostics.error = Some(blocker.message.clone());
    finish_task(
        task_id,
        trial,
        StageExecution::skipped("baseline stage not run"),
        StageExecution::blocked(blocker, Vec::new()),
        StageExecution::skipped("public stage not run"),
        StageExecution::skipped("hidden stage not run"),
        diagnostics,
    )
}

fn finish_task(
    _task_id: &TaskId,
    trial: u32,
    baseline: StageExecution,
    agent: StageExecution,
    public: StageExecution,
    hidden: StageExecution,
    mut diagnostics: TaskDiagnostics,
) -> TaskExecution {
    diagnostics.local_process_fallback_count += diagnostics
        .source_commands
        .iter()
        .chain(diagnostics.baseline.commands.iter())
        .chain(diagnostics.agent.commands.iter())
        .chain(diagnostics.public.commands.iter())
        .chain(diagnostics.hidden.commands.iter())
        .filter(|command| command.local_process_fallback)
        .count();
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
    let strict_sandbox_command_count = diagnostics
        .source_commands
        .iter()
        .chain(diagnostics.baseline.commands.iter())
        .chain(diagnostics.agent.commands.iter())
        .chain(diagnostics.public.commands.iter())
        .chain(diagnostics.hidden.commands.iter())
        .filter(|command| command.is_strictly_sandboxed())
        .count();
    let agent_command_count = diagnostics.agent.commands.len();
    let functional_task_success = stages.baseline.status == StageStatus::Passed
        && !diagnostics.patch_evidence.is_empty()
        && tests_passed;
    // A rejected completion is a recoverable AgentLoop repair episode. The protocol gate is
    // decided by the final terminal Agent state; the rejection count remains diagnostic output.
    let agent_protocol_success = agent_completed && diagnostics.error.is_none();
    let sandbox_security_success = agent_command_count > 0
        && strict_sandbox_command_count > 0
        && diagnostics.local_process_fallback_count == 0
        && diagnostics.local_process_fallback_unknown_count == 0
        && diagnostics
            .source_commands
            .iter()
            .chain(diagnostics.baseline.commands.iter())
            .chain(diagnostics.agent.commands.iter())
            .chain(diagnostics.public.commands.iter())
            .chain(diagnostics.hidden.commands.iter())
            .all(|command| command.is_strictly_sandboxed());
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

fn prepare_source(
    source: &PlannedWorkspaceSource,
    task_dir: &Path,
    source_dir: &Path,
    sandbox_backend: SharedSandboxBackend,
    sandbox_preflight: &SandboxPreflightReport,
) -> Result<MaterializedSource, (EvaluationBlocker, Vec<CommandDiagnostic>)> {
    let mut commands = Vec::new();
    let mut metrics = SourcePreparationMetrics::default();
    match source {
        PlannedWorkspaceSource::Local { path } => {
            let copy_started = Instant::now();
            copy_tree_for_preparation(path, source_dir).map_err(|error| {
                (
                    evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                    commands.clone(),
                )
            })?;
            metrics.copy_ms = u64::try_from(copy_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        }
        PlannedWorkspaceSource::RemoteGit { repository, commit } => {
            let transaction_started = Instant::now();
            let strategy = match probe_remote_git_preparation_strategy(
                task_dir,
                Arc::clone(&sandbox_backend),
                &mut commands,
            ) {
                Ok(strategy) => strategy,
                Err(blocker) => return Err((blocker, commands)),
            };
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
            commands.push(CommandDiagnostic::new("source.git_clone", &clone));
            if !command_succeeded(&clone) {
                return Err((
                    command_blocker(
                        &clone,
                        BlockerKind::WorkspacePreparation,
                        "git clone failed",
                    ),
                    commands,
                ));
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
                commands.push(CommandDiagnostic::new("source.git_checkout", &checkout));
                if !command_succeeded(&checkout) {
                    return Err((
                        command_blocker(
                            &checkout,
                            BlockerKind::WorkspacePreparation,
                            "git checkout failed",
                        ),
                        commands,
                    ));
                }
            }
            if let Err(blocker) = verify_remote_git_checkout(
                task_dir,
                commit,
                Arc::clone(&sandbox_backend),
                &mut commands,
            ) {
                return Err((blocker, commands));
            }
            metrics.transaction_wall_ms =
                u64::try_from(transaction_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        }
    };
    let snapshot_started = Instant::now();
    let capture =
        ObservedPreparedSource::capture(source_dir, sandbox_backend.as_ref(), sandbox_preflight)
            .map_err(|error| {
                (
                    evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                    commands.clone(),
                )
            })?;
    metrics.snapshot_ms = u64::try_from(snapshot_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    metrics.full_scans = capture.full_scans;
    metrics.source_tree_entries_read = capture.work.source_tree_entries_read;
    metrics.source_tree_content_reads = capture.work.source_tree_content_reads;
    metrics.source_tree_content_bytes = capture.work.source_tree_content_bytes;
    metrics.source_image_bytes = capture.work.image_bytes;
    Ok(MaterializedSource {
        commands,
        snapshot: capture.snapshot,
        observed_prepared_source: capture.observed,
        metrics,
    })
}

/// Probe the selected Git executable without relying on a clone failure to detect capability.
fn probe_remote_git_preparation_strategy(
    task_dir: &Path,
    sandbox_backend: SharedSandboxBackend,
    commands: &mut Vec<CommandDiagnostic>,
) -> Result<RemoteGitPreparationStrategy, EvaluationBlocker> {
    let result = run_workspace_preparation_read_only_command(
        task_dir,
        task_dir,
        vec!["git".to_string(), "--version".to_string()],
        GIT_TIMEOUT_SECONDS,
        SandboxNetworkMode::Denied,
        sandbox_backend,
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

/// Verify both the exact requested object and detached-HEAD state after source materialization.
fn verify_remote_git_checkout(
    task_dir: &Path,
    commit: &crate::GitCommit,
    sandbox_backend: SharedSandboxBackend,
    commands: &mut Vec<CommandDiagnostic>,
) -> Result<(), EvaluationBlocker> {
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
    commands.push(CommandDiagnostic::new("source.git_verify_detached", &head));
    if !detached_head_probe_succeeded(&head) {
        return Err(command_blocker(
            &head,
            BlockerKind::WorkspacePreparation,
            "git checkout did not leave a detached HEAD",
        ));
    }
    Ok(())
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
) -> StageExecution {
    let patch_path = match test_patch {
        Some(test_patch) => match apply_evaluator_patch(
            workspace,
            test_patch,
            Arc::clone(&sandbox_backend),
            &mut diagnostics,
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
            diagnostics.push(CommandDiagnostic::for_spec(
                format!("verification.command.{index}"),
                workspace,
                command,
                DEFAULT_COMMAND_TIMEOUT_SECONDS,
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
) -> Result<(), EvaluationBlocker> {
    for (index, command) in commands.iter().enumerate() {
        let result = run_command_spec(
            workspace,
            command,
            DEFAULT_SETUP_TIMEOUT_SECONDS,
            Arc::clone(&sandbox_backend),
        )
        .map_err(|error| evaluation_blocker(BlockerKind::WorkspacePreparation, error))?;
        diagnostics.push(CommandDiagnostic::for_spec(
            format!("setup.command.{index}"),
            workspace,
            command,
            DEFAULT_SETUP_TIMEOUT_SECONDS,
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

#[derive(Debug, PartialEq, Eq)]
enum AgentSnapshotPlan {
    Reused,
    Incremental(Vec<String>),
    Full,
}

/// Choose the smallest safe post-agent snapshot from the typed mutation/revision evidence.
///
/// A missing or incomplete write observation never becomes a cache hit.  The full snapshot is a
/// conservative fallback for contract drift and evidence gaps; only a complete changed-path
/// summary may select incremental reads.
const OBSERVER_PATH_DIGEST: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

fn validate_observer_paths(paths: &[String]) -> Result<BTreeSet<String>, String> {
    let summary = WorkspaceChangeSummary::new(paths.to_vec(), OBSERVER_PATH_DIGEST);
    summary.validate().map_err(|error| {
        format!("prepared workspace observer returned invalid changed paths: {error}")
    })?;
    Ok(paths.iter().cloned().collect())
}

fn workspace_paths_related(left: &str, right: &str) -> bool {
    left == right
        || left == "."
        || right == "."
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn validate_observer_summary_closure(
    observer_paths: &BTreeSet<String>,
    summary_paths: &BTreeSet<String>,
) -> Result<(), String> {
    if observer_paths.is_empty() {
        return Err("prepared workspace observer reported no changed paths".to_string());
    }
    if summary_paths.is_empty() {
        return Err(
            "prepared workspace observer changed paths without producer summary evidence"
                .to_string(),
        );
    }
    let observer_covered = observer_paths.iter().all(|observer_path| {
        summary_paths
            .iter()
            .any(|summary_path| workspace_paths_related(observer_path, summary_path))
    });
    let summary_covered = summary_paths.iter().all(|summary_path| {
        observer_paths
            .iter()
            .any(|observer_path| workspace_paths_related(observer_path, summary_path))
    });
    if !observer_covered || !summary_covered {
        return Err(
            "prepared workspace observer and producer summary paths do not describe the same change set"
                .to_string(),
        );
    }
    Ok(())
}

fn require_agent_baseline_unchanged(
    observation: Result<PreparedWorkspaceObservation, String>,
) -> Result<(), String> {
    match observation {
        Ok(PreparedWorkspaceObservation::Unchanged) => Ok(()),
        Ok(PreparedWorkspaceObservation::Changed(_)) => {
            Err("agent workspace changed before authoritative baseline snapshot".to_string())
        }
        Ok(PreparedWorkspaceObservation::Unknown) => Err(
            "agent workspace observation was incomplete before authoritative baseline snapshot"
                .to_string(),
        ),
        Err(error) => Err(error),
    }
}

/// Require the observer to prove that no workspace mutation occurred after the authoritative
/// after-snapshot completed. A later change cannot be represented by that snapshot and therefore
/// blocks the agent stage instead of being folded into a second, unbounded scan.
fn require_agent_final_snapshot_unchanged(
    observation: Result<PreparedWorkspaceObservation, String>,
) -> Result<(), String> {
    match observation {
        Ok(PreparedWorkspaceObservation::Unchanged) => Ok(()),
        Ok(PreparedWorkspaceObservation::Changed(_)) => {
            Err("agent workspace changed after authoritative final snapshot".to_string())
        }
        Ok(PreparedWorkspaceObservation::Unknown) => Err(
            "agent workspace observation was incomplete after authoritative final snapshot"
                .to_string(),
        ),
        Err(error) => Err(error),
    }
}

/// Choose the smallest safe post-agent snapshot from producer and continuous observer evidence.
///
/// A backend without a continuous observer, or an observer that lost event detail, keeps the
/// conservative full-tree path. When complete observer paths exist, they must close over the
/// producer summaries before incremental reads are allowed; an extra observer path is an
/// out-of-band mutation and blocks evaluation.
fn agent_snapshot_plan(
    result: &AgentLoopResult,
    observer: Option<&PreparedWorkspaceObservation>,
) -> Result<AgentSnapshotPlan, String> {
    let observer_paths = match observer {
        None | Some(PreparedWorkspaceObservation::Unchanged) => None,
        Some(PreparedWorkspaceObservation::Changed(paths)) => Some(validate_observer_paths(paths)?),
        Some(PreparedWorkspaceObservation::Unknown) => None,
    };
    let observer_requires_full_snapshot =
        matches!(observer, None | Some(PreparedWorkspaceObservation::Unknown));
    let mut revision = 0u64;
    let mut changed_paths = BTreeSet::new();
    let mut observed_contract: Option<&str> = None;
    let mut full = false;

    for tool_result in &result.tool_results {
        let summary = if let Some(summary) = tool_result.workspace_change_summary() {
            summary.validate().map_err(|error| {
                format!(
                    "invalid workspace change summary from {}: {error}",
                    tool_result.tool_name
                )
            })?;
            changed_paths.extend(summary.changed_files.iter().cloned());
            Some(summary)
        } else {
            None
        };
        if let Some(contract) = tool_result
            .audit_metadata()
            .and_then(|audit| audit.get("workspace_observation_metrics"))
            .and_then(|metrics| metrics.get("contract"))
            .and_then(Value::as_str)
        {
            if observed_contract.is_some_and(|previous| previous != contract) {
                full = true;
            }
            observed_contract = Some(contract);
        }

        let is_write_tool = matches!(tool_result.tool_name.as_str(), TOOL_PATCH | TOOL_COMMAND);
        let Some(observation) = tool_result.workspace_observation() else {
            if summary.is_some() {
                return Err(format!(
                    "workspace change summary from {} has no workspace observation",
                    tool_result.tool_name
                ));
            }
            if is_write_tool {
                full = true;
            }
            continue;
        };
        let observed_revision = observation.revision().map(|value| value.value());
        match observation.mutation() {
            WorkspaceMutation::Unchanged => {
                if summary.is_some_and(|summary| summary.verification_relevant) {
                    return Err(format!(
                        "verification-relevant workspace change summary from {} was projected as unchanged",
                        tool_result.tool_name
                    ));
                }
                if observed_revision != Some(revision) {
                    full = true;
                }
            }
            WorkspaceMutation::Changed => {
                let Some(next_revision) = revision.checked_add(1) else {
                    full = true;
                    continue;
                };
                if observed_revision != Some(next_revision) {
                    full = true;
                }
                revision = next_revision;
                let Some(summary) = summary else {
                    full = true;
                    continue;
                };
                if !summary.verification_relevant {
                    return Err(format!(
                        "workspace revision advanced for verification-irrelevant summary from {}",
                        tool_result.tool_name
                    ));
                }
            }
            WorkspaceMutation::Unknown => full = true,
        }
    }

    match observer {
        Some(PreparedWorkspaceObservation::Unchanged) => {
            if !changed_paths.is_empty() {
                return Err(
                    "producer reported physical workspace changes while observer was unchanged"
                        .to_string(),
                );
            }
        }
        Some(PreparedWorkspaceObservation::Changed(_)) => {
            validate_observer_summary_closure(
                observer_paths
                    .as_ref()
                    .expect("changed observer paths are validated above"),
                &changed_paths,
            )?;
        }
        Some(PreparedWorkspaceObservation::Unknown) | None => {}
    }

    if full || observer_requires_full_snapshot {
        Ok(AgentSnapshotPlan::Full)
    } else if changed_paths.is_empty() {
        Ok(AgentSnapshotPlan::Reused)
    } else {
        if let Some(observer_paths) = observer_paths {
            changed_paths.extend(observer_paths);
        }
        Ok(AgentSnapshotPlan::Incremental(
            changed_paths.into_iter().collect(),
        ))
    }
}

fn snapshot_agent_workspace_after(
    agent_dir: &Path,
    before: &WorkspaceSnapshot,
    before_identity: &workspace::WorkspaceRootIdentity,
    result: &AgentLoopResult,
    observer: Option<&PreparedWorkspaceObservation>,
) -> Result<
    (
        WorkspaceSnapshot,
        workspace::SourceCaptureWork,
        AgentSnapshotObservation,
        u64,
    ),
    String,
> {
    let current_identity = workspace_root_identity(agent_dir)?;
    if current_identity != *before_identity {
        return Err("agent workspace root identity changed during execution".to_string());
    }
    let plan = agent_snapshot_plan(result, observer)?;
    let (snapshot, work, observation, full_scans) = match plan {
        AgentSnapshotPlan::Reused => (
            before.clone(),
            workspace::SourceCaptureWork::default(),
            AgentSnapshotObservation::Reused,
            0,
        ),
        AgentSnapshotPlan::Incremental(paths) => {
            match snapshot_workspace_incremental(agent_dir, before, &paths) {
                Ok((snapshot, work)) => (snapshot, work, AgentSnapshotObservation::Incremental, 0),
                Err(_) => {
                    let (snapshot, work) = snapshot_workspace_with_work(agent_dir)?;
                    (snapshot, work, AgentSnapshotObservation::Full, 1)
                }
            }
        }
        AgentSnapshotPlan::Full => {
            let (snapshot, work) = snapshot_workspace_with_work(agent_dir)?;
            (snapshot, work, AgentSnapshotObservation::Full, 1)
        }
    };
    let final_identity = workspace_root_identity(agent_dir)?;
    if final_identity != *before_identity {
        return Err("agent workspace root identity changed while snapshotting".to_string());
    }
    Ok((snapshot, work, observation, full_scans))
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
    let mut agent_observer = match prepared
        .sandbox_backend
        .observe_prepared_workspace(agent_dir)
    {
        Ok(observer) => observer,
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                command_diagnostics,
            );
        }
    };
    let before_identity = match workspace_root_identity(agent_dir) {
        Ok(identity) => identity,
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                command_diagnostics,
            );
        }
    };
    let snapshot_before_started = Instant::now();
    let before = match snapshot_workspace_with_work(agent_dir) {
        Ok((snapshot, work)) => {
            diagnostics.agent_snapshot_before_tree_entries_read = work.source_tree_entries_read;
            diagnostics.agent_snapshot_before_tree_content_reads = work.source_tree_content_reads;
            diagnostics.agent_snapshot_before_tree_content_bytes = work.source_tree_content_bytes;
            diagnostics.agent_snapshot_full_scans = 1;
            snapshot
        }
        Err(error) => {
            diagnostics.agent_snapshot_before_ms =
                u64::try_from(snapshot_before_started.elapsed().as_millis()).unwrap_or(u64::MAX);
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                command_diagnostics,
            );
        }
    };
    diagnostics.agent_snapshot_before_ms =
        u64::try_from(snapshot_before_started.elapsed().as_millis()).unwrap_or(u64::MAX);
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
    if let Some(observer) = agent_observer.as_mut()
        && let Err(error) = require_agent_baseline_unchanged(observer.checkpoint())
    {
        return blocked_agent_stage(
            evaluation_blocker(BlockerKind::WorkspacePreparation, error),
            command_diagnostics,
        );
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
    let agent_duration_ms = u64::try_from(agent_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    diagnostics.agent_duration_ms = agent_duration_ms;
    let run_status = result.to_run_status();
    let agent_command_projection = agent_command_projection(&result);
    command_diagnostics = agent_command_projection.diagnostics.clone();
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
                audit_events: run_status.audit_events,
                local_process_fallback_unknown_count,
                trace_path: None,
                error: Some(safe_text(error)),
                provider_diagnostic: run_status.provider_diagnostic,
                prompt_structure: Some(prompt_structure.clone()),
                prompt_fingerprint: Some(prompt_fingerprint.clone()),
                tool_schema_fingerprint: Some(tool_schema_fingerprint.clone()),
                provider_evidence: Some(provider_evidence.clone()),
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
                audit_events: run_status.audit_events,
                local_process_fallback_unknown_count,
                trace_path: None,
                error: Some(safe_text(error)),
                provider_diagnostic: run_status.provider_diagnostic,
                prompt_structure: Some(prompt_structure.clone()),
                prompt_fingerprint: Some(prompt_fingerprint.clone()),
                tool_schema_fingerprint: Some(tool_schema_fingerprint.clone()),
                provider_evidence: Some(provider_evidence.clone()),
            };
        }
    };

    let agent_observation = match agent_observer.as_mut() {
        None => Ok(None),
        Some(observer) => observer.checkpoint().map(Some),
    };
    let snapshot_after_started = Instant::now();
    let after = match agent_observation.and_then(|observation| {
        snapshot_agent_workspace_after(
            agent_dir,
            &before,
            &before_identity,
            &result,
            observation.as_ref(),
        )
    }) {
        Ok((snapshot, work, observation, full_scans)) => {
            diagnostics.agent_snapshot_after_tree_entries_read = work.source_tree_entries_read;
            diagnostics.agent_snapshot_after_tree_content_reads = work.source_tree_content_reads;
            diagnostics.agent_snapshot_after_tree_content_bytes = work.source_tree_content_bytes;
            diagnostics.agent_snapshot_after_observation = Some(observation);
            diagnostics.agent_snapshot_full_scans = diagnostics
                .agent_snapshot_full_scans
                .saturating_add(full_scans);
            snapshot
        }
        Err(error) => {
            diagnostics.agent_snapshot_after_ms =
                u64::try_from(snapshot_after_started.elapsed().as_millis()).unwrap_or(u64::MAX);
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
                audit_events: run_status.audit_events,
                local_process_fallback_unknown_count,
                trace_path: trace_path_string,
                error: Some(safe_text(error)),
                provider_diagnostic: run_status.provider_diagnostic,
                prompt_structure: Some(prompt_structure.clone()),
                prompt_fingerprint: Some(prompt_fingerprint.clone()),
                tool_schema_fingerprint: Some(tool_schema_fingerprint.clone()),
                provider_evidence: Some(provider_evidence.clone()),
            };
        }
    };
    if let Some(observer) = agent_observer.as_mut()
        && let Err(error) = require_agent_final_snapshot_unchanged(observer.checkpoint())
    {
        diagnostics.agent_snapshot_after_ms =
            u64::try_from(snapshot_after_started.elapsed().as_millis()).unwrap_or(u64::MAX);
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
            audit_events: run_status.audit_events,
            local_process_fallback_unknown_count,
            trace_path: trace_path_string,
            error: Some(safe_text(error)),
            provider_diagnostic: run_status.provider_diagnostic,
            prompt_structure: Some(prompt_structure.clone()),
            prompt_fingerprint: Some(prompt_fingerprint.clone()),
            tool_schema_fingerprint: Some(tool_schema_fingerprint.clone()),
            provider_evidence: Some(provider_evidence.clone()),
        };
    }
    diagnostics.agent_snapshot_after_ms =
        u64::try_from(snapshot_after_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let patch_digest_started = Instant::now();
    let changed_files = evaluation_changed_paths(&before, &after, pristine_source);
    let patch_evidence = workspace_change_evidence(&before, &after, pristine_source);
    let patch_digest = patch_evidence_digest(&patch_evidence);
    diagnostics.agent_patch_digest_ms =
        u64::try_from(patch_digest_started.elapsed().as_millis()).unwrap_or(u64::MAX);
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
                audit_events: run_status.audit_events,
                local_process_fallback_unknown_count,
                trace_path: trace_path_string,
                error: Some(safe_text(error)),
                provider_diagnostic: run_status.provider_diagnostic,
                prompt_structure: Some(prompt_structure.clone()),
                prompt_fingerprint: Some(prompt_fingerprint.clone()),
                tool_schema_fingerprint: Some(tool_schema_fingerprint.clone()),
                provider_evidence: Some(provider_evidence.clone()),
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
            evaluation_blocker(
                kind,
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
        audit_events: run_status.audit_events,
        local_process_fallback_unknown_count,
        trace_path: trace_path_string,
        error,
        provider_diagnostic: run_status.provider_diagnostic,
        prompt_structure: Some(prompt_structure),
        prompt_fingerprint: Some(prompt_fingerprint),
        tool_schema_fingerprint: Some(tool_schema_fingerprint),
        provider_evidence: Some(provider_evidence),
    }
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
        audit_events: Vec::new(),
        local_process_fallback_unknown_count: 0,
        trace_path: None,
        error: Some(blocker.message),
        provider_diagnostic: None,
        prompt_structure: None,
        prompt_fingerprint: None,
        tool_schema_fingerprint: None,
        provider_evidence: None,
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

#[cfg(test)]
fn evaluation_agent_trace(
    store: &singularity_store::SessionStore,
    run_id: &str,
    session_id: &str,
    task_span_id: &str,
) -> Result<Value, String> {
    let events = evaluation_agent_trace_events(store, run_id, session_id, task_span_id)?;
    evaluation_agent_trace_value(run_id, session_id, events)
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
    if projection
        .diagnostics
        .iter()
        .any(|command| !command.is_strictly_sandboxed())
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
    evaluation_blocker(kind, error.message.clone())
}

fn provider_configuration_blocker(error: &ProviderError) -> EvaluationBlocker {
    let diagnostic = error.error.provider_diagnostic();
    EvaluationBlocker {
        code: diagnostic
            .code
            .filter(|code| !code.trim().is_empty())
            .or_else(|| Some("provider_configuration_invalid".to_string())),
        kind: BlockerKind::ProviderConfiguration,
        message: safe_text(&error.message),
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
    EvaluationBlocker {
        code: None,
        kind,
        message: safe_text(message.into()),
    }
}

fn sandbox_preflight_blocker(
    code: impl Into<String>,
    message: impl Into<String>,
) -> EvaluationBlocker {
    EvaluationBlocker {
        code: Some(code.into()),
        kind: BlockerKind::Environment,
        message: safe_text(message.into()),
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
) -> Result<(), RemoteSourcePreflightFailure> {
    for repository in remote_git_repositories(plans) {
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
        match preflight_remote_sources(&capability_workspace, plans, sandbox_backend, cancellation)
        {
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

/// Return the selected trial as an in-memory, non-gating CLI result.
///
/// Diagnostic runs deliberately stop at the run-owned workspace/trace boundary.  They do not
/// create the formal `publication/` result/report/evidence artifact set consumed by the full
/// Evaluation gate.
fn diagnostic_sampled_run_result(
    params: &EvaluationRunParams,
    run_dir: &Path,
    run_id: &RunId,
    selection: &EvaluationSelection,
    task_executions: &[TaskEvaluation],
) -> Result<EvaluationRunResult, EvaluationRunError> {
    let Some(execution) = task_executions.first() else {
        return Err(preserve_incomplete_run(
            run_dir,
            EvaluationRunError::infrastructure(
                "diagnostic run completed without a selected task result",
            ),
        ));
    };
    if task_executions.len() != 1
        || execution.result.task_id != selection.task_id
        || execution.trials.len() != 1
        || execution.trials[0].result.trial != selection.trial
    {
        return Err(preserve_incomplete_run(
            run_dir,
            EvaluationRunError::infrastructure(
                "diagnostic run task/trial execution does not match the selection",
            ),
        ));
    }
    let trial = execution.trials[0].result.clone();
    let diagnostic_passed = trial.functional_task_success
        && trial.agent_protocol_success
        && trial.sandbox_security_success;
    let status = enum_string(trial.status).map_err(|error| {
        preserve_incomplete_run(run_dir, EvaluationRunError::infrastructure(error))
    })?;
    let blocker = trial
        .blocker
        .as_ref()
        .map(blocker_code)
        .transpose()
        .map_err(|error| {
            preserve_incomplete_run(run_dir, EvaluationRunError::infrastructure(error))
        })?;
    let task_report = task_report(execution);
    Ok(EvaluationRunResult {
        run_id: run_id.as_str().to_string(),
        manifest: params.manifest.clone(),
        runner: RUNNER_NAME.to_string(),
        max_workers: params.max_workers,
        status,
        blocker,
        tasks: vec![task_report],
        result_path: None,
        report_path: None,
        evidence_path: None,
        evaluation_passed: false,
        gate_applicable: Some(false),
        selection: Some(selection.clone()),
        diagnostic_passed: Some(diagnostic_passed),
    })
}

/// Return a pre-sampling diagnostic blocker without writing formal Evaluation artifacts.
fn diagnostic_blocked_run_result(
    params: &EvaluationRunParams,
    run_id: &RunId,
    selection: &EvaluationSelection,
    blocker: EvaluationBlocker,
) -> Result<EvaluationRunResult, EvaluationRunError> {
    let blocker = Some(blocker)
        .as_ref()
        .map(blocker_code)
        .transpose()
        .map_err(EvaluationRunError::infrastructure)?;
    Ok(EvaluationRunResult {
        run_id: run_id.as_str().to_string(),
        manifest: params.manifest.clone(),
        runner: RUNNER_NAME.to_string(),
        max_workers: params.max_workers,
        status: "blocked".to_string(),
        blocker,
        tasks: Vec::new(),
        result_path: None,
        report_path: None,
        evidence_path: None,
        evaluation_passed: false,
        gate_applicable: Some(false),
        selection: Some(selection.clone()),
        diagnostic_passed: Some(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Argv, GitCommit, RemoteRepository};
    #[cfg(windows)]
    use crate::{EvaluationStage, WorkspaceSeed};
    use serde::Serializer;
    use singularity_policy::{
        ApprovalPolicy, NetworkAccess, PermissionDecisionOutcome, PermissionOperation,
        PermissionProfileName, PermissionResource, WorkspaceRelativePath,
    };
    use singularity_tools::{
        CommandExecutionStatus, CommandRequest, CommandResult, SandboxBackendEnforcement,
        SandboxCapabilities, WorkspaceChangeSummary, WorkspaceMutation, WorkspaceObservation,
        WorkspaceRevision,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn unconfigured_provider_snapshot() -> ProviderConfigSnapshot {
        ProviderConfigSnapshot::capture(|name| {
            (name == "SINGULARITY_MODEL_PROVIDER").then(|| "openai_compatible".to_string())
        })
    }

    fn command(argv: &[&str]) -> CommandSpec {
        CommandSpec {
            argv: Argv::new(argv.iter().map(|value| (*value).to_string()).collect()).expect("argv"),
            cwd: None,
            timeout_seconds: Some(30),
            network_access: NetworkAccess::Denied,
        }
    }

    fn supported_sandbox_preflight(backend: &str) -> SandboxPreflightReport {
        SandboxPreflightReport {
            outcome: SandboxPreflightOutcome::Supported,
            error_code: None,
            profile: "workspace_write_network_denied".to_string(),
            backend: backend.to_string(),
            missing_capabilities: Vec::new(),
            os: "test".to_string(),
            arch: "test".to_string(),
            kernel: Some("test-kernel".to_string()),
            filesystem: Some("test-filesystem".to_string()),
            overlayfs: SandboxPreflightFact::NotApplicable,
            user_namespace: SandboxPreflightFact::NotApplicable,
            mount_namespace: SandboxPreflightFact::NotApplicable,
            pid_namespace: SandboxPreflightFact::NotApplicable,
            network_namespace: SandboxPreflightFact::NotApplicable,
            no_new_privs: SandboxPreflightFact::NotApplicable,
            seccomp: SandboxPreflightFact::NotApplicable,
            landlock: SandboxPreflightFact::NotApplicable,
            transactional_workspace: SandboxPreflightFact::Passed,
            network_denied: SandboxPreflightFact::Passed,
            protected_paths: SandboxPreflightFact::Passed,
        }
    }

    fn completed_agent_result(tool_results: Vec<singularity_tools::ToolResult>) -> AgentLoopResult {
        AgentLoopResult {
            status: AgentStatus::Completed,
            completed: true,
            final_answer: Some("done".to_string()),
            model_turns: 1,
            tool_calls: tool_results.len() as u32,
            approval_count: 0,
            pending_approvals: Vec::new(),
            tool_results,
            verification: singularity_agent::AgentVerification::default(),
            recovery_metrics: AgentRecoveryMetrics::default(),
            model_usage: ModelUsage::default(),
            provider_attempts: ProviderAttemptMetadata::default(),
            error: None,
            model_turn_limit: 0,
            context_trace: None,
            error_category: None,
            provider_diagnostic: None,
            provider_protocol_contract: None,
            provider_capability_metadata: None,
        }
    }

    #[test]
    fn agent_snapshot_plan_reuses_complete_unchanged_revision() {
        let result = completed_agent_result(vec![
            singularity_tools::ToolResult::summary("read", TOOL_COMMAND, true, "ok")
                .with_workspace_observation(WorkspaceObservation::unchanged(
                    WorkspaceRevision::initial(),
                )),
        ]);
        assert_eq!(
            agent_snapshot_plan(&result, Some(&PreparedWorkspaceObservation::Unchanged)),
            Ok(AgentSnapshotPlan::Reused)
        );
    }

    #[test]
    fn agent_snapshot_plan_reads_only_producer_reported_mutations() {
        let revision = WorkspaceRevision::initial().next().expect("revision");
        let result = completed_agent_result(vec![
            singularity_tools::ToolResult::summary("patch", TOOL_PATCH, true, "changed")
                .with_workspace_observation(WorkspaceObservation::changed(revision))
                .with_workspace_change_summary(WorkspaceChangeSummary::new(
                    vec!["src/lib.rs".to_string()],
                    "sha256:0123456789012345678901234567890123456789012345678901234567890123",
                )),
        ]);
        assert_eq!(
            agent_snapshot_plan(
                &result,
                Some(&PreparedWorkspaceObservation::Changed(vec![
                    "src/lib.rs".to_string(),
                ]))
            ),
            Ok(AgentSnapshotPlan::Incremental(vec![
                "src/lib.rs".to_string()
            ]))
        );
    }

    #[test]
    fn agent_snapshot_plan_without_observer_keeps_full_scan_path() {
        let result = completed_agent_result(vec![
            singularity_tools::ToolResult::summary("read", TOOL_COMMAND, true, "ok")
                .with_workspace_observation(WorkspaceObservation::unchanged(
                    WorkspaceRevision::initial(),
                )),
        ]);
        assert_eq!(
            agent_snapshot_plan(&result, None),
            Ok(AgentSnapshotPlan::Full)
        );
    }

    #[test]
    fn agent_baseline_observer_rejects_changed_unknown_and_error() {
        for observation in [
            Ok(PreparedWorkspaceObservation::Changed(vec![
                "src/lib.rs".to_string(),
            ])),
            Ok(PreparedWorkspaceObservation::Unknown),
            Err("observer failed".to_string()),
        ] {
            assert!(require_agent_baseline_unchanged(observation).is_err());
        }
    }

    #[test]
    fn agent_final_snapshot_observer_rejects_changed_unknown_and_error() {
        assert!(
            require_agent_final_snapshot_unchanged(Ok(PreparedWorkspaceObservation::Unchanged,))
                .is_ok()
        );
        for observation in [
            Ok(PreparedWorkspaceObservation::Changed(vec![
                "src/lib.rs".to_string(),
            ])),
            Ok(PreparedWorkspaceObservation::Unknown),
            Err("observer failed".to_string()),
        ] {
            assert!(require_agent_final_snapshot_unchanged(observation).is_err());
        }
    }

    #[test]
    fn agent_snapshot_plan_rejects_out_of_band_observer_paths() {
        let revision = WorkspaceRevision::initial().next().expect("revision");
        let result = completed_agent_result(vec![
            singularity_tools::ToolResult::summary("patch", TOOL_PATCH, true, "changed")
                .with_workspace_observation(WorkspaceObservation::changed(revision))
                .with_workspace_change_summary(WorkspaceChangeSummary::new(
                    vec!["src/lib.rs".to_string()],
                    OBSERVER_PATH_DIGEST,
                )),
        ]);
        let observation = PreparedWorkspaceObservation::Changed(vec![
            "src/lib.rs".to_string(),
            "out-of-band.txt".to_string(),
        ]);
        assert!(agent_snapshot_plan(&result, Some(&observation)).is_err());
    }

    #[test]
    fn agent_snapshot_plan_includes_artifact_only_paths_without_revision_advance() {
        let artifact = WorkspaceChangeSummary {
            changed_files: vec!["target/cache.bin".to_string()],
            diff_digest: OBSERVER_PATH_DIGEST.to_string(),
            verification_relevant: false,
        };
        let result = completed_agent_result(vec![
            singularity_tools::ToolResult::summary("artifact", TOOL_COMMAND, true, "changed")
                .with_workspace_observation(WorkspaceObservation::unchanged(
                    WorkspaceRevision::initial(),
                ))
                .with_workspace_change_summary(artifact),
        ]);
        assert_eq!(
            agent_snapshot_plan(
                &result,
                Some(&PreparedWorkspaceObservation::Changed(vec![
                    "target/cache.bin".to_string(),
                ])),
            ),
            Ok(AgentSnapshotPlan::Incremental(vec![
                "target/cache.bin".to_string()
            ]))
        );
    }

    #[test]
    fn agent_snapshot_plan_rejects_any_physical_change_with_unchanged_observer() {
        let artifact = WorkspaceChangeSummary {
            changed_files: vec!["target/cache.bin".to_string()],
            diff_digest: OBSERVER_PATH_DIGEST.to_string(),
            verification_relevant: false,
        };
        let artifact_result = completed_agent_result(vec![
            singularity_tools::ToolResult::summary("artifact", TOOL_COMMAND, true, "changed")
                .with_workspace_observation(WorkspaceObservation::unchanged(
                    WorkspaceRevision::initial(),
                ))
                .with_workspace_change_summary(artifact),
        ]);
        assert!(
            agent_snapshot_plan(
                &artifact_result,
                Some(&PreparedWorkspaceObservation::Unchanged)
            )
            .is_err()
        );

        let revision = WorkspaceRevision::initial().next().expect("revision");
        let source_result = completed_agent_result(vec![
            singularity_tools::ToolResult::summary("patch", TOOL_PATCH, true, "changed")
                .with_workspace_observation(WorkspaceObservation::changed(revision))
                .with_workspace_change_summary(WorkspaceChangeSummary::new(
                    vec!["src/lib.rs".to_string()],
                    OBSERVER_PATH_DIGEST,
                )),
        ]);
        assert!(
            agent_snapshot_plan(
                &source_result,
                Some(&PreparedWorkspaceObservation::Unchanged)
            )
            .is_err()
        );
    }

    #[test]
    fn agent_snapshot_plan_rejects_summary_without_matching_semantic_observation() {
        let summary =
            WorkspaceChangeSummary::new(vec!["src/lib.rs".to_string()], OBSERVER_PATH_DIGEST);
        let missing_observation = completed_agent_result(vec![
            singularity_tools::ToolResult::summary("patch", TOOL_PATCH, true, "changed")
                .with_workspace_change_summary(summary.clone()),
        ]);
        assert!(
            agent_snapshot_plan(
                &missing_observation,
                Some(&PreparedWorkspaceObservation::Changed(vec![
                    "src/lib.rs".to_string(),
                ])),
            )
            .is_err()
        );

        let unchanged_observation = completed_agent_result(vec![
            singularity_tools::ToolResult::summary("patch", TOOL_PATCH, true, "changed")
                .with_workspace_observation(WorkspaceObservation::unchanged(
                    WorkspaceRevision::initial(),
                ))
                .with_workspace_change_summary(summary),
        ]);
        assert!(
            agent_snapshot_plan(
                &unchanged_observation,
                Some(&PreparedWorkspaceObservation::Changed(vec![
                    "src/lib.rs".to_string(),
                ])),
            )
            .is_err()
        );
    }

    #[test]
    fn agent_snapshot_plan_rejects_invalid_summary_before_scanning() {
        let result = completed_agent_result(vec![
            singularity_tools::ToolResult::summary("patch", TOOL_PATCH, true, "changed")
                .with_workspace_observation(WorkspaceObservation::unchanged(
                    WorkspaceRevision::initial(),
                ))
                .with_workspace_change_summary(WorkspaceChangeSummary::new(
                    vec!["src/lib.rs".to_string()],
                    "not-a-sha256",
                )),
        ]);
        for observation in [
            PreparedWorkspaceObservation::Unchanged,
            PreparedWorkspaceObservation::Unknown,
        ] {
            assert!(agent_snapshot_plan(&result, Some(&observation)).is_err());
        }
    }

    #[test]
    fn agent_snapshot_plan_revision_drift_cannot_reuse() {
        let revision = WorkspaceRevision::initial().next().expect("revision");
        let result = completed_agent_result(vec![
            singularity_tools::ToolResult::summary("read", TOOL_COMMAND, true, "ok")
                .with_workspace_observation(WorkspaceObservation::unchanged(revision)),
        ]);
        assert_eq!(
            agent_snapshot_plan(&result, Some(&PreparedWorkspaceObservation::Unchanged)),
            Ok(AgentSnapshotPlan::Full)
        );
    }

    #[test]
    fn agent_snapshot_plan_falls_back_on_unknown_or_contract_drift() {
        let unknown = completed_agent_result(vec![
            singularity_tools::ToolResult::summary("command", TOOL_COMMAND, false, "unknown")
                .with_workspace_observation(WorkspaceObservation::unknown()),
        ]);
        assert_eq!(
            agent_snapshot_plan(&unknown, Some(&PreparedWorkspaceObservation::Unknown)),
            Ok(AgentSnapshotPlan::Full)
        );

        let first = singularity_tools::ToolResult::summary("first", TOOL_COMMAND, true, "ok")
            .with_workspace_observation(WorkspaceObservation::unchanged(
                WorkspaceRevision::initial(),
            ))
            .with_audit(json!({
                "workspace_observation_metrics": {"contract": "backend/a"}
            }));
        let second = singularity_tools::ToolResult::summary("second", TOOL_COMMAND, true, "ok")
            .with_workspace_observation(WorkspaceObservation::unchanged(
                WorkspaceRevision::initial(),
            ))
            .with_audit(json!({
                "workspace_observation_metrics": {"contract": "backend/b"}
            }));
        assert_eq!(
            agent_snapshot_plan(
                &completed_agent_result(vec![first, second]),
                Some(&PreparedWorkspaceObservation::Unchanged),
            ),
            Ok(AgentSnapshotPlan::Full)
        );
    }

    #[test]
    fn post_agent_snapshot_reuses_baseline_without_tree_reads_when_unchanged() {
        let temp = tempfile::tempdir().expect("workspace");
        fs::write(temp.path().join("large.txt"), vec![b'x'; 1024]).expect("file");
        let (before, _) = snapshot_workspace_with_work(temp.path()).expect("before snapshot");
        let identity = workspace_root_identity(temp.path()).expect("root identity");
        let result = completed_agent_result(vec![
            singularity_tools::ToolResult::summary("command", TOOL_COMMAND, true, "ok")
                .with_workspace_observation(WorkspaceObservation::unchanged(
                    WorkspaceRevision::initial(),
                )),
        ]);
        let (after, work, observation, full_scans) = snapshot_agent_workspace_after(
            temp.path(),
            &before,
            &identity,
            &result,
            Some(&PreparedWorkspaceObservation::Unchanged),
        )
        .expect("post-agent snapshot");
        assert_eq!(after, before);
        assert_eq!(work, workspace::SourceCaptureWork::default());
        assert_eq!(observation, AgentSnapshotObservation::Reused);
        assert_eq!(full_scans, 0);
    }

    #[test]
    fn post_agent_snapshot_full_rescans_when_observer_is_unknown() {
        let temp = tempfile::tempdir().expect("workspace");
        let value_path = temp.path().join("value.txt");
        fs::write(&value_path, b"before").expect("initial file");
        let (before, _) = snapshot_workspace_with_work(temp.path()).expect("before snapshot");
        let identity = workspace_root_identity(temp.path()).expect("root identity");
        fs::write(&value_path, b"after!").expect("updated file");
        let result = completed_agent_result(vec![
            singularity_tools::ToolResult::summary("command", TOOL_COMMAND, false, "unknown")
                .with_workspace_observation(WorkspaceObservation::unknown()),
        ]);

        let (after, work, observation, full_scans) = snapshot_agent_workspace_after(
            temp.path(),
            &before,
            &identity,
            &result,
            Some(&PreparedWorkspaceObservation::Unknown),
        )
        .expect("post-agent full snapshot");

        assert_ne!(after, before);
        assert_eq!(
            after,
            snapshot_workspace(temp.path()).expect("current snapshot")
        );
        assert_eq!(work.source_tree_content_reads, 1);
        assert_eq!(work.source_tree_content_bytes, 6);
        assert_eq!(observation, AgentSnapshotObservation::Full);
        assert_eq!(full_scans, 1);
    }

    #[test]
    fn agent_prompt_contains_only_task_instructions_and_stable_tools() {
        let projection = AgentTaskProjection {
            task_id: TaskId::new("task-1").expect("task id"),
            description: "description".to_string(),
            instructions: "fix the bug".to_string(),
        };

        let prompt = agent_prompt(
            &projection,
            &[TOOL_READ.to_string(), TOOL_COMMAND.to_string()],
        );
        assert!(prompt.contains("fix the bug"));
        assert!(prompt.contains("read, command"));
        assert!(!prompt.contains("cargo test"));
        assert!(!prompt.contains("sandbox_mode"));
        assert!(!prompt.contains("network_access"));
        assert!(!prompt.contains("evaluator"));
        assert!(!prompt.contains("test_patch"));
    }

    #[test]
    fn agent_blocker_kind_maps_typed_provider_categories() {
        let response_validation = ProviderDiagnostic {
            code: Some("provider_does_not_support_parallel_tool_calls".to_string()),
            stage: Some(ProviderErrorStage::ResponseValidation),
            transport_category: None,
            timeout_seconds: None,
            http_status: None,
            validation_errors: vec!["max_tool_calls_exceeded".to_string()],
        };
        for (category, diagnostic, expected) in [
            (
                ModelErrorCategory::Authentication,
                None,
                BlockerKind::ProviderAuthentication,
            ),
            (ModelErrorCategory::Network, None, BlockerKind::Network),
            (
                ModelErrorCategory::ProviderUnavailable,
                None,
                BlockerKind::Network,
            ),
            (
                ModelErrorCategory::ModelConfiguration,
                None,
                BlockerKind::ProviderConfiguration,
            ),
            (
                ModelErrorCategory::UnsupportedCapability,
                Some(&response_validation),
                BlockerKind::ProviderResponse,
            ),
        ] {
            assert_eq!(
                agent_blocker_kind(Some(&category), diagnostic),
                Some(expected)
            );
        }
        assert_eq!(agent_blocker_kind(None, None), None);
        assert_eq!(
            agent_blocker_kind(Some(&ModelErrorCategory::UnknownProviderError), None),
            None
        );
        assert_eq!(
            agent_blocker_kind(
                Some(&ModelErrorCategory::InvalidRequest),
                Some(&ProviderDiagnostic {
                    code: Some("provider_request_invalid".to_string()),
                    stage: Some(ProviderErrorStage::RequestSend),
                    transport_category: None,
                    timeout_seconds: None,
                    http_status: None,
                    validation_errors: vec!["request_id_missing".to_string()],
                }),
            ),
            None
        );
    }

    #[test]
    fn task_diagnostics_serializes_only_safe_provider_fields() {
        let diagnostics = TaskDiagnostics {
            source_preparation_duration_ms: 11,
            copy_ms: 7,
            transaction_wall_ms: 8,
            snapshot_ms: 9,
            digest_ms: 10,
            source_full_scans: 1,
            source_tree_entries_read: 3,
            source_tree_content_reads: 2,
            source_tree_content_bytes: 11,
            source_image_bytes: 11,
            trial_duration_ms: 22,
            baseline_duration_ms: 3,
            agent_duration_ms: 4,
            public_duration_ms: 5,
            hidden_duration_ms: 6,
            agent_copy_ms: 12,
            agent_setup_ms: 13,
            agent_snapshot_before_ms: 14,
            agent_snapshot_before_tree_entries_read: 17,
            agent_snapshot_before_tree_content_reads: 18,
            agent_snapshot_before_tree_content_bytes: 19,
            agent_snapshot_after_ms: 15,
            agent_snapshot_after_tree_entries_read: 20,
            agent_snapshot_after_tree_content_reads: 21,
            agent_snapshot_after_tree_content_bytes: 22,
            agent_snapshot_after_observation: Some(AgentSnapshotObservation::Incremental),
            agent_snapshot_full_scans: 1,
            agent_patch_digest_ms: 16,
            provider_diagnostic: Some(ProviderDiagnostic {
                code: Some("provider_response_invalid".to_string()),
                stage: Some(singularity_model::ProviderErrorStage::ResponseValidation),
                transport_category: None,
                timeout_seconds: Some(120),
                http_status: None,
                validation_errors: vec!["missing_tool_call_id".to_string()],
            }),
            ..TaskDiagnostics::default()
        };
        let serialized = serde_json::to_string(&diagnostics).expect("serialize diagnostics");
        assert!(serialized.contains("missing_tool_call_id"));
        assert!(serialized.contains("\"timeout_seconds\":120"));
        assert!(serialized.contains("\"source_preparation_duration_ms\":11"));
        assert!(serialized.contains("\"copy_ms\":7"));
        assert!(serialized.contains("\"transaction_wall_ms\":8"));
        assert!(serialized.contains("\"snapshot_ms\":9"));
        assert!(serialized.contains("\"digest_ms\":10"));
        assert!(serialized.contains("\"source_full_scans\":1"));
        assert!(serialized.contains("\"source_tree_entries_read\":3"));
        assert!(serialized.contains("\"source_tree_content_reads\":2"));
        assert!(serialized.contains("\"source_tree_content_bytes\":11"));
        assert!(serialized.contains("\"source_image_bytes\":11"));
        assert!(serialized.contains("\"trial_duration_ms\":22"));
        assert!(serialized.contains("\"baseline_duration_ms\":3"));
        assert!(serialized.contains("\"agent_duration_ms\":4"));
        assert!(serialized.contains("\"public_duration_ms\":5"));
        assert!(serialized.contains("\"hidden_duration_ms\":6"));
        assert!(serialized.contains("\"agent_copy_ms\":12"));
        assert!(serialized.contains("\"agent_setup_ms\":13"));
        assert!(serialized.contains("\"agent_snapshot_before_ms\":14"));
        assert!(serialized.contains("\"agent_snapshot_before_tree_entries_read\":17"));
        assert!(serialized.contains("\"agent_snapshot_before_tree_content_reads\":18"));
        assert!(serialized.contains("\"agent_snapshot_before_tree_content_bytes\":19"));
        assert!(serialized.contains("\"agent_snapshot_after_ms\":15"));
        assert!(serialized.contains("\"agent_snapshot_after_tree_entries_read\":20"));
        assert!(serialized.contains("\"agent_snapshot_after_tree_content_reads\":21"));
        assert!(serialized.contains("\"agent_snapshot_after_tree_content_bytes\":22"));
        assert!(serialized.contains("\"agent_snapshot_after_observation\":\"incremental\""));
        assert!(serialized.contains("\"agent_snapshot_full_scans\":1"));
        assert!(serialized.contains("\"agent_patch_digest_ms\":16"));
        assert!(!serialized.contains("Authorization"));
        assert!(!serialized.contains("raw_response"));
    }

    #[test]
    fn evaluation_registry_exposes_the_stable_workspace_tool_surface() {
        let registry = evaluation_registry().expect("registry");
        let schemas = registry.registry.schema_payloads();
        let names = schemas
            .iter()
            .filter_map(|payload| payload["name"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["command", "grep", "list", "patch", "read"]);
        assert!(registry.registry.get("update_plan").is_none());
        assert!(registry.registry.get("edit").is_none());
    }

    #[test]
    fn agent_command_projection_separates_not_executed_from_unknown() {
        let mut missing_evidence =
            singularity_tools::ToolResult::summary("command-unknown", TOOL_COMMAND, true, "ok");
        missing_evidence.result_id = Some(format!("sha256:{}", "b".repeat(64)));
        let missing_projection =
            agent_command_projection(&completed_agent_result(vec![missing_evidence]));
        assert_eq!(missing_projection.unknown_count, 1);
        assert!(missing_projection.diagnostics.is_empty());

        let mut rejected = singularity_tools::ToolResult::summary(
            "command-rejected",
            TOOL_COMMAND,
            false,
            "invalid arguments",
        );
        rejected.failure_kind = Some(singularity_tools::ToolFailureKind::Input);
        let rejected_projection = agent_command_projection(&completed_agent_result(vec![rejected]));
        assert_eq!(rejected_projection.unknown_count, 0);
        assert!(rejected_projection.diagnostics.is_empty());

        let explicitly_not_executed = singularity_tools::ToolResult::summary(
            "command-not-executed",
            TOOL_COMMAND,
            false,
            "policy denied",
        )
        .with_audit(json!({
            "command_provenance": "agent_requested",
            "sandbox_backend": "not_executed"
        }));
        let not_executed_projection =
            agent_command_projection(&completed_agent_result(vec![explicitly_not_executed]));
        assert_eq!(not_executed_projection.unknown_count, 0);
        assert!(not_executed_projection.diagnostics.is_empty());
    }

    #[test]
    fn agent_command_projection_rejects_non_strict_and_counts_strict_typed_evidence() {
        let strict_digest = format!("sha256:{}", "c".repeat(64));
        let mut strict =
            singularity_tools::ToolResult::summary("strict-command", TOOL_COMMAND, true, "ok")
                .with_audit(json!({
                    "command_scope_digest": strict_digest.clone(),
                    "command_provenance": "agent_requested",
                    "sandbox_backend": "strict-test",
                    "sandbox_enforcement": "strict",
                    "local_process_fallback": false
                }));
        strict.result_id = Some(strict_digest);
        let strict_projection = agent_command_projection(&completed_agent_result(vec![strict]));
        assert_eq!(strict_projection.unknown_count, 0);
        assert_eq!(strict_projection.diagnostics.len(), 1);
        assert!(agent_sandbox_blocker(&strict_projection).is_none());
        assert!(strict_projection.diagnostics[0].is_strictly_sandboxed());

        let restricted_digest = format!("sha256:{}", "d".repeat(64));
        let mut restricted =
            singularity_tools::ToolResult::summary("restricted-command", TOOL_COMMAND, true, "ok")
                .with_audit(json!({
                    "command_scope_digest": restricted_digest.clone(),
                    "command_provenance": "agent_requested",
                    "sandbox_backend": "restricted-test",
                    "sandbox_enforcement": "restricted_token",
                    "local_process_fallback": false
                }));
        restricted.result_id = Some(restricted_digest);
        let restricted_projection =
            agent_command_projection(&completed_agent_result(vec![restricted]));
        assert_eq!(restricted_projection.unknown_count, 0);
        assert_eq!(restricted_projection.diagnostics.len(), 1);
        assert!(agent_sandbox_blocker(&restricted_projection).is_some());
        assert!(!restricted_projection.diagnostics[0].is_strictly_sandboxed());
    }

    #[test]
    fn evaluation_output_root_preserves_explicit_and_environment_precedence() {
        let system_temp = Path::new("C:/system-temp");
        assert_eq!(
            evaluation_output_root_for_sources(None, None, system_temp),
            PathBuf::from("C:/system-temp/singularity/evaluations")
        );
        assert_eq!(
            evaluation_output_root_for_sources(None, Some("C:/configured"), system_temp),
            PathBuf::from("C:/configured")
        );
        assert_eq!(
            evaluation_output_root_for_sources(
                Some("C:/explicit"),
                Some("C:/configured"),
                system_temp
            ),
            PathBuf::from("C:/explicit")
        );
    }

    #[test]
    fn workspace_copy_skips_git_metadata() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).expect("source");
        fs::write(source.join(".git"), "gitdir: elsewhere").expect("gitfile");
        fs::write(source.join("README.md"), "content").expect("readme");

        copy_tree_checked(&source, &destination).expect("copy");

        assert!(!destination.join(".git").exists());
        assert_eq!(
            fs::read_to_string(destination.join("README.md")).expect("readme"),
            "content"
        );
    }

    #[test]
    fn workspace_copy_snapshot_exposes_prepared_source_drift() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).expect("source");
        fs::write(source.join("README.md"), "prepared").expect("prepared source");
        let prepared = snapshot_workspace(&source).expect("prepared snapshot");
        fs::write(source.join("README.md"), "drifted").expect("source drift");

        let copied = copy_tree_checked(&source, &destination).expect("copy");

        assert_ne!(copied, prepared);
        assert_eq!(
            copied,
            snapshot_workspace(&destination).expect("copied workspace snapshot")
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_copy_preserves_opaque_symlink_targets_without_following_them() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        let outside = temp.path().join("outside.txt");
        fs::create_dir(&source).expect("source");
        fs::write(&outside, "outside").expect("outside");
        symlink(".", source.join("loop")).expect("loop link");
        symlink("../outside.txt", source.join("relative-escape")).expect("relative link");
        symlink(&outside, source.join("absolute-escape")).expect("absolute link");

        let source_snapshot = snapshot_workspace(&source).expect("source snapshot");
        copy_tree_checked(&source, &destination).expect("copy");
        let destination_snapshot = snapshot_workspace(&destination).expect("destination snapshot");

        assert_eq!(destination_snapshot, source_snapshot);
        assert_eq!(
            fs::read_link(destination.join("loop")).expect("loop target"),
            Path::new(".")
        );
        assert_eq!(
            fs::read_link(destination.join("relative-escape")).expect("relative target"),
            Path::new("../outside.txt")
        );
        assert_eq!(
            fs::read_link(destination.join("absolute-escape")).expect("absolute target"),
            outside
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_snapshot_distinguishes_symlink_target_and_object_kind() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("entry");
        symlink("first", &path).expect("first link");
        let first = snapshot_workspace(temp.path()).expect("first snapshot");

        fs::remove_file(&path).expect("remove first link");
        symlink("second", &path).expect("second link");
        let second = snapshot_workspace(temp.path()).expect("second snapshot");
        assert_eq!(super::workspace::changed_paths(&first, &second), ["entry"]);

        fs::remove_file(&path).expect("remove second link");
        fs::write(&path, "second").expect("replacement file");
        let regular = snapshot_workspace(temp.path()).expect("regular snapshot");
        assert_eq!(
            super::workspace::changed_paths(&second, &regular),
            ["entry"]
        );
    }

    #[test]
    fn workspace_copy_rejects_destination_inside_source() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        let output = source.join("output");
        let destination = output.join("copy");
        fs::create_dir_all(&output).expect("output");
        fs::write(source.join("README.md"), "content").expect("readme");

        let error = copy_tree_checked(&source, &destination).expect_err("overlap");

        assert!(error.to_string().contains("source and destination overlap"));
        assert!(!destination.exists());
    }

    #[test]
    fn windows_path_budget_rejects_long_full_sha_projection_before_creation() {
        let temp = tempfile::tempdir().expect("temp");
        let output_root = temp.path().join("r".repeat(220));
        let long_run_id = RunId::new("a".repeat(40)).expect("git full-SHA run id");
        let long_task_id = TaskId::new("b".repeat(40)).expect("git full-SHA task id");
        let run_dir = output_root.join(long_run_id.as_str());

        let error = preflight_evaluation_path_budget_with_limit(
            &output_root,
            &long_run_id,
            std::slice::from_ref(&long_task_id),
            1,
            WINDOWS_MAX_PATH_CHARS,
        )
        .expect_err("full-SHA evaluation paths must exceed the conservative budget");

        assert!(
            error
                .to_string()
                .contains("evaluation path budget exceeded")
        );
        assert!(!run_dir.exists());
    }

    #[test]
    fn windows_path_budget_accepts_short_projection() {
        let temp = tempfile::tempdir().expect("temp");
        let output_root = temp.path().join("short");
        let run_id = RunId::new("run").expect("run id");
        let task_id = TaskId::new("task").expect("task id");

        preflight_evaluation_path_budget_with_limit(
            &output_root,
            &run_id,
            std::slice::from_ref(&task_id),
            1,
            WINDOWS_MAX_PATH_CHARS,
        )
        .expect("short evaluation paths must fit the conservative budget");
    }

    #[test]
    fn pre_cancelled_evaluation_returns_a_typed_cancelled_error() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let mut trace_store =
            singularity_store::SessionStore::open(":memory:").expect("trace store");
        let error = run_evaluation(
            &EvaluationRunParams {
                manifest: "missing-manifest.json".to_string(),
                run_id: "cancelled-run".to_string(),
                output_root: None,
                max_workers: 1,
            },
            Arc::new(SourceSandboxBackend),
            &unconfigured_provider_snapshot(),
            &cancellation,
            &mut trace_store,
        )
        .expect_err("cancelled evaluation must not start");

        assert_eq!(error.kind(), EvaluationRunErrorKind::Cancelled);
        let partial = error.partial_result().expect("partial terminal result");
        assert_eq!(partial.status, "blocked");
        assert_eq!(partial.blocker.as_deref(), Some("evaluation_cancelled"));
        assert!(partial.tasks.is_empty());
    }

    #[test]
    fn run_evaluation_rejects_out_of_range_max_workers_before_manifest_access() {
        for max_workers in [0, 3] {
            let mut trace_store =
                singularity_store::SessionStore::open(":memory:").expect("trace store");
            let error = run_evaluation(
                &EvaluationRunParams {
                    manifest: "missing-manifest.json".to_string(),
                    run_id: format!("invalid-workers-{max_workers}"),
                    output_root: None,
                    max_workers,
                },
                Arc::new(SourceSandboxBackend),
                &unconfigured_provider_snapshot(),
                &CancellationToken::new(),
                &mut trace_store,
            )
            .expect_err("out-of-range max-workers must be rejected");
            assert_eq!(error.kind(), EvaluationRunErrorKind::Input);
            assert!(error.to_string().contains("max_workers"));
        }
    }

    #[test]
    fn diagnostic_selection_rejects_missing_task_and_out_of_range_trial_before_provider() {
        let temp = tempfile::tempdir().expect("temp");
        let fixture = temp.path().join("fixture");
        fs::create_dir(&fixture).expect("fixture");
        fs::write(fixture.join("README.md"), "seed").expect("fixture README");
        let manifest_path = write_preflight_manifest(
            temp.path(),
            "diagnostic-selection",
            "diagnostic selector validation",
            3,
            json!({"type": "local", "path": "fixture"}),
        );
        let params = EvaluationRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "diagnostic-selection-run".to_string(),
            output_root: Some(temp.path().join("output").to_string_lossy().into_owned()),
            max_workers: 1,
        };
        let provider_snapshot = unconfigured_provider_snapshot();
        for selection in [
            EvaluationSelection::new(TaskId::new("missing-task").expect("task id"), 2)
                .expect("selection"),
            EvaluationSelection::new(TaskId::new("diagnostic-selection").expect("task id"), 4)
                .expect("selection"),
        ] {
            let mut trace_store =
                singularity_store::SessionStore::open(":memory:").expect("trace store");
            let error = run_evaluation_with_selection(
                &params,
                Arc::new(SourceSandboxBackend),
                &provider_snapshot,
                &CancellationToken::new(),
                &mut trace_store,
                Some(selection),
            )
            .expect_err("invalid diagnostic selection must stop before provider sampling");
            assert_eq!(error.kind(), EvaluationRunErrorKind::Input);
            assert!(!temp.path().join("output").exists());
        }
    }

    #[test]
    fn incomplete_run_preserves_trial_artifacts_and_bounded_failure_evidence() {
        let temp = tempfile::tempdir().expect("temp");
        let run_dir = temp.path().join("failed-run");
        let trial_dir = run_dir.join("task").join("trial-0001");
        fs::create_dir_all(&trial_dir).expect("trial directory");
        let trace_path = trial_dir.join(AGENT_TRACE_FILE);
        fs::write(&trace_path, b"preserved trace").expect("trial trace");

        let error = preserve_incomplete_run(
            &run_dir,
            EvaluationRunError::infrastructure("invalid evaluation evidence"),
        );

        assert_eq!(error.kind(), EvaluationRunErrorKind::Infrastructure);
        assert!(trace_path.is_file(), "sampled trial evidence must survive");
        let failure: Value = serde_json::from_slice(
            &fs::read(run_dir.join(FAILURE_FILE)).expect("failure evidence"),
        )
        .expect("valid failure evidence");
        assert_eq!(failure["schema_version"], FAILURE_SCHEMA_VERSION);
        assert_eq!(failure["kind"], "infrastructure");
        assert_eq!(failure["message"], "invalid evaluation evidence");
        assert!(
            !run_dir.join(PUBLICATION_DIR).is_dir(),
            "a failed run must not masquerade as an atomic publication"
        );
    }

    #[derive(Default)]
    struct CancellableProbeBackend {
        execute_calls: AtomicUsize,
        cancellable_calls: AtomicUsize,
    }

    impl SandboxBackend for CancellableProbeBackend {
        fn name(&self) -> &'static str {
            "cancellable_probe"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict().with_change_detection()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            self.execute_calls.fetch_add(1, Ordering::SeqCst);
            CommandResult::backend_error(&request.command_id, "non-cancellable path used")
        }

        fn execute_cancellable(
            &self,
            request: &CommandRequest,
            _cancellation: &CancellationToken,
        ) -> CommandResult {
            self.cancellable_calls.fetch_add(1, Ordering::SeqCst);
            CommandResult::cancelled(&request.command_id, 0)
        }
    }

    #[test]
    fn evaluation_commands_use_the_run_cancellation_token() {
        let temp = tempfile::tempdir().expect("workspace");
        let backend = Arc::new(CancellableProbeBackend::default());
        let shared: SharedSandboxBackend = backend.clone();
        let wrapped = cancellation_aware_sandbox_backend(&shared, &CancellationToken::new());

        let result = run_command_spec(
            temp.path(),
            &command(&["git", "status"]),
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
            wrapped,
        )
        .expect("cancellable command result");

        assert_eq!(result.execution_status, CommandExecutionStatus::Cancelled);
        assert_eq!(backend.cancellable_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.execute_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cancelled_partial_result_uses_bounded_safe_task_projection() {
        let task_id = TaskId::new("partial-task").expect("task id");
        let diagnostics = TaskDiagnostics {
            trace_path: Some("C:\\secret-workspace\\agent-trace.json".to_string()),
            patch_evidence_path: Some("C:\\secret-workspace\\patch-evidence.json".to_string()),
            ..TaskDiagnostics::default()
        };
        let task = finish_task(
            &task_id,
            1,
            StageExecution::blocked(
                evaluation_blocker(BlockerKind::AgentRuntime, "evaluation cancelled"),
                Vec::new(),
            ),
            StageExecution::skipped("agent stage skipped"),
            StageExecution::skipped("public stage skipped"),
            StageExecution::skipped("hidden stage skipped"),
            diagnostics,
        );
        let task_result =
            EvaluationTaskResult::from_trials(task_id, Vec::new(), vec![task.result.clone()]);
        let execution = TaskEvaluation {
            result: task_result,
            trials: vec![task],
        };
        let run_id = RunId::new("partial-safe").expect("run id");
        let partial = partial_evaluation_result(
            &EvaluationRunParams {
                manifest: "C:\\secret-workspace\\manifest.json".to_string(),
                run_id: run_id.as_str().to_string(),
                output_root: None,
                max_workers: 1,
            },
            &run_id,
            std::slice::from_ref(&execution),
            None,
        );
        let serialized = serde_json::to_string(&partial).expect("partial result serializes");

        assert_eq!(partial.manifest, "[redacted]");
        assert_eq!(partial.tasks.len(), 1);
        assert!(!serialized.contains("secret-workspace"));
        assert!(!serialized.contains("agent-trace.json"));
        assert!(!serialized.contains("patch-evidence.json"));
        assert!(partial.selection.is_none());
        assert!(partial.gate_applicable.is_none());
        assert!(partial.diagnostic_passed.is_none());
    }

    #[test]
    fn cancelled_diagnostic_partial_remains_explicitly_non_gating() {
        let params = EvaluationRunParams {
            manifest: "manifest.json".to_string(),
            run_id: "diagnostic-partial".to_string(),
            output_root: None,
            max_workers: 1,
        };
        let run_id = RunId::new(&params.run_id).expect("run id");
        let selection = EvaluationSelection::new(TaskId::new("task-a").expect("task id"), 2)
            .expect("selection");
        let partial = partial_evaluation_result(&params, &run_id, &[], Some(&selection));

        assert_eq!(partial.selection, Some(selection));
        assert_eq!(partial.gate_applicable, Some(false));
        assert_eq!(partial.diagnostic_passed, Some(false));
        assert!(!partial.evaluation_passed);
    }

    #[cfg(windows)]
    #[test]
    fn run_evaluation_rejects_long_paths_before_creating_run_directory() {
        let temp = tempfile::tempdir().expect("temp");
        let manifest_path = temp.path().join("manifest.json");
        let run_id = "a".repeat(40);
        let task_id = "b".repeat(40);
        let manifest = json!({
            "schema_version": "evaluation.task_set/v6",
            "trial_count": 1,
            "tasks": [{
                "task_id": task_id,
                "description": "path budget preflight",
                "capabilities": ["rust"],
                "workspace": {
                    "source": {"type": "local", "path": "missing-source"}
                },
                "agent": {
                    "instructions": "inspect"
                },
                "evaluator": {
                    "baseline": {"commands": [{"argv": ["cargo", "test"]}]},
                    "public": {"commands": [{"argv": ["cargo", "test"]}]},
                    "hidden": {"commands": [{"argv": ["cargo", "test", "--hidden"]}]}
                }
            }]
        });
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("manifest json"),
        )
        .expect("manifest");
        let output_root = temp.path().join("r".repeat(40));
        let run_dir = output_root.join(&run_id);
        let params = EvaluationRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id,
            output_root: Some(output_root.to_string_lossy().into_owned()),
            max_workers: 1,
        };
        let mut trace_store =
            singularity_store::SessionStore::open(":memory:").expect("trace store");

        let error = run_evaluation(
            &params,
            Arc::new(SourceSandboxBackend),
            &unconfigured_provider_snapshot(),
            &CancellationToken::new(),
            &mut trace_store,
        )
        .expect_err("long evaluation paths must fail before execution");

        assert!(
            error
                .to_string()
                .contains("evaluation path budget exceeded")
        );
        assert!(!run_dir.exists());
    }

    #[test]
    fn workspace_snapshot_detects_add_modify_and_delete() {
        let temp = tempfile::tempdir().expect("temp");
        fs::write(temp.path().join("a.txt"), "before").expect("write a");
        fs::write(temp.path().join("b.txt"), "delete").expect("write b");
        let before = snapshot_workspace(temp.path()).expect("before");
        fs::write(temp.path().join("a.txt"), "after").expect("modify a");
        fs::remove_file(temp.path().join("b.txt")).expect("delete b");
        fs::write(temp.path().join("c.txt"), "add").expect("add c");
        let after = snapshot_workspace(temp.path()).expect("after");
        assert_eq!(
            super::workspace::changed_paths(&before, &after),
            ["a.txt", "b.txt", "c.txt"]
        );
    }

    #[test]
    fn evaluation_change_evidence_ignores_toolchain_artifacts_but_keeps_source_changes() {
        let temp = tempfile::tempdir().expect("workspace");
        fs::create_dir_all(temp.path().join("src")).expect("src");
        fs::create_dir_all(temp.path().join("target")).expect("tracked target");
        fs::create_dir_all(temp.path().join("coverage")).expect("tracked coverage");
        fs::write(temp.path().join("src/lib.rs"), "before").expect("source");
        fs::write(temp.path().join("target/tracked.rs"), "before").expect("tracked target file");
        fs::write(temp.path().join("coverage/tracked.txt"), "before")
            .expect("tracked coverage file");
        let pristine_source = snapshot_workspace(temp.path()).expect("pristine source");
        let before = pristine_source.clone();

        fs::create_dir_all(temp.path().join("target/debug")).expect("cargo target");
        fs::write(temp.path().join("target/debug/app"), "binary").expect("cargo output");
        fs::create_dir_all(temp.path().join("python/__pycache__")).expect("python cache");
        fs::write(
            temp.path()
                .join("python/__pycache__/module.cpython-312.pyc"),
            "bytecode",
        )
        .expect("python bytecode");
        fs::create_dir_all(temp.path().join("node_modules/.cache")).expect("node cache");
        fs::write(temp.path().join("node_modules/.cache/bundle"), "cache").expect("node output");
        fs::create_dir_all(temp.path().join("generated")).expect("unknown output");
        fs::write(temp.path().join("generated/cache.bin"), "unknown").expect("unknown artifact");
        fs::create_dir(temp.path().join("unknown-empty")).expect("unknown empty directory");
        fs::write(temp.path().join("target/tracked.rs"), "after")
            .expect("modify tracked target file");
        fs::remove_file(temp.path().join("coverage/tracked.txt"))
            .expect("delete tracked coverage file");
        fs::write(temp.path().join("src/new.rs"), "user source").expect("new source");
        let after = snapshot_workspace(temp.path()).expect("after");
        let changed_files = evaluation_changed_paths(&before, &after, &pristine_source);

        let evidence = workspace_change_evidence(&before, &after, &pristine_source);
        let paths = evidence
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            changed_files,
            [
                "coverage/tracked.txt",
                "generated/cache.bin",
                "src/new.rs",
                "target/tracked.rs",
                "unknown-empty",
            ]
        );
        assert_eq!(
            paths,
            [
                "coverage/tracked.txt",
                "generated/cache.bin",
                "src/new.rs",
                "target/tracked.rs",
                "unknown-empty",
            ]
        );
    }

    #[test]
    fn evaluation_reuses_standard_workspace_policy_for_write_and_command_operations() {
        let policy = workspace_policy(PermissionProfileName::WorkspaceWrite, ApprovalPolicy::Never);
        let write = policy.evaluate(&singularity_policy::PermissionRequest::new(
            singularity_policy::ToolId::new(TOOL_PATCH).expect("tool id"),
            PermissionOperation::Write,
            PermissionResource::WorkspacePath(
                WorkspaceRelativePath::from_canonical("src2/lib.rs").expect("path"),
            ),
        ));
        let command = policy.evaluate(&singularity_policy::PermissionRequest::new(
            singularity_policy::ToolId::new(TOOL_COMMAND).expect("tool id"),
            PermissionOperation::Execute,
            PermissionResource::Tool(
                singularity_policy::ToolId::new(TOOL_COMMAND).expect("tool id"),
            ),
        ));

        assert_eq!(write.outcome, PermissionDecisionOutcome::Allow);
        assert_eq!(command.outcome, PermissionDecisionOutcome::Allow);
    }

    struct SourceSandboxBackend;

    impl SandboxBackend for SourceSandboxBackend {
        fn name(&self) -> &'static str {
            "source_test"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict().with_change_detection()
        }

        fn preflight(
            &self,
            workspace: &Path,
            _cancellation: &CancellationToken,
        ) -> SandboxPreflightReport {
            assert!(
                workspace.is_absolute(),
                "evaluation preflight must pass one canonical workspace to every probe"
            );
            supported_sandbox_preflight(self.name())
        }

        fn probe_executable(
            &self,
            _workspace: &Path,
            _executable: &str,
            _environment: &CommandEnvironmentPolicy,
        ) -> ExecutableAvailability {
            ExecutableAvailability::Available
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            if !request.is_trusted_workspace_preparation() {
                assert_eq!(request.network.mode, SandboxNetworkMode::Denied);
                assert_eq!(
                    request.filesystem.mode,
                    SandboxFilesystemMode::WorkspaceWrite
                );
                assert_eq!(request.environment, CommandEnvironmentPolicy::Isolated);
                #[cfg(windows)]
                let is_baseline = request.argv.as_slice() == ["cmd.exe", "/d", "/c", "exit", "1"];
                #[cfg(not(windows))]
                let is_baseline = request.argv.as_slice() == ["false"];
                let result = if is_baseline {
                    CommandResult::executed(
                        &request.command_id,
                        1,
                        0,
                        "",
                        "expected failure",
                        false,
                    )
                } else {
                    CommandResult::completed(&request.command_id, "ok")
                };
                return result
                    .with_workspace_mutation(WorkspaceMutation::Unchanged)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            if request.argv.as_slice() == ["git", "--version"] {
                return CommandResult::completed(&request.command_id, "git version 2.55.0")
                    .with_workspace_mutation(WorkspaceMutation::Unchanged)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            if request.argv.get(1).map(String::as_str) == Some("init") {
                return CommandResult::completed(&request.command_id, "prepared")
                    .with_workspace_mutation(WorkspaceMutation::Changed)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            if request.argv.get(1).map(String::as_str) == Some("clone") {
                assert_eq!(request.argv.get(2).map(String::as_str), Some("--quiet"));
                assert_eq!(request.argv.get(3).map(String::as_str), Some("--revision"));
                assert_eq!(
                    request.argv.get(4).map(String::as_str),
                    Some("0123456789abcdef0123456789abcdef01234567")
                );
                let source = Path::new(&request.cwd).join(SOURCE_DIR);
                fs::create_dir(&source).expect("source directory");
                fs::write(source.join("README.md"), "fixture").expect("source file");
            } else if request.argv.get(3).map(String::as_str) == Some("rev-parse") {
                return CommandResult::completed(
                    &request.command_id,
                    "0123456789abcdef0123456789abcdef01234567",
                )
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            } else if request.argv.get(3).map(String::as_str) == Some("symbolic-ref") {
                return CommandResult::executed(&request.command_id, 1, 0, "", "", false)
                    .with_workspace_mutation(WorkspaceMutation::Unchanged)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            } else {
                panic!("unexpected source preparation command: {:?}", request.argv);
            }
            CommandResult::completed(&request.command_id, "ok")
                .with_workspace_mutation(WorkspaceMutation::Changed)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
    }

    struct LegacyGitSourceSandboxBackend;

    impl SandboxBackend for LegacyGitSourceSandboxBackend {
        fn name(&self) -> &'static str {
            "legacy_git_source_test"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict().with_change_detection()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            assert!(request.is_trusted_workspace_preparation());
            if request.argv.as_slice() == ["git", "--version"] {
                return CommandResult::completed(&request.command_id, "git version 2.43.0")
                    .with_workspace_mutation(WorkspaceMutation::Unchanged)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            if request.argv.get(1).map(String::as_str) == Some("clone") {
                assert_eq!(request.argv.get(2).map(String::as_str), Some("--quiet"));
                assert_eq!(
                    request.argv.get(3).map(String::as_str),
                    Some("--no-checkout")
                );
                let source = Path::new(&request.cwd).join(SOURCE_DIR);
                fs::create_dir(&source).expect("source directory");
                fs::write(source.join("README.md"), "fixture").expect("source file");
                return CommandResult::completed(&request.command_id, "ok")
                    .with_workspace_mutation(WorkspaceMutation::Changed)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            if request.argv.get(3).map(String::as_str) == Some("checkout") {
                assert_eq!(request.argv.get(5).map(String::as_str), Some("--detach"));
                return CommandResult::completed(&request.command_id, "ok")
                    .with_workspace_mutation(WorkspaceMutation::Changed)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            if request.argv.get(3).map(String::as_str) == Some("rev-parse") {
                return CommandResult::completed(
                    &request.command_id,
                    "0123456789abcdef0123456789abcdef01234567",
                )
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            if request.argv.get(3).map(String::as_str) == Some("symbolic-ref") {
                return CommandResult::executed(&request.command_id, 1, 0, "", "", false)
                    .with_workspace_mutation(WorkspaceMutation::Unchanged)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            panic!(
                "unexpected legacy source preparation command: {:?}",
                request.argv
            );
        }
    }

    struct UnsupportedPreflightBackend {
        executions: Arc<AtomicUsize>,
    }

    impl SandboxBackend for UnsupportedPreflightBackend {
        fn name(&self) -> &'static str {
            "unsupported_preflight_test"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict().with_change_detection()
        }

        fn preflight(
            &self,
            _workspace: &Path,
            _cancellation: &CancellationToken,
        ) -> SandboxPreflightReport {
            let mut report = supported_sandbox_preflight(self.name());
            report.outcome = SandboxPreflightOutcome::Unsupported;
            report.error_code = Some("sandbox_preflight_test_unsupported".to_string());
            report.missing_capabilities = vec!["transactional_workspace".to_string()];
            report.transactional_workspace = SandboxPreflightFact::Failed;
            report
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            self.executions.fetch_add(1, Ordering::SeqCst);
            CommandResult::backend_error(
                &request.command_id,
                "unsupported preflight must prevent command execution",
            )
            .with_workspace_mutation(WorkspaceMutation::Unknown)
            .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
    }

    struct TaskWorkspaceUnavailableBackend {
        executions: Arc<AtomicUsize>,
        released_workspaces: Arc<Mutex<Vec<PathBuf>>>,
        release_error: bool,
    }

    impl SandboxBackend for TaskWorkspaceUnavailableBackend {
        fn name(&self) -> &'static str {
            "task_workspace_unavailable_test"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict().with_change_detection()
        }

        fn preflight(
            &self,
            _workspace: &Path,
            _cancellation: &CancellationToken,
        ) -> SandboxPreflightReport {
            supported_sandbox_preflight(self.name())
        }

        fn release_workspace_observation(&self, workspace: &Path) -> Result<(), String> {
            self.released_workspaces
                .lock()
                .expect("release workspace tracking lock")
                .push(workspace.to_path_buf());
            if self.release_error {
                Err("test observation release failure".to_string())
            } else {
                Ok(())
            }
        }

        fn probe_executable(
            &self,
            _workspace: &Path,
            _executable: &str,
            _environment: &CommandEnvironmentPolicy,
        ) -> ExecutableAvailability {
            ExecutableAvailability::Available
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            self.executions.fetch_add(1, Ordering::SeqCst);
            assert!(!request.is_trusted_workspace_preparation());
            CommandResult::backend_error(&request.command_id, "task workspace rejected")
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
    }

    struct UnavailableExecutableBackend {
        executions: Arc<AtomicUsize>,
    }

    impl SandboxBackend for UnavailableExecutableBackend {
        fn name(&self) -> &'static str {
            "unavailable_executable_test"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict().with_change_detection()
        }

        fn preflight(
            &self,
            _workspace: &Path,
            _cancellation: &CancellationToken,
        ) -> SandboxPreflightReport {
            supported_sandbox_preflight(self.name())
        }

        fn probe_executable(
            &self,
            _workspace: &Path,
            _executable: &str,
            _environment: &CommandEnvironmentPolicy,
        ) -> ExecutableAvailability {
            ExecutableAvailability::Unavailable
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            self.executions.fetch_add(1, Ordering::SeqCst);
            CommandResult::completed(&request.command_id, "task workspace available")
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
    }

    struct RemoteSourceProbeBackend {
        calls: AtomicUsize,
        fail_probe: bool,
    }

    impl SandboxBackend for RemoteSourceProbeBackend {
        fn name(&self) -> &'static str {
            "remote_source_probe_test"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict().with_change_detection()
        }

        fn preflight(
            &self,
            _workspace: &Path,
            _cancellation: &CancellationToken,
        ) -> SandboxPreflightReport {
            supported_sandbox_preflight(self.name())
        }

        fn probe_executable(
            &self,
            _workspace: &Path,
            _executable: &str,
            _environment: &CommandEnvironmentPolicy,
        ) -> ExecutableAvailability {
            ExecutableAvailability::Available
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !request.is_trusted_workspace_preparation() {
                assert_eq!(request.network.mode, SandboxNetworkMode::Denied);
                assert_eq!(
                    request.filesystem.mode,
                    SandboxFilesystemMode::WorkspaceWrite
                );
                assert_eq!(request.environment, CommandEnvironmentPolicy::Isolated);
                return CommandResult::completed(&request.command_id, "task workspace available")
                    .with_workspace_mutation(WorkspaceMutation::Unchanged)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            if request.argv.get(1).map(String::as_str) == Some("ls-remote") {
                assert_eq!(request.network.mode, SandboxNetworkMode::Allowed);
                assert!(request.is_trusted_workspace_preparation());
                assert_eq!(request.filesystem.mode, SandboxFilesystemMode::ReadOnly);
                assert_eq!(request.argv.len(), 5);
                assert_eq!(request.argv[4], "https://example.invalid/preflight.git");
                let result = if self.fail_probe {
                    CommandResult::executed(
                        &request.command_id,
                        2,
                        0,
                        "",
                        "remote source unavailable",
                        false,
                    )
                } else {
                    CommandResult::completed(&request.command_id, "remote source reachable")
                };
                return result
                    .with_workspace_mutation(WorkspaceMutation::Unknown)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            assert!(request.is_trusted_workspace_preparation());
            CommandResult::completed(&request.command_id, "prepared")
                .with_workspace_mutation(WorkspaceMutation::Changed)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
    }

    struct UnknownPreparationBackend {
        executions: Arc<AtomicUsize>,
    }

    impl SandboxBackend for UnknownPreparationBackend {
        fn name(&self) -> &'static str {
            "unknown_preparation_test"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict().with_change_detection()
        }

        fn preflight(
            &self,
            _workspace: &Path,
            _cancellation: &CancellationToken,
        ) -> SandboxPreflightReport {
            supported_sandbox_preflight(self.name())
        }

        fn probe_executable(
            &self,
            _workspace: &Path,
            _executable: &str,
            _environment: &CommandEnvironmentPolicy,
        ) -> ExecutableAvailability {
            ExecutableAvailability::Available
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            self.executions.fetch_add(1, Ordering::SeqCst);
            if !request.is_trusted_workspace_preparation() {
                return CommandResult::completed(&request.command_id, "task workspace available")
                    .with_workspace_mutation(WorkspaceMutation::Unchanged)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            if request.argv.get(1).map(String::as_str) == Some("ls-remote") {
                assert_eq!(request.network.mode, SandboxNetworkMode::Allowed);
                assert!(request.is_trusted_workspace_preparation());
                assert_eq!(request.filesystem.mode, SandboxFilesystemMode::ReadOnly);
                assert_eq!(request.argv.len(), 5);
                return CommandResult::completed(&request.command_id, "remote source reachable")
                    .with_workspace_mutation(WorkspaceMutation::Unchanged)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            assert!(request.is_trusted_workspace_preparation());
            CommandResult::completed(&request.command_id, "unverified")
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
    }

    struct FixedPreparedWorkspaceObserver {
        observation: PreparedWorkspaceObservation,
        checkpoint_calls: Arc<AtomicUsize>,
    }

    impl singularity_sandbox::PreparedWorkspaceObserver for FixedPreparedWorkspaceObserver {
        fn checkpoint(&mut self) -> Result<PreparedWorkspaceObservation, String> {
            self.checkpoint_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.observation.clone())
        }
    }

    #[derive(Default)]
    struct AgentLoopReachBackend {
        setup_calls: AtomicUsize,
        observer_baseline: Mutex<Option<PreparedWorkspaceObservation>>,
        observer_checkpoint_calls: Arc<AtomicUsize>,
    }

    impl SandboxBackend for AgentLoopReachBackend {
        fn name(&self) -> &'static str {
            "agent_loop_reach_test"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict().with_change_detection()
        }

        fn preflight(
            &self,
            _workspace: &Path,
            _cancellation: &CancellationToken,
        ) -> SandboxPreflightReport {
            supported_sandbox_preflight(self.name())
        }

        fn probe_executable(
            &self,
            _workspace: &Path,
            _executable: &str,
            _environment: &CommandEnvironmentPolicy,
        ) -> ExecutableAvailability {
            ExecutableAvailability::Available
        }

        fn observe_prepared_workspace(
            &self,
            workspace: &Path,
        ) -> Result<Option<Box<dyn singularity_sandbox::PreparedWorkspaceObserver>>, String>
        {
            if workspace.file_name().and_then(|name| name.to_str()) != Some(AGENT_DIR) {
                return Ok(None);
            }
            let observation = self
                .observer_baseline
                .lock()
                .map_err(|_| "agent observer fixture lock poisoned".to_string())?
                .clone();
            Ok(observation.map(|observation| {
                Box::new(FixedPreparedWorkspaceObserver {
                    observation,
                    checkpoint_calls: Arc::clone(&self.observer_checkpoint_calls),
                }) as Box<dyn singularity_sandbox::PreparedWorkspaceObserver>
            }))
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            let result = if request.argv.first().map(String::as_str) == Some("prepare-once") {
                self.setup_calls.fetch_add(1, Ordering::SeqCst);
                CommandResult::completed(&request.command_id, "prepared")
                    .with_workspace_mutation(WorkspaceMutation::Changed)
            } else if request.argv.first().map(String::as_str) == Some("verify-baseline") {
                CommandResult::executed(&request.command_id, 1, 0, "", "expected failure", false)
                    .with_workspace_mutation(WorkspaceMutation::Unchanged)
            } else {
                CommandResult::completed(&request.command_id, "verified")
                    .with_workspace_mutation(WorkspaceMutation::Unchanged)
            };
            result.with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
    }

    #[cfg(windows)]
    #[derive(Default)]
    struct ExclusiveVerificationBackend {
        active: AtomicUsize,
    }

    #[cfg(windows)]
    impl SandboxBackend for ExclusiveVerificationBackend {
        fn name(&self) -> &'static str {
            "exclusive_verification_test"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict().with_change_detection()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            if self.active.fetch_add(1, Ordering::SeqCst) != 0 {
                self.active.fetch_sub(1, Ordering::SeqCst);
                return CommandResult::backend_error(
                    &request.command_id,
                    "shared protected marker lease is already held",
                )
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable);
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
            self.active.fetch_sub(1, Ordering::SeqCst);
            CommandResult::completed(&request.command_id, "verified")
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
    }

    #[cfg(windows)]
    #[test]
    fn public_and_hidden_verification_do_not_overlap_shared_sandbox_resources() {
        let temp = tempfile::tempdir().expect("temp");
        let agent = temp.path().join("agent");
        fs::create_dir(&agent).expect("agent");
        fs::write(agent.join("source.txt"), "source").expect("agent file");
        let plan = |stage| VerificationStagePlan {
            stage,
            seed: WorkspaceSeed::AgentOutput,
            expectation: CommandExpectation::Success,
            test_patch: None,
            commands: vec![command(&["verify"])],
        };
        let public_plan = plan(EvaluationStage::Public);
        let hidden_plan = plan(EvaluationStage::Hidden);
        let backend = Arc::new(ExclusiveVerificationBackend::default());
        let shared: SharedSandboxBackend = backend.clone();

        let ((public, _), (hidden, _)) =
            run_post_agent_verification_stages(&agent, &public_plan, &hidden_plan, &shared);

        assert_eq!(public.result.status, StageStatus::Passed);
        assert_eq!(hidden.result.status, StageStatus::Passed);
        assert_eq!(backend.active.load(Ordering::SeqCst), 0);
    }

    struct EvaluatorPatchSandboxBackend {
        calls: AtomicUsize,
    }

    impl SandboxBackend for EvaluatorPatchSandboxBackend {
        fn name(&self) -> &'static str {
            "evaluator_patch_test"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if request.is_trusted_workspace_preparation() {
                panic!("evaluator patches must retain ordinary protected-path enforcement");
            }
            if request.argv.as_slice() == ["verify"] {
                assert_eq!(request.argv.as_slice(), ["verify"]);
                return CommandResult::executed(
                    &request.command_id,
                    1,
                    0,
                    "",
                    "expected failure",
                    false,
                )
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            assert_eq!(
                request.argv.get(1).map(String::as_str),
                Some(if cfg!(windows) {
                    "--git-dir=NUL"
                } else {
                    "--git-dir=/dev/null"
                })
            );
            assert_eq!(
                request.argv.get(2).map(String::as_str),
                Some("--work-tree=.")
            );
            assert_eq!(request.argv.get(3).map(String::as_str), Some("-c"));
            assert_eq!(
                request.argv.get(4).map(String::as_str),
                Some("core.autocrlf=false")
            );
            assert_eq!(request.argv.get(5).map(String::as_str), Some("apply"));
            assert_eq!(request.argv.get(6).map(String::as_str), Some("--no-index"));
            assert!(
                !request
                    .argv
                    .iter()
                    .any(|argument| argument == "--check" || argument == "--reject"),
                "Git's default whole-patch atomicity must not be replaced by a redundant check or partial application"
            );
            assert!(
                matches!(
                    request.argv.get(7..),
                    Some([
                        whitespace,
                        patch_file
                    ]) if whitespace == "--whitespace=nowarn"
                        && patch_file == EVALUATOR_PATCH_FILE
                ) || matches!(
                    request.argv.get(7..),
                    Some([
                        reverse,
                        whitespace,
                        patch_file
                    ]) if reverse == "--reverse"
                        && whitespace == "--whitespace=nowarn"
                        && patch_file == EVALUATOR_PATCH_FILE
                ),
                "only the fixed atomic apply and reverse operations are trusted"
            );
            CommandResult::completed(&request.command_id, "ok")
                .with_workspace_mutation(WorkspaceMutation::Changed)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
    }

    #[test]
    fn evaluator_patch_uses_only_fixed_strict_git_operations() {
        let workspace = tempfile::tempdir().expect("workspace");
        let patch: crate::EvaluatorTestPatch = serde_json::from_value(serde_json::json!({
            "format": "unified_diff",
            "content": "--- a/example.txt\n+++ b/example.txt\n"
        }))
        .expect("test patch");
        let backend = Arc::new(EvaluatorPatchSandboxBackend {
            calls: AtomicUsize::new(0),
        });
        let mut diagnostics = Vec::new();

        let patch_path =
            apply_evaluator_patch(workspace.path(), &patch, backend.clone(), &mut diagnostics)
                .expect("apply evaluator patch");
        revert_evaluator_patch(
            workspace.path(),
            &patch_path,
            backend.clone(),
            &mut diagnostics,
        )
        .expect("revert evaluator patch");

        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.phase.as_str())
                .collect::<Vec<_>>(),
            ["evaluator.apply_patch", "evaluator.revert_patch"]
        );
        assert!(!workspace.path().join(".git").exists());
        assert!(!workspace.path().join(EVALUATOR_PATCH_FILE).exists());
    }

    #[test]
    fn evaluator_patch_is_reverted_after_verification_failure() {
        let workspace = tempfile::tempdir().expect("workspace");
        let patch: crate::EvaluatorTestPatch = serde_json::from_value(serde_json::json!({
            "format": "unified_diff",
            "content": "--- a/example.txt\n+++ b/example.txt\n"
        }))
        .expect("test patch");
        let backend = Arc::new(EvaluatorPatchSandboxBackend {
            calls: AtomicUsize::new(0),
        });

        let execution = run_verification_after_setup(
            workspace.path(),
            Some(&patch),
            &[command(&["verify"])],
            CommandExpectation::Success,
            backend.clone(),
            Vec::new(),
        );

        assert_eq!(execution.result.status, StageStatus::Failed);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            execution
                .diagnostics
                .commands
                .iter()
                .map(|diagnostic| diagnostic.phase.as_str())
                .collect::<Vec<_>>(),
            [
                "evaluator.apply_patch",
                "verification.command.0",
                "evaluator.revert_patch",
            ]
        );
        assert!(!workspace.path().join(EVALUATOR_PATCH_FILE).exists());
    }

    #[test]
    fn evaluator_patch_leaves_an_unrelated_git_metadata_directory_untouched() {
        let workspace = tempfile::tempdir().expect("workspace");
        let git_dir = workspace.path().join(".singularity-evaluator-git");
        fs::create_dir(&git_dir).expect("source directory");
        fs::write(git_dir.join("owned.txt"), "source content").expect("source content");
        let patch: crate::EvaluatorTestPatch = serde_json::from_value(serde_json::json!({
            "format": "unified_diff",
            "content": "--- a/example.txt\n+++ b/example.txt\n"
        }))
        .expect("test patch");
        let backend = Arc::new(EvaluatorPatchSandboxBackend {
            calls: AtomicUsize::new(0),
        });
        let mut diagnostics = Vec::new();

        let patch_path =
            apply_evaluator_patch(workspace.path(), &patch, backend.clone(), &mut diagnostics)
                .expect("unrelated source directory must not affect patch application");
        revert_evaluator_patch(
            workspace.path(),
            &patch_path,
            backend.clone(),
            &mut diagnostics,
        )
        .expect("revert evaluator patch");

        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            fs::read_to_string(git_dir.join("owned.txt")).expect("preserved source content"),
            "source content"
        );
        assert!(!workspace.path().join(EVALUATOR_PATCH_FILE).exists());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires the installed native Windows strict sandbox"]
    fn native_windows_evaluator_patch_reverts_after_private_artifact_changes() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::write(workspace.path().join("example.txt"), "before\n").expect("source file");
        let patch: crate::EvaluatorTestPatch = serde_json::from_value(serde_json::json!({
            "format": "unified_diff",
            "content": "--- a/example.txt\n+++ b/example.txt\n@@ -1 +1 @@\n-before\n+after\n"
        }))
        .expect("test patch");
        let backend: SharedSandboxBackend =
            Arc::new(singularity_sandbox::PlatformSandboxBackend::new());
        let execution = run_verification_after_setup(
            workspace.path(),
            Some(&patch),
            &[command(&[
                "python",
                "-c",
                "import os, pathlib, sys; os.mkdir('.pytest_cache', 0o700); [pathlib.Path(f'.pytest_cache/node-{i:03}.json').write_text('artifact') for i in range(65)]; sys.exit(1)",
            ])],
            CommandExpectation::Success,
            backend.clone(),
            Vec::new(),
        );

        assert_eq!(execution.result.status, StageStatus::Failed);
        assert_eq!(
            execution.diagnostics.message.as_deref(),
            Some("1 verification command(s) failed")
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("example.txt")).expect("restored file"),
            "before\n"
        );
        let artifact = run_raw_command(
            workspace.path(),
            workspace.path(),
            vec![
                "python".to_string(),
                "-c".to_string(),
                "from pathlib import Path; print(Path('.pytest_cache/node-000.json').read_text())"
                    .to_string(),
            ],
            30,
            SandboxNetworkMode::Denied,
            backend,
        );
        assert_eq!(
            artifact.execution_status,
            CommandExecutionStatus::Completed,
            "{artifact:#?}"
        );
        assert_eq!(artifact.semantic_status, CommandSemanticStatus::Succeeded);
        assert_eq!(
            artifact.sandbox.enforcement,
            singularity_tools::SandboxBackendEnforcement::Strict
        );
        assert!(!artifact.sandbox.local_process_fallback);
        assert_eq!(artifact.stdout_preview.trim(), "artifact");
        assert_eq!(
            execution
                .diagnostics
                .commands
                .iter()
                .map(|diagnostic| diagnostic.phase.as_str())
                .collect::<Vec<_>>(),
            [
                "evaluator.apply_patch",
                "verification.command.0",
                "evaluator.revert_patch",
            ]
        );
        assert!(!workspace.path().join(EVALUATOR_PATCH_FILE).exists());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires the installed native Windows strict sandbox"]
    fn native_windows_evaluator_patch_rejects_paths_outside_the_workspace() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let outside = root.path().join("outside.txt");
        fs::write(&outside, "protected\n").expect("outside file");
        let patch: crate::EvaluatorTestPatch =
            serde_json::from_value(serde_json::json!({
                "format": "unified_diff",
                "content": "--- a/../outside.txt\n+++ b/../outside.txt\n@@ -1 +1 @@\n-protected\n+changed\n"
            }))
            .expect("test patch");
        let backend: SharedSandboxBackend =
            Arc::new(singularity_sandbox::PlatformSandboxBackend::new());
        let mut diagnostics = Vec::new();

        let blocker = apply_evaluator_patch(&workspace, &patch, backend, &mut diagnostics)
            .expect_err("workspace escape must be rejected");

        assert_eq!(blocker.kind, BlockerKind::WorkspacePreparation);
        assert_eq!(
            fs::read_to_string(outside).expect("outside file"),
            "protected\n"
        );
        assert!(!workspace.join(EVALUATOR_PATCH_FILE).exists());
    }

    struct PathBudgetSandboxBackend;

    impl SandboxBackend for PathBudgetSandboxBackend {
        fn name(&self) -> &'static str {
            "path_budget_test"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            CommandResult::executed(&request.command_id, 101, 0, "", "Filename too long", false)
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
    }

    struct MutatingVerificationBackend;

    impl SandboxBackend for MutatingVerificationBackend {
        fn name(&self) -> &'static str {
            "mutating_verification_test"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            CommandResult::completed(&request.command_id, "changed")
                .with_workspace_mutation(WorkspaceMutation::Changed)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
    }

    #[test]
    fn verification_workspace_mutation_blocks_shared_workspace_reuse() {
        let temp = tempfile::tempdir().expect("temp");
        let execution = run_verification_after_setup(
            temp.path(),
            None,
            &[command(&["verify"])],
            CommandExpectation::Success,
            Arc::new(MutatingVerificationBackend),
            Vec::new(),
        );

        assert_eq!(execution.result.status, StageStatus::Blocked);
        assert_eq!(
            execution.result.blocker.expect("sandbox blocker").kind,
            BlockerKind::Sandbox
        );
    }

    #[test]
    fn baseline_expected_failure_with_unknown_mutation_is_blocked() {
        let temp = tempfile::tempdir().expect("temp");
        let execution = run_verification_after_setup(
            temp.path(),
            None,
            &[command(&["cargo", "test"])],
            CommandExpectation::Failure,
            Arc::new(PathBudgetSandboxBackend),
            Vec::new(),
        );

        assert_eq!(execution.result.status, StageStatus::Blocked);
        assert_eq!(
            execution.result.blocker.expect("sandbox blocker").kind,
            BlockerKind::Sandbox
        );
        assert_eq!(execution.diagnostics.commands.len(), 1);
    }

    #[test]
    fn verification_expected_success_preserves_nonzero_as_task_failure() {
        let temp = tempfile::tempdir().expect("temp");
        let execution = run_verification_after_setup(
            temp.path(),
            None,
            &[command(&["cargo", "test"])],
            CommandExpectation::Success,
            Arc::new(PathBudgetSandboxBackend),
            Vec::new(),
        );

        assert_eq!(execution.result.status, StageStatus::Failed);
        assert!(execution.result.blocker.is_none());
        assert_eq!(execution.diagnostics.commands.len(), 1);
    }

    #[test]
    fn remote_source_uses_capability_bound_revision_and_verifies_checkout() {
        let temp = tempfile::tempdir().expect("temp");
        let task_dir = temp.path().join("task");
        fs::create_dir(&task_dir).expect("task directory");
        let source_dir = task_dir.join(SOURCE_DIR);
        let source = PlannedWorkspaceSource::RemoteGit {
            repository: RemoteRepository::new("https://github.com/example/example.git")
                .expect("repository"),
            commit: GitCommit::new("0123456789abcdef0123456789abcdef01234567").expect("commit"),
        };
        let backend = Arc::new(SourceSandboxBackend);
        let preflight = supported_sandbox_preflight(backend.name());

        let MaterializedSource {
            commands: diagnostics,
            snapshot,
            metrics,
            ..
        } = prepare_source(&source, &task_dir, &source_dir, backend, &preflight)
            .expect("prepare source");

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.phase.as_str())
                .collect::<Vec<_>>(),
            [
                "source.git_version",
                "source.git_clone",
                "source.git_verify_commit",
                "source.git_verify_detached",
            ]
        );
        assert!(source_dir.join("README.md").is_file());
        assert_eq!(snapshot.len(), 2);
        assert_eq!(metrics.full_scans, 1);
    }

    #[test]
    fn remote_source_uses_explicit_legacy_checkout_when_revision_is_unsupported() {
        let temp = tempfile::tempdir().expect("temp");
        let task_dir = temp.path().join("task");
        fs::create_dir(&task_dir).expect("task directory");
        let source_dir = task_dir.join(SOURCE_DIR);
        let source = PlannedWorkspaceSource::RemoteGit {
            repository: RemoteRepository::new("https://github.com/example/example.git")
                .expect("repository"),
            commit: GitCommit::new("0123456789abcdef0123456789abcdef01234567").expect("commit"),
        };
        let backend = Arc::new(LegacyGitSourceSandboxBackend);
        let preflight = supported_sandbox_preflight(backend.name());

        let MaterializedSource {
            commands,
            snapshot,
            metrics,
            ..
        } = prepare_source(&source, &task_dir, &source_dir, backend, &preflight)
            .expect("prepare source through the legacy Git path");

        assert_eq!(
            commands
                .iter()
                .map(|diagnostic| diagnostic.phase.as_str())
                .collect::<Vec<_>>(),
            [
                "source.git_version",
                "source.git_clone",
                "source.git_checkout",
                "source.git_verify_commit",
                "source.git_verify_detached",
            ]
        );
        assert!(source_dir.join("README.md").is_file());
        assert_eq!(snapshot.len(), 2);
        assert_eq!(metrics.full_scans, 1);
    }

    #[test]
    fn git_version_parser_accepts_platform_suffixes_and_rejects_unrelated_text() {
        assert_eq!(
            parse_git_version("git version 2.43.0.windows.1"),
            Some((2, 43))
        );
        assert_eq!(parse_git_version("git version 2.49.0"), Some((2, 49)));
        assert_eq!(parse_git_version("not a git version"), None);
    }

    #[test]
    fn trials_reuse_one_read_only_prepared_source_and_keep_stage_roots_isolated() {
        let temp = tempfile::tempdir().expect("temp");
        let fixture = temp.path().join("fixture");
        let run_dir = temp.path().join("run");
        fs::create_dir(&fixture).expect("fixture");
        fs::create_dir(&run_dir).expect("run directory");
        fs::write(fixture.join("README.md"), "seed").expect("fixture file");
        #[cfg(windows)]
        let baseline_argv = json!(["cmd.exe", "/d", "/c", "exit", "1"]);
        #[cfg(windows)]
        let public_verification_argv = json!(["cmd.exe", "/d", "/c", "exit", "0"]);
        #[cfg(windows)]
        let hidden_verification_argv = json!(["cmd.exe", "/d", "/c", "rem", "hidden"]);
        #[cfg(not(windows))]
        let baseline_argv = json!(["false"]);
        #[cfg(not(windows))]
        let public_verification_argv = json!(["true"]);
        #[cfg(not(windows))]
        let hidden_verification_argv = json!(["printf", "hidden"]);
        let manifest_json = json!({
            "schema_version": "evaluation.task_set/v6",
            "trial_count": 3,
            "tasks": [{
                "task_id": "source-reuse",
                "description": "verify source reuse",
                "capabilities": ["repository_context"],
                "workspace": {"source": {"type": "local", "path": "fixture"}},
                "agent": {
                    "instructions": "inspect"
                },
                "evaluator": {
                    "baseline": {"commands": [{"argv": baseline_argv}]},
                    "public": {"commands": [{"argv": public_verification_argv}]},
                    "hidden": {"commands": [{"argv": hidden_verification_argv}]}
                }
            }]
        });
        let manifest = EvaluationManifest::from_json_str(
            &serde_json::to_string(&manifest_json).expect("manifest JSON"),
            temp.path(),
        )
        .expect("manifest");
        let plan = manifest
            .workspace_plan(&TaskId::new("source-reuse").expect("task id"))
            .expect("plan");

        let run_id = RunId::new("source-reuse-run").expect("run id");
        let sandbox_backend: SharedSandboxBackend = Arc::new(SourceSandboxBackend);
        let provider_snapshot = unconfigured_provider_snapshot();
        let cancellation = CancellationToken::new();
        let mut trace_store =
            singularity_store::SessionStore::open(":memory:").expect("trace store");
        let shared_trace_store = Arc::new(Mutex::new(&mut trace_store));
        let sandbox_preflight = SandboxPreflightReport {
            outcome: SandboxPreflightOutcome::Supported,
            error_code: None,
            profile: "workspace_write_network_denied".to_string(),
            backend: sandbox_backend.name().to_string(),
            missing_capabilities: Vec::new(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            kernel: None,
            filesystem: None,
            overlayfs: SandboxPreflightFact::NotApplicable,
            user_namespace: SandboxPreflightFact::NotApplicable,
            mount_namespace: SandboxPreflightFact::NotApplicable,
            pid_namespace: SandboxPreflightFact::NotApplicable,
            network_namespace: SandboxPreflightFact::NotApplicable,
            no_new_privs: SandboxPreflightFact::NotApplicable,
            seccomp: SandboxPreflightFact::NotApplicable,
            landlock: SandboxPreflightFact::NotApplicable,
            transactional_workspace: SandboxPreflightFact::Passed,
            network_denied: SandboxPreflightFact::Passed,
            protected_paths: SandboxPreflightFact::Passed,
        };
        let context = EvaluationRunContext {
            run_id: &run_id,
            run_dir: &run_dir,
            manifest_dir: temp.path(),
            sandbox_backend: &sandbox_backend,
            provider_snapshot: &provider_snapshot,
            cancellation: &cancellation,
            trace_store: shared_trace_store,
            trace_failures: Arc::new(Mutex::new(Vec::new())),
            sandbox_preflight: &sandbox_preflight,
        };
        let evaluation = run_task_trials(&context, &plan, 3);

        let task_root = run_dir.join("source-reuse");
        assert_eq!(
            fs::read_to_string(task_root.join(SOURCE_DIR).join("README.md"))
                .expect("prepared source"),
            "seed"
        );
        assert_eq!(evaluation.trials.len(), 3);
        for trial in 1usize..=3 {
            let trial_dir = task_root.join(format!("trial-{trial:04}"));
            assert!(trial_dir.is_dir());
            assert!(!trial_dir.join(SOURCE_DIR).exists());
            assert_eq!(
                evaluation.trials[trial - 1].result.status,
                EvaluationStatus::Blocked,
                "trial_result={:#?}\ndiagnostics={:#?}",
                evaluation.trials[trial - 1].result,
                evaluation.trials[trial - 1].diagnostics
            );
            assert_eq!(
                evaluation.trials[trial - 1]
                    .result
                    .blocker
                    .as_ref()
                    .map(|blocker| blocker.kind),
                Some(BlockerKind::ProviderConfiguration)
            );
            assert_eq!(
                evaluation.trials[trial - 1]
                    .result
                    .evidence
                    .provider_attempt_count,
                0
            );
            assert_eq!(
                evaluation.trials[trial - 1].diagnostics.source_full_scans,
                1
            );
        }

        let selected_run_dir = temp.path().join("selected-run");
        fs::create_dir(&selected_run_dir).expect("selected run directory");
        let mut selected_trace_store =
            singularity_store::SessionStore::open(":memory:").expect("selected trace store");
        let selected_shared_trace_store = Arc::new(Mutex::new(&mut selected_trace_store));
        let selected_context = EvaluationRunContext {
            run_id: &run_id,
            run_dir: &selected_run_dir,
            manifest_dir: temp.path(),
            sandbox_backend: &sandbox_backend,
            provider_snapshot: &provider_snapshot,
            cancellation: &cancellation,
            trace_store: selected_shared_trace_store,
            trace_failures: Arc::new(Mutex::new(Vec::new())),
            sandbox_preflight: &sandbox_preflight,
        };
        let selected_source = prepare_task_source(&selected_context, &plan);
        let selected =
            run_task_trials_inner(&selected_context, &plan, 3, &selected_source, Some(2));
        assert_eq!(selected.trials.len(), 1);
        assert_eq!(selected.trials[0].result.trial, 2);
        assert!(
            selected_run_dir
                .join("source-reuse")
                .join("trial-0002")
                .is_dir()
        );
    }

    fn write_preflight_manifest(
        root: &Path,
        task_id: &str,
        description: &str,
        trial_count: u32,
        source: Value,
    ) -> PathBuf {
        let manifest_path = root.join(format!("{task_id}.json"));
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&json!({
                "schema_version": "evaluation.task_set/v6",
                "trial_count": trial_count,
                "tasks": [{
                    "task_id": task_id,
                    "description": description,
                    "capabilities": ["repository_context"],
                    "workspace": {"source": source},
                    "agent": {
                        "instructions": "inspect README.md"
                    },
                    "evaluator": {
                        "baseline": {"commands": [{"argv": ["verify-baseline"]}]},
                        "public": {"commands": [{"argv": ["verify-public"]}]},
                        "hidden": {"commands": [{"argv": ["verify-hidden"]}]}
                    }
                }]
            }))
            .expect("manifest JSON"),
        )
        .expect("manifest file");
        manifest_path
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_linux_preflight_verifies_trusted_workspace_preparation() {
        let temp = tempfile::tempdir().expect("temp");
        let run_dir = temp.path().join("run");
        fs::create_dir(&run_dir).expect("run directory");
        let fixture = temp.path().join("fixture");
        fs::create_dir(&fixture).expect("fixture");
        fs::write(fixture.join("README.md"), "seed\n").expect("fixture README");
        let manifest_path = write_preflight_manifest(
            temp.path(),
            "native-linux-preflight",
            "native preflight verifies trusted Git preparation",
            1,
            json!({"type": "local", "path": "fixture"}),
        );
        let mut manifest_value: Value =
            serde_json::from_str(&fs::read_to_string(&manifest_path).expect("manifest"))
                .expect("manifest JSON");
        for stage in ["baseline", "public", "hidden"] {
            manifest_value["tasks"][0]["evaluator"][stage]["commands"][0]["argv"] =
                json!(["git", "--version"]);
        }
        manifest_value["tasks"][0]["evaluator"]["public_test_patch"] = json!({
            "format": "unified_diff",
            "content": "--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-seed\n+patched\n"
        });
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest_value).expect("manifest JSON"),
        )
        .expect("write trusted-preparation manifest");
        let manifest_json = fs::read_to_string(&manifest_path).expect("manifest");
        let manifest = EvaluationManifest::from_json_str(&manifest_json, temp.path())
            .expect("evaluation manifest");
        let plans = manifest
            .task_set()
            .tasks
            .iter()
            .map(|task| {
                manifest
                    .workspace_plan(&task.task_id)
                    .expect("workspace plan")
            })
            .collect::<Vec<_>>();
        let backend: SharedSandboxBackend =
            Arc::new(singularity_sandbox::LinuxSandboxBackend::new());

        let report = run_sandbox_preflight(&run_dir, &plans, &backend, &CancellationToken::new())
            .unwrap_or_else(|failure| panic!("native preflight failed: {:?}", failure.report));

        assert_eq!(report.outcome, SandboxPreflightOutcome::Supported);
        assert!(report.missing_capabilities.is_empty());
        assert!(report.proves_supported_contract_for(backend.name()));
        assert!(!run_dir.join(".sandbox-preflight").exists());
    }

    #[test]
    fn preflight_canonicalizes_a_relative_run_directory_before_backend_calls() {
        let current_dir = std::env::current_dir().expect("current directory");
        let temp = tempfile::Builder::new()
            .prefix("relative-preflight-")
            .tempdir_in(&current_dir)
            .expect("relative temp");
        let relative_temp = temp
            .path()
            .strip_prefix(&current_dir)
            .expect("temp below current directory");
        assert!(!relative_temp.is_absolute());
        let run_dir = relative_temp.join("run");
        fs::create_dir(&run_dir).expect("run directory");
        let fixture = relative_temp.join("fixture");
        fs::create_dir(&fixture).expect("fixture");
        fs::write(fixture.join("README.md"), "seed\n").expect("fixture README");
        let manifest_path = write_preflight_manifest(
            relative_temp,
            "relative-preflight",
            "preflight canonicalizes its run-owned scratch path",
            1,
            json!({"type": "local", "path": "fixture"}),
        );
        let manifest = EvaluationManifest::from_json_str(
            &fs::read_to_string(&manifest_path).expect("manifest"),
            relative_temp,
        )
        .expect("evaluation manifest");
        let plans = manifest
            .task_set()
            .tasks
            .iter()
            .map(|task| {
                manifest
                    .workspace_plan(&task.task_id)
                    .expect("workspace plan")
            })
            .collect::<Vec<_>>();
        let backend: SharedSandboxBackend = Arc::new(SourceSandboxBackend);

        let report = run_sandbox_preflight(&run_dir, &plans, &backend, &CancellationToken::new())
            .expect("relative run directory preflight");

        assert_eq!(report.outcome, SandboxPreflightOutcome::Supported);
        assert!(!run_dir.join(".sandbox-preflight").exists());
    }

    #[test]
    fn supported_preflight_reaches_agent_loop_and_calls_provider() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        use std::thread;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
        listener
            .set_nonblocking(true)
            .expect("nonblocking provider fixture");
        let provider_address = listener.local_addr().expect("provider address");
        let provider = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .expect("provider read timeout");
                        let mut reader = BufReader::new(
                            stream.try_clone().expect("clone provider fixture stream"),
                        );
                        let mut request_line = String::new();
                        reader
                            .read_line(&mut request_line)
                            .expect("provider request line");
                        let mut content_length = 0usize;
                        loop {
                            let mut line = String::new();
                            reader
                                .read_line(&mut line)
                                .expect("provider request header");
                            if line == "\r\n" || line.is_empty() {
                                break;
                            }
                            if let Some((name, value)) = line.split_once(':')
                                && name.eq_ignore_ascii_case("content-length")
                            {
                                content_length =
                                    value.trim().parse().expect("provider content length");
                            }
                        }
                        let mut request_body = vec![0; content_length];
                        reader
                            .read_exact(&mut request_body)
                            .expect("provider request body");
                        let body = br#"{"error":{"message":"fixture authentication rejected","type":"authentication_error"}}"#;
                        let response = format!(
                            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        stream
                            .write_all(response.as_bytes())
                            .expect("provider response headers");
                        stream.write_all(body).expect("provider response body");
                        return 1usize;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return 0;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("provider fixture accept failed: {error}"),
                }
            }
        });

        let temp = tempfile::tempdir().expect("temp");
        let fixture = temp.path().join("fixture");
        let output_root = temp.path().join("output");
        fs::create_dir(&fixture).expect("fixture");
        fs::write(fixture.join("README.md"), "seed").expect("fixture file");
        let manifest_path = write_preflight_manifest(
            temp.path(),
            "preflight-supported",
            "supported preflight reaches AgentLoop",
            1,
            json!({"type": "local", "path": "fixture"}),
        );
        let mut manifest: Value = serde_json::from_slice(
            &fs::read(&manifest_path).expect("read supported preflight manifest"),
        )
        .expect("supported preflight manifest JSON");
        manifest["tasks"][0]["workspace"]["setup_commands"] = json!([{"argv": ["prepare-once"]}]);
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("updated manifest JSON"),
        )
        .expect("updated manifest");
        let params = EvaluationRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "preflight-supported-run".to_string(),
            output_root: Some(output_root.to_string_lossy().into_owned()),
            max_workers: 1,
        };
        let base_url = format!("http://{provider_address}/v1");
        let provider_snapshot = ProviderConfigSnapshot::capture(|name| match name {
            "SINGULARITY_API_KEY" => Some("fixture-key".to_string()),
            "SINGULARITY_BASE_URL" => Some(base_url.clone()),
            "SINGULARITY_MODEL" => Some("fixture-model".to_string()),
            _ => None,
        });
        let mut trace_store =
            singularity_store::SessionStore::open(":memory:").expect("trace store");

        let backend = Arc::new(AgentLoopReachBackend::default());
        let response = run_evaluation(
            &params,
            backend.clone(),
            &provider_snapshot,
            &CancellationToken::new(),
            &mut trace_store,
        )
        .expect("provider rejection still publishes evaluation artifacts");
        let result = EvaluationResult::from_json_str(
            &fs::read_to_string(response.result_path.expect("result path"))
                .expect("result artifact"),
        )
        .expect("result v9");
        let provider_calls = provider.join().expect("provider fixture join");

        assert_eq!(
            provider_calls, 1,
            "AgentLoop must issue one provider request; result={result:?}"
        );
        assert_eq!(result.summary.configured_trial_count, 1);
        assert_eq!(result.summary.sampled_trial_count, 1);
        assert_eq!(result.summary.trial_count, 1);
        assert_eq!(backend.setup_calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.tasks.len(), 1);
        assert_eq!(result.tasks[0].trials.len(), 1);
        let agent = &result.tasks[0].trials[0].stages.agent;
        assert_eq!(agent.status, StageStatus::Blocked);
        assert!(
            agent.blocker.as_ref().is_some_and(|blocker| matches!(
                blocker.kind,
                BlockerKind::ProviderAuthentication | BlockerKind::ProviderResponse
            )),
            "agent={agent:?}"
        );
        let trial_dir = output_root
            .join("preflight-supported-run")
            .join("preflight-supported")
            .join("trial-0001");
        let workspace_dirs = fs::read_dir(&trial_dir)
            .expect("trial directory")
            .map(|entry| entry.expect("trial entry"))
            .filter(|entry| entry.file_type().expect("trial entry type").is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(workspace_dirs, [AGENT_DIR]);
    }

    #[test]
    fn agent_baseline_observer_blocks_before_provider_request() {
        use std::net::TcpListener;
        use std::thread;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
        listener
            .set_nonblocking(true)
            .expect("nonblocking provider fixture");
        let provider_address = listener.local_addr().expect("provider address");
        let provider = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut requests = 0usize;
            loop {
                match listener.accept() {
                    Ok((_stream, _)) => requests += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return requests;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("provider fixture accept failed: {error}"),
                }
            }
        });

        let temp = tempfile::tempdir().expect("temp");
        let fixture = temp.path().join("fixture");
        let output_root = temp.path().join("output");
        fs::create_dir(&fixture).expect("fixture");
        fs::write(fixture.join("README.md"), "seed").expect("fixture file");
        let manifest_path = write_preflight_manifest(
            temp.path(),
            "agent-observer-baseline-blocked",
            "baseline observer blocks before AgentLoop",
            1,
            json!({"type": "local", "path": "fixture"}),
        );
        let params = EvaluationRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "agent-observer-baseline-blocked-run".to_string(),
            output_root: Some(output_root.to_string_lossy().into_owned()),
            max_workers: 1,
        };
        let base_url = format!("http://{provider_address}/v1");
        let provider_snapshot = ProviderConfigSnapshot::capture(|name| match name {
            "SINGULARITY_API_KEY" => Some("fixture-key".to_string()),
            "SINGULARITY_BASE_URL" => Some(base_url.clone()),
            "SINGULARITY_MODEL" => Some("fixture-model".to_string()),
            _ => None,
        });
        let mut trace_store =
            singularity_store::SessionStore::open(":memory:").expect("trace store");
        let backend = Arc::new(AgentLoopReachBackend {
            observer_baseline: Mutex::new(Some(PreparedWorkspaceObservation::Changed(vec![
                "README.md".to_string(),
            ]))),
            ..AgentLoopReachBackend::default()
        });

        let response = run_evaluation(
            &params,
            backend.clone(),
            &provider_snapshot,
            &CancellationToken::new(),
            &mut trace_store,
        )
        .expect("baseline observer blocker publishes result");
        let result = EvaluationResult::from_json_str(
            &fs::read_to_string(response.result_path.expect("result path"))
                .expect("result artifact"),
        )
        .expect("result JSON");
        let provider_calls = provider.join().expect("provider fixture join");

        assert_eq!(provider_calls, 0, "provider must not receive a request");
        assert_eq!(
            backend.observer_checkpoint_calls.load(Ordering::SeqCst),
            1,
            "only the pre-AgentLoop baseline checkpoint should run"
        );
        let agent = &result.tasks[0].trials[0].stages.agent;
        assert_eq!(agent.status, StageStatus::Blocked);
        assert!(agent.blocker.as_ref().is_some_and(|blocker| {
            blocker.kind == BlockerKind::WorkspacePreparation
                && blocker
                    .message
                    .contains("before authoritative baseline snapshot")
        }));
    }

    #[test]
    fn source_preparation_batch_barrier_blocks_before_sampling_any_task() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        use std::thread;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider fixture");
        listener
            .set_nonblocking(true)
            .expect("nonblocking provider fixture");
        let provider_address = listener.local_addr().expect("provider address");
        let provider = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut requests = 0usize;
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        requests += 1;
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .expect("provider read timeout");
                        let mut reader = BufReader::new(
                            stream.try_clone().expect("clone provider fixture stream"),
                        );
                        let mut request_line = String::new();
                        reader
                            .read_line(&mut request_line)
                            .expect("provider request line");
                        let mut content_length = 0usize;
                        loop {
                            let mut line = String::new();
                            reader
                                .read_line(&mut line)
                                .expect("provider request header");
                            if line == "\r\n" || line.is_empty() {
                                break;
                            }
                            if let Some((name, value)) = line.split_once(':')
                                && name.eq_ignore_ascii_case("content-length")
                            {
                                content_length = value.trim().parse().expect("content length");
                            }
                        }
                        let mut request_body = vec![0; content_length];
                        reader
                            .read_exact(&mut request_body)
                            .expect("provider request body");
                        let body = br#"{"error":{"message":"fixture authentication rejected","type":"authentication_error"}}"#;
                        let response = format!(
                            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        stream
                            .write_all(response.as_bytes())
                            .expect("provider response headers");
                        stream.write_all(body).expect("provider response body");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return requests;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("provider fixture accept failed: {error}"),
                }
            }
        });

        let temp = tempfile::tempdir().expect("temp");
        let fixture = temp.path().join("fixture");
        let output_root = temp.path().join("output");
        fs::create_dir(&fixture).expect("fixture");
        fs::write(fixture.join("README.md"), "seed").expect("fixture file");
        let task = |task_id: &str, source: &str| {
            json!({
                "task_id": task_id,
                "description": "source preparation batch barrier",
                "capabilities": ["repository_context"],
                "workspace": {"source": {"type": "local", "path": source}},
                "agent": {
                    "instructions": "inspect README.md"
                },
                "evaluator": {
                    "baseline": {"commands": [{"argv": ["verify-baseline"]}]},
                    "public": {"commands": [{"argv": ["verify-public"]}]},
                    "hidden": {"commands": [{"argv": ["verify-hidden"]}]}
                }
            })
        };
        let manifest_path = temp.path().join("manifest.json");
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&json!({
                "schema_version": "evaluation.task_set/v6",
                "trial_count": 1,
                "tasks": [task("source-ok", "fixture"), task("source-blocked", "missing-source")]
            }))
            .expect("manifest JSON"),
        )
        .expect("manifest file");
        let params = EvaluationRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "source-batch-barrier-run".to_string(),
            output_root: Some(output_root.to_string_lossy().into_owned()),
            max_workers: 1,
        };
        let base_url = format!("http://{provider_address}/v1");
        let provider_snapshot = ProviderConfigSnapshot::capture(|name| match name {
            "SINGULARITY_API_KEY" => Some("fixture-key".to_string()),
            "SINGULARITY_BASE_URL" => Some(base_url.clone()),
            "SINGULARITY_MODEL" => Some("fixture-model".to_string()),
            _ => None,
        });
        let mut trace_store =
            singularity_store::SessionStore::open(":memory:").expect("trace store");

        let response = run_evaluation(
            &params,
            Arc::new(AgentLoopReachBackend::default()),
            &provider_snapshot,
            &CancellationToken::new(),
            &mut trace_store,
        )
        .expect("source blocker publishes a zero-sampling run");
        let requests = provider.join().expect("provider fixture join");
        assert_eq!(response.status, "blocked");
        assert!(response.tasks.is_empty());
        let result = EvaluationResult::from_json_str(
            &fs::read_to_string(response.result_path.expect("result path"))
                .expect("result artifact"),
        )
        .expect("result v9");
        assert!(result.tasks.is_empty());
        assert_eq!(result.summary.task_count, 2);
        assert_eq!(result.summary.trials_per_task, 1);
        assert_eq!(result.summary.configured_trial_count, 2);
        assert_eq!(result.summary.sampled_trial_count, 0);
        assert_eq!(result.summary.trial_count, 0);
        assert_eq!(
            result.blocker.as_ref().map(|blocker| blocker.kind),
            Some(BlockerKind::WorkspacePreparation)
        );
        assert_eq!(
            result
                .blocker
                .as_ref()
                .and_then(|blocker| blocker.code.as_deref()),
            Some("workspace_preparation_failed")
        );
        assert_eq!(
            requests, 0,
            "source preparation barrier must prevent provider requests"
        );
        let no_provider_attempt = match trace_store.list_trace("source-batch-barrier-run") {
            Ok(events) => events
                .iter()
                .all(|event| event.span_kind != Some(TraceSpanKind::ProviderAttempt)),
            Err(singularity_store::StoreError::NotFound(_)) => true,
            Err(error) => panic!("source barrier trace: {error}"),
        };
        assert!(
            no_provider_attempt,
            "source barrier must not create provider attempts"
        );
    }

    #[test]
    fn provider_configuration_blocker_is_run_level_and_not_a_sandbox_blocker() {
        let temp = tempfile::tempdir().expect("temp");
        let fixture = temp.path().join("fixture");
        let output_root = temp.path().join("output");
        fs::create_dir(&fixture).expect("fixture");
        fs::write(fixture.join("README.md"), "seed").expect("fixture file");
        let manifest_path = write_preflight_manifest(
            temp.path(),
            "provider-config-blocked",
            "provider configuration must block before source preparation",
            1,
            json!({"type": "local", "path": "fixture"}),
        );
        let params = EvaluationRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "provider-config-blocked-run".to_string(),
            output_root: Some(output_root.to_string_lossy().into_owned()),
            max_workers: 1,
        };
        let mut trace_store =
            singularity_store::SessionStore::open(":memory:").expect("trace store");
        let provider_snapshot = ProviderConfigSnapshot::capture(|name| match name {
            "SINGULARITY_API_KEY" => Some("fixture-key".to_string()),
            _ => None,
        });

        let response = run_evaluation(
            &params,
            Arc::new(SourceSandboxBackend),
            &provider_snapshot,
            &CancellationToken::new(),
            &mut trace_store,
        )
        .expect("provider configuration blocker publishes a run-level result");
        assert_eq!(response.status, "blocked");
        assert!(response.tasks.is_empty());
        let result = EvaluationResult::from_json_str(
            &fs::read_to_string(response.result_path.expect("result path"))
                .expect("result artifact"),
        )
        .expect("result v9");
        assert_eq!(
            result.blocker.as_ref().map(|blocker| blocker.kind),
            Some(BlockerKind::ProviderConfiguration)
        );
        assert_ne!(
            result.blocker.as_ref().map(|blocker| blocker.kind),
            Some(BlockerKind::Sandbox)
        );
        assert_eq!(result.summary.configured_trial_count, 1);
        assert_eq!(result.summary.sampled_trial_count, 0);
        assert_eq!(result.summary.trial_count, 0);
        let no_provider_attempt = match trace_store.list_trace("provider-config-blocked-run") {
            Ok(events) => events
                .iter()
                .all(|event| event.span_kind != Some(TraceSpanKind::ProviderAttempt)),
            Err(singularity_store::StoreError::NotFound(_)) => true,
            Err(error) => panic!("provider configuration trace: {error}"),
        };
        assert!(no_provider_attempt);
        assert!(
            !output_root
                .join("provider-config-blocked-run/provider-config-blocked")
                .exists(),
            "provider configuration must be checked before source materialization"
        );
    }

    #[test]
    fn unsupported_preflight_publishes_one_environment_blocker_without_trials_or_commands() {
        let temp = tempfile::tempdir().expect("temp");
        let fixture = temp.path().join("fixture");
        let output_root = temp.path().join("output");
        fs::create_dir(&fixture).expect("fixture");
        fs::write(fixture.join("README.md"), "seed").expect("fixture file");
        let manifest_path = write_preflight_manifest(
            temp.path(),
            "preflight-blocked",
            "preflight must fail before sampling",
            2,
            json!({"type": "local", "path": "fixture"}),
        );
        let params = EvaluationRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "preflight-blocked-run".to_string(),
            output_root: Some(output_root.to_string_lossy().into_owned()),
            max_workers: 1,
        };
        let executions = Arc::new(AtomicUsize::new(0));
        let mut trace_store =
            singularity_store::SessionStore::open(":memory:").expect("trace store");

        let response = run_evaluation(
            &params,
            Arc::new(UnsupportedPreflightBackend {
                executions: Arc::clone(&executions),
            }),
            &unconfigured_provider_snapshot(),
            &CancellationToken::new(),
            &mut trace_store,
        )
        .expect("preflight blocker publishes typed artifacts");

        assert_eq!(response.status, "blocked");
        assert!(response.tasks.is_empty());
        assert_eq!(executions.load(Ordering::SeqCst), 0);
        let result = EvaluationResult::from_json_str(
            &fs::read_to_string(response.result_path.expect("result path"))
                .expect("result artifact"),
        )
        .expect("result v9");
        assert!(result.tasks.is_empty());
        assert_eq!(result.summary.configured_trial_count, 2);
        assert_eq!(result.summary.sampled_trial_count, 0);
        assert_eq!(result.summary.trial_count, 0);
        assert_eq!(result.summary.blocked_trial_count, 0);
        assert_eq!(
            result
                .blocker
                .as_ref()
                .and_then(|blocker| blocker.code.as_deref()),
            Some("sandbox_preflight_test_unsupported")
        );
        let run_dir = output_root.join("preflight-blocked-run");
        assert!(!run_dir.join("preflight-blocked").exists());
        assert!(run_dir.join(PUBLICATION_DIR).is_dir());
    }

    #[test]
    fn task_workspace_preflight_blocks_before_sampling_when_ordinary_command_is_rejected() {
        let temp = tempfile::tempdir().expect("temp");
        let fixture = temp.path().join("fixture");
        let output_root = temp.path().join("output");
        fs::create_dir(&fixture).expect("fixture");
        fs::write(fixture.join("README.md"), "seed").expect("fixture file");
        let manifest_path = write_preflight_manifest(
            temp.path(),
            "task-workspace-blocked",
            "ordinary task workspace must be strict",
            2,
            json!({"type": "local", "path": "fixture"}),
        );
        let params = EvaluationRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "task-workspace-blocked-run".to_string(),
            output_root: Some(output_root.to_string_lossy().into_owned()),
            max_workers: 1,
        };
        let executions = Arc::new(AtomicUsize::new(0));
        let released_workspaces = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
        let mut trace_store =
            singularity_store::SessionStore::open(":memory:").expect("trace store");

        let response = run_evaluation(
            &params,
            Arc::new(TaskWorkspaceUnavailableBackend {
                executions: Arc::clone(&executions),
                released_workspaces: Arc::clone(&released_workspaces),
                release_error: true,
            }),
            &unconfigured_provider_snapshot(),
            &CancellationToken::new(),
            &mut trace_store,
        )
        .expect("task workspace blocker publishes typed artifacts");

        assert_eq!(response.status, "blocked");
        assert!(response.tasks.is_empty());
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        let released_workspaces = released_workspaces
            .lock()
            .expect("released workspace tracking lock")
            .clone();
        assert_eq!(released_workspaces.len(), 2);
        assert!(
            released_workspaces
                .iter()
                .any(|workspace| workspace.ends_with(Path::new("capability")))
        );
        assert!(released_workspaces.iter().any(|workspace| {
            workspace.ends_with(Path::new("task").join("trial-0001").join(AGENT_DIR))
        }));
        let result = EvaluationResult::from_json_str(
            &fs::read_to_string(response.result_path.expect("result artifact"))
                .expect("result artifact"),
        )
        .expect("result v9");
        assert_eq!(result.summary.configured_trial_count, 2);
        assert_eq!(result.summary.sampled_trial_count, 0);
        assert_eq!(result.summary.trial_count, 0);
        assert!(result.tasks.is_empty());
        assert_eq!(
            result
                .blocker
                .as_ref()
                .and_then(|blocker| blocker.code.as_deref()),
            Some("sandbox_preflight_task_workspace_unavailable")
        );
        let blocker_message = &result.blocker.as_ref().expect("run blocker").message;
        assert!(blocker_message.contains("task workspace rejected"));
        assert!(blocker_message.contains("test observation release failure"));
        let missing_capabilities = &result
            .sandbox_preflight
            .as_ref()
            .expect("sandbox preflight evidence")
            .missing_capabilities;
        assert!(missing_capabilities.contains(&"strict_task_workspace".to_string()));
        assert!(missing_capabilities.contains(&"workspace_observation_release".to_string()));
        assert!(
            !output_root
                .join("task-workspace-blocked-run/task-workspace-blocked")
                .exists()
        );
        assert!(
            output_root
                .join("task-workspace-blocked-run")
                .join(PUBLICATION_DIR)
                .is_dir()
        );
        let provider_attempts = trace_store
            .list_trace("task-workspace-blocked-run")
            .map(|events| {
                events
                    .iter()
                    .filter(|event| event.span_kind == Some(TraceSpanKind::ProviderAttempt))
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(provider_attempts, 0);
    }

    #[test]
    fn unavailable_task_executable_blocks_before_trials_provider_or_commands() {
        let temp = tempfile::tempdir().expect("temp");
        let fixture = temp.path().join("fixture");
        let output_root = temp.path().join("output");
        fs::create_dir(&fixture).expect("fixture");
        fs::write(fixture.join("README.md"), "seed").expect("fixture file");
        let manifest_path = write_preflight_manifest(
            temp.path(),
            "executable-blocked",
            "task executables must be available before sampling",
            2,
            json!({"type": "local", "path": "fixture"}),
        );
        let params = EvaluationRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "executable-blocked-run".to_string(),
            output_root: Some(output_root.to_string_lossy().into_owned()),
            max_workers: 1,
        };
        let executions = Arc::new(AtomicUsize::new(0));
        let mut trace_store =
            singularity_store::SessionStore::open(":memory:").expect("trace store");

        let response = run_evaluation(
            &params,
            Arc::new(UnavailableExecutableBackend {
                executions: Arc::clone(&executions),
            }),
            &unconfigured_provider_snapshot(),
            &CancellationToken::new(),
            &mut trace_store,
        )
        .expect("executable blocker publishes typed artifacts");

        assert_eq!(response.status, "blocked");
        assert!(response.tasks.is_empty());
        assert_eq!(
            executions.load(Ordering::SeqCst),
            1,
            "the ordinary strict task-workspace probe runs before executable discovery"
        );
        let no_provider_attempt = match trace_store.list_trace("executable-blocked-run") {
            Ok(events) => events
                .iter()
                .all(|event| event.span_kind != Some(TraceSpanKind::ProviderAttempt)),
            Err(singularity_store::StoreError::NotFound(_)) => true,
            Err(error) => panic!("preflight trace: {error}"),
        };
        assert!(
            no_provider_attempt,
            "unsupported executable preflight must make zero Provider calls"
        );
        let result = EvaluationResult::from_json_str(
            &fs::read_to_string(response.result_path.expect("result path"))
                .expect("result artifact"),
        )
        .expect("result v9");
        assert_eq!(result.summary.configured_trial_count, 2);
        assert_eq!(result.summary.sampled_trial_count, 0);
        assert_eq!(result.summary.trial_count, 0);
        assert!(result.tasks.is_empty());
        assert_eq!(
            result
                .blocker
                .as_ref()
                .and_then(|blocker| blocker.code.as_deref()),
            Some("sandbox_preflight_task_executable_unavailable")
        );
        assert_eq!(
            result
                .sandbox_preflight
                .as_ref()
                .map(|preflight| preflight.missing_capabilities.as_slice()),
            Some(
                [
                    "task_executable:verify-baseline".to_string(),
                    "task_executable:verify-hidden".to_string(),
                    "task_executable:verify-public".to_string(),
                ]
                .as_slice()
            )
        );
        assert!(
            !output_root
                .join("executable-blocked-run/executable-blocked")
                .exists()
        );
    }

    #[test]
    fn remote_source_preflight_probes_repository_before_trusted_setup() {
        let temp = tempfile::tempdir().expect("temp");
        let run_dir = temp.path().join("run");
        fs::create_dir(&run_dir).expect("run directory");
        let manifest_path = write_preflight_manifest(
            temp.path(),
            "remote-source-preflight",
            "remote source preflight probes repository transport",
            1,
            json!({
                "type": "remote_git",
                "repository": "https://example.invalid/preflight.git",
                "commit": "0000000000000000000000000000000000000000"
            }),
        );
        let manifest_json = fs::read_to_string(&manifest_path).expect("manifest");
        let manifest = EvaluationManifest::from_json_str(&manifest_json, temp.path())
            .expect("evaluation manifest");
        let plans = manifest
            .task_set()
            .tasks
            .iter()
            .map(|task| {
                manifest
                    .workspace_plan(&task.task_id)
                    .expect("workspace plan")
            })
            .collect::<Vec<_>>();
        let backend = Arc::new(RemoteSourceProbeBackend {
            calls: AtomicUsize::new(0),
            fail_probe: false,
        });
        let shared: SharedSandboxBackend = backend.clone();
        let report = run_sandbox_preflight(&run_dir, &plans, &shared, &CancellationToken::new())
            .expect("reachable remote source must pass preflight");

        assert_eq!(report.outcome, SandboxPreflightOutcome::Supported);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 3);
        assert!(!run_dir.join(".sandbox-preflight").exists());
    }

    #[test]
    fn unreachable_remote_repository_blocks_before_provider_sampling() {
        let temp = tempfile::tempdir().expect("temp");
        let output_root = temp.path().join("output");
        let manifest_path = write_preflight_manifest(
            temp.path(),
            "remote-source-blocked",
            "remote source preflight blocks unavailable source",
            2,
            json!({
                "type": "remote_git",
                "repository": "https://example.invalid/preflight.git",
                "commit": "0000000000000000000000000000000000000000"
            }),
        );
        let params = EvaluationRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "remote-source-blocked-run".to_string(),
            output_root: Some(output_root.to_string_lossy().into_owned()),
            max_workers: 1,
        };
        let backend = Arc::new(RemoteSourceProbeBackend {
            calls: AtomicUsize::new(0),
            fail_probe: true,
        });
        let mut trace_store =
            singularity_store::SessionStore::open(":memory:").expect("trace store");
        let response = run_evaluation(
            &params,
            backend.clone(),
            &unconfigured_provider_snapshot(),
            &CancellationToken::new(),
            &mut trace_store,
        )
        .expect("remote source blocker publishes typed artifacts");

        assert_eq!(response.status, "blocked");
        assert!(response.tasks.is_empty());
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        let result = EvaluationResult::from_json_str(
            &fs::read_to_string(response.result_path.expect("result path"))
                .expect("result artifact"),
        )
        .expect("result v9");
        assert_eq!(result.summary.configured_trial_count, 2);
        assert_eq!(result.summary.sampled_trial_count, 0);
        assert_eq!(
            result
                .blocker
                .as_ref()
                .and_then(|blocker| blocker.code.as_deref()),
            Some("sandbox_preflight_remote_source_unavailable")
        );
    }

    #[test]
    fn unverified_trusted_preparation_blocks_before_trials_or_provider() {
        let temp = tempfile::tempdir().expect("temp");
        let output_root = temp.path().join("output");
        let manifest_path = write_preflight_manifest(
            temp.path(),
            "preparation-blocked",
            "trusted preparation must be verified before sampling",
            2,
            json!({
                "type": "remote_git",
                "repository": "https://example.invalid/preflight.git",
                "commit": "0000000000000000000000000000000000000000"
            }),
        );
        let params = EvaluationRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "preparation-blocked-run".to_string(),
            output_root: Some(output_root.to_string_lossy().into_owned()),
            max_workers: 1,
        };
        let executions = Arc::new(AtomicUsize::new(0));
        let mut trace_store =
            singularity_store::SessionStore::open(":memory:").expect("trace store");

        let response = run_evaluation(
            &params,
            Arc::new(UnknownPreparationBackend {
                executions: Arc::clone(&executions),
            }),
            &unconfigured_provider_snapshot(),
            &CancellationToken::new(),
            &mut trace_store,
        )
        .expect("trusted preparation blocker publishes typed artifacts");

        assert_eq!(response.status, "blocked");
        assert!(response.tasks.is_empty());
        assert_eq!(executions.load(Ordering::SeqCst), 3);
        let result = EvaluationResult::from_json_str(
            &fs::read_to_string(response.result_path.expect("result path"))
                .expect("result artifact"),
        )
        .expect("result v9");
        assert!(result.tasks.is_empty());
        assert_eq!(result.summary.configured_trial_count, 2);
        assert_eq!(result.summary.sampled_trial_count, 0);
        assert_eq!(result.summary.trial_count, 0);
        assert_eq!(
            result
                .blocker
                .as_ref()
                .and_then(|blocker| blocker.code.as_deref()),
            Some("sandbox_preflight_trusted_preparation_unverified")
        );
        assert!(
            result
                .blocker
                .as_ref()
                .expect("run blocker")
                .message
                .contains("workspace mutation could not be verified")
        );
        assert_eq!(
            result
                .sandbox_preflight
                .as_ref()
                .map(|preflight| preflight.missing_capabilities.as_slice()),
            Some(["trusted_workspace_preparation".to_string()].as_slice())
        );
        assert!(
            !output_root
                .join("preparation-blocked-run/preparation-blocked")
                .exists()
        );
    }

    #[test]
    fn v6_blocked_run_publishes_v9_result_and_v4_evidence_as_one_artifact_set() {
        let temp = tempfile::tempdir().expect("temp");
        let fixture = temp.path().join("fixture");
        let output_root = temp.path().join("output");
        fs::create_dir(&fixture).expect("fixture");
        fs::write(fixture.join("README.md"), "seed").expect("fixture file");
        #[cfg(windows)]
        let baseline_argv = json!(["cmd.exe", "/d", "/c", "exit", "1"]);
        #[cfg(windows)]
        let public_argv = json!(["cmd.exe", "/d", "/c", "exit", "0"]);
        #[cfg(windows)]
        let hidden_argv = json!(["cmd.exe", "/d", "/c", "rem", "hidden"]);
        #[cfg(not(windows))]
        let baseline_argv = json!(["false"]);
        #[cfg(not(windows))]
        let public_argv = json!(["true"]);
        #[cfg(not(windows))]
        let hidden_argv = json!(["printf", "hidden"]);
        let manifest_path = temp.path().join("manifest.json");
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&json!({
                "schema_version": "evaluation.task_set/v6",
                "trial_count": 2,
                "tasks": [{
                    "task_id": "blocked-artifacts",
                    "description": "verify blocked artifacts",
                    "capabilities": ["repository_context"],
                    "workspace": {"source": {"type": "local", "path": "fixture"}},
                    "agent": {
                        "instructions": "inspect"
                    },
                    "evaluator": {
                        "baseline": {"commands": [{"argv": baseline_argv}]},
                        "public": {"commands": [{"argv": public_argv}]},
                        "hidden": {"commands": [{"argv": hidden_argv}]}
                    }
                }]
            }))
            .expect("manifest JSON"),
        )
        .expect("manifest file");
        let params = EvaluationRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "blocked-artifacts-run".to_string(),
            output_root: Some(output_root.to_string_lossy().into_owned()),
            max_workers: 1,
        };
        let mut trace_store =
            singularity_store::SessionStore::open(":memory:").expect("trace store");

        let response = run_evaluation(
            &params,
            Arc::new(SourceSandboxBackend),
            &ProviderConfigSnapshot::capture(|name| match name {
                "SINGULARITY_API_KEY" => Some("fixture-key".to_string()),
                "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:1/v1".to_string()),
                "SINGULARITY_MODEL" => Some("fixture-model".to_string()),
                _ => None,
            }),
            &CancellationToken::new(),
            &mut trace_store,
        )
        .expect("blocked run still publishes typed artifacts");

        let result_path = PathBuf::from(response.result_path.expect("result path"));
        let evidence_path = PathBuf::from(response.evidence_path.expect("evidence path"));
        let result = EvaluationResult::from_json_str(
            &fs::read_to_string(&result_path).expect("result artifact"),
        )
        .expect("result v9");
        assert_eq!(response.status, "blocked", "result={result:?}");
        let evidence = crate::EvaluationEvidence::from_json_str(
            &fs::read_to_string(&evidence_path).expect("evidence artifact"),
        )
        .expect("evidence v4");
        evidence
            .validate_against_result(&result)
            .expect("result/evidence binding");
        assert_eq!(result.summary.trials_per_task, 2);
        assert_eq!(result.summary.blocked_trial_count, 2);
        assert_eq!(result.summary.agent_scored_trial_count, 0);
        assert!(
            output_root
                .join("blocked-artifacts-run")
                .join(PUBLICATION_DIR)
                .join(PUBLICATION_MANIFEST_FILE)
                .is_file()
        );
    }

    #[test]
    fn report_source_provenance_records_tree_identity_without_remote_credentials() {
        let temp = tempfile::tempdir().expect("temp");
        let local_source = temp.path().join("local-source");
        let local_materialized = temp.path().join("local-materialized");
        fs::create_dir_all(local_source.join(".git")).expect("local git marker");
        fs::write(local_source.join("README.md"), "fixture").expect("local fixture");
        copy_tree_checked(&local_source, &local_materialized).expect("materialize local source");
        let snapshot = snapshot_workspace(&local_materialized).expect("source snapshot");

        let local = source_provenance(
            &PlannedWorkspaceSource::Local {
                path: local_source.clone(),
            },
            Some(workspace_snapshot_digest(&snapshot)),
            temp.path(),
        );
        assert_eq!(local.source_type, "local");
        assert_eq!(local.path, Some("local-source".to_string()));
        assert!(
            local
                .tree_digest
                .as_deref()
                .is_some_and(|digest| { digest.starts_with("sha256:") })
        );
        assert!(local.tree_digest_error.is_none());
        let local_serialized = serde_json::to_string(&local).expect("local source provenance");
        assert!(!local_serialized.contains(&temp.path().to_string_lossy().to_string()));

        let remote = source_provenance(
            &PlannedWorkspaceSource::RemoteGit {
                repository: RemoteRepository::new(
                    "https://operator:remote-secret@example.com/repo.git?token=private",
                )
                .expect("remote repository"),
                commit: GitCommit::new("0123456789abcdef0123456789abcdef01234567").expect("commit"),
            },
            Some(workspace_snapshot_digest(&snapshot)),
            temp.path(),
        );
        assert_eq!(remote.source_type, "remote_git");
        assert_eq!(
            remote.repository.as_deref(),
            Some("https://example.com/repo.git")
        );
        assert_eq!(
            remote.commit.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        let serialized = serde_json::to_string(&remote).expect("source provenance");
        assert!(!serialized.contains("remote-secret"));
        assert!(!serialized.contains("token=private"));
    }

    struct SerializationFailure;

    impl Serialize for SerializationFailure {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Err(serde::ser::Error::custom(
                "intentional serialization failure",
            ))
        }
    }

    #[test]
    fn atomic_publication_requires_complete_report_and_evidence_artifacts() {
        let temp = tempfile::tempdir().expect("temp");
        let run_id = RunId::new("atomic-publication").expect("run id");
        fs::create_dir(temp.path().join(PUBLICATION_DIR)).expect("blocking publication directory");
        fs::write(
            temp.path().join(PUBLICATION_DIR).join("sentinel"),
            "existing publication",
        )
        .expect("non-empty publication directory");

        let error = publish_evaluation_artifacts(
            temp.path(),
            &run_id,
            &json!({"status": "completed"}),
            &json!({"runner": RUNNER_NAME}),
            &json!({"schema_version": "evaluation.evidence/v4"}),
        )
        .expect_err("publish must fail");

        assert!(
            error
                .to_string()
                .contains("failed to publish evaluation artifact set")
        );
        assert!(temp.path().join(PUBLICATION_DIR).is_dir());
        assert!(temp.path().join(PUBLICATION_DIR).join("sentinel").is_file());
        assert!(
            !temp
                .path()
                .join(PUBLICATION_DIR)
                .join(PUBLICATION_MANIFEST_FILE)
                .exists()
        );
        assert_eq!(
            fs::read_dir(temp.path())
                .expect("directory")
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
                .count(),
            0
        );
    }

    #[test]
    fn publication_manifest_binds_one_immutable_artifact_set() {
        let temp = tempfile::tempdir().expect("temp");
        let run_id = RunId::new("atomic-publication").expect("run id");
        let published = publish_evaluation_artifacts(
            temp.path(),
            &run_id,
            &json!({"schema_version": "evaluation.result/v9"}),
            &json!({"runner": RUNNER_NAME}),
            &json!({"schema_version": "evaluation.evidence/v4"}),
        )
        .expect("publish artifact set");

        let manifest: Value = serde_json::from_str(
            &fs::read_to_string(
                temp.path()
                    .join(PUBLICATION_DIR)
                    .join(PUBLICATION_MANIFEST_FILE),
            )
            .expect("publication manifest"),
        )
        .expect("manifest JSON");
        assert_eq!(manifest["schema_version"], PUBLICATION_SCHEMA_VERSION);
        assert_eq!(manifest["run_id"], run_id.as_str());
        assert_eq!(
            manifest["result"]["digest"],
            content_digest(&fs::read(&published.result_path).expect("result artifact"))
        );
        assert_eq!(
            manifest["report"]["digest"],
            content_digest(&fs::read(&published.report_path).expect("report artifact"))
        );
        assert_eq!(
            manifest["evidence"]["digest"],
            content_digest(&fs::read(&published.evidence_path).expect("evidence artifact"))
        );
        assert!(!temp.path().join(RESULT_FILE).exists());
        assert!(!temp.path().join(REPORT_FILE).exists());
        assert!(!temp.path().join(EVIDENCE_FILE).exists());
    }

    #[test]
    fn diagnostic_blocker_wrapper_is_non_gating_and_has_no_publication() {
        let temp = tempfile::tempdir().expect("temp");
        let params = EvaluationRunParams {
            manifest: "manifest.json".to_string(),
            run_id: "diagnostic-blocked".to_string(),
            output_root: Some(temp.path().to_string_lossy().into_owned()),
            max_workers: 1,
        };
        let run_id = RunId::new(&params.run_id).expect("run id");
        let selection = EvaluationSelection::new(TaskId::new("task-a").expect("task id"), 2)
            .expect("selection");
        let wrapper = diagnostic_blocked_run_result(
            &params,
            &run_id,
            &selection,
            evaluation_blocker(BlockerKind::ProviderConfiguration, "provider unavailable"),
        )
        .expect("diagnostic blocker wrapper");

        assert_eq!(wrapper.selection, Some(selection));
        assert_eq!(wrapper.diagnostic_passed, Some(false));
        assert!(!wrapper.evaluation_passed);
        assert!(wrapper.result_path.is_none());
        assert!(wrapper.report_path.is_none());
        assert!(wrapper.evidence_path.is_none());
        assert!(!temp.path().join(PUBLICATION_DIR).exists());
    }

    #[test]
    fn diagnostic_sampled_wrapper_keeps_selected_task_report_without_publication() {
        let temp = tempfile::tempdir().expect("temp");
        let params = EvaluationRunParams {
            manifest: "manifest.json".to_string(),
            run_id: "diagnostic-sampled".to_string(),
            output_root: Some(temp.path().to_string_lossy().into_owned()),
            max_workers: 1,
        };
        let run_id = RunId::new(&params.run_id).expect("run id");
        let task_id = TaskId::new("task-a").expect("task id");
        let selection = EvaluationSelection::new(task_id.clone(), 2).expect("selection");
        let execution = finish_task(
            &task_id,
            2,
            StageExecution::passed(Vec::new()),
            StageExecution::passed(Vec::new()),
            StageExecution::passed(Vec::new()),
            StageExecution::passed(Vec::new()),
            TaskDiagnostics::default(),
        );
        let task = TaskEvaluation {
            result: EvaluationTaskResult::from_trials(
                task_id,
                Vec::new(),
                vec![execution.result.clone()],
            ),
            trials: vec![execution],
        };

        let wrapper =
            diagnostic_sampled_run_result(&params, temp.path(), &run_id, &selection, &[task])
                .expect("diagnostic sampled wrapper");
        assert_eq!(wrapper.selection, Some(selection));
        assert_eq!(wrapper.diagnostic_passed, Some(false));
        assert_eq!(wrapper.tasks.len(), 1);
        assert!(wrapper.result_path.is_none());
        assert!(wrapper.report_path.is_none());
        assert!(wrapper.evidence_path.is_none());
        assert!(!temp.path().join(PUBLICATION_DIR).exists());
    }

    #[test]
    fn failed_artifact_write_removes_its_temp_file() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("result.json");

        let error = write_json_atomic(&path, &SerializationFailure).expect_err("write failure");

        assert!(
            error
                .to_string()
                .contains("intentional serialization failure")
        );
        assert!(!path.exists());
        assert_eq!(fs::read_dir(temp.path()).expect("directory").count(), 0);
    }

    #[test]
    fn evaluation_agent_trace_is_exported_from_store_events() {
        let store = singularity_store::SessionStore::open(":memory:").expect("trace store");
        let run_id = "evaluation-trace-run";
        let task_span = "task-span";
        let turn_span = "turn-span";
        let mut task_start = TraceEvent::new(
            "task-start",
            run_id,
            "task:task-1",
            "evaluation",
            "evaluation task",
        );
        task_start.span_id = Some(task_span.to_string());
        task_start.span_kind = Some(TraceSpanKind::Task);
        task_start.span_phase = Some(TraceSpanPhase::Start);
        task_start.span_projection = Some(TraceSpanProjection::default());
        task_start.payload = json!({"evaluation_span": "task"});
        let mut turn_start = TraceEvent::new(
            "turn-start",
            run_id,
            "trial:task-1:1",
            "evaluation",
            "evaluation trial",
        );
        turn_start.span_id = Some(turn_span.to_string());
        turn_start.parent_span_id = Some(task_span.to_string());
        turn_start.span_kind = Some(TraceSpanKind::Turn);
        turn_start.span_phase = Some(TraceSpanPhase::Start);
        turn_start.span_projection = Some(TraceSpanProjection::default());
        turn_start.payload = json!({"evaluation_span": "turn"});
        let mut turn_end = turn_start.clone();
        turn_end.event_id = "turn-end".to_string();
        turn_end.span_phase = Some(TraceSpanPhase::End);
        turn_end.span_status = Some(TraceSpanStatus::Ok);
        turn_end.duration_ms = Some(1);
        let mut task_end = task_start.clone();
        task_end.event_id = "task-end".to_string();
        task_end.span_phase = Some(TraceSpanPhase::End);
        task_end.span_status = Some(TraceSpanStatus::Ok);
        task_end.duration_ms = Some(1);
        store
            .append_trace_batch(&[task_start, turn_start, turn_end, task_end])
            .expect("store trace");

        let trace = evaluation_agent_trace(&store, run_id, "trial:task-1:1", task_span)
            .expect("export trace");
        assert_eq!(trace["schema"], "evaluation.agent-trace/v2");
        assert_eq!(trace["events"].as_array().expect("events").len(), 4);
        assert_eq!(trace["events"][2]["span_phase"], "end");
    }

    #[test]
    fn concurrent_trace_projection_keeps_unique_ids_and_parent_links() {
        let run_id = RunId::new("trace-concurrent").expect("run id");
        let mut store = singularity_store::SessionStore::open(":memory:").expect("trace store");
        let shared_store = Arc::new(Mutex::new(&mut store));
        let failures = Arc::new(Mutex::new(Vec::new()));
        thread::scope(|scope| {
            for task in ["task-a", "task-b"] {
                let shared_store = Arc::clone(&shared_store);
                let failures = Arc::clone(&failures);
                let run_id = run_id.clone();
                scope.spawn(move || {
                    let task_span =
                        evaluation_span_id(&run_id, &format!("task:{task}"), TraceSpanKind::Task);
                    let task_trace =
                        EvaluationTraceSink::new(Arc::clone(&shared_store), &run_id, &failures);
                    task_trace.start(
                        task,
                        &task_span,
                        None,
                        TraceSpanKind::Task,
                        "evaluation task",
                    );
                    let trial_span = evaluation_span_id(
                        &run_id,
                        &format!("trial:{task}:1"),
                        TraceSpanKind::Turn,
                    );
                    let trial_trace =
                        EvaluationTraceSink::new(Arc::clone(&shared_store), &run_id, &failures);
                    trial_trace.start(
                        &format!("trial:{task}:1"),
                        &trial_span,
                        Some(&task_span),
                        TraceSpanKind::Turn,
                        "evaluation trial",
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    trial_trace.end(
                        &format!("trial:{task}:1"),
                        &trial_span,
                        Some(&task_span),
                        TraceSpanKind::Turn,
                        TraceSpanStatus::Ok,
                        Instant::now(),
                    );
                    task_trace.end(
                        task,
                        &task_span,
                        None,
                        TraceSpanKind::Task,
                        TraceSpanStatus::Ok,
                        Instant::now(),
                    );
                });
            }
        });
        assert!(failures.lock().expect("trace failures").is_empty());
        let events = store.list_trace(run_id.as_str()).expect("trace events");
        assert_eq!(events.len(), 8);
        let ids = events
            .iter()
            .map(|event| event.event_id.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), events.len(), "trace event IDs must not collide");
        for event in events {
            match event.span_kind {
                Some(TraceSpanKind::Task) => assert!(event.parent_span_id.is_none()),
                Some(TraceSpanKind::Turn) => assert!(event.parent_span_id.is_some()),
                _ => panic!("unexpected evaluation trace kind"),
            }
        }
    }

    #[test]
    fn task_fallback_count_includes_agent_setup_commands() {
        let mut result = CommandResult::completed("command", "ok");
        result.sandbox.local_process_fallback = true;
        let mut diagnostics = TaskDiagnostics::default();
        diagnostics
            .agent
            .commands
            .push(CommandDiagnostic::new("agent.setup", &result));
        let task_id = TaskId::new("task-1").expect("task id");

        let execution = finish_task(
            &task_id,
            1,
            StageExecution::passed(Vec::new()),
            StageExecution::passed(Vec::new()),
            StageExecution::passed(Vec::new()),
            StageExecution::passed(Vec::new()),
            diagnostics,
        );

        assert_eq!(execution.diagnostics.local_process_fallback_count, 1);
        assert_eq!(execution.result.status, EvaluationStatus::Failed);
        assert!(!execution.result.evaluation_passed);
    }

    #[test]
    fn recovered_completion_rejection_counts_as_protocol_success() {
        let task_id = TaskId::new("protocol-recovery").expect("task id");
        let strict_command = || {
            let result = CommandResult::completed("command", "ok")
                .with_sandbox_execution("test", SandboxBackendEnforcement::Strict);
            CommandDiagnostic::new("agent.command", &result)
        };
        let mut diagnostics = TaskDiagnostics::default();
        diagnostics.completion_rejection_count = 1;
        diagnostics.agent.commands.push(strict_command());
        let recovered = finish_task(
            &task_id,
            1,
            StageExecution::passed(Vec::new()),
            StageExecution::passed(diagnostics.agent.commands.clone()),
            StageExecution::passed(Vec::new()),
            StageExecution::passed(Vec::new()),
            diagnostics.clone(),
        );
        assert!(recovered.result.agent_protocol_success);
        assert_eq!(
            recovered.result.evidence.completion_rejection_count, 1,
            "recovery history remains a diagnostic metric"
        );

        diagnostics.error = Some("terminal agent error".to_string());
        let terminal_error = finish_task(
            &task_id,
            1,
            StageExecution::passed(Vec::new()),
            StageExecution::passed(diagnostics.agent.commands.clone()),
            StageExecution::passed(Vec::new()),
            StageExecution::passed(Vec::new()),
            diagnostics,
        );
        assert!(!terminal_error.result.agent_protocol_success);

        let unfinished = finish_task(
            &task_id,
            1,
            StageExecution::passed(Vec::new()),
            StageExecution::skipped("agent unfinished"),
            StageExecution::passed(Vec::new()),
            StageExecution::passed(Vec::new()),
            TaskDiagnostics {
                completion_rejection_count: 1,
                ..TaskDiagnostics::default()
            },
        );
        assert!(!unfinished.result.agent_protocol_success);
    }

    #[test]
    fn concurrent_task_result_projection_validates_functional_success_with_blocked_agent() {
        let cancellation = CancellationToken::new();
        let task_evaluations = run_bounded_indexed_workers(2, 2, &cancellation, |index| {
            let task_id = TaskId::new(format!("projection-task-{index}")).expect("task id");
            let mut trials = Vec::with_capacity(2);
            for trial in 1..=2_u32 {
                let strict_command = || {
                    let result = CommandResult::completed("command", "ok")
                        .with_sandbox_execution("test", SandboxBackendEnforcement::Strict);
                    CommandDiagnostic::new("agent.command", &result)
                };
                let agent_command = strict_command();
                let mut diagnostics = TaskDiagnostics::default();
                diagnostics.agent.commands.push(agent_command.clone());
                diagnostics.patch_evidence.push(WorkspaceChangeEvidence {
                    path: format!("src/lib-{index}-{trial}.rs"),
                    change_kind: "modified",
                    before_sha256: Some("sha256:before".to_string()),
                    after_sha256: Some("sha256:after".to_string()),
                });
                let agent = if index == 0 && trial == 2 {
                    StageExecution::blocked(
                        evaluation_blocker(BlockerKind::Sandbox, "agent sandbox blocker"),
                        vec![agent_command],
                    )
                } else {
                    StageExecution::passed(vec![agent_command])
                };
                trials.push(finish_task(
                    &task_id,
                    trial,
                    StageExecution::passed(Vec::new()),
                    agent,
                    StageExecution::passed(Vec::new()),
                    StageExecution::passed(Vec::new()),
                    diagnostics,
                ));
            }
            let result = EvaluationTaskResult::from_trials(
                task_id,
                vec![crate::EvaluationCapability::RequiredVerification],
                trials.iter().map(|trial| trial.result.clone()).collect(),
            );
            TaskEvaluation { result, trials }
        })
        .unwrap_or_else(|_| panic!("task workers complete"));

        assert_eq!(
            task_evaluations[0].result.task_id.as_str(),
            "projection-task-0"
        );
        assert_eq!(
            task_evaluations[1].result.task_id.as_str(),
            "projection-task-1"
        );
        assert_eq!(
            task_evaluations[0].trials[0].result.status,
            EvaluationStatus::Completed
        );
        assert_eq!(
            task_evaluations[0].trials[1].result.status,
            EvaluationStatus::Blocked
        );
        assert!(task_evaluations[0].trials[1].result.functional_task_success);
        assert!(!task_evaluations[0].trials[1].result.agent_protocol_success);
        assert!(
            task_evaluations[0].trials[1]
                .result
                .sandbox_security_success
        );
        assert!(!task_evaluations[0].trials[1].result.evaluation_passed);
        assert!(
            task_evaluations[1]
                .trials
                .iter()
                .all(|trial| trial.result.evaluation_passed)
        );
        assert!(task_evaluations[1].result.evaluation_passed);

        let task = &task_evaluations[0].result;
        assert_eq!(task.summary.agent_scored_trial_count, 1);
        assert_eq!(task.summary.blocked_trial_count, 1);
        assert_eq!(task.summary.functional_task_success_count, 2);
        assert_eq!(
            task.summary.functional_task_success_rate_basis_points,
            10_000
        );
        assert_eq!(task.summary.agent_protocol_success_count, 1);
        assert_eq!(
            task.summary.agent_protocol_success_rate_basis_points,
            10_000
        );
        assert_eq!(task.summary.sandbox_security_success_count, 2);
        assert_eq!(
            task.summary.sandbox_security_success_rate_basis_points,
            10_000
        );
        assert!(task.summary.functional_task_success_rate_basis_points <= 10_000);
        assert!(task.summary.agent_protocol_success_rate_basis_points <= 10_000);
        assert!(task.summary.sandbox_security_success_rate_basis_points <= 10_000);
        assert!(task.functional_task_success);
        assert!(!task.agent_protocol_success);
        assert!(task.sandbox_security_success);
        assert!(!task.evaluation_passed);

        let run_id = RunId::new("projection-contract").expect("run id");
        let mut result = EvaluationResult::from_tasks(
            run_id,
            2,
            task_evaluations
                .iter()
                .map(|task| task.result.clone())
                .collect(),
        );
        result.sandbox_preflight = Some(sandbox_preflight_evidence(&supported_sandbox_preflight(
            "test",
        )));
        result
            .validate()
            .expect("blocked protocol must not invalidate independent functional evidence");
        assert_eq!(result.tasks[0].task_id.as_str(), "projection-task-0");
        assert_eq!(result.tasks[1].task_id.as_str(), "projection-task-1");
        assert!(result.summary.functional_task_success_rate_basis_points <= 10_000);
        assert!(result.summary.agent_protocol_success_rate_basis_points <= 10_000);
        assert!(result.summary.sandbox_security_success_rate_basis_points <= 10_000);
        assert!(!result.tasks[0].evaluation_passed);
        assert!(!result.evaluation_passed);
    }

    #[test]
    fn bounded_task_workers_balance_work_and_restore_manifest_order() {
        let cancellation = CancellationToken::new();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let completion_order = Arc::new(Mutex::new(Vec::new()));
        let release_first = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let active_for_worker = Arc::clone(&active);
        let maximum_for_worker = Arc::clone(&maximum);
        let completion_for_worker = Arc::clone(&completion_order);
        let release_for_worker = Arc::clone(&release_first);
        let results = run_bounded_indexed_workers(4, 2, &cancellation, move |index| {
            let now = active_for_worker.fetch_add(1, Ordering::SeqCst) + 1;
            maximum_for_worker.fetch_max(now, Ordering::SeqCst);
            if index == 0 {
                let (released, wake) = &*release_for_worker;
                let mut released = released.lock().expect("release state");
                while !*released {
                    released = wake.wait(released).expect("release wait");
                }
                completion_for_worker
                    .lock()
                    .expect("completion order")
                    .push(index);
            } else {
                completion_for_worker
                    .lock()
                    .expect("completion order")
                    .push(index);
                if index == 1 {
                    let (released, wake) = &*release_for_worker;
                    *released.lock().expect("release state") = true;
                    wake.notify_one();
                }
            }
            active_for_worker.fetch_sub(1, Ordering::SeqCst);
            index
        })
        .expect("workers complete");
        assert_eq!(results, vec![0, 1, 2, 3], "results use manifest order");
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
        assert_ne!(
            *completion_order.lock().expect("completion order"),
            vec![0, 1, 2, 3],
            "different durations must produce observable completion reordering"
        );

        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let active_for_worker = Arc::clone(&active);
        let maximum_for_worker = Arc::clone(&maximum);
        let results = run_bounded_indexed_workers(3, 1, &cancellation, move |index| {
            let now = active_for_worker.fetch_add(1, Ordering::SeqCst) + 1;
            maximum_for_worker.fetch_max(now, Ordering::SeqCst);
            active_for_worker.fetch_sub(1, Ordering::SeqCst);
            index
        })
        .expect("serial worker completes");
        assert_eq!(results, vec![0, 1, 2]);
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bounded_task_workers_fail_closed_after_panic_and_join_workers() {
        let serial_cancellation = CancellationToken::new();
        let serial_result = run_bounded_indexed_workers(1, 1, &serial_cancellation, |_| -> usize {
            panic!("serial worker failure");
        });
        assert!(matches!(
            serial_result,
            Err(IndexedWorkerError::Failed(message)) if message.contains("worker panicked")
        ));
        assert!(serial_cancellation.is_cancelled());

        let cancellation = CancellationToken::new();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let other_worker_finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let entered_for_worker = Arc::clone(&entered);
        let finished_for_worker = Arc::clone(&other_worker_finished);
        let result = run_bounded_indexed_workers(2, 2, &cancellation, move |index| {
            entered_for_worker.wait();
            if index == 0 {
                panic!("worker failure");
            }
            finished_for_worker.store(true, Ordering::SeqCst);
            index
        });
        match result {
            Err(IndexedWorkerError::Failed(message)) => {
                assert!(message.contains("worker panicked"));
            }
            _ => panic!("worker panic must fail closed"),
        }
        assert!(cancellation.is_cancelled());
        assert!(other_worker_finished.load(Ordering::SeqCst));
    }

    #[test]
    fn bounded_task_workers_cancel_after_join_with_only_a_manifest_prefix() {
        let cancellation = CancellationToken::new();
        let canceller = cancellation.clone();
        let workers_started = Arc::new(std::sync::Barrier::new(3));
        let cancellation_published = Arc::new(std::sync::Barrier::new(3));
        let started_for_canceller = Arc::clone(&workers_started);
        let published_for_canceller = Arc::clone(&cancellation_published);
        let cancel_thread = std::thread::spawn(move || {
            started_for_canceller.wait();
            canceller.cancel();
            published_for_canceller.wait();
        });
        let started_for_worker = Arc::clone(&workers_started);
        let published_for_worker = Arc::clone(&cancellation_published);
        let result = run_bounded_indexed_workers(4, 2, &cancellation, move |index| {
            assert!(index < 2, "cancellation must stop new task claims");
            started_for_worker.wait();
            published_for_worker.wait();
            index
        });
        cancel_thread.join().expect("cancellation thread");
        let Err(IndexedWorkerError::Cancelled(partial)) = result else {
            panic!("cancellation must fail closed");
        };
        assert_eq!(partial, vec![0, 1]);
    }
}
