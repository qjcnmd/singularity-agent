//! Evaluation runner 的任务投影、Agent stage、验证证据与安全产物协调。
//!
//! 本模块只把 manifest 的可信内部命令和模型可见 command string 分开投影，
//! 并在固定 gate、sandbox 与 evidence 合同下汇总结果。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use serde_json::{Value, json};
use singularity_agent::{
    AgentLoop, AgentLoopEventSinkError, AgentLoopInput, AgentLoopResult, AgentRecoveryMetrics,
    AgentStatus, AgentVerificationRequirement, agent_control_tool_entries,
    terminal_command_scope_digests,
};
use singularity_core::{
    CancellationToken, Timestamp, contains_sensitive_text, load_project_instructions,
};
use singularity_evaluation::{
    AgentStagePlan, AgentTaskProjection, BlockerKind, CommandExpectation, CommandSpec,
    EvaluationBlocker, EvaluationEvidenceSummary, EvaluationManifest, EvaluationPromptStructure,
    EvaluationProviderEvidence, EvaluationResult, EvaluationSandboxPreflight,
    EvaluationSandboxPreflightFact, EvaluationSandboxPreflightOutcome, EvaluationStageResults,
    EvaluationStatus, EvaluationTaskResult, EvaluationTrialResult, PatchFormat,
    PlannedWorkspaceSource, RunId, StageResult, StageStatus, TaskId, VerificationStagePlan,
    WorkspacePlan,
};
use singularity_model::{
    ModelErrorCategory, ModelUsage, OpenAiProvider, ProviderAttemptMetadata,
    ProviderCapabilityMetadata, ProviderConfigSnapshot, ProviderDiagnostic, ProviderError,
    ProviderErrorStage, ProviderProtocolContract, ProviderProtocolNegotiation,
};
use singularity_policy::{
    ApprovalPolicy, CommandScopeDigest, NetworkAccess, PermissionDecisionOutcome,
    PermissionOperation, PermissionProfile, PermissionResource, PermissionRule, PolicyEngine,
    SettingsScope, WorkspaceRelativePath,
};
use singularity_protocol::{
    EvalRunParams, EvalRunResult, TraceEvent, TraceSpanKind, TraceSpanPhase, TraceSpanProjection,
    TraceSpanStatus,
};
use singularity_tools::{
    CommandEnvironmentPolicy, CommandExecutionStatus, CommandRequest, CommandResult,
    CommandScriptRequest, CommandSemanticStatus, ExecutableAvailability, SandboxBackend,
    SandboxCapabilities, SandboxFilesystemMode, SandboxNetworkMode, SandboxPreflightFact,
    SandboxPreflightOutcome, SandboxPreflightReport, ToolAuthorization, ToolBroker, ToolCapability,
    ToolExecutor, ToolRegistry, WorkspaceMutation, WorkspaceToolExecutor, WorkspaceTools,
    command_script_scope_digest_with_policy, workspace_tool_entries,
};

#[allow(unused_imports)]
use super::{TOOL_COMMAND, TOOL_EDIT, TOOL_GREP, TOOL_LIST, TOOL_PATCH, TOOL_READ};

mod command;
mod evidence;
mod workspace;

use command::{
    CommandDiagnostic, command_blocker, command_succeeded, infrastructure_blocker,
    run_command_spec, run_workspace_preparation_command,
    run_workspace_preparation_read_only_command, sandbox_network_mode,
};
use evidence::{
    agent_command_observation, build_evaluation_evidence, build_preflight_evidence,
    canonical_json_digest, content_digest,
};
use workspace::{
    WorkspaceChangeEvidence, apply_agent_changes, copy_tree_checked, evaluation_changed_paths,
    patch_evidence_digest, path_is_allowed, snapshot_workspace, validate_tree,
    workspace_change_evidence, workspace_tree_digest,
};

use super::observability::TraceProjector;

const RUNNER_NAME: &str = "agent_loop";
const OUTPUT_ROOT_ENV: &str = "SINGULARITY_EVAL_OUTPUT_DIR";
const DEFAULT_OUTPUT_ROOT: [&str; 2] = ["work", "evaluations"];
const DEFAULT_AGENT_MAX_TURNS: u32 = 24;
const DEFAULT_COMMAND_TIMEOUT_SECONDS: u64 = 300;
const DEFAULT_SETUP_TIMEOUT_SECONDS: u64 = 900;
const GIT_TIMEOUT_SECONDS: u64 = 900;
const SOURCE_DIR: &str = "source";
const BASELINE_DIR: &str = "baseline";
const AGENT_DIR: &str = "agent";
const PUBLIC_DIR: &str = "public";
const HIDDEN_DIR: &str = "hidden";
const EVALUATOR_PATCH_FILE: &str = ".singularity-evaluator.patch";
const EVALUATOR_GIT_DIR: &str = ".singularity-evaluator-git";
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

type SharedSandboxBackend = Arc<dyn SandboxBackend + Send + Sync>;

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

#[derive(Debug, Clone, Default, Serialize)]
struct StageDiagnostics {
    message: Option<String>,
    commands: Vec<CommandDiagnostic>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct TaskDiagnostics {
    source: Option<SourceProvenance>,
    source_commands: Vec<CommandDiagnostic>,
    baseline: StageDiagnostics,
    agent: StageDiagnostics,
    public: StageDiagnostics,
    hidden: StageDiagnostics,
    changed_files: Vec<String>,
    patch_evidence: Vec<WorkspaceChangeEvidence>,
    patch_digest: Option<String>,
    patch_evidence_path: Option<String>,
    disallowed_changed_files: Vec<String>,
    smoke_command_satisfied: bool,
    model_turns: u32,
    tool_calls: u32,
    approval_count: u32,
    plan_update_count: u32,
    plan_completed: bool,
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
    agent_duration_ms: u64,
    local_process_fallback_count: usize,
    local_process_fallback_unknown_count: usize,
    observed_smoke_scope_digests: Vec<String>,
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
    disallowed_changed_files: Vec<String>,
    smoke_command_satisfied: bool,
    model_turns: u32,
    tool_calls: u32,
    approval_count: u32,
    plan_update_count: u32,
    plan_completed: bool,
    recovery_metrics: AgentRecoveryMetrics,
    compaction_count: u32,
    verification_required_command_count: u32,
    verification_satisfied_command_count: u32,
    model_usage: ModelUsage,
    provider_attempts: ProviderAttemptMetadata,
    agent_duration_ms: u64,
    audit_events: Vec<Value>,
    observed_smoke_scope_digests: Vec<String>,
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

/// Materialized source state prepared before any provider trial in the run.
struct PreparedTaskSource {
    task_root: PathBuf,
    source_dir: PathBuf,
    source: SourceProvenance,
    source_commands: Vec<CommandDiagnostic>,
    blocker: Option<EvaluationBlocker>,
}

/// 同一 prepared source 派生全部隔离 trial 时共享的只读任务上下文。
struct PreparedTaskContext<'a> {
    run_id: &'a RunId,
    task_root: &'a Path,
    source_dir: &'a Path,
    source: &'a SourceProvenance,
    source_commands: &'a [CommandDiagnostic],
    plan: &'a WorkspacePlan,
    sandbox_backend: &'a SharedSandboxBackend,
    provider_snapshot: &'a ProviderConfigSnapshot,
    cancellation: &'a CancellationToken,
    trace_store: &'a singularity_store::SessionStore,
    trace_failures: &'a Arc<Mutex<Vec<String>>>,
}

/// 一个 Evaluation run 内所有 task trial 共享的只读执行上下文。
struct EvaluationRunContext<'a> {
    run_id: &'a RunId,
    run_dir: &'a Path,
    manifest_dir: &'a Path,
    sandbox_backend: &'a SharedSandboxBackend,
    provider_snapshot: &'a ProviderConfigSnapshot,
    cancellation: &'a CancellationToken,
    trace_store: &'a singularity_store::SessionStore,
    trace_failures: Arc<Mutex<Vec<String>>>,
    sandbox_preflight: &'a SandboxPreflightReport,
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

struct EvaluationTraceSink<'a> {
    store: &'a singularity_store::SessionStore,
    run_id: &'a RunId,
    failures: &'a Arc<Mutex<Vec<String>>>,
}

impl<'a> EvaluationTraceSink<'a> {
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
            self.run_id.as_str(),
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
        if let Err(error) = self.store.append_trace_idempotent(&event) {
            record_trace_failure(self.failures, format!("{summary} start: {error}"));
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
            self.run_id.as_str(),
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
        if let Err(error) = self.store.append_trace(&event) {
            record_trace_failure(
                self.failures,
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
}

struct EvaluationTrialTrace<'a> {
    sink: EvaluationTraceSink<'a>,
    session_id: String,
    turn_span_id: String,
    task_span_id: String,
}

impl<'a> EvaluationTrialTrace<'a> {
    fn new(
        store: &'a singularity_store::SessionStore,
        run_id: &'a RunId,
        failures: &'a Arc<Mutex<Vec<String>>>,
        session_id: String,
        turn_span_id: String,
        task_span_id: String,
    ) -> Self {
        Self {
            sink: EvaluationTraceSink {
                store,
                run_id,
                failures,
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
        EvaluationStatus::Pending | EvaluationStatus::Running => TraceSpanStatus::Unset,
        EvaluationStatus::Failed | EvaluationStatus::Blocked => TraceSpanStatus::Error,
    }
}

#[derive(Debug)]
struct ResolvedEvaluationTools {
    registry: ToolRegistry,
    names: Vec<String>,
    schema_fingerprint: String,
    allow_read: bool,
    allow_write: bool,
    allow_command: bool,
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
pub(crate) struct EvaluationRunError {
    kind: EvaluationRunErrorKind,
    message: String,
    partial_result: Option<Box<EvalRunResult>>,
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

    fn cancelled(message: impl Into<String>, partial_result: Option<EvalRunResult>) -> Self {
        Self {
            kind: EvaluationRunErrorKind::Cancelled,
            message: message.into(),
            partial_result: partial_result.map(Box::new),
        }
    }

    pub(crate) fn kind(&self) -> EvaluationRunErrorKind {
        self.kind
    }

    pub(crate) fn partial_result(&self) -> Option<&EvalRunResult> {
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

pub(crate) fn run_evaluation(
    params: &EvalRunParams,
    sandbox_backend: SharedSandboxBackend,
    provider_snapshot: &ProviderConfigSnapshot,
    cancellation: &CancellationToken,
    trace_store: &singularity_store::SessionStore,
) -> Result<EvalRunResult, EvaluationRunError> {
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
    let plans = manifest
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
    let task_ids = plans
        .iter()
        .map(|plan| plan.task_id.clone())
        .collect::<Vec<_>>();
    let trials_per_task = manifest.task_set().trial_count;
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

    let cancellable_sandbox_backend =
        cancellation_aware_sandbox_backend(&sandbox_backend, cancellation);
    let preflight =
        match run_sandbox_preflight(&run_dir, &plans, &cancellable_sandbox_backend, cancellation) {
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
                return publish_preflight_blocked_run(
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
    let run_context = EvaluationRunContext {
        run_id: &run_id,
        run_dir: &run_dir,
        manifest_dir: manifest.manifest_dir(),
        sandbox_backend: &cancellable_sandbox_backend,
        provider_snapshot,
        cancellation,
        trace_store,
        trace_failures: Arc::new(Mutex::new(Vec::new())),
        sandbox_preflight: &preflight,
    };
    // Materialize every task source before entering the first provider trial. This keeps
    // source preparation failures deterministic and prevents a later task from being
    // prepared after an earlier task has already sampled the provider.
    let prepared_sources = plans
        .iter()
        .map(|plan| prepare_task_source(&run_context, plan))
        .collect::<Vec<_>>();
    let mut task_executions = Vec::new();
    for (plan, prepared_source) in plans.iter().zip(&prepared_sources) {
        if cancellation.is_cancelled() {
            let partial = partial_evaluation_result(params, &run_id, &task_executions);
            return Err(preserve_incomplete_run(
                &run_dir,
                EvaluationRunError::cancelled("evaluation cancelled", Some(partial)),
            ));
        }
        task_executions.push(run_task_trials_with_prepared_source(
            &run_context,
            plan,
            trials_per_task,
            prepared_source,
        ));
        if cancellation.is_cancelled() {
            let partial = partial_evaluation_result(params, &run_id, &task_executions);
            return Err(preserve_incomplete_run(
                &run_dir,
                EvaluationRunError::cancelled("evaluation cancelled", Some(partial)),
            ));
        }
    }

    if cancellation.is_cancelled() {
        let partial = partial_evaluation_result(params, &run_id, &task_executions);
        return Err(preserve_incomplete_run(
            &run_dir,
            EvaluationRunError::cancelled("evaluation cancelled", Some(partial)),
        ));
    }

    if let Ok(failures) = run_context.trace_failures.lock()
        && !failures.is_empty()
    {
        return Err(preserve_incomplete_run(
            &run_dir,
            EvaluationRunError::infrastructure(format!(
                "evaluation SQLite trace projection failed: {}",
                failures.join("; ")
            )),
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
        "tasks": task_reports,
        "summary": result.summary,
        "sandbox_preflight": sandbox_preflight_evidence(&preflight),
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

    Ok(EvalRunResult {
        run_id: run_id.as_str().to_string(),
        manifest: params.manifest.clone(),
        runner: RUNNER_NAME.to_string(),
        status: status_string,
        blocker,
        tasks: task_reports,
        result_path: Some(published.result_path.to_string_lossy().into_owned()),
        report_path: Some(published.report_path.to_string_lossy().into_owned()),
        evidence_path: Some(published.evidence_path.to_string_lossy().into_owned()),
        evaluation_passed: result.evaluation_passed,
    })
}

fn partial_evaluation_result(
    params: &EvalRunParams,
    run_id: &RunId,
    task_executions: &[TaskEvaluation],
) -> EvalRunResult {
    let status = if task_executions
        .iter()
        .any(|execution| execution.result.status == EvaluationStatus::Failed)
    {
        "failed"
    } else {
        "blocked"
    };
    EvalRunResult {
        run_id: run_id.as_str().to_string(),
        manifest: safe_text(&params.manifest),
        runner: RUNNER_NAME.to_string(),
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
    context: &EvaluationRunContext<'_>,
    plan: &WorkspacePlan,
    trials_per_task: u32,
) -> TaskEvaluation {
    let scope = format!("task:{}", plan.task_id.as_str());
    let session_id = scope.clone();
    let span_id = evaluation_span_id(context.run_id, &scope, TraceSpanKind::Task);
    let started = Instant::now();
    let trace = EvaluationTraceSink {
        store: context.trace_store,
        run_id: context.run_id,
        failures: &context.trace_failures,
    };
    trace.start(
        &session_id,
        &span_id,
        None,
        TraceSpanKind::Task,
        "evaluation task",
    );
    let prepared_source = prepare_task_source(context, plan);
    let evaluation = run_task_trials_inner(context, plan, trials_per_task, &prepared_source);
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

fn publish_preflight_blocked_run(
    params: &EvalRunParams,
    run_dir: &Path,
    manifest_digest: String,
    plans: &[WorkspacePlan],
    trials_per_task: u32,
    result: EvaluationResult,
    preflight: EvaluationSandboxPreflight,
) -> Result<EvalRunResult, EvaluationRunError> {
    let run_id = &result.run_id;
    if let Err(error) = result.validate() {
        return Err(preserve_incomplete_run(
            run_dir,
            EvaluationRunError::infrastructure(format!(
                "invalid sandbox-preflight-blocked evaluation result: {error}"
            )),
        ));
    }
    let evidence = match build_preflight_evidence(
        run_id,
        manifest_digest,
        plans,
        trials_per_task,
        preflight.clone(),
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
                "sandbox preflight evidence/result mismatch: {error}"
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
        "tasks": [],
        "summary": result.summary,
        "sandbox_preflight": preflight,
        "result_path": result_path.to_string_lossy(),
        "report_path": report_path.to_string_lossy(),
        "evidence_path": evidence_path.to_string_lossy(),
    });
    let published = publish_evaluation_artifacts(run_dir, run_id, &result, &report, &evidence)
        .map_err(|error| {
            preserve_incomplete_run(run_dir, EvaluationRunError::publication(error))
        })?;
    Ok(EvalRunResult {
        run_id: run_id.as_str().to_string(),
        manifest: params.manifest.clone(),
        runner: RUNNER_NAME.to_string(),
        status,
        blocker,
        tasks: Vec::new(),
        result_path: Some(published.result_path.to_string_lossy().into_owned()),
        report_path: Some(published.report_path.to_string_lossy().into_owned()),
        evidence_path: Some(published.evidence_path.to_string_lossy().into_owned()),
        evaluation_passed: false,
    })
}

fn run_task_trials_with_prepared_source(
    context: &EvaluationRunContext<'_>,
    plan: &WorkspacePlan,
    trials_per_task: u32,
    prepared_source: &PreparedTaskSource,
) -> TaskEvaluation {
    let scope = format!("task:{}", plan.task_id.as_str());
    let session_id = scope.clone();
    let span_id = evaluation_span_id(context.run_id, &scope, TraceSpanKind::Task);
    let started = Instant::now();
    let trace = EvaluationTraceSink {
        store: context.trace_store,
        run_id: context.run_id,
        failures: &context.trace_failures,
    };
    trace.start(
        &session_id,
        &span_id,
        None,
        TraceSpanKind::Task,
        "evaluation task",
    );
    let evaluation = run_task_trials_inner(context, plan, trials_per_task, prepared_source);
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
    context: &EvaluationRunContext<'_>,
    plan: &WorkspacePlan,
) -> PreparedTaskSource {
    let task_root = context.run_dir.join(plan.task_id.as_str());
    let source_dir = task_root.join(SOURCE_DIR);
    let initial_source = source_provenance(&plan.source, &source_dir, context.manifest_dir);
    let mut prepared = PreparedTaskSource {
        task_root,
        source_dir,
        source: initial_source,
        source_commands: Vec::new(),
        blocker: None,
    };
    if context.cancellation.is_cancelled() {
        prepared.blocker = Some(evaluation_blocker(
            BlockerKind::AgentRuntime,
            "evaluation cancelled",
        ));
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
        return prepared;
    }
    if matches!(plan.source, PlannedWorkspaceSource::RemoteGit { .. })
        && let Err(error) = context.provider_snapshot.provider()
    {
        prepared.blocker = Some(provider_blocker(&error));
        return prepared;
    }
    match prepare_source(
        &plan.source,
        &prepared.task_root,
        &prepared.source_dir,
        Arc::clone(context.sandbox_backend),
    ) {
        Ok(commands) => {
            prepared.source_commands = commands;
            prepared.source =
                source_provenance(&plan.source, &prepared.source_dir, context.manifest_dir);
        }
        Err((blocker, commands)) => {
            prepared.source_commands = commands;
            prepared.blocker = Some(blocker);
            prepared.source =
                source_provenance(&plan.source, &prepared.source_dir, context.manifest_dir);
        }
    }
    prepared
}

fn run_task_trials_inner(
    context: &EvaluationRunContext<'_>,
    plan: &WorkspacePlan,
    trials_per_task: u32,
    prepared_source: &PreparedTaskSource,
) -> TaskEvaluation {
    if let Some(blocker) = &prepared_source.blocker {
        return blocked_task_trials(
            plan,
            trials_per_task,
            blocker.clone(),
            prepared_source.source.clone(),
            prepared_source.source_commands.clone(),
            matches!(blocker.kind, BlockerKind::WorkspacePreparation),
        );
    }
    if context.cancellation.is_cancelled() {
        return blocked_task_trials(
            plan,
            trials_per_task,
            evaluation_blocker(BlockerKind::AgentRuntime, "evaluation cancelled"),
            prepared_source.source.clone(),
            prepared_source.source_commands.clone(),
            false,
        );
    }
    let prepared = PreparedTaskContext {
        run_id: context.run_id,
        task_root: &prepared_source.task_root,
        source_dir: &prepared_source.source_dir,
        source: &prepared_source.source,
        source_commands: &prepared_source.source_commands,
        plan,
        sandbox_backend: context.sandbox_backend,
        provider_snapshot: context.provider_snapshot,
        cancellation: context.cancellation,
        trace_store: context.trace_store,
        trace_failures: &context.trace_failures,
    };
    let trials = (1..=trials_per_task)
        .map(|trial| run_task(&prepared, trial))
        .collect::<Vec<_>>();
    task_evaluation_from_trials(plan, trials)
}

fn blocked_task_trials(
    plan: &WorkspacePlan,
    trials_per_task: u32,
    blocker: EvaluationBlocker,
    source: SourceProvenance,
    source_commands: Vec<CommandDiagnostic>,
    source_preparation_failed: bool,
) -> TaskEvaluation {
    let trials = (1..=trials_per_task)
        .map(|trial| {
            let mut diagnostics = TaskDiagnostics {
                source: Some(source.clone()),
                source_commands: source_commands.clone(),
                smoke_command_satisfied: plan.agent.projection.smoke_commands.is_empty(),
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
        plan.agent.projection.required_tool_capabilities.clone(),
        trials.iter().map(|trial| trial.result.clone()).collect(),
    );
    TaskEvaluation { result, trials }
}

fn run_task(prepared: &PreparedTaskContext<'_>, trial: u32) -> TaskExecution {
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
        prepared.trace_store,
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
    let execution = run_task_inner(prepared, trial, &trace);
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
    prepared: &PreparedTaskContext<'_>,
    trial: u32,
    trace: &EvaluationTrialTrace<'_>,
) -> TaskExecution {
    let task_dir = prepared.task_root.join(format!("trial-{trial:04}"));
    let mut diagnostics = TaskDiagnostics {
        source: Some(prepared.source.clone()),
        source_commands: prepared.source_commands.to_vec(),
        smoke_command_satisfied: prepared.plan.agent.projection.smoke_commands.is_empty(),
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

    let baseline = run_verification_stage(
        prepared.source_dir,
        &task_dir.join(BASELINE_DIR),
        &prepared.plan.baseline.setup_commands,
        prepared.plan.baseline.test_patch.as_ref(),
        &prepared.plan.baseline.commands,
        prepared.plan.baseline.expectation,
        Arc::clone(prepared.sandbox_backend),
    );
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
        &prepared.plan.agent,
        trial,
        provider,
        trace,
    );
    diagnostics.agent = agent_execution.stage.diagnostics.clone();
    diagnostics.changed_files = agent_execution.changed_files.clone();
    diagnostics.patch_evidence = agent_execution.patch_evidence.clone();
    diagnostics.patch_digest = agent_execution.patch_digest.clone();
    diagnostics.patch_evidence_path = agent_execution.patch_evidence_path.clone();
    diagnostics.disallowed_changed_files = agent_execution.disallowed_changed_files.clone();
    diagnostics.smoke_command_satisfied = agent_execution.smoke_command_satisfied;
    diagnostics.model_turns = agent_execution.model_turns;
    diagnostics.tool_calls = agent_execution.tool_calls;
    diagnostics.approval_count = agent_execution.approval_count;
    diagnostics.plan_update_count = agent_execution.plan_update_count;
    diagnostics.plan_completed = agent_execution.plan_completed;
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
    diagnostics.observed_smoke_scope_digests = agent_execution.observed_smoke_scope_digests.clone();
    diagnostics.local_process_fallback_unknown_count =
        agent_execution.local_process_fallback_unknown_count;
    diagnostics.trace_path = agent_execution.trace_path.clone();
    diagnostics.error = agent_execution.error.clone();
    diagnostics.provider_diagnostic = agent_execution.provider_diagnostic.clone();
    diagnostics.prompt_structure = agent_execution.prompt_structure.clone();
    diagnostics.prompt_fingerprint = agent_execution.prompt_fingerprint.clone();
    diagnostics.tool_schema_fingerprint = agent_execution.tool_schema_fingerprint.clone();
    diagnostics.provider_evidence = agent_execution.provider_evidence.clone();
    diagnostics.local_process_fallback_count = agent_execution
        .audit_events
        .iter()
        .filter(|event| event.get("local_process_fallback").and_then(Value::as_bool) == Some(true))
        .count();

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

    let public = run_verification_stage_with_agent_changes(
        prepared.source_dir,
        agent_workspace,
        &agent_execution.changed_files,
        &task_dir.join(PUBLIC_DIR),
        &prepared.plan.public,
        Arc::clone(prepared.sandbox_backend),
    );
    diagnostics.public = public.diagnostics.clone();
    let hidden = run_verification_stage_with_agent_changes(
        prepared.source_dir,
        agent_workspace,
        &agent_execution.changed_files,
        &task_dir.join(HIDDEN_DIR),
        &prepared.plan.hidden,
        Arc::clone(prepared.sandbox_backend),
    );
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

fn source_provenance(
    source: &PlannedWorkspaceSource,
    materialized_path: &Path,
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
    let (tree_digest, tree_digest_error) = if materialized_path.is_dir() {
        match workspace_tree_digest(materialized_path) {
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
    let evaluation_passed = stages.baseline.status == StageStatus::Passed
        && agent_completed
        && tests_passed
        && diagnostics.local_process_fallback_count == 0
        && diagnostics.local_process_fallback_unknown_count == 0;
    let strict_sandbox_command_count = diagnostics
        .source_commands
        .iter()
        .chain(diagnostics.baseline.commands.iter())
        .chain(diagnostics.agent.commands.iter())
        .chain(diagnostics.public.commands.iter())
        .chain(diagnostics.hidden.commands.iter())
        .filter(|command| command.is_strictly_sandboxed())
        .count();
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
        evaluation_passed,
        evidence: EvaluationEvidenceSummary {
            workspace_change_count: u32::try_from(diagnostics.patch_evidence.len())
                .unwrap_or(u32::MAX),
            patch_digest: diagnostics.patch_digest.clone(),
            tool_calls: diagnostics.tool_calls,
            model_turns: diagnostics.model_turns,
            approval_count: diagnostics.approval_count,
            plan_update_count: diagnostics.plan_update_count,
            plan_completed: diagnostics.plan_completed,
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
            smoke_command_satisfied: diagnostics.smoke_command_satisfied,
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
) -> Result<Vec<CommandDiagnostic>, (EvaluationBlocker, Vec<CommandDiagnostic>)> {
    let mut commands = Vec::new();
    match source {
        PlannedWorkspaceSource::Local { path } => {
            copy_tree_checked(path, source_dir).map_err(|error| {
                (
                    evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                    commands.clone(),
                )
            })?;
        }
        PlannedWorkspaceSource::RemoteGit { repository, commit } => {
            let clone = run_workspace_preparation_command(
                task_dir,
                task_dir,
                vec![
                    "git".to_string(),
                    "clone".to_string(),
                    "--quiet".to_string(),
                    repository.as_str().to_string(),
                    SOURCE_DIR.to_string(),
                ],
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
                sandbox_backend,
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
            validate_tree(source_dir).map_err(|error| {
                (
                    evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                    commands.clone(),
                )
            })?;
        }
    }
    Ok(commands)
}
fn run_verification_stage(
    source_dir: &Path,
    stage_dir: &Path,
    setup_commands: &[CommandSpec],
    test_patch: Option<&singularity_evaluation::EvaluatorTestPatch>,
    commands: &[CommandSpec],
    expectation: CommandExpectation,
    sandbox_backend: SharedSandboxBackend,
) -> StageExecution {
    if let Err(error) = copy_tree_checked(source_dir, stage_dir) {
        return StageExecution::blocked(
            evaluation_blocker(BlockerKind::WorkspacePreparation, error),
            Vec::new(),
        );
    }
    run_verification_in_workspace(
        stage_dir,
        setup_commands,
        test_patch,
        commands,
        expectation,
        sandbox_backend,
    )
}

fn run_verification_stage_with_agent_changes(
    source_dir: &Path,
    agent_dir: &Path,
    changed_files: &[String],
    stage_dir: &Path,
    plan: &VerificationStagePlan,
    sandbox_backend: SharedSandboxBackend,
) -> StageExecution {
    if let Err(error) = copy_tree_checked(source_dir, stage_dir) {
        return StageExecution::blocked(
            evaluation_blocker(BlockerKind::WorkspacePreparation, error),
            Vec::new(),
        );
    }
    let mut setup_diagnostics = Vec::new();
    if let Err(blocker) = run_setup_commands(
        stage_dir,
        &plan.setup_commands,
        Arc::clone(&sandbox_backend),
        &mut setup_diagnostics,
    ) {
        return StageExecution::blocked(blocker, setup_diagnostics);
    }
    if let Err(error) = apply_agent_changes(agent_dir, stage_dir, changed_files) {
        return StageExecution::blocked(
            evaluation_blocker(BlockerKind::WorkspacePreparation, error),
            setup_diagnostics,
        );
    }
    run_verification_after_setup(
        stage_dir,
        plan.test_patch.as_ref(),
        &plan.commands,
        plan.expectation,
        sandbox_backend,
        setup_diagnostics,
    )
}

fn run_verification_in_workspace(
    stage_dir: &Path,
    setup_commands: &[CommandSpec],
    test_patch: Option<&singularity_evaluation::EvaluatorTestPatch>,
    commands: &[CommandSpec],
    expectation: CommandExpectation,
    sandbox_backend: SharedSandboxBackend,
) -> StageExecution {
    let mut diagnostics = Vec::new();
    if let Err(blocker) = run_setup_commands(
        stage_dir,
        setup_commands,
        Arc::clone(&sandbox_backend),
        &mut diagnostics,
    ) {
        return StageExecution::blocked(blocker, diagnostics);
    }
    run_verification_after_setup(
        stage_dir,
        test_patch,
        commands,
        expectation,
        sandbox_backend,
        diagnostics,
    )
}

fn run_verification_after_setup(
    stage_dir: &Path,
    test_patch: Option<&singularity_evaluation::EvaluatorTestPatch>,
    commands: &[CommandSpec],
    expectation: CommandExpectation,
    sandbox_backend: SharedSandboxBackend,
    mut diagnostics: Vec<CommandDiagnostic>,
) -> StageExecution {
    if let Some(test_patch) = test_patch
        && let Err(blocker) = apply_evaluator_patch(
            stage_dir,
            test_patch,
            Arc::clone(&sandbox_backend),
            &mut diagnostics,
        )
    {
        return StageExecution::blocked(blocker, diagnostics);
    }

    let mut successes = 0usize;
    let mut failures = 0usize;
    for (index, command) in commands.iter().enumerate() {
        let result = match run_command_spec(
            stage_dir,
            command,
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
            Arc::clone(&sandbox_backend),
        ) {
            Ok(result) => result,
            Err(error) => {
                return StageExecution::blocked(
                    evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                    diagnostics,
                );
            }
        };
        diagnostics.push(CommandDiagnostic::for_spec(
            format!("verification.command.{index}"),
            stage_dir,
            command,
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
            &result,
        ));
        if let Some(blocker) = infrastructure_blocker(&result, "verification command failed") {
            return StageExecution::blocked(blocker, diagnostics);
        }
        // A nonzero result is ordinary verification evidence when success was expected, but it
        // becomes the baseline's accepted outcome when failure was expected.  Accepting that
        // outcome still requires a proven workspace observation.
        if expectation == CommandExpectation::Failure
            && result.execution_status == CommandExecutionStatus::Completed
            && result.semantic_status != CommandSemanticStatus::Succeeded
            && result.workspace_mutation == WorkspaceMutation::Unknown
        {
            return StageExecution::blocked(
                evaluation_blocker(
                    BlockerKind::Sandbox,
                    "verification command failed: workspace mutation could not be verified",
                ),
                diagnostics,
            );
        }
        if command_succeeded(&result) {
            successes += 1;
        } else {
            failures += 1;
        }
    }

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
    patch: &singularity_evaluation::EvaluatorTestPatch,
    sandbox_backend: SharedSandboxBackend,
    diagnostics: &mut Vec<CommandDiagnostic>,
) -> Result<(), EvaluationBlocker> {
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

    let git_dir = workspace.join(EVALUATOR_GIT_DIR);
    if let Err(error) = fs::create_dir(&git_dir) {
        let cleanup = fs::remove_file(&patch_path);
        let message = match cleanup {
            Ok(()) => format!("failed to create isolated evaluator git metadata: {error}"),
            Err(cleanup_error) => format!(
                "failed to create isolated evaluator git metadata: {error}; failed to remove evaluator patch file: {cleanup_error}"
            ),
        };
        return Err(evaluation_blocker(
            BlockerKind::WorkspacePreparation,
            message,
        ));
    }

    let operation = (|| {
        let init_result = run_workspace_preparation_command(
            workspace,
            workspace,
            evaluator_git_argv(&["init", "--quiet"]),
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
            SandboxNetworkMode::Denied,
            Arc::clone(&sandbox_backend),
        );
        diagnostics.push(CommandDiagnostic::new("evaluator.git_init", &init_result));
        if !command_succeeded(&init_result) {
            return Err(command_blocker(
                &init_result,
                BlockerKind::WorkspacePreparation,
                "failed to isolate evaluator patch workspace",
            ));
        }

        let check_result = run_workspace_preparation_command(
            workspace,
            workspace,
            evaluator_git_argv(&[
                "apply",
                "--check",
                "--whitespace=nowarn",
                EVALUATOR_PATCH_FILE,
            ]),
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
            SandboxNetworkMode::Denied,
            Arc::clone(&sandbox_backend),
        );
        diagnostics.push(CommandDiagnostic::new(
            "evaluator.apply_check",
            &check_result,
        ));
        if !command_succeeded(&check_result) {
            return Err(command_blocker(
                &check_result,
                BlockerKind::WorkspacePreparation,
                "evaluator patch validation failed",
            ));
        }

        let result = run_workspace_preparation_command(
            workspace,
            workspace,
            evaluator_git_argv(&["apply", "--whitespace=nowarn", EVALUATOR_PATCH_FILE]),
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
            SandboxNetworkMode::Denied,
            Arc::clone(&sandbox_backend),
        );
        diagnostics.push(CommandDiagnostic::new("evaluator.apply_patch", &result));
        if !command_succeeded(&result) {
            return Err(command_blocker(
                &result,
                BlockerKind::WorkspacePreparation,
                "failed to apply evaluator patch",
            ));
        }
        let reverse_check = run_workspace_preparation_command(
            workspace,
            workspace,
            evaluator_git_argv(&[
                "apply",
                "--reverse",
                "--check",
                "--whitespace=nowarn",
                EVALUATOR_PATCH_FILE,
            ]),
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
            SandboxNetworkMode::Denied,
            sandbox_backend,
        );
        diagnostics.push(CommandDiagnostic::new(
            "evaluator.reverse_check",
            &reverse_check,
        ));
        if !command_succeeded(&reverse_check) {
            return Err(command_blocker(
                &reverse_check,
                BlockerKind::WorkspacePreparation,
                "evaluator patch did not materialize in stage workspace",
            ));
        }
        Ok(())
    })();
    let cleanup = cleanup_evaluator_control_files(&patch_path, &git_dir);
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

fn evaluator_git_argv(arguments: &[&str]) -> Vec<String> {
    let mut argv = vec![
        "git".to_string(),
        format!("--git-dir={EVALUATOR_GIT_DIR}"),
        "--work-tree=.".to_string(),
    ];
    argv.extend(arguments.iter().map(|argument| (*argument).to_string()));
    argv
}

fn cleanup_evaluator_control_files(
    patch_path: &Path,
    git_dir: &Path,
) -> Result<(), EvaluationBlocker> {
    let mut errors = Vec::new();
    if let Err(error) = fs::remove_dir_all(git_dir)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        errors.push(format!("failed to remove evaluator git metadata: {error}"));
    }
    if let Err(error) = fs::remove_file(patch_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        errors.push(format!("failed to remove evaluator patch file: {error}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(evaluation_blocker(
            BlockerKind::WorkspacePreparation,
            errors.join("; "),
        ))
    }
}
fn run_agent_stage(
    prepared: &PreparedTaskContext<'_>,
    task_dir: &Path,
    plan: &AgentStagePlan,
    trial: u32,
    provider: OpenAiProvider,
    trace: &EvaluationTrialTrace<'_>,
) -> AgentStageExecution {
    let agent_dir = task_dir.join(AGENT_DIR);
    let projection = &plan.projection;
    if prepared.cancellation.is_cancelled() {
        return blocked_agent_stage(
            evaluation_blocker(BlockerKind::AgentRuntime, "evaluation cancelled"),
            Vec::new(),
        );
    }
    let pristine_source = match snapshot_workspace(prepared.source_dir) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                Vec::new(),
            );
        }
    };
    if let Err(error) = copy_tree_checked(prepared.source_dir, &agent_dir) {
        return blocked_agent_stage(
            evaluation_blocker(BlockerKind::WorkspacePreparation, error),
            Vec::new(),
        );
    }
    let mut command_diagnostics = Vec::new();
    if let Err(blocker) = run_setup_commands(
        &agent_dir,
        &plan.setup_commands,
        Arc::clone(prepared.sandbox_backend),
        &mut command_diagnostics,
    ) {
        return blocked_agent_stage(blocker, command_diagnostics);
    }
    let before = match snapshot_workspace(&agent_dir) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                command_diagnostics,
            );
        }
    };
    let project_instructions = match load_project_instructions(&agent_dir, &agent_dir) {
        Ok(instructions) => instructions,
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error.to_string()),
                command_diagnostics,
            );
        }
    };

    let resolved_tools = match evaluation_registry(projection) {
        Ok(resolved_tools) => resolved_tools,
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::AgentRuntime, error),
                command_diagnostics,
            );
        }
    };
    let policy = evaluation_policy(&agent_dir, projection, &resolved_tools);
    let prompt = agent_prompt(projection, &resolved_tools.names);
    let project_instructions_fingerprint = project_instructions
        .as_ref()
        .map(|instructions| instructions.aggregate_digest().to_string());
    let prompt_structure = EvaluationPromptStructure {
        contract: "evaluation.agent_prompt/v1".to_string(),
        model_message_roles: vec!["developer".to_string(), "user".to_string()],
        section_kinds: vec![
            "task_instructions".to_string(),
            "allowed_paths".to_string(),
            "resolved_tools".to_string(),
            "smoke_requirements".to_string(),
            "completion_instruction".to_string(),
        ],
        allowed_path_count: u32::try_from(projection.allowed_paths.len()).unwrap_or(u32::MAX),
        resolved_tool_count: u32::try_from(resolved_tools.names.len()).unwrap_or(u32::MAX),
        smoke_command_count: u32::try_from(projection.smoke_commands.len()).unwrap_or(u32::MAX),
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
    let verification_requirements = match agent_verification_requirements(&agent_dir, projection) {
        Ok(requirements) => requirements,
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                command_diagnostics,
            );
        }
    };
    let mut input = AgentLoopInput::new(
        projection.task_id.as_str(),
        format!(
            "eval_{}_{}_trial_{trial}",
            prepared.run_id.as_str(),
            projection.task_id.as_str()
        ),
        prompt,
    )
    .with_max_turns(DEFAULT_AGENT_MAX_TURNS)
    .with_verification_requirements(verification_requirements);
    if let Some(instructions) = project_instructions {
        input = input.with_project_instructions(instructions);
    }
    let command_runtime_executables = projection
        .smoke_commands
        .iter()
        .filter_map(|command| command.argv.as_slice().first().cloned())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let workspace_tools = match WorkspaceTools::new(&agent_dir) {
        Ok(tools) => tools
            .with_shared_sandbox_backend(Arc::clone(prepared.sandbox_backend))
            .with_command_environment(CommandEnvironmentPolicy::EvaluationIsolated)
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
    let mut projector = TraceProjector::new_external(
        prepared.trace_store,
        prepared.run_id.as_str(),
        &trace.session_id,
        &trace.turn_span_id,
    );
    let trace_failures = Arc::clone(prepared.trace_failures);
    let mut on_event = |event| match projector.project_event(event) {
        Ok(()) => Ok(()),
        Err(error) => {
            record_trace_failure(&trace_failures, format!("agent event projection: {error}"));
            Err(AgentLoopEventSinkError)
        }
    };
    let result = AgentLoop::new(provider, ToolBroker::new(resolved_tools.registry), policy)
        .with_workspace_tools(workspace_tools)
        .with_cancellation_token(prepared.cancellation.clone())
        .run_with_events(&input, &mut on_event);
    let agent_duration_ms = u64::try_from(agent_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let run_status = result.to_run_status();
    if let Err(error) = projector.project_result(&run_status) {
        record_trace_failure(
            prepared.trace_failures,
            format!("agent result projection: {error}"),
        );
    }
    let provider_evidence = provider_evidence(
        &provider_identity,
        run_status.provider_protocol_contract.as_ref(),
        run_status.provider_capability_metadata.as_ref(),
    );
    let (observed_smoke_scope_digests, local_process_fallback_unknown_count) =
        agent_command_observation(&result, projection.smoke_commands.len());
    let trace_path = task_dir.join(AGENT_TRACE_FILE);
    let trace = match evaluation_agent_trace(
        prepared.trace_store,
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
                disallowed_changed_files: Vec::new(),
                smoke_command_satisfied: false,
                model_turns: result.model_turns,
                tool_calls: result.tool_calls,
                approval_count: result.approval_count,
                plan_update_count: result.plan_update_count,
                plan_completed: result.plan.as_ref().is_some_and(|plan| plan.is_completed()),
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
                observed_smoke_scope_digests: observed_smoke_scope_digests.clone(),
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
                disallowed_changed_files: Vec::new(),
                smoke_command_satisfied: false,
                model_turns: result.model_turns,
                tool_calls: result.tool_calls,
                approval_count: result.approval_count,
                plan_update_count: result.plan_update_count,
                plan_completed: result.plan.as_ref().is_some_and(|plan| plan.is_completed()),
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
                observed_smoke_scope_digests: observed_smoke_scope_digests.clone(),
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

    let after = match snapshot_workspace(&agent_dir) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return AgentStageExecution {
                stage: StageExecution::failed(error.clone(), command_diagnostics),
                workspace: Some(agent_dir.to_path_buf()),
                changed_files: Vec::new(),
                patch_evidence: Vec::new(),
                patch_digest: None,
                patch_evidence_path: None,
                disallowed_changed_files: Vec::new(),
                smoke_command_satisfied: false,
                model_turns: result.model_turns,
                tool_calls: result.tool_calls,
                approval_count: result.approval_count,
                plan_update_count: result.plan_update_count,
                plan_completed: result.plan.as_ref().is_some_and(|plan| plan.is_completed()),
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
                observed_smoke_scope_digests: observed_smoke_scope_digests.clone(),
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
    let changed_files = evaluation_changed_paths(&before, &after, &pristine_source);
    let patch_evidence =
        workspace_change_evidence(&before, &after, &pristine_source, &projection.allowed_paths);
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
                disallowed_changed_files: Vec::new(),
                smoke_command_satisfied: false,
                model_turns: result.model_turns,
                tool_calls: result.tool_calls,
                approval_count: result.approval_count,
                plan_update_count: result.plan_update_count,
                plan_completed: result.plan.as_ref().is_some_and(|plan| plan.is_completed()),
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
                observed_smoke_scope_digests: observed_smoke_scope_digests.clone(),
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
    let disallowed_changed_files = changed_files
        .iter()
        .filter(|path| !path_is_allowed(path, &projection.allowed_paths))
        .cloned()
        .collect::<Vec<_>>();
    let smoke_command_satisfied = smoke_commands_satisfied(&agent_dir, projection, &result);
    let loop_completed = result.completed && result.status == AgentStatus::Completed;
    let error = result.error.clone().map(safe_text);
    let sandbox_blocker = agent_sandbox_blocker(&run_status.audit_events);
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
    } else if !disallowed_changed_files.is_empty() {
        StageExecution::failed(
            format!(
                "agent changed paths outside the manifest allowlist: {}",
                disallowed_changed_files.join(", ")
            ),
            command_diagnostics,
        )
    } else if !smoke_command_satisfied {
        StageExecution::failed(
            "agent did not produce successful exact results for every declared smoke command",
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
        disallowed_changed_files,
        smoke_command_satisfied,
        model_turns: result.model_turns,
        tool_calls: result.tool_calls,
        approval_count: result.approval_count,
        plan_update_count: result.plan_update_count,
        plan_completed: result.plan.as_ref().is_some_and(|plan| plan.is_completed()),
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
        observed_smoke_scope_digests,
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
        disallowed_changed_files: Vec::new(),
        smoke_command_satisfied: false,
        model_turns: 0,
        tool_calls: 0,
        approval_count: 0,
        plan_update_count: 0,
        plan_completed: false,
        recovery_metrics: AgentRecoveryMetrics::default(),
        compaction_count: 0,
        verification_required_command_count: 0,
        verification_satisfied_command_count: 0,
        model_usage: ModelUsage::default(),
        provider_attempts: ProviderAttemptMetadata::default(),
        agent_duration_ms: 0,
        audit_events: Vec::new(),
        observed_smoke_scope_digests: Vec::new(),
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

fn evaluation_registry(
    projection: &AgentTaskProjection,
) -> Result<ResolvedEvaluationTools, String> {
    let mut catalog = ToolRegistry::default();
    for entry in workspace_tool_entries()
        .into_iter()
        .chain(agent_control_tool_entries())
    {
        catalog.register(entry)?;
    }
    let mut selected = std::collections::BTreeMap::new();
    for requirement in &projection.required_tool_capabilities {
        let capability = serde_json::from_value::<ToolCapability>(Value::String(
            requirement.capability.as_str().to_string(),
        ))
        .map_err(|_| {
            format!(
                "unsupported evaluation tool capability {}",
                requirement.capability.as_str()
            )
        })?;
        let entries = catalog.entries_for_capability(capability, requirement.minimum_version);
        if entries.is_empty() {
            return Err(format!(
                "evaluation tool capability {} requires version {} but the registry has no matching model-visible entry",
                requirement.capability.as_str(),
                requirement.minimum_version
            ));
        }
        for entry in entries {
            selected.insert(entry.id.as_str().to_string(), entry.clone());
        }
    }
    let mut registry = ToolRegistry::default();
    let mut allow_read = false;
    let mut allow_write = false;
    let mut allow_command = false;
    for (_, mut entry) in selected {
        allow_read |= matches!(
            entry.authorization,
            ToolAuthorization::WorkspaceRead | ToolAuthorization::AgentControl
        );
        allow_write |= entry.authorization == ToolAuthorization::WorkspaceWrite;
        allow_command |= entry.authorization == ToolAuthorization::Command;
        if entry.executor == ToolExecutor::Workspace(WorkspaceToolExecutor::Command) {
            let bindings = projection
                .smoke_commands
                .iter()
                .map(|command| {
                    (
                        smoke_command_model_input(command),
                        smoke_command_execution_input(command),
                    )
                })
                .collect::<Vec<_>>();
            entry.spec.restrict_to_input_bindings(bindings)?;
        }
        registry.register(entry)?;
    }
    if !projection.smoke_commands.is_empty() && !allow_command {
        return Err(
            "evaluation smoke commands require a registry-resolved command execution tool"
                .to_string(),
        );
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
        allow_read,
        allow_write,
        allow_command,
    })
}

fn evaluation_policy(
    _workspace: &Path,
    projection: &AgentTaskProjection,
    resolved_tools: &ResolvedEvaluationTools,
) -> PolicyEngine {
    let mut profile = PermissionProfile::workspace_write();
    profile.approval_policy = ApprovalPolicy::Never;
    if projection
        .smoke_commands
        .iter()
        .any(|command| command.network_access == NetworkAccess::Allowed)
    {
        profile.network_access = NetworkAccess::Allowed;
    }
    let mut policy = PolicyEngine::new(profile);
    if resolved_tools.allow_read {
        policy = policy.with_rule(
            PermissionRule::new(
                "allow_evaluation_read_tools",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Read),
        );
    }
    if resolved_tools.allow_write {
        for (index, path) in projection.allowed_paths.iter().enumerate() {
            policy = policy.with_rule(
                PermissionRule::new(
                    format!("allow_evaluation_write_{index}"),
                    SettingsScope::Project,
                    PermissionDecisionOutcome::Allow,
                )
                .for_operation(PermissionOperation::Write)
                .for_workspace_subtree(
                    WorkspaceRelativePath::from_canonical(path.as_str())
                        .expect("evaluation paths are canonical workspace-relative paths"),
                ),
            );
        }
    }
    if resolved_tools.allow_command {
        for (index, command) in projection.smoke_commands.iter().enumerate() {
            let network = sandbox_network_mode(command.network_access);
            let resource = PermissionResource::CommandScope(
                CommandScopeDigest::new(command_script_scope_digest_with_policy(
                    &command_script_from_argv(command.argv.as_slice()),
                    command.cwd.as_ref().map_or(".", |cwd| cwd.as_str()),
                    command
                        .timeout_seconds
                        .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECONDS),
                    SandboxFilesystemMode::WorkspaceWrite,
                    network.clone(),
                ))
                .expect("evaluation command scope digest is valid"),
            );
            policy = policy.with_rule(
                PermissionRule::new(
                    format!("allow_evaluation_command_{index}"),
                    SettingsScope::Project,
                    PermissionDecisionOutcome::Allow,
                )
                .for_operation(PermissionOperation::Execute)
                .for_resource(resource.clone()),
            );
            if command.network_access == NetworkAccess::Allowed {
                policy = policy.with_rule(
                    PermissionRule::new(
                        format!("allow_evaluation_command_network_{index}"),
                        SettingsScope::Project,
                        PermissionDecisionOutcome::Allow,
                    )
                    .for_operation(PermissionOperation::Network)
                    .for_resource(resource),
                );
            }
        }
    }
    policy
}

fn agent_prompt(projection: &AgentTaskProjection, resolved_tools: &[String]) -> String {
    let allowed_paths = projection
        .allowed_paths
        .iter()
        .map(|path| path.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let allowed_tools = resolved_tools.join(", ");
    let mut sections = vec![
        projection.instructions.clone(),
        format!("Only modify these workspace paths: {allowed_paths}."),
        format!("Only these tools are available: {allowed_tools}."),
    ];
    for (index, command) in projection.smoke_commands.iter().enumerate() {
        sections.push(format!(
            "Before the final answer, call {TOOL_COMMAND} for smoke command {} with exactly this JSON input: {}. The task is not agent-completed unless that exact tool result succeeds.",
            index + 1,
            smoke_command_model_input(command)
        ));
    }
    sections.push(
        "Finish with a concise answer describing the change and the verification actually run."
            .to_string(),
    );
    sections.join("\n\n")
}

fn smoke_command_model_input(command: &CommandSpec) -> Value {
    json!({
        "command": command_script_from_argv(command.argv.as_slice()),
        "cwd": command.cwd.as_ref().map(|cwd| cwd.as_str()).unwrap_or("."),
        "timeout_seconds": command.timeout_seconds.unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECONDS),
    })
}

fn smoke_command_execution_input(command: &CommandSpec) -> Value {
    smoke_command_model_input(command)
}

fn command_script_from_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|argument| {
            if !argument.is_empty()
                && argument.chars().all(|character| {
                    character.is_ascii_alphanumeric()
                        || matches!(character, '-' | '_' | '.' | '/' | '\\' | ':')
                })
            {
                argument.clone()
            } else {
                format!("'{}'", argument.replace('\'', "''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn evaluation_agent_trace(
    store: &singularity_store::SessionStore,
    run_id: &str,
    session_id: &str,
    task_span_id: &str,
) -> Result<Value, String> {
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
    serde_json::to_value(json!({
        "schema": "evaluation.agent-trace/v2",
        "run_id": run_id,
        "session_id": session_id,
        "events": events,
    }))
    .map_err(|error| format!("failed to serialize evaluation SQLite trace: {error}"))
}

fn agent_verification_requirements(
    workspace: &Path,
    projection: &AgentTaskProjection,
) -> Result<Vec<AgentVerificationRequirement>, String> {
    projection
        .smoke_commands
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let scope_digest = smoke_command_scope_digest(workspace, command).map_err(|_| {
                format!(
                    "evaluation smoke command {} cwd could not be resolved inside the prepared workspace",
                    index + 1
                )
            })?;
            Ok(AgentVerificationRequirement::new(
                scope_digest,
                1,
            ))
        })
        .collect()
}

fn smoke_commands_satisfied(
    workspace: &Path,
    projection: &AgentTaskProjection,
    result: &AgentLoopResult,
) -> bool {
    let expected_results = projection
        .smoke_commands
        .iter()
        .map(|command| smoke_command_scope_digest(workspace, command))
        .collect::<Result<Vec<_>, _>>();
    let Ok(expected_results) = expected_results else {
        return false;
    };
    let observed_results =
        terminal_command_scope_digests(&result.tool_results, expected_results.len());
    let mut matched_results = vec![false; observed_results.len()];
    expected_results.iter().all(|expected| {
        let Some(index) = observed_results
            .iter()
            .enumerate()
            .position(|(index, tool_result)| !matched_results[index] && tool_result == expected)
        else {
            return false;
        };
        matched_results[index] = true;
        true
    })
}

// 统一 Agent completion、post-agent smoke 和 evidence 使用的精确 command scope。
fn smoke_command_scope_digest(workspace: &Path, command: &CommandSpec) -> Result<String, String> {
    let cwd = resolved_smoke_cwd(workspace, command)
        .ok_or_else(|| "evaluation smoke command cwd is unavailable".to_string())?;
    Ok(command_script_scope_digest_with_policy(
        &command_script_from_argv(command.argv.as_slice()),
        &cwd,
        command
            .timeout_seconds
            .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECONDS),
        SandboxFilesystemMode::WorkspaceWrite,
        sandbox_network_mode(command.network_access),
    ))
}

fn resolved_smoke_cwd(workspace: &Path, command: &CommandSpec) -> Option<String> {
    let workspace = fs::canonicalize(workspace).ok()?;
    let cwd = command
        .cwd
        .as_ref()
        .map_or_else(|| workspace.clone(), |cwd| workspace.join(cwd.as_str()));
    let cwd = fs::canonicalize(cwd).ok()?;
    let relative = cwd.strip_prefix(&workspace).ok()?;
    let relative = if relative.as_os_str().is_empty() {
        ".".to_string()
    } else {
        relative
            .components()
            .map(|component| component.as_os_str().to_str())
            .collect::<Option<Vec<_>>>()?
            .join("/")
    };
    WorkspaceRelativePath::from_canonical(relative)
        .ok()
        .map(|path| path.as_str().to_string())
}
fn agent_sandbox_blocker(audit_events: &[Value]) -> Option<EvaluationBlocker> {
    if audit_events
        .iter()
        .any(|event| event.get("local_process_fallback").and_then(Value::as_bool) == Some(true))
    {
        return Some(evaluation_blocker(
            BlockerKind::Sandbox,
            "agent command used forbidden local process fallback",
        ));
    }
    if audit_events.iter().any(|event| {
        event.get("sandbox_enforcement").and_then(Value::as_str) == Some("unavailable")
    }) {
        return Some(evaluation_blocker(
            BlockerKind::Sandbox,
            "agent command sandbox enforcement was unavailable",
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
            .agent
            .setup_commands
            .iter()
            .chain(&plan.baseline.setup_commands)
            .chain(&plan.public.setup_commands)
            .chain(&plan.hidden.setup_commands)
            .chain(&plan.agent.projection.smoke_commands)
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
            &CommandEnvironmentPolicy::EvaluationIsolated,
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
    // The run-owned scratch directory is on the same filesystem as task roots. One probe
    // therefore establishes the backend/profile contract for the entire task set without
    // touching any task source or starting a provider trial.
    let scratch = run_dir.join(".sandbox-preflight");
    if let Err(error) = fs::create_dir_all(&scratch) {
        let mut report = SandboxPreflightReport::unverified_for_backend(sandbox_backend.as_ref());
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
    let mut report = sandbox_backend.preflight(&scratch, cancellation);
    if report.outcome == SandboxPreflightOutcome::Supported
        && let Err((code, missing)) =
            preflight_task_executables(&scratch, plans, sandbox_backend, cancellation)
    {
        report.outcome = SandboxPreflightOutcome::Unsupported;
        report.error_code = Some(code.to_string());
        report.missing_capabilities.extend(missing);
    }
    if report.outcome == SandboxPreflightOutcome::Supported {
        match preflight_remote_sources(&scratch, plans, sandbox_backend, cancellation) {
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
            &scratch,
            &scratch,
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
            report.outcome = SandboxPreflightOutcome::Unsupported;
            report.error_code =
                Some("sandbox_preflight_trusted_preparation_unverified".to_string());
            report
                .missing_capabilities
                .push("trusted_workspace_preparation".to_string());
        }
    }
    if let Err(error) = fs::remove_dir_all(&scratch) {
        report.outcome = SandboxPreflightOutcome::Unsupported;
        report.error_code = Some("sandbox_preflight_scratch_cleanup".to_string());
        report
            .missing_capabilities
            .push("scratch_cleanup".to_string());
        return Err(Box::new(SandboxPreflightFailure {
            report,
            blocker: sandbox_preflight_blocker(
                "sandbox_preflight_scratch_cleanup",
                format!("sandbox preflight scratch cleanup failed: {error}"),
            ),
        }));
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
        return Err(Box::new(SandboxPreflightFailure {
            report,
            blocker: sandbox_preflight_blocker(
                code.clone(),
                format!("sandbox preflight unsupported: {code}"),
            ),
        }));
    }
    Ok(report)
}

fn stage_result(status: StageStatus, blocker: Option<EvaluationBlocker>) -> StageResult {
    StageResult { status, blocker }
}

fn evaluation_output_root(explicit: Option<&str>) -> PathBuf {
    explicit
        .map(PathBuf::from)
        .or_else(|| std::env::var(OUTPUT_ROOT_ENV).ok().map(PathBuf::from))
        .unwrap_or_else(|| DEFAULT_OUTPUT_ROOT.iter().collect())
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

        for stage in [BASELINE_DIR, AGENT_DIR, PUBLIC_DIR, HIDDEN_DIR] {
            let stage_dir = trial_dir.join(stage);
            check_path_budget(
                "Cargo target dependency artifact",
                &stage_dir
                    .join("target")
                    .join("debug")
                    .join("deps")
                    .join(format!("singularity_evaluation-{CARGO_DEP_HEX}.rlib")),
                max_path_chars,
            )?;
        }
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
    use serde::Serializer;
    use singularity_agent::UPDATE_PLAN_TOOL;
    use singularity_evaluation::{
        Argv, GitCommit, RelativePath, RemoteRepository, ToolCapabilityName,
        ToolCapabilityRequirement,
    };
    use singularity_tools::{
        CommandExecutionStatus, CommandRequest, CommandResult, SandboxBackendEnforcement,
        SandboxCapabilities, WorkspaceMutation, WorkspaceObservation, WorkspaceRevision,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn command(argv: &[&str]) -> CommandSpec {
        CommandSpec {
            argv: Argv::new(argv.iter().map(|value| (*value).to_string()).collect()).expect("argv"),
            cwd: None,
            timeout_seconds: Some(30),
            network_access: NetworkAccess::Denied,
        }
    }

    fn requirement(capability: ToolCapability) -> ToolCapabilityRequirement {
        let name = serde_json::to_value(capability)
            .expect("capability serializes")
            .as_str()
            .expect("capability serializes as string")
            .to_string();
        ToolCapabilityRequirement {
            capability: ToolCapabilityName::new(name).expect("capability name"),
            minimum_version: 1,
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

    #[test]
    fn command_script_projection_preserves_argument_boundaries() {
        let argv = vec![
            "python".to_string(),
            "script with spaces.py".to_string(),
            "it's-safe".to_string(),
        ];
        assert_eq!(
            command_script_from_argv(&argv),
            "python 'script with spaces.py' 'it''s-safe'"
        );
    }

    fn successful_command_result(
        tool_call_id: &str,
        command: &CommandSpec,
        workspace: &Path,
    ) -> singularity_tools::ToolResult {
        successful_command_result_at_revision(
            tool_call_id,
            command,
            workspace,
            WorkspaceRevision::initial(),
        )
    }

    fn successful_command_result_at_revision(
        tool_call_id: &str,
        command: &CommandSpec,
        workspace: &Path,
        revision: WorkspaceRevision,
    ) -> singularity_tools::ToolResult {
        let mut result =
            singularity_tools::ToolResult::summary(tool_call_id, TOOL_COMMAND, true, "ok");
        result.result_id = Some(command_script_scope_digest_with_policy(
            &command_script_from_argv(command.argv.as_slice()),
            &resolved_smoke_cwd(workspace, command).expect("resolved smoke cwd"),
            command
                .timeout_seconds
                .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECONDS),
            SandboxFilesystemMode::WorkspaceWrite,
            sandbox_network_mode(command.network_access),
        ));
        result.with_workspace_observation(WorkspaceObservation::unchanged(revision))
    }

    fn changed_command_result(
        tool_call_id: &str,
        command: &CommandSpec,
        workspace: &Path,
    ) -> singularity_tools::ToolResult {
        let mut result = successful_command_result(tool_call_id, command, workspace);
        result = result.with_workspace_observation(WorkspaceObservation::changed(
            WorkspaceRevision::initial().next().expect("revision"),
        ));
        result
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
            plan: None,
            plan_update_count: 0,
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
    fn agent_prompt_contains_only_projection_and_exact_smoke_input() {
        let projection = AgentTaskProjection {
            task_id: TaskId::new("task-1").expect("task id"),
            description: "description".to_string(),
            instructions: "fix the bug".to_string(),
            allowed_paths: vec![RelativePath::new("src/lib.rs").expect("path")],
            required_tool_capabilities: vec![
                requirement(ToolCapability::WorkspaceRead),
                requirement(ToolCapability::CommandExecution),
            ],
            smoke_commands: vec![command(&["cargo", "test"])],
        };

        let prompt = agent_prompt(
            &projection,
            &[TOOL_READ.to_string(), TOOL_COMMAND.to_string()],
        );
        assert!(prompt.contains("\"command\":\"cargo test\""));
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
        assert!(!serialized.contains("Authorization"));
        assert!(!serialized.contains("raw_response"));
    }

    #[test]
    fn duplicate_smoke_commands_require_distinct_successful_tool_results() {
        let workspace = tempfile::tempdir().expect("workspace");
        let smoke = command(&["cargo", "test"]);
        let projection = AgentTaskProjection {
            task_id: TaskId::new("task-1").expect("task id"),
            description: "description".to_string(),
            instructions: "fix".to_string(),
            allowed_paths: vec![RelativePath::new("src/lib.rs").expect("path")],
            required_tool_capabilities: vec![requirement(ToolCapability::CommandExecution)],
            smoke_commands: vec![smoke.clone(), smoke.clone()],
        };
        let tool_result = successful_command_result("call-1", &smoke, workspace.path());
        let result = completed_agent_result(vec![tool_result.clone()]);
        assert!(!smoke_commands_satisfied(
            workspace.path(),
            &projection,
            &result
        ));

        let result = completed_agent_result(vec![tool_result.clone(), tool_result]);
        assert!(smoke_commands_satisfied(
            workspace.path(),
            &projection,
            &result
        ));

        let other = command(&["touch", "result.txt"]);
        let other_result = changed_command_result("call-other", &other, workspace.path());
        let result = completed_agent_result(vec![
            successful_command_result("call-smoke-1", &smoke, workspace.path()),
            successful_command_result("call-smoke-2", &smoke, workspace.path()),
            other_result,
        ]);
        assert!(!smoke_commands_satisfied(
            workspace.path(),
            &projection,
            &result
        ));
    }

    #[test]
    fn smoke_commands_must_run_after_the_last_workspace_mutation() {
        let workspace = tempfile::tempdir().expect("workspace");
        let smoke = command(&["cargo", "test"]);
        let projection = AgentTaskProjection {
            task_id: TaskId::new("task-1").expect("task id"),
            description: "description".to_string(),
            instructions: "fix".to_string(),
            allowed_paths: vec![RelativePath::new("src/lib.rs").expect("path")],
            required_tool_capabilities: vec![
                requirement(ToolCapability::WorkspaceWrite),
                requirement(ToolCapability::CommandExecution),
            ],
            smoke_commands: vec![smoke.clone()],
        };
        let revision = WorkspaceRevision::initial().next().expect("revision");
        let mutation =
            singularity_tools::ToolResult::summary("call-edit", TOOL_EDIT, true, "changed")
                .with_workspace_observation(WorkspaceObservation::changed(revision));
        let smoke_result =
            successful_command_result_at_revision("call-smoke", &smoke, workspace.path(), revision);

        let stale = completed_agent_result(vec![smoke_result.clone(), mutation.clone()]);
        assert!(!smoke_commands_satisfied(
            workspace.path(),
            &projection,
            &stale
        ));

        let current = completed_agent_result(vec![mutation, smoke_result]);
        assert!(smoke_commands_satisfied(
            workspace.path(),
            &projection,
            &current
        ));
    }

    #[test]
    fn smoke_commands_must_be_the_terminal_successful_command_suffix() {
        let workspace = tempfile::tempdir().expect("workspace");
        let smoke = command(&["cargo", "test"]);
        let write = command(&["touch", "result.txt"]);
        let projection = AgentTaskProjection {
            task_id: TaskId::new("task-1").expect("task id"),
            description: "description".to_string(),
            instructions: "fix".to_string(),
            allowed_paths: vec![RelativePath::new("src/lib.rs").expect("path")],
            required_tool_capabilities: vec![requirement(ToolCapability::CommandExecution)],
            smoke_commands: vec![smoke.clone()],
        };
        let revision = WorkspaceRevision::initial().next().expect("revision");
        let smoke_result = successful_command_result("call-smoke", &smoke, workspace.path());
        let write_result = changed_command_result("call-write", &write, workspace.path());

        let stale = completed_agent_result(vec![smoke_result.clone(), write_result.clone()]);
        assert!(!smoke_commands_satisfied(
            workspace.path(),
            &projection,
            &stale
        ));

        let current_smoke =
            successful_command_result_at_revision("call-smoke", &smoke, workspace.path(), revision);
        let current = completed_agent_result(vec![write_result, current_smoke]);
        assert!(smoke_commands_satisfied(
            workspace.path(),
            &projection,
            &current
        ));
    }

    #[test]
    fn smoke_observation_rejects_failed_result_even_when_digest_matches() {
        let workspace = tempfile::tempdir().expect("workspace");
        let smoke = command(&["cargo", "test"]);
        let mut failed = successful_command_result("call-failed", &smoke, workspace.path());
        failed.ok = false;
        let result = completed_agent_result(vec![failed]);

        let (observed, unknown_count) = agent_command_observation(&result, 1);

        assert!(observed.is_empty());
        assert_eq!(unknown_count, 1);
    }

    #[test]
    fn smoke_observation_preserves_post_mutation_order_and_duplicates() {
        let workspace = tempfile::tempdir().expect("workspace");
        let smoke = command(&["cargo", "test"]);
        let revision = WorkspaceRevision::initial().next().expect("revision");
        let mutation =
            singularity_tools::ToolResult::summary("call-edit", TOOL_EDIT, true, "changed")
                .with_workspace_observation(WorkspaceObservation::changed(revision));
        let first = successful_command_result_at_revision(
            "call-smoke-1",
            &smoke,
            workspace.path(),
            revision,
        );
        let second = successful_command_result_at_revision(
            "call-smoke-2",
            &smoke,
            workspace.path(),
            revision,
        );
        let expected = first.result_id.clone().expect("scope digest");

        let result = completed_agent_result(vec![mutation, first, second]);
        let (observed, _) = agent_command_observation(&result, 2);

        assert_eq!(observed, vec![expected.clone(), expected]);
    }

    #[test]
    fn registry_exposes_only_manifest_tools() {
        let mut projection = AgentTaskProjection {
            task_id: TaskId::new("task-1").expect("task id"),
            description: "description".to_string(),
            instructions: "fix".to_string(),
            allowed_paths: vec![RelativePath::new("src/lib.rs").expect("path")],
            required_tool_capabilities: vec![requirement(ToolCapability::WorkspaceRead)],
            smoke_commands: Vec::new(),
        };
        let registry = evaluation_registry(&projection).expect("registry");
        assert!(registry.registry.get(TOOL_READ).is_some());
        assert!(registry.registry.get(TOOL_LIST).is_some());
        assert!(registry.registry.get(UPDATE_PLAN_TOOL).is_none());
        assert!(registry.registry.get(TOOL_COMMAND).is_none());
        assert!(registry.registry.get(TOOL_EDIT).is_none());

        projection.required_tool_capabilities = vec![requirement(ToolCapability::WorkspaceSearch)];
        let registry = evaluation_registry(&projection).expect("search registry");
        assert!(registry.registry.get(TOOL_GREP).is_some());
        assert!(registry.registry.get(TOOL_READ).is_none());
    }

    #[test]
    fn registry_command_schema_exposes_only_allowed_smoke_inputs() {
        let smoke = command(&["cargo", "test"]);
        let projection = AgentTaskProjection {
            task_id: TaskId::new("task-1").expect("task id"),
            description: "description".to_string(),
            instructions: "fix".to_string(),
            allowed_paths: vec![RelativePath::new("src/lib.rs").expect("path")],
            required_tool_capabilities: vec![requirement(ToolCapability::CommandExecution)],
            smoke_commands: vec![smoke.clone()],
        };

        let registry = evaluation_registry(&projection).expect("registry");
        let command = registry.registry.get(TOOL_COMMAND).expect("command tool");
        let payload = smoke_command_model_input(&smoke);

        assert_eq!(
            command.input_schema["properties"]["command"]["const"],
            payload["command"]
        );
        assert!(singularity_model::is_strict_tool_schema_compatible(
            &command.input_schema
        ));
        assert!(command.input_schema.get("sandbox_mode").is_none());
        assert!(!command.input_schema.to_string().contains("network_access"));
        let (_, execution_input) = registry
            .registry
            .prepare_model_input(TOOL_COMMAND, &payload)
            .expect("declared command model input");
        assert_eq!(execution_input, payload);
        assert!(
            registry
                .registry
                .validate_execution_input(TOOL_COMMAND, &execution_input)
                .is_ok()
        );
        let mut undeclared = payload;
        undeclared["command"] = json!("cargo check");
        assert_eq!(
            registry
                .registry
                .prepare_model_input(TOOL_COMMAND, &undeclared)
                .expect_err("undeclared command must fail locally")
                .code,
            "input_not_allowed"
        );
    }

    #[test]
    fn smoke_commands_require_a_registry_resolved_command_capability() {
        let projection = AgentTaskProjection {
            task_id: TaskId::new("task-1").expect("task id"),
            description: "description".to_string(),
            instructions: "fix".to_string(),
            allowed_paths: vec![RelativePath::new("src/lib.rs").expect("path")],
            required_tool_capabilities: vec![requirement(ToolCapability::WorkspaceRead)],
            smoke_commands: vec![command(&["cargo", "test"])],
        };

        let error = evaluation_registry(&projection).expect_err("missing command capability");
        assert!(
            error
                .to_string()
                .contains("registry-resolved command execution tool")
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
        let trace_store = singularity_store::SessionStore::open(":memory:").expect("trace store");
        let error = run_evaluation(
            &EvalRunParams {
                manifest: "missing-manifest.json".to_string(),
                run_id: "cancelled-run".to_string(),
                output_root: None,
            },
            Arc::new(SourceSandboxBackend),
            &ProviderConfigSnapshot::capture(|_| None),
            &cancellation,
            &trace_store,
        )
        .expect_err("cancelled evaluation must not start");

        assert_eq!(error.kind(), EvaluationRunErrorKind::Cancelled);
        let partial = error.partial_result().expect("partial terminal result");
        assert_eq!(partial.status, "blocked");
        assert_eq!(partial.blocker.as_deref(), Some("evaluation_cancelled"));
        assert!(partial.tasks.is_empty());
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
        let task_result = EvaluationTaskResult::from_trials(
            task_id,
            Vec::new(),
            Vec::new(),
            vec![task.result.clone()],
        );
        let execution = TaskEvaluation {
            result: task_result,
            trials: vec![task],
        };
        let run_id = RunId::new("partial-safe").expect("run id");
        let partial = partial_evaluation_result(
            &EvalRunParams {
                manifest: "C:\\secret-workspace\\manifest.json".to_string(),
                run_id: run_id.as_str().to_string(),
                output_root: None,
            },
            &run_id,
            std::slice::from_ref(&execution),
        );
        let serialized = serde_json::to_string(&partial).expect("partial result serializes");

        assert_eq!(partial.manifest, "[redacted]");
        assert_eq!(partial.tasks.len(), 1);
        assert!(!serialized.contains("secret-workspace"));
        assert!(!serialized.contains("agent-trace.json"));
        assert!(!serialized.contains("patch-evidence.json"));
    }

    #[cfg(windows)]
    #[test]
    fn run_evaluation_rejects_long_paths_before_creating_run_directory() {
        let temp = tempfile::tempdir().expect("temp");
        let manifest_path = temp.path().join("manifest.json");
        let run_id = "a".repeat(40);
        let task_id = "b".repeat(40);
        let manifest = json!({
            "schema_version": "evaluation.task_set/v5",
            "trial_count": 1,
            "tasks": [{
                "task_id": task_id,
                "description": "path budget preflight",
                "capabilities": ["rust"],
                "workspace": {
                    "source": {"type": "local", "path": "missing-source"}
                },
                "agent": {
                    "instructions": "inspect",
                    "allowed_paths": ["README.md"],
                    "required_tool_capabilities": [
                        {"capability": "workspace_read", "minimum_version": 1}
                    ]
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
        let params = EvalRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id,
            output_root: Some(output_root.to_string_lossy().into_owned()),
        };
        let trace_store = singularity_store::SessionStore::open(":memory:").expect("trace store");

        let error = run_evaluation(
            &params,
            Arc::new(SourceSandboxBackend),
            &ProviderConfigSnapshot::capture(|_| None),
            &CancellationToken::new(),
            &trace_store,
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
    fn evaluation_change_evidence_ignores_toolchain_artifacts_but_keeps_disallowed_source() {
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
        fs::write(temp.path().join("target/tracked.rs"), "after")
            .expect("modify tracked target file");
        fs::remove_file(temp.path().join("coverage/tracked.txt"))
            .expect("delete tracked coverage file");
        fs::write(temp.path().join("src/disallowed.rs"), "user source").expect("disallowed source");
        let after = snapshot_workspace(temp.path()).expect("after");
        let allowed_paths = [RelativePath::new("src/lib.rs").expect("allowed path")];
        let changed_files = evaluation_changed_paths(&before, &after, &pristine_source);

        let evidence = workspace_change_evidence(&before, &after, &pristine_source, &allowed_paths);
        let paths = evidence
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            changed_files,
            [
                "coverage/tracked.txt",
                "generated/cache.bin",
                "src/disallowed.rs",
                "target/tracked.rs",
            ]
        );
        assert_eq!(
            paths,
            [
                "coverage/tracked.txt",
                "generated/cache.bin",
                "src/disallowed.rs",
                "target/tracked.rs",
            ]
        );
        assert!(evidence.iter().all(|change| !change.allowed));
    }

    #[test]
    fn evaluation_write_policy_allows_only_declared_path_trees() {
        let projection = AgentTaskProjection {
            task_id: TaskId::new("task-1").expect("task id"),
            description: "description".to_string(),
            instructions: "fix".to_string(),
            allowed_paths: vec![RelativePath::new("src").expect("path")],
            required_tool_capabilities: vec![requirement(ToolCapability::WorkspaceWrite)],
            smoke_commands: Vec::new(),
        };
        let resolved_tools = evaluation_registry(&projection).expect("resolved write tools");
        assert!(resolved_tools.registry.get(TOOL_EDIT).is_some());
        assert!(resolved_tools.registry.get(TOOL_PATCH).is_some());
        let policy = evaluation_policy(Path::new("C:/workspace"), &projection, &resolved_tools);
        let allowed = policy.evaluate(&singularity_policy::PermissionRequest::new(
            singularity_policy::ToolId::new(TOOL_EDIT).expect("tool id"),
            PermissionOperation::Write,
            PermissionResource::WorkspacePath(
                WorkspaceRelativePath::from_canonical("src/lib.rs").expect("path"),
            ),
        ));
        let denied = policy.evaluate(&singularity_policy::PermissionRequest::new(
            singularity_policy::ToolId::new(TOOL_EDIT).expect("tool id"),
            PermissionOperation::Write,
            PermissionResource::WorkspacePath(
                WorkspaceRelativePath::from_canonical("src2/lib.rs").expect("path"),
            ),
        ));

        assert_eq!(allowed.outcome, PermissionDecisionOutcome::Allow);
        assert_eq!(denied.outcome, PermissionDecisionOutcome::Deny);
    }

    #[test]
    fn allowed_paths_cover_exact_file_or_descendants_only() {
        let allowed = [RelativePath::new("src").expect("path")];
        assert!(path_is_allowed("src/lib.rs", &allowed));
        assert!(!path_is_allowed("src2/lib.rs", &allowed));
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
            if !request.is_trusted_workspace_preparation() {
                return CommandResult::backend_error(
                    &request.command_id,
                    "source test backend does not execute evaluator commands",
                )
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            if request.argv.get(1).map(String::as_str) == Some("init") {
                return CommandResult::completed(&request.command_id, "prepared")
                    .with_workspace_mutation(WorkspaceMutation::Changed)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            if request.argv.get(1).map(String::as_str) == Some("clone") {
                let source = Path::new(&request.cwd).join(SOURCE_DIR);
                fs::create_dir(&source).expect("source directory");
                fs::write(source.join("README.md"), "fixture").expect("source file");
            } else {
                assert_eq!(request.cwd, request.filesystem.workspace_root);
                assert_eq!(
                    request.argv.get(1..4),
                    Some(["-C", SOURCE_DIR, "checkout"].map(String::from).as_slice())
                );
            }
            CommandResult::completed(&request.command_id, "ok")
                .with_workspace_mutation(WorkspaceMutation::Changed)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
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
            CommandResult::backend_error(
                &request.command_id,
                "executable preflight must prevent command execution",
            )
            .with_workspace_mutation(WorkspaceMutation::Unknown)
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

    struct AgentLoopReachBackend;

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

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            let result = if request.argv.first().map(String::as_str) == Some("verify-baseline") {
                CommandResult::executed(&request.command_id, 1, 0, "", "expected failure", false)
            } else {
                CommandResult::completed(&request.command_id, "verified")
            };
            result
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
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
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                request.argv.get(1).map(String::as_str),
                Some("--git-dir=.singularity-evaluator-git")
            );
            assert_eq!(
                request.argv.get(2).map(String::as_str),
                Some("--work-tree=.")
            );
            if call == 0 {
                assert_eq!(
                    request.argv.get(3).map(String::as_str),
                    Some("init"),
                    "the first evaluator patch command must create the isolated Git metadata"
                );
                assert!(request.is_trusted_workspace_preparation());
            } else {
                assert_eq!(request.argv.get(3).map(String::as_str), Some("apply"));
                assert!(
                    request.is_trusted_workspace_preparation(),
                    "fixed evaluator Git operations share the internal workspace-preparation boundary"
                );
            }
            CommandResult::completed(&request.command_id, "ok")
                .with_workspace_mutation(WorkspaceMutation::Changed)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
    }

    #[test]
    fn evaluator_patch_uses_only_fixed_trusted_git_operations() {
        let workspace = tempfile::tempdir().expect("workspace");
        let patch: singularity_evaluation::EvaluatorTestPatch =
            serde_json::from_value(serde_json::json!({
                "format": "unified_diff",
                "content": "--- a/example.txt\n+++ b/example.txt\n"
            }))
            .expect("test patch");
        let backend = Arc::new(EvaluatorPatchSandboxBackend {
            calls: AtomicUsize::new(0),
        });
        let mut diagnostics = Vec::new();

        apply_evaluator_patch(workspace.path(), &patch, backend.clone(), &mut diagnostics)
            .expect("apply evaluator patch");

        assert_eq!(backend.calls.load(Ordering::SeqCst), 4);
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.phase.as_str())
                .collect::<Vec<_>>(),
            [
                "evaluator.git_init",
                "evaluator.apply_check",
                "evaluator.apply_patch",
                "evaluator.reverse_check",
            ]
        );
        assert!(!workspace.path().join(".git").exists());
        assert!(!workspace.path().join(EVALUATOR_GIT_DIR).exists());
        assert!(!workspace.path().join(EVALUATOR_PATCH_FILE).exists());
    }

    #[test]
    fn evaluator_patch_rejects_a_preexisting_control_git_directory() {
        let workspace = tempfile::tempdir().expect("workspace");
        let git_dir = workspace.path().join(EVALUATOR_GIT_DIR);
        fs::create_dir(&git_dir).expect("preexisting control directory");
        fs::write(git_dir.join("owned.txt"), "source content").expect("source content");
        let patch: singularity_evaluation::EvaluatorTestPatch =
            serde_json::from_value(serde_json::json!({
                "format": "unified_diff",
                "content": "--- a/example.txt\n+++ b/example.txt\n"
            }))
            .expect("test patch");
        let backend = Arc::new(EvaluatorPatchSandboxBackend {
            calls: AtomicUsize::new(0),
        });
        let mut diagnostics = Vec::new();

        let blocker =
            apply_evaluator_patch(workspace.path(), &patch, backend.clone(), &mut diagnostics)
                .expect_err("control path collision must fail closed");

        assert_eq!(blocker.kind, BlockerKind::WorkspacePreparation);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            fs::read_to_string(git_dir.join("owned.txt")).expect("preserved source content"),
            "source content"
        );
        assert!(!workspace.path().join(EVALUATOR_PATCH_FILE).exists());
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
    fn successful_remote_source_preserves_clone_and_checkout_diagnostics() {
        let temp = tempfile::tempdir().expect("temp");
        let task_dir = temp.path().join("task");
        fs::create_dir(&task_dir).expect("task directory");
        let source_dir = task_dir.join(SOURCE_DIR);
        let source = PlannedWorkspaceSource::RemoteGit {
            repository: RemoteRepository::new("https://github.com/example/example.git")
                .expect("repository"),
            commit: GitCommit::new("0123456789abcdef0123456789abcdef01234567").expect("commit"),
        };

        let diagnostics = prepare_source(
            &source,
            &task_dir,
            &source_dir,
            Arc::new(SourceSandboxBackend),
        )
        .expect("prepare source");

        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].phase, "source.git_clone");
        assert_eq!(diagnostics[1].phase, "source.git_checkout");
        assert!(source_dir.join("README.md").is_file());
    }

    #[test]
    fn trials_reuse_one_read_only_prepared_source_and_keep_stage_roots_isolated() {
        let temp = tempfile::tempdir().expect("temp");
        let fixture = temp.path().join("fixture");
        let run_dir = temp.path().join("run");
        fs::create_dir(&fixture).expect("fixture");
        fs::create_dir(&run_dir).expect("run directory");
        fs::write(fixture.join("README.md"), "seed").expect("fixture file");
        let manifest_json = json!({
            "schema_version": "evaluation.task_set/v5",
            "trial_count": 3,
            "tasks": [{
                "task_id": "source-reuse",
                "description": "verify source reuse",
                "capabilities": ["repository_context"],
                "workspace": {"source": {"type": "local", "path": "fixture"}},
                "agent": {
                    "instructions": "inspect",
                    "allowed_paths": ["README.md"],
                    "required_tool_capabilities": [
                        {"capability": "workspace_read", "minimum_version": 1}
                    ]
                },
                "evaluator": {
                    "baseline": {"commands": [{"argv": ["cargo", "test"]}]},
                    "public": {"commands": [{"argv": ["cargo", "test"]}]},
                    "hidden": {"commands": [{"argv": ["cargo", "check"]}]}
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
        let provider_snapshot = ProviderConfigSnapshot::capture(|_| None);
        let cancellation = CancellationToken::new();
        let trace_store = singularity_store::SessionStore::open(":memory:").expect("trace store");
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
            trace_store: &trace_store,
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
                EvaluationStatus::Blocked
            );
        }
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
                "schema_version": "evaluation.task_set/v5",
                "trial_count": trial_count,
                "tasks": [{
                    "task_id": task_id,
                    "description": description,
                    "capabilities": ["repository_context"],
                    "workspace": {"source": source},
                    "agent": {
                        "instructions": "inspect README.md",
                        "allowed_paths": ["README.md"],
                        "required_tool_capabilities": [
                            {"capability": "workspace_read", "minimum_version": 1}
                        ]
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
        let params = EvalRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "preflight-supported-run".to_string(),
            output_root: Some(output_root.to_string_lossy().into_owned()),
        };
        let base_url = format!("http://{provider_address}/v1");
        let provider_snapshot = ProviderConfigSnapshot::capture(|name| match name {
            "SINGULARITY_API_KEY" => Some("fixture-key".to_string()),
            "SINGULARITY_BASE_URL" => Some(base_url.clone()),
            "SINGULARITY_MODEL" => Some("fixture-model".to_string()),
            _ => None,
        });
        let trace_store = singularity_store::SessionStore::open(":memory:").expect("trace store");

        let response = run_evaluation(
            &params,
            Arc::new(AgentLoopReachBackend),
            &provider_snapshot,
            &CancellationToken::new(),
            &trace_store,
        )
        .expect("provider rejection still publishes evaluation artifacts");
        let result = EvaluationResult::from_json_str(
            &fs::read_to_string(response.result_path.expect("result path"))
                .expect("result artifact"),
        )
        .expect("result v8");
        let provider_calls = provider.join().expect("provider fixture join");

        assert_eq!(
            provider_calls, 1,
            "AgentLoop must issue one provider request; result={result:?}"
        );
        assert_eq!(result.summary.configured_trial_count, 1);
        assert_eq!(result.summary.sampled_trial_count, 1);
        assert_eq!(result.summary.trial_count, 1);
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
        assert!(
            output_root
                .join("preflight-supported-run")
                .join("preflight-supported")
                .join("trial-0001")
                .is_dir()
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
        let params = EvalRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "preflight-blocked-run".to_string(),
            output_root: Some(output_root.to_string_lossy().into_owned()),
        };
        let executions = Arc::new(AtomicUsize::new(0));
        let trace_store = singularity_store::SessionStore::open(":memory:").expect("trace store");

        let response = run_evaluation(
            &params,
            Arc::new(UnsupportedPreflightBackend {
                executions: Arc::clone(&executions),
            }),
            &ProviderConfigSnapshot::capture(|_| None),
            &CancellationToken::new(),
            &trace_store,
        )
        .expect("preflight blocker publishes typed artifacts");

        assert_eq!(response.status, "blocked");
        assert!(response.tasks.is_empty());
        assert_eq!(executions.load(Ordering::SeqCst), 0);
        let result = EvaluationResult::from_json_str(
            &fs::read_to_string(response.result_path.expect("result path"))
                .expect("result artifact"),
        )
        .expect("result v8");
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
        let params = EvalRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "executable-blocked-run".to_string(),
            output_root: Some(output_root.to_string_lossy().into_owned()),
        };
        let executions = Arc::new(AtomicUsize::new(0));
        let trace_store = singularity_store::SessionStore::open(":memory:").expect("trace store");

        let response = run_evaluation(
            &params,
            Arc::new(UnavailableExecutableBackend {
                executions: Arc::clone(&executions),
            }),
            &ProviderConfigSnapshot::capture(|_| None),
            &CancellationToken::new(),
            &trace_store,
        )
        .expect("executable blocker publishes typed artifacts");

        assert_eq!(response.status, "blocked");
        assert!(response.tasks.is_empty());
        assert_eq!(executions.load(Ordering::SeqCst), 0);
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
        .expect("result v8");
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
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
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
        let params = EvalRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "remote-source-blocked-run".to_string(),
            output_root: Some(output_root.to_string_lossy().into_owned()),
        };
        let backend = Arc::new(RemoteSourceProbeBackend {
            calls: AtomicUsize::new(0),
            fail_probe: true,
        });
        let trace_store = singularity_store::SessionStore::open(":memory:").expect("trace store");
        let response = run_evaluation(
            &params,
            backend.clone(),
            &ProviderConfigSnapshot::capture(|_| None),
            &CancellationToken::new(),
            &trace_store,
        )
        .expect("remote source blocker publishes typed artifacts");

        assert_eq!(response.status, "blocked");
        assert!(response.tasks.is_empty());
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        let result = EvaluationResult::from_json_str(
            &fs::read_to_string(response.result_path.expect("result path"))
                .expect("result artifact"),
        )
        .expect("result v8");
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
        let params = EvalRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "preparation-blocked-run".to_string(),
            output_root: Some(output_root.to_string_lossy().into_owned()),
        };
        let executions = Arc::new(AtomicUsize::new(0));
        let trace_store = singularity_store::SessionStore::open(":memory:").expect("trace store");

        let response = run_evaluation(
            &params,
            Arc::new(UnknownPreparationBackend {
                executions: Arc::clone(&executions),
            }),
            &ProviderConfigSnapshot::capture(|_| None),
            &CancellationToken::new(),
            &trace_store,
        )
        .expect("trusted preparation blocker publishes typed artifacts");

        assert_eq!(response.status, "blocked");
        assert!(response.tasks.is_empty());
        assert_eq!(executions.load(Ordering::SeqCst), 2);
        let result = EvaluationResult::from_json_str(
            &fs::read_to_string(response.result_path.expect("result path"))
                .expect("result artifact"),
        )
        .expect("result v8");
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
    fn v5_blocked_run_publishes_v8_result_and_v3_evidence_as_one_artifact_set() {
        let temp = tempfile::tempdir().expect("temp");
        let fixture = temp.path().join("fixture");
        let output_root = temp.path().join("output");
        fs::create_dir(&fixture).expect("fixture");
        fs::write(fixture.join("README.md"), "seed").expect("fixture file");
        let manifest_path = temp.path().join("manifest.json");
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&json!({
                "schema_version": "evaluation.task_set/v5",
                "trial_count": 2,
                "tasks": [{
                    "task_id": "blocked-artifacts",
                    "description": "verify blocked artifacts",
                    "capabilities": ["repository_context"],
                    "workspace": {"source": {"type": "local", "path": "fixture"}},
                    "agent": {
                        "instructions": "inspect",
                        "allowed_paths": ["README.md"],
                        "required_tool_capabilities": [
                            {"capability": "workspace_read", "minimum_version": 1}
                        ]
                    },
                    "evaluator": {
                        "baseline": {"commands": [{"argv": ["cargo", "test"]}]},
                        "public": {"commands": [{"argv": ["cargo", "test"]}]},
                        "hidden": {"commands": [{"argv": ["cargo", "check"]}]}
                    }
                }]
            }))
            .expect("manifest JSON"),
        )
        .expect("manifest file");
        let params = EvalRunParams {
            manifest: manifest_path.to_string_lossy().into_owned(),
            run_id: "blocked-artifacts-run".to_string(),
            output_root: Some(output_root.to_string_lossy().into_owned()),
        };
        let trace_store = singularity_store::SessionStore::open(":memory:").expect("trace store");

        let response = run_evaluation(
            &params,
            Arc::new(SourceSandboxBackend),
            &ProviderConfigSnapshot::capture(|_| None),
            &CancellationToken::new(),
            &trace_store,
        )
        .expect("blocked run still publishes typed artifacts");

        assert_eq!(response.status, "blocked");
        let result_path = PathBuf::from(response.result_path.expect("result path"));
        let evidence_path = PathBuf::from(response.evidence_path.expect("evidence path"));
        let result = EvaluationResult::from_json_str(
            &fs::read_to_string(&result_path).expect("result artifact"),
        )
        .expect("result v8");
        let evidence = singularity_evaluation::EvaluationEvidence::from_json_str(
            &fs::read_to_string(&evidence_path).expect("evidence artifact"),
        )
        .expect("evidence v3");
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

        let local = source_provenance(
            &PlannedWorkspaceSource::Local {
                path: local_source.clone(),
            },
            &local_materialized,
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
            &local_materialized,
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
            &json!({"schema_version": "evaluation.evidence/v3"}),
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
            &json!({"schema_version": "evaluation.result/v8"}),
            &json!({"runner": RUNNER_NAME}),
            &json!({"schema_version": "evaluation.evidence/v3"}),
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
}
