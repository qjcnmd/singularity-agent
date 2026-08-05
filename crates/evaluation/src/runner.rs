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
    AgentStagePlan, AgentTaskProjection, BlockerKind, CacheMetrics, CommandExpectation,
    CommandSpec, ControlLoopMetrics, EvaluationBlocker, EvaluationDimensions,
    EvaluationEvidenceSummary, EvaluationManifest, EvaluationMetrics, EvaluationPromptStructure,
    EvaluationProviderEvidence, EvaluationReport, EvaluationReportSchemaVersion, EvaluationResult,
    EvaluationSandboxPreflight, EvaluationSandboxPreflightFact, EvaluationSandboxPreflightOutcome,
    EvaluationStageResults, EvaluationStatus, EvaluationSystemResult, EvaluationTaskResult,
    EvaluationTrialResult, FailureAttribution, FailureOwner, FailureStage, MetricRatio,
    MetricStatistics, MetricUnavailableReason, MetricValue, PatchFormat, PlannedWorkspaceSource,
    ProviderUsageMetrics, RunId, StageResult, StageStatus, TaskId, TimingMetrics,
    VerificationStagePlan, WorkspacePlan, failure_owner_for_blocker,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use singularity_agent::{
    AgentLoop, AgentLoopEventSinkError, AgentLoopInput, AgentRecoveryMetrics, AgentStatus,
};
use singularity_app_server::TraceProjector;
use singularity_core::{
    CancellationToken, Timestamp, contains_sensitive_text, load_project_instructions,
};
use singularity_model::{
    ModelErrorCategory, ModelUsage, OpenAiProvider, ProviderAttemptMetadata,
    ProviderCapabilityCacheLookupResult, ProviderCapabilityMetadata, ProviderConfigSnapshot,
    ProviderDiagnostic, ProviderError, ProviderErrorStage, ProviderProtocolContract,
    ProviderProtocolNegotiation,
};
use singularity_policy::{ApprovalPolicy, PermissionProfileName, workspace_policy};
use singularity_protocol::{
    TraceEvent, TraceMetricAvailability, TraceMetricName, TraceMetricUnavailableReason,
    TraceMetrics, TraceSpanKind, TraceSpanPhase, TraceSpanProjection, TraceSpanStatus,
};
#[cfg(windows)]
use singularity_sandbox::{TrustedWorkspaceError, TrustedWorkspaceLease};
use singularity_tools::{
    COMMAND_TOOL as TOOL_COMMAND, CommandEnvironmentPolicy, CommandExecutionStatus, CommandRequest,
    CommandResult, CommandScriptRequest, CommandSemanticStatus, ExecutableAvailability,
    SandboxBackend, SandboxCapabilities, SandboxNetworkMode, SandboxPreflightFact,
    SandboxPreflightOutcome, SandboxPreflightReport, ToolBroker, ToolRegistry, WorkspaceMutation,
    WorkspaceTools, workspace_tool_entries,
};

mod command;
mod evidence;
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

/// The one Evaluation trace Store is shared by task workers only for short SQLite operations.
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
    let result = run_bounded_indexed_workers(plans.len(), max_workers, context.cancellation, {
        move |index| {
            run_task_trials_with_prepared_source(
                context,
                &plans[index],
                trials_per_task,
                &prepared_sources[index],
                selected_trial,
            )
        }
    });
    match result {
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
    if !(1..=2).contains(&params.max_workers) {
        return Err(EvaluationRunError::input(
            "evaluation max_workers must be between 1 and 2",
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
    ) {
        Ok(task_executions) => task_executions,
        Err(TaskWorkerError::Cancelled(task_executions)) => {
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
        schema: EvaluationReportSchemaVersion::V1,
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
            |_| true,
            |d| Some(u64::from(d.verification_required_command_count)),
        ),
        verification_satisfied_commands: metric_sum(
            &diagnostics,
            |_| true,
            |d| Some(u64::from(d.verification_satisfied_command_count)),
        ),
    };
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
                    .and_then(|diagnostic| diagnostic.code.clone()),
                message: trial
                    .diagnostics
                    .error
                    .clone()
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
            prepared_source.source_commands.clone(),
            prepared_source.strict_sandbox_command_count,
            prepared_source.local_process_fallback_count,
            prepared_source.duration_ms,
            prepared_source.source_template_expected,
            prepared_source.source_template.as_ref(),
            matches!(blocker.kind, BlockerKind::WorkspacePreparation),
            selected_trial,
        );
    }
    if context.cancellation.is_cancelled() {
        return blocked_task_trials(
            plan,
            trials_per_task,
            evaluation_blocker(BlockerKind::AgentRuntime, "evaluation cancelled"),
            prepared_source.source_commands.clone(),
            prepared_source.strict_sandbox_command_count,
            prepared_source.local_process_fallback_count,
            prepared_source.duration_ms,
            prepared_source.source_template_expected,
            prepared_source.source_template.as_ref(),
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
            prepared_source.source_commands.clone(),
            prepared_source.strict_sandbox_command_count,
            prepared_source.local_process_fallback_count,
            prepared_source.duration_ms,
            prepared_source.source_template_expected,
            prepared_source.source_template.as_ref(),
            true,
            selected_trial,
        );
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
    EvaluationBlocker {
        code: None,
        kind,
        message: safe_text(message.into()),
        task_id: None,
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        let result = run_bounded_indexed_workers(1, 1, &cancellation, |_| -> usize {
            panic!("worker failure");
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
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_worker = Arc::clone(&calls);
        let result = run_bounded_indexed_workers(4, 1, &cancellation, move |index| {
            if calls_for_worker.fetch_add(1, Ordering::SeqCst) == 1 {
                canceller.cancel();
            }
            index
        });
        assert!(matches!(
            result,
            Err(IndexedWorkerError::Cancelled(prefix)) if prefix == vec![0, 1]
        ));
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
