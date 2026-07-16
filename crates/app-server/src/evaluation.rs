//! Evaluation runner 的任务投影、Agent stage、验证证据与安全产物协调。
//!
//! 本模块只把 manifest 的可信内部命令和模型可见 command string 分开投影，
//! 并在固定 gate、sandbox 与 evidence 合同下汇总结果。

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::Serialize;
use serde_json::{Value, json};
use singularity_agent::{
    AgentLoop, AgentLoopInput, AgentLoopResult, AgentRecoveryMetrics, AgentStatus,
    AgentVerificationRequirement, UPDATE_PLAN_TOOL, agent_control_tool_specs,
    eligible_command_scope_digests,
};
use singularity_core::{contains_sensitive_text, load_project_instructions_from_cwd};
use singularity_evaluation::{
    AgentStagePlan, AgentTaskProjection, BlockerKind, CommandExpectation, CommandSpec,
    EvaluationBlocker, EvaluationCapability, EvaluationEvidenceSummary, EvaluationManifest,
    EvaluationResult, EvaluationResultSchemaVersion, EvaluationRunSummary, EvaluationStageResults,
    EvaluationStatus, EvaluationTaskResult, PatchFormat, PlannedWorkspaceSource, RunId,
    StageResult, StageStatus, TaskId, VerificationStagePlan, WorkspacePlan,
};
use singularity_model::{
    ModelErrorCategory, ModelUsage, OpenAiProvider, ProviderAttemptMetadata,
    ProviderConfigSnapshot, ProviderDiagnostic, ProviderError, ProviderErrorStage,
};
use singularity_policy::{
    ApprovalPolicy, NetworkAccess, PermissionDecisionOutcome, PermissionOperation,
    PermissionProfile, PermissionRule, PolicyEngine, SettingsScope,
};
use singularity_protocol::{EvalRunParams, EvalRunResult};
use singularity_tools::{
    CommandEnvironmentPolicy, SandboxBackend, SandboxFilesystemMode, SandboxNetworkMode,
    ToolBroker, ToolRegistry, WorkspaceTools, command_script_scope_digest_with_policy,
    command_script_scope_resource_with_policy, workspace_tool_specs,
};

use super::{TOOL_COMMAND, TOOL_EDIT, TOOL_GREP, TOOL_LIST, TOOL_PATCH, TOOL_READ};

mod command;
mod evidence;
mod workspace;

use command::{
    CommandDiagnostic, command_blocker, command_succeeded, infrastructure_blocker,
    run_command_spec, run_raw_command, sandbox_network_mode,
};
use evidence::{
    agent_command_observation, build_evaluation_evidence, content_digest, safe_command_scope_digest,
};
use workspace::{
    WorkspaceChangeEvidence, apply_agent_changes, copy_tree_checked, evaluation_changed_paths,
    patch_evidence_digest, path_is_allowed, snapshot_workspace, validate_tree,
    workspace_change_evidence, workspace_tree_digest,
};

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
const RESULT_FILE: &str = "result.json";
const REPORT_FILE: &str = "report.json";
const EVIDENCE_FILE: &str = "evidence.json";
const AGENT_TRACE_FILE: &str = "agent-trace.json";
const PATCH_EVIDENCE_FILE: &str = "patch-evidence.json";
const ARTIFACT_TEMP_FILE_ATTEMPTS: usize = 64;
const WINDOWS_MAX_PATH_CHARS: usize = 260;
const GIT_PACK_HEX: &str = "0123456789012345678901234567890123456789";
const CARGO_DEP_HEX: &str = "0123456789012345678901234567890123456789012345678901234567890123";

static ARTIFACT_COUNTER: AtomicU64 = AtomicU64::new(0);

type SharedSandboxBackend = Arc<dyn SandboxBackend + Send + Sync>;

#[derive(Debug, Clone, Default, Serialize)]
struct StageDiagnostics {
    message: Option<String>,
    commands: Vec<CommandDiagnostic>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct TaskDiagnostics {
    #[serde(skip)]
    capabilities: Vec<EvaluationCapability>,
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
}

struct TaskExecution {
    result: EvaluationTaskResult,
    diagnostics: TaskDiagnostics,
}

pub(crate) fn run_evaluation(
    params: &EvalRunParams,
    sandbox_backend: SharedSandboxBackend,
    provider_snapshot: &ProviderConfigSnapshot,
) -> Result<EvalRunResult, String> {
    let manifest_path = Path::new(&params.manifest);
    let manifest_json = fs::read_to_string(manifest_path).map_err(|error| {
        format!(
            "invalid eval manifest: failed to read {}: {error}",
            manifest_path.display()
        )
    })?;
    let manifest_digest = content_digest(manifest_json.as_bytes());
    let manifest_dir = manifest_path.parent().ok_or_else(|| {
        format!(
            "invalid eval manifest: manifest path has no parent: {}",
            manifest_path.display()
        )
    })?;
    let manifest = EvaluationManifest::from_json_str(&manifest_json, manifest_dir)
        .map_err(|error| format!("invalid eval manifest: {error}"))?;
    let run_id = RunId::new(params.run_id.clone())
        .map_err(|error| format!("invalid eval run id: {error}"))?;
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
        .collect::<Result<Vec<_>, _>>()?;
    let task_ids = plans
        .iter()
        .map(|plan| plan.task_id.clone())
        .collect::<Vec<_>>();
    preflight_evaluation_path_budget(&output_root, &run_id, &task_ids)?;
    fs::create_dir_all(&output_root).map_err(|error| {
        format!(
            "failed to create evaluation output root {}: {error}",
            output_root.display()
        )
    })?;
    let run_dir = output_root.join(run_id.as_str());
    fs::create_dir(&run_dir).map_err(|error| {
        format!(
            "failed to create new evaluation run directory {}: {error}",
            run_dir.display()
        )
    })?;

    let mut task_executions = Vec::new();
    for plan in &plans {
        task_executions.push(run_task(
            &run_id,
            &run_dir,
            manifest.manifest_dir(),
            plan,
            Arc::clone(&sandbox_backend),
            provider_snapshot,
        ));
    }

    let tasks = task_executions
        .iter()
        .map(|execution| execution.result.clone())
        .collect::<Vec<_>>();
    let evaluation_passed = tasks.iter().all(|task| task.evaluation_passed);
    let blocker = tasks.iter().find_map(|task| task.blocker.clone());
    let status = if blocker.is_some() {
        EvaluationStatus::Blocked
    } else if evaluation_passed {
        EvaluationStatus::Completed
    } else {
        EvaluationStatus::Failed
    };
    let result = EvaluationResult {
        schema_version: EvaluationResultSchemaVersion::V5,
        run_id: run_id.clone(),
        status,
        blocker,
        evaluation_passed,
        summary: EvaluationRunSummary::from_tasks(&tasks),
        tasks,
    };
    if let Err(error) = result.validate() {
        return Err(cleanup_incomplete_run(
            &run_dir,
            format!("invalid evaluation result: {error}"),
        ));
    }

    let result_path = run_dir.join(RESULT_FILE);
    let report_path = run_dir.join(REPORT_FILE);
    let evidence_path = run_dir.join(EVIDENCE_FILE);
    let evidence = match build_evaluation_evidence(
        &run_id,
        manifest_digest,
        &plans,
        &task_executions,
        &run_dir,
    ) {
        Ok(evidence) => evidence,
        Err(error) => return Err(cleanup_incomplete_run(&run_dir, error)),
    };
    if let Err(error) = evidence.validate_against_result(&result) {
        return Err(cleanup_incomplete_run(
            &run_dir,
            format!("evaluation evidence/result mismatch: {error}"),
        ));
    }
    let status_string = match enum_string(result.status) {
        Ok(status) => status,
        Err(error) => return Err(cleanup_incomplete_run(&run_dir, error)),
    };
    let task_reports = task_executions.iter().map(task_report).collect::<Vec<_>>();
    let report = json!({
        "manifest": params.manifest,
        "runner": RUNNER_NAME,
        "result": result,
        "tasks": task_reports,
        "result_path": result_path.to_string_lossy(),
        "report_path": report_path.to_string_lossy(),
        "evidence_path": evidence_path.to_string_lossy(),
    });
    if let Err(error) = publish_evaluation_artifacts(
        &result_path,
        &result,
        &report_path,
        &report,
        &evidence_path,
        &evidence,
    ) {
        return Err(cleanup_incomplete_run(&run_dir, error));
    }

    Ok(EvalRunResult {
        run_id: run_id.as_str().to_string(),
        manifest: params.manifest.clone(),
        runner: RUNNER_NAME.to_string(),
        status: status_string,
        blocker: result.blocker.as_ref().map(blocker_code),
        tasks: report["tasks"].as_array().cloned().unwrap_or_default(),
        result_path: Some(result_path.to_string_lossy().into_owned()),
        report_path: Some(report_path.to_string_lossy().into_owned()),
        evidence_path: Some(evidence_path.to_string_lossy().into_owned()),
        evaluation_passed: result.evaluation_passed,
    })
}

fn cleanup_incomplete_run(run_dir: &Path, error: String) -> String {
    match fs::remove_dir_all(run_dir) {
        Ok(()) => error,
        Err(cleanup_error) => format!(
            "{error}; failed to clean incomplete evaluation run {}: {cleanup_error}",
            run_dir.display()
        ),
    }
}

fn task_report(execution: &TaskExecution) -> Value {
    let mut report =
        serde_json::to_value(&execution.result).expect("evaluation task result serializes");
    if let Some(object) = report.as_object_mut() {
        object.insert(
            "diagnostics".to_string(),
            serde_json::to_value(&execution.diagnostics).expect("evaluation diagnostics serialize"),
        );
    }
    report
}

fn run_task(
    run_id: &RunId,
    run_dir: &Path,
    manifest_dir: &Path,
    plan: &WorkspacePlan,
    sandbox_backend: SharedSandboxBackend,
    provider_snapshot: &ProviderConfigSnapshot,
) -> TaskExecution {
    let task_dir = run_dir.join(plan.task_id.as_str());
    let source_dir = task_dir.join(SOURCE_DIR);
    let mut diagnostics = TaskDiagnostics {
        capabilities: plan.capabilities.clone(),
        source: Some(source_provenance(&plan.source, &source_dir, manifest_dir)),
        smoke_command_satisfied: plan.agent.projection.smoke_commands.is_empty(),
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
        return blocked_task_before_workspace(&plan.task_id, blocker, diagnostics);
    }

    let capability = super::agent_loop_capability(sandbox_backend.as_ref());
    if !capability.available {
        let blocker = evaluation_blocker(
            BlockerKind::Environment,
            format!("{}: {}", capability.status.as_str(), capability.reason),
        );
        return blocked_task_before_workspace(&plan.task_id, blocker, diagnostics);
    }

    let mut provider = if matches!(plan.source, PlannedWorkspaceSource::RemoteGit { .. }) {
        match provider_snapshot.provider() {
            Ok(provider) => Some(provider),
            Err(error) => {
                let blocker = provider_blocker(&error);
                diagnostics.error = Some(safe_text(error.message));
                return blocked_task_before_workspace(&plan.task_id, blocker, diagnostics);
            }
        }
    } else {
        None
    };

    match prepare_source(
        &plan.source,
        &task_dir,
        &source_dir,
        Arc::clone(&sandbox_backend),
    ) {
        Ok(commands) => {
            diagnostics.source_commands = commands;
            diagnostics.source = Some(source_provenance(&plan.source, &source_dir, manifest_dir));
        }
        Err((blocker, commands)) => {
            diagnostics.source_commands = commands;
            let baseline = StageExecution::blocked(blocker, Vec::new());
            let agent =
                StageExecution::skipped("agent stage skipped because source preparation failed");
            let public =
                StageExecution::skipped("public stage skipped because source preparation failed");
            let hidden =
                StageExecution::skipped("hidden stage skipped because source preparation failed");
            return finish_task(&plan.task_id, baseline, agent, public, hidden, diagnostics);
        }
    }

    let provider = match provider.take() {
        Some(provider) => provider,
        None => match provider_snapshot.provider() {
            Ok(provider) => provider,
            Err(error) => {
                let blocker = provider_blocker(&error);
                diagnostics.error = Some(safe_text(error.message));
                return blocked_task_before_workspace(&plan.task_id, blocker, diagnostics);
            }
        },
    };

    let baseline = run_verification_stage(
        &source_dir,
        &task_dir.join(BASELINE_DIR),
        &plan.baseline.setup_commands,
        plan.baseline.test_patch.as_ref(),
        &plan.baseline.commands,
        plan.baseline.expectation,
        Arc::clone(&sandbox_backend),
    );
    diagnostics.baseline = baseline.diagnostics.clone();
    if baseline.result.status != StageStatus::Passed {
        let agent = StageExecution::skipped(
            "agent stage skipped because baseline did not fail as expected",
        );
        let public = StageExecution::skipped("public stage skipped because baseline did not pass");
        let hidden = StageExecution::skipped("hidden stage skipped because baseline did not pass");
        return finish_task(&plan.task_id, baseline, agent, public, hidden, diagnostics);
    }

    let agent_execution = run_agent_stage(
        run_id,
        &source_dir,
        &task_dir.join(AGENT_DIR),
        &plan.agent,
        provider,
        Arc::clone(&sandbox_backend),
        &task_dir,
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
            &plan.task_id,
            baseline,
            agent_execution.stage,
            public,
            hidden,
            diagnostics,
        );
    };

    let public = run_verification_stage_with_agent_changes(
        &source_dir,
        agent_workspace,
        &agent_execution.changed_files,
        &task_dir.join(PUBLIC_DIR),
        &plan.public,
        Arc::clone(&sandbox_backend),
    );
    diagnostics.public = public.diagnostics.clone();
    let hidden = run_verification_stage_with_agent_changes(
        &source_dir,
        agent_workspace,
        &agent_execution.changed_files,
        &task_dir.join(HIDDEN_DIR),
        &plan.hidden,
        sandbox_backend,
    );
    diagnostics.hidden = hidden.diagnostics.clone();
    finish_task(
        &plan.task_id,
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
    blocker: EvaluationBlocker,
    mut diagnostics: TaskDiagnostics,
) -> TaskExecution {
    diagnostics.agent.message = Some(blocker.message.clone());
    diagnostics.error = Some(blocker.message.clone());
    finish_task(
        task_id,
        StageExecution::skipped("baseline stage not run"),
        StageExecution::blocked(blocker, Vec::new()),
        StageExecution::skipped("public stage not run"),
        StageExecution::skipped("hidden stage not run"),
        diagnostics,
    )
}

fn finish_task(
    task_id: &TaskId,
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
    let result = EvaluationTaskResult {
        task_id: task_id.clone(),
        capabilities: diagnostics.capabilities.clone(),
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
            let clone = run_raw_command(
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
            let checkout = run_raw_command(
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

    let init_result = run_raw_command(
        workspace,
        workspace,
        vec!["git".to_string(), "init".to_string(), "--quiet".to_string()],
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

    let check_result = run_raw_command(
        workspace,
        workspace,
        vec![
            "git".to_string(),
            "apply".to_string(),
            "--check".to_string(),
            "--whitespace=nowarn".to_string(),
            EVALUATOR_PATCH_FILE.to_string(),
        ],
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

    let result = run_raw_command(
        workspace,
        workspace,
        vec![
            "git".to_string(),
            "apply".to_string(),
            "--whitespace=nowarn".to_string(),
            EVALUATOR_PATCH_FILE.to_string(),
        ],
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
    let reverse_check = run_raw_command(
        workspace,
        workspace,
        vec![
            "git".to_string(),
            "apply".to_string(),
            "--reverse".to_string(),
            "--check".to_string(),
            "--whitespace=nowarn".to_string(),
            EVALUATOR_PATCH_FILE.to_string(),
        ],
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
    let remove_result = fs::remove_file(&patch_path);
    let git_dir = workspace.join(".git");
    if git_dir.exists() {
        fs::remove_dir_all(&git_dir).map_err(|error| {
            evaluation_blocker(
                BlockerKind::WorkspacePreparation,
                format!("failed to remove evaluator git metadata: {error}"),
            )
        })?;
    }
    remove_result.map_err(|error| {
        evaluation_blocker(
            BlockerKind::WorkspacePreparation,
            format!("failed to remove evaluator patch file: {error}"),
        )
    })
}
fn run_agent_stage(
    run_id: &RunId,
    source_dir: &Path,
    agent_dir: &Path,
    plan: &AgentStagePlan,
    provider: OpenAiProvider,
    sandbox_backend: SharedSandboxBackend,
    task_dir: &Path,
) -> AgentStageExecution {
    let projection = &plan.projection;
    let pristine_source = match snapshot_workspace(source_dir) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                Vec::new(),
            );
        }
    };
    if let Err(error) = copy_tree_checked(source_dir, agent_dir) {
        return blocked_agent_stage(
            evaluation_blocker(BlockerKind::WorkspacePreparation, error),
            Vec::new(),
        );
    }
    let mut command_diagnostics = Vec::new();
    if let Err(blocker) = run_setup_commands(
        agent_dir,
        &plan.setup_commands,
        Arc::clone(&sandbox_backend),
        &mut command_diagnostics,
    ) {
        return blocked_agent_stage(blocker, command_diagnostics);
    }
    let before = match snapshot_workspace(agent_dir) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error),
                command_diagnostics,
            );
        }
    };
    let project_instructions = match load_project_instructions_from_cwd(agent_dir) {
        Ok(instructions) => instructions,
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::WorkspacePreparation, error.to_string()),
                command_diagnostics,
            );
        }
    };

    let registry = match evaluation_registry(projection) {
        Ok(registry) => registry,
        Err(error) => {
            return blocked_agent_stage(
                evaluation_blocker(BlockerKind::AgentRuntime, error),
                command_diagnostics,
            );
        }
    };
    let policy = evaluation_policy(agent_dir, projection);
    let verification_requirements = match agent_verification_requirements(agent_dir, projection) {
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
        format!("eval_{}_{}", run_id.as_str(), projection.task_id.as_str()),
        agent_prompt(projection),
    )
    .with_max_turns(DEFAULT_AGENT_MAX_TURNS)
    .with_verification_requirements(verification_requirements);
    if let Some(instructions) = project_instructions {
        input = input.with_project_instructions(instructions.content);
    }
    let agent_started = Instant::now();
    let result = AgentLoop::new(provider, ToolBroker::new(registry), policy)
        .with_workspace_tools(
            WorkspaceTools::new(agent_dir)
                .with_shared_sandbox_backend(sandbox_backend)
                .with_command_environment(CommandEnvironmentPolicy::EvaluationIsolated),
        )
        .run(&input);
    let agent_duration_ms = u64::try_from(agent_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let run_status = result.to_run_status();
    let (observed_smoke_scope_digests, local_process_fallback_unknown_count) =
        agent_command_observation(&result);
    let trace_path = task_dir.join(AGENT_TRACE_FILE);
    let trace = json!({
        "status": run_status.status,
        "completed": run_status.completed,
        "run_id": run_id.as_str(),
        "thread_id": &input.thread_id,
        "turn_id": &input.turn_id,
        "task_id": projection.task_id.as_str(),
        "model_turns": run_status.model_turns,
        "model_turn_limit": run_status.model_turn_limit,
        "context": run_status.context_trace.as_ref().map(|context| json!({
            "included_item_ids": context.included_item_ids.iter().map(safe_text).collect::<Vec<_>>(),
            "excluded_item_ids": context.excluded_item_ids.iter().map(safe_text).collect::<Vec<_>>(),
            "budget": &context.budget,
            "compaction_count": context.compaction_count,
            "compacted_message_count": context.compacted_message_count,
            "last_compaction_before_tokens": context.last_compaction_before_tokens,
            "last_compaction_after_tokens": context.last_compaction_after_tokens,
        })),
        "tool_calls": run_status.tool_calls,
        "plan": run_status.plan,
        "plan_update_count": run_status.plan_update_count,
        "recovery_metrics": run_status.recovery_metrics,
        "model_usage": run_status.model_usage,
        "provider_attempts": run_status.provider_attempts,
        "provider_protocol": {
            "contract": run_status.provider_protocol_contract,
            "capability_metadata": run_status.provider_capability_metadata,
        },
        "tool_outcomes": result.tool_results.iter().map(|tool_result| json!({
            "tool_call_id": safe_text(&tool_result.tool_call_id),
            "tool_name": safe_text(&tool_result.tool_name),
            "ok": tool_result.ok,
            "error_code": tool_result.error_code.as_deref().map(safe_text),
            "truncated": tool_result.truncated,
            "result_id": safe_command_scope_digest(tool_result),
        })).collect::<Vec<_>>(),
        "approval_count": run_status.approval_count,
        "verification": run_status.verification,
        "audit_events": run_status.audit_events,
        "error": run_status.error.as_deref().map(safe_text),
        "provider_diagnostic": run_status.provider_diagnostic,
    });
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
            };
        }
    };

    let after = match snapshot_workspace(agent_dir) {
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
            };
        }
    };
    let disallowed_changed_files = changed_files
        .iter()
        .filter(|path| !path_is_allowed(path, &projection.allowed_paths))
        .cloned()
        .collect::<Vec<_>>();
    let smoke_command_satisfied = smoke_commands_satisfied(agent_dir, projection, &result);
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
    }
}

fn evaluation_registry(projection: &AgentTaskProjection) -> Result<ToolRegistry, String> {
    let allowed = projection
        .allowed_tools
        .iter()
        .map(|tool| tool.as_str())
        .collect::<BTreeSet<_>>();
    let mut registry = ToolRegistry::default();
    for mut spec in workspace_tool_specs()
        .into_iter()
        .chain(agent_control_tool_specs())
    {
        if !allowed.contains(spec.name.as_str()) {
            continue;
        }
        if spec.name == TOOL_COMMAND {
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
            spec.restrict_to_input_bindings(bindings)?;
        }
        registry.register(spec)?;
    }
    Ok(registry)
}

fn evaluation_policy(workspace: &Path, projection: &AgentTaskProjection) -> PolicyEngine {
    let mut profile = PermissionProfile::workspace_write(workspace.to_string_lossy().into_owned());
    profile.approval_policy = ApprovalPolicy::Never;
    if projection
        .smoke_commands
        .iter()
        .any(|command| command.network_access == NetworkAccess::Allowed)
    {
        profile.network_access = NetworkAccess::Allowed;
    }
    let allowed = projection
        .allowed_tools
        .iter()
        .map(|tool| tool.as_str())
        .collect::<BTreeSet<_>>();
    let mut policy = PolicyEngine::new(profile);
    if [TOOL_READ, TOOL_LIST, TOOL_GREP, UPDATE_PLAN_TOOL]
        .iter()
        .any(|tool| allowed.contains(tool))
    {
        policy = policy.with_rule(
            PermissionRule::new(
                "allow_evaluation_read_tools",
                SettingsScope::Project,
                PermissionDecisionOutcome::Allow,
            )
            .for_operation(PermissionOperation::Read),
        );
    }
    if allowed.contains(TOOL_EDIT) || allowed.contains(TOOL_PATCH) {
        for (index, path) in projection.allowed_paths.iter().enumerate() {
            policy = policy.with_rule(
                PermissionRule::new(
                    format!("allow_evaluation_write_{index}"),
                    SettingsScope::Project,
                    PermissionDecisionOutcome::Allow,
                )
                .for_operation(PermissionOperation::Write)
                .for_resource_prefix(path.as_str()),
            );
        }
    }
    if allowed.contains(TOOL_COMMAND) {
        for (index, command) in projection.smoke_commands.iter().enumerate() {
            let network = sandbox_network_mode(command.network_access);
            let resource = command_script_scope_resource_with_policy(
                &command_script_from_argv(command.argv.as_slice()),
                command.cwd.as_ref().map_or(".", |cwd| cwd.as_str()),
                command
                    .timeout_seconds
                    .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECONDS),
                SandboxFilesystemMode::WorkspaceWrite,
                network.clone(),
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

fn agent_prompt(projection: &AgentTaskProjection) -> String {
    let allowed_paths = projection
        .allowed_paths
        .iter()
        .map(|path| path.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let allowed_tools = projection
        .allowed_tools
        .iter()
        .map(|tool| tool.as_str())
        .collect::<Vec<_>>()
        .join(", ");
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
    let eligible_results = eligible_command_scope_digests(&result.tool_results);
    let mut matched_results = vec![false; eligible_results.len()];
    projection.smoke_commands.iter().all(|command| {
        let Ok(expected) = smoke_command_scope_digest(workspace, command) else {
            return false;
        };
        let Some(index) = eligible_results
            .iter()
            .enumerate()
            .position(|(index, tool_result)| !matched_results[index] && tool_result == &expected)
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
    let cwd = command.cwd.as_ref().map_or_else(
        || workspace.to_path_buf(),
        |cwd| workspace.join(cwd.as_str()),
    );
    fs::canonicalize(cwd)
        .ok()
        .map(|cwd| cwd.to_string_lossy().into_owned())
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
        kind,
        message: safe_text(message.into()),
    }
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
) -> Result<(), String> {
    if cfg!(windows) {
        preflight_evaluation_path_budget_with_limit(
            output_root,
            run_id,
            task_ids,
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
    max_path_chars: usize,
) -> Result<(), String> {
    let output_root = absolute_path_for_path_budget(output_root)?;
    let run_dir = output_root.join(run_id.as_str());
    for (context, path) in [
        ("evaluation result artifact", run_dir.join(RESULT_FILE)),
        ("evaluation report artifact", run_dir.join(REPORT_FILE)),
        ("evaluation evidence artifact", run_dir.join(EVIDENCE_FILE)),
        (
            "evaluation report temporary artifact",
            run_dir.join(format!(
                ".{REPORT_FILE}.4294967295.18446744073709551615.tmp"
            )),
        ),
        (
            "evaluation evidence temporary artifact",
            run_dir.join(format!(
                ".{EVIDENCE_FILE}.4294967295.18446744073709551615.tmp"
            )),
        ),
    ] {
        check_path_budget(context, &path, max_path_chars)?;
    }

    for task_id in task_ids {
        let task_dir = run_dir.join(task_id.as_str());
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
            ("agent trace artifact", task_dir.join(AGENT_TRACE_FILE)),
            (
                "patch evidence artifact",
                task_dir.join(PATCH_EVIDENCE_FILE),
            ),
        ] {
            check_path_budget(context, &path, max_path_chars)?;
        }

        for stage in [BASELINE_DIR, AGENT_DIR, PUBLIC_DIR, HIDDEN_DIR] {
            let stage_dir = task_dir.join(stage);
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
    result_path: &Path,
    result: &impl Serialize,
    report_path: &Path,
    report: &impl Serialize,
    evidence_path: &Path,
    evidence: &impl Serialize,
) -> Result<(), String> {
    let report_temp = write_json_temp(report_path, report)?;
    let evidence_temp = match write_json_temp(evidence_path, evidence) {
        Ok(temp) => temp,
        Err(error) => {
            let _ = fs::remove_file(&report_temp);
            return Err(error);
        }
    };
    let result_temp = match write_json_temp(result_path, result) {
        Ok(temp) => temp,
        Err(error) => {
            let _ = fs::remove_file(&report_temp);
            let _ = fs::remove_file(&evidence_temp);
            return Err(error);
        }
    };
    if let Err(error) = publish_json_temp(&evidence_temp, evidence_path) {
        let _ = fs::remove_file(&report_temp);
        let _ = fs::remove_file(&result_temp);
        return Err(error);
    }
    if let Err(error) = publish_json_temp(&report_temp, report_path) {
        let _ = fs::remove_file(&result_temp);
        return Err(cleanup_published_artifact(evidence_path, error));
    }
    if let Err(error) = publish_json_temp(&result_temp, result_path) {
        let error = cleanup_published_artifact(report_path, error);
        return Err(cleanup_published_artifact(evidence_path, error));
    }
    Ok(())
}

fn cleanup_published_artifact(path: &Path, error: String) -> String {
    match fs::remove_file(path) {
        Ok(()) => error,
        Err(cleanup_error) => format!(
            "{error}; failed to remove incomplete artifact {}: {cleanup_error}",
            path.display()
        ),
    }
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

fn blocker_code(blocker: &EvaluationBlocker) -> String {
    enum_string(blocker.kind).unwrap_or_else(|_| "environment".to_string())
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
    use singularity_evaluation::{Argv, GitCommit, RelativePath, RemoteRepository, ToolName};
    use singularity_tools::{
        CommandRequest, CommandResult, SandboxBackendEnforcement, SandboxCapabilities,
    };

    fn command(argv: &[&str]) -> CommandSpec {
        CommandSpec {
            argv: Argv::new(argv.iter().map(|value| (*value).to_string()).collect()).expect("argv"),
            cwd: None,
            timeout_seconds: Some(30),
            network_access: NetworkAccess::Denied,
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
            approval_requests: Vec::new(),
            pending_tool_calls: Vec::new(),
            tool_results,
            approval_checkpoints: Vec::new(),
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
            allowed_tools: vec![
                ToolName::new(TOOL_READ).expect("tool"),
                ToolName::new(TOOL_COMMAND).expect("tool"),
            ],
            smoke_commands: vec![command(&["cargo", "test"])],
        };

        let prompt = agent_prompt(&projection);
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
            allowed_tools: vec![ToolName::new(TOOL_COMMAND).expect("tool")],
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
            allowed_tools: vec![
                ToolName::new(TOOL_EDIT).expect("tool"),
                ToolName::new(TOOL_COMMAND).expect("tool"),
            ],
            smoke_commands: vec![smoke.clone()],
        };
        let mutation =
            singularity_tools::ToolResult::summary("call-edit", TOOL_EDIT, true, "changed");
        let smoke_result = successful_command_result("call-smoke", &smoke, workspace.path());

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
    fn smoke_observation_rejects_failed_result_even_when_digest_matches() {
        let workspace = tempfile::tempdir().expect("workspace");
        let smoke = command(&["cargo", "test"]);
        let mut failed = successful_command_result("call-failed", &smoke, workspace.path());
        failed.ok = false;
        let result = completed_agent_result(vec![failed]);

        let (observed, unknown_count) = agent_command_observation(&result);

        assert!(observed.is_empty());
        assert_eq!(unknown_count, 1);
    }

    #[test]
    fn smoke_observation_preserves_post_mutation_order_and_duplicates() {
        let workspace = tempfile::tempdir().expect("workspace");
        let smoke = command(&["cargo", "test"]);
        let mutation =
            singularity_tools::ToolResult::summary("call-edit", TOOL_EDIT, true, "changed");
        let first = successful_command_result("call-smoke-1", &smoke, workspace.path());
        let second = successful_command_result("call-smoke-2", &smoke, workspace.path());
        let expected = first.result_id.clone().expect("scope digest");

        let result = completed_agent_result(vec![mutation, first, second]);
        let (observed, _) = agent_command_observation(&result);

        assert_eq!(observed, vec![expected.clone(), expected]);
    }

    #[test]
    fn registry_exposes_only_manifest_tools() {
        let projection = AgentTaskProjection {
            task_id: TaskId::new("task-1").expect("task id"),
            description: "description".to_string(),
            instructions: "fix".to_string(),
            allowed_paths: vec![RelativePath::new("src/lib.rs").expect("path")],
            allowed_tools: vec![ToolName::new(TOOL_READ).expect("tool")],
            smoke_commands: Vec::new(),
        };
        let registry = evaluation_registry(&projection).expect("registry");
        assert!(registry.get(TOOL_READ).is_some());
        assert!(registry.get(UPDATE_PLAN_TOOL).is_none());
        assert!(registry.get(TOOL_COMMAND).is_none());
        assert!(registry.get(TOOL_EDIT).is_none());
    }

    #[test]
    fn registry_command_schema_exposes_only_allowed_smoke_inputs() {
        let smoke = command(&["cargo", "test"]);
        let projection = AgentTaskProjection {
            task_id: TaskId::new("task-1").expect("task id"),
            description: "description".to_string(),
            instructions: "fix".to_string(),
            allowed_paths: vec![RelativePath::new("src/lib.rs").expect("path")],
            allowed_tools: vec![ToolName::new(TOOL_COMMAND).expect("tool")],
            smoke_commands: vec![smoke.clone()],
        };

        let registry = evaluation_registry(&projection).expect("registry");
        let command = registry.get(TOOL_COMMAND).expect("command tool");
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
            .prepare_model_input(TOOL_COMMAND, &payload)
            .expect("declared command model input");
        assert_eq!(execution_input, payload);
        assert!(
            registry
                .validate_execution_input(TOOL_COMMAND, &execution_input)
                .is_ok()
        );
        let mut undeclared = payload;
        undeclared["command"] = json!("cargo check");
        assert_eq!(
            registry
                .prepare_model_input(TOOL_COMMAND, &undeclared)
                .expect_err("undeclared command must fail locally")
                .code,
            "input_not_allowed"
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

        assert!(error.contains("source and destination overlap"));
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
            WINDOWS_MAX_PATH_CHARS,
        )
        .expect_err("full-SHA evaluation paths must exceed the conservative budget");

        assert!(error.contains("evaluation path budget exceeded"));
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
            WINDOWS_MAX_PATH_CHARS,
        )
        .expect("short evaluation paths must fit the conservative budget");
    }

    #[cfg(windows)]
    #[test]
    fn run_evaluation_rejects_long_paths_before_creating_run_directory() {
        let temp = tempfile::tempdir().expect("temp");
        let manifest_path = temp.path().join("manifest.json");
        let run_id = "a".repeat(40);
        let task_id = "b".repeat(40);
        let manifest = json!({
            "schema_version": "evaluation.task_set/v4",
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
                    "allowed_tools": ["read"]
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

        let error = run_evaluation(
            &params,
            Arc::new(SourceSandboxBackend),
            &ProviderConfigSnapshot::capture(|_| None),
        )
        .expect_err("long evaluation paths must fail before execution");

        assert!(error.contains("evaluation path budget exceeded"));
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
            allowed_tools: vec![ToolName::new(TOOL_EDIT).expect("tool")],
            smoke_commands: Vec::new(),
        };
        let policy = evaluation_policy(Path::new("C:/workspace"), &projection);
        let allowed = policy.evaluate(&singularity_policy::PermissionRequest::new(
            TOOL_EDIT,
            PermissionOperation::Write,
            "src/lib.rs",
        ));
        let denied = policy.evaluate(&singularity_policy::PermissionRequest::new(
            TOOL_EDIT,
            PermissionOperation::Write,
            "src2/lib.rs",
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
            SandboxCapabilities::strict()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
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
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
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
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
    }

    #[test]
    fn baseline_expected_failure_with_path_marker_passes_semantically() {
        let temp = tempfile::tempdir().expect("temp");
        let execution = run_verification_after_setup(
            temp.path(),
            None,
            &[command(&["cargo", "test"])],
            CommandExpectation::Failure,
            Arc::new(PathBudgetSandboxBackend),
            Vec::new(),
        );

        assert_eq!(execution.result.status, StageStatus::Passed);
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
    fn result_commit_marker_requires_complete_report_and_evidence_artifacts() {
        let temp = tempfile::tempdir().expect("temp");
        let result_path = temp.path().join("result.json");
        let report_path = temp.path().join("report.json");
        let evidence_path = temp.path().join("evidence.json");
        fs::create_dir(&result_path).expect("blocking result directory");

        let error = publish_evaluation_artifacts(
            &result_path,
            &json!({"status": "completed"}),
            &report_path,
            &json!({"runner": RUNNER_NAME}),
            &evidence_path,
            &json!({"schema_version": "evaluation.evidence/v1"}),
        )
        .expect_err("publish must fail");

        assert!(error.contains("failed to publish artifact"));
        assert!(!report_path.exists());
        assert!(!evidence_path.exists());
        assert!(result_path.is_dir());
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
    fn failed_artifact_write_removes_its_temp_file() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("result.json");

        let error = write_json_atomic(&path, &SerializationFailure).expect_err("write failure");

        assert!(error.contains("intentional serialization failure"));
        assert!(!path.exists());
        assert_eq!(fs::read_dir(temp.path()).expect("directory").count(), 0);
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
