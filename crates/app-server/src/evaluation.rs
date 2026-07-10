use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde_json::{Value, json};
use singularity_agent::{
    AgentLoop, AgentLoopCapability, AgentLoopInput, AgentLoopResult, AgentStatus,
};
use singularity_core::{contains_sensitive_text, load_project_instructions_from_cwd};
use singularity_evaluation::{
    AgentStagePlan, AgentTaskProjection, BlockerKind, CommandExpectation, CommandSpec,
    EvaluationBlocker, EvaluationManifest, EvaluationResult, EvaluationResultSchemaVersion,
    EvaluationStageResults, EvaluationStatus, EvaluationTaskResult, PatchFormat,
    PlannedWorkspaceSource, RunId, StageResult, StageStatus, TaskId, VerificationStagePlan,
    WorkspacePlan,
};
use singularity_model::{
    ModelErrorCategory, OpenAiProvider, ProviderConfigSnapshot, ProviderError,
};
use singularity_policy::{
    ApprovalPolicy, NetworkAccess, PermissionDecisionOutcome, PermissionOperation,
    PermissionProfile, PermissionRule, PolicyEngine, SettingsScope,
};
use singularity_protocol::{EvalRunParams, EvalRunResult};
use singularity_tools::{
    SandboxBackend, SandboxFilesystemMode, SandboxNetworkMode, ToolBroker, ToolRegistry,
    WorkspaceTools, command_scope_digest, command_scope_resource,
};

use super::{
    TOOL_COMMAND, TOOL_EDIT, TOOL_GREP, TOOL_LIST, TOOL_PATCH, TOOL_READ,
    native_workspace_tool_specs,
};

mod command;
mod workspace;

use command::{
    CommandDiagnostic, command_blocker, command_succeeded, infrastructure_blocker,
    run_command_spec, run_raw_command, sandbox_network_mode,
};
use workspace::{
    apply_agent_changes, changed_paths, copy_tree_checked, path_is_allowed, snapshot_workspace,
    validate_tree,
};

const RUNNER_NAME: &str = "rust_native";
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
const AGENT_TRACE_FILE: &str = "agent-trace.json";
const ARTIFACT_TEMP_FILE_ATTEMPTS: usize = 64;

static ARTIFACT_COUNTER: AtomicU64 = AtomicU64::new(0);

type SharedSandboxBackend = Arc<dyn SandboxBackend + Send + Sync>;

#[derive(Debug, Clone, Default, Serialize)]
struct StageDiagnostics {
    message: Option<String>,
    commands: Vec<CommandDiagnostic>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct TaskDiagnostics {
    source_commands: Vec<CommandDiagnostic>,
    baseline: StageDiagnostics,
    agent: StageDiagnostics,
    public: StageDiagnostics,
    hidden: StageDiagnostics,
    changed_files: Vec<String>,
    disallowed_changed_files: Vec<String>,
    smoke_command_satisfied: bool,
    model_turns: u32,
    tool_calls: u32,
    approval_count: u32,
    local_process_fallback_count: usize,
    trace_path: Option<String>,
    error: Option<String>,
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
    disallowed_changed_files: Vec<String>,
    smoke_command_satisfied: bool,
    model_turns: u32,
    tool_calls: u32,
    approval_count: u32,
    audit_events: Vec<Value>,
    trace_path: Option<String>,
    error: Option<String>,
}

struct TaskExecution {
    result: EvaluationTaskResult,
    report: Value,
}

pub(crate) fn run_evaluation(
    params: &EvalRunParams,
    sandbox_backend: SharedSandboxBackend,
    provider_snapshot: &ProviderConfigSnapshot,
) -> Result<EvalRunResult, String> {
    let manifest = EvaluationManifest::load(&params.manifest)
        .map_err(|error| format!("invalid eval manifest: {error}"))?;
    let run_id = RunId::new(params.run_id.clone())
        .map_err(|error| format!("invalid eval run id: {error}"))?;
    let output_root = evaluation_output_root(params.output_root.as_deref());
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
    for task in &manifest.task_set().tasks {
        let plan = manifest
            .workspace_plan(&task.task_id)
            .map_err(|error| error.to_string())?;
        task_executions.push(run_task(
            &run_id,
            &run_dir,
            &plan,
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
        schema_version: EvaluationResultSchemaVersion::V2,
        run_id: run_id.clone(),
        status,
        blocker,
        evaluation_passed,
        tasks,
    };
    result
        .validate()
        .map_err(|error| format!("invalid evaluation result: {error}"))?;

    let result_path = run_dir.join(RESULT_FILE);
    let report_path = run_dir.join(REPORT_FILE);
    let task_reports = task_executions
        .into_iter()
        .map(|execution| execution.report)
        .collect::<Vec<_>>();
    let report = json!({
        "manifest": params.manifest,
        "runner": RUNNER_NAME,
        "result": result,
        "tasks": task_reports,
        "result_path": result_path.to_string_lossy(),
        "report_path": report_path.to_string_lossy(),
    });
    if let Err(error) = publish_evaluation_artifacts(&result_path, &result, &report_path, &report) {
        return match fs::remove_dir_all(&run_dir) {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(format!(
                "{error}; failed to clean incomplete evaluation run {}: {cleanup_error}",
                run_dir.display()
            )),
        };
    }

    Ok(EvalRunResult {
        run_id: run_id.as_str().to_string(),
        manifest: params.manifest.clone(),
        runner: RUNNER_NAME.to_string(),
        status: enum_string(result.status)?,
        blocker: result.blocker.as_ref().map(blocker_code),
        tasks: report["tasks"].as_array().cloned().unwrap_or_default(),
        result_path: Some(result_path.to_string_lossy().into_owned()),
        report_path: Some(report_path.to_string_lossy().into_owned()),
        evaluation_passed: result.evaluation_passed,
    })
}
fn run_task(
    run_id: &RunId,
    run_dir: &Path,
    plan: &WorkspacePlan,
    sandbox_backend: SharedSandboxBackend,
    provider_snapshot: &ProviderConfigSnapshot,
) -> TaskExecution {
    let task_dir = run_dir.join(plan.task_id.as_str());
    let mut diagnostics = TaskDiagnostics {
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

    let capability = AgentLoopCapability::current();
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

    let source_dir = task_dir.join(SOURCE_DIR);
    match prepare_source(
        &plan.source,
        &task_dir,
        &source_dir,
        Arc::clone(&sandbox_backend),
    ) {
        Ok(commands) => diagnostics.source_commands = commands,
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
    diagnostics.disallowed_changed_files = agent_execution.disallowed_changed_files.clone();
    diagnostics.smoke_command_satisfied = agent_execution.smoke_command_satisfied;
    diagnostics.model_turns = agent_execution.model_turns;
    diagnostics.tool_calls = agent_execution.tool_calls;
    diagnostics.approval_count = agent_execution.approval_count;
    diagnostics.trace_path = agent_execution.trace_path.clone();
    diagnostics.error = agent_execution.error.clone();
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
        && diagnostics.local_process_fallback_count == 0;
    let status = if blocker.is_some() {
        EvaluationStatus::Blocked
    } else if evaluation_passed {
        EvaluationStatus::Completed
    } else {
        EvaluationStatus::Failed
    };
    let result = EvaluationTaskResult {
        task_id: task_id.clone(),
        status,
        blocker,
        stages,
        agent_completed,
        tests_passed,
        evaluation_passed,
    };
    let mut report = serde_json::to_value(&result).expect("evaluation task result serializes");
    if let Some(object) = report.as_object_mut() {
        object.insert(
            "diagnostics".to_string(),
            serde_json::to_value(diagnostics).expect("evaluation diagnostics serialize"),
        );
    }
    TaskExecution { result, report }
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
                source_dir,
                vec![
                    "git".to_string(),
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
        diagnostics.push(CommandDiagnostic::new(
            format!("verification.command.{index}"),
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
        diagnostics.push(CommandDiagnostic::new(
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
        sandbox_backend,
    );
    diagnostics.push(CommandDiagnostic::new("evaluator.apply_patch", &result));
    let remove_result = fs::remove_file(&patch_path);
    if !command_succeeded(&result) {
        return Err(command_blocker(
            &result,
            BlockerKind::WorkspacePreparation,
            "failed to apply evaluator patch",
        ));
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
    let mut input = AgentLoopInput::new(
        projection.task_id.as_str(),
        format!("eval_{}_{}", run_id.as_str(), projection.task_id.as_str()),
        agent_prompt(projection),
    )
    .with_max_turns(DEFAULT_AGENT_MAX_TURNS);
    if let Some(instructions) = project_instructions {
        input = input.with_project_instructions(instructions.content);
    }
    let result = AgentLoop::new(provider, ToolBroker::new(registry), policy)
        .with_workspace_tools(
            WorkspaceTools::new(agent_dir).with_shared_sandbox_backend(sandbox_backend),
        )
        .run(&input);
    let run_status = result.to_run_status(&input);
    let trace_path = task_dir.join(AGENT_TRACE_FILE);
    let trace = json!({
        "status": run_status.status,
        "completed": run_status.completed,
        "run_id": run_status.run_id,
        "session_id": run_status.session_id,
        "task_id": run_status.task_id,
        "model_turns": run_status.model_turns,
        "tool_calls": run_status.tool_calls,
        "approval_count": run_status.approval_count,
        "audit_events": run_status.audit_events,
        "error": run_status.error.as_deref().map(safe_text),
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
                disallowed_changed_files: Vec::new(),
                smoke_command_satisfied: false,
                model_turns: result.model_turns,
                tool_calls: result.tool_calls,
                approval_count: result.approval_count,
                audit_events: run_status.audit_events,
                trace_path: None,
                error: Some(safe_text(error)),
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
                disallowed_changed_files: Vec::new(),
                smoke_command_satisfied: false,
                model_turns: result.model_turns,
                tool_calls: result.tool_calls,
                approval_count: result.approval_count,
                audit_events: run_status.audit_events,
                trace_path: trace_path_string,
                error: Some(safe_text(error)),
            };
        }
    };
    let changed_files = changed_paths(&before, &after);
    let disallowed_changed_files = changed_files
        .iter()
        .filter(|path| !path_is_allowed(path, &projection.allowed_paths))
        .cloned()
        .collect::<Vec<_>>();
    let smoke_command_satisfied = smoke_commands_satisfied(projection, &result);
    let loop_completed = result.completed && result.status == AgentStatus::Completed;
    let error = result.error.clone().map(safe_text);
    let sandbox_blocker = agent_sandbox_blocker(&run_status.audit_events);
    let stage = if let Some(blocker) = sandbox_blocker {
        StageExecution::blocked(blocker, command_diagnostics)
    } else if result.status == AgentStatus::Blocked {
        StageExecution::blocked(
            evaluation_blocker(
                agent_blocker_kind(result.error.as_deref()),
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
        disallowed_changed_files,
        smoke_command_satisfied,
        model_turns: result.model_turns,
        tool_calls: result.tool_calls,
        approval_count: result.approval_count,
        audit_events: run_status.audit_events,
        trace_path: trace_path_string,
        error,
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
        disallowed_changed_files: Vec::new(),
        smoke_command_satisfied: false,
        model_turns: 0,
        tool_calls: 0,
        approval_count: 0,
        audit_events: Vec::new(),
        trace_path: None,
        error: Some(blocker.message),
    }
}

fn evaluation_registry(projection: &AgentTaskProjection) -> Result<ToolRegistry, String> {
    let allowed = projection
        .allowed_tools
        .iter()
        .map(|tool| tool.as_str())
        .collect::<BTreeSet<_>>();
    let allows_network = projection
        .smoke_commands
        .iter()
        .any(|command| command.network_access == NetworkAccess::Allowed);
    let mut registry = ToolRegistry::default();
    for mut spec in native_workspace_tool_specs() {
        if !allowed.contains(spec.name.as_str()) {
            continue;
        }
        if spec.name == TOOL_COMMAND {
            let network_values = if allows_network {
                json!(["denied", "allowed"])
            } else {
                json!(["denied"])
            };
            spec.input_schema = json!({
                "type": "object",
                "properties": {
                    "argv": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1
                    },
                    "cwd": {"type": "string"},
                    "timeout_seconds": {"type": "integer", "minimum": 1},
                    "sandbox_mode": {"type": "string", "enum": ["workspace_write"]},
                    "network_access": {"type": "string", "enum": network_values}
                },
                "required": [
                    "argv", "cwd", "timeout_seconds", "sandbox_mode", "network_access"
                ],
                "additionalProperties": false
            });
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
    if [TOOL_READ, TOOL_LIST, TOOL_GREP]
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
            let resource = command_scope_resource(
                command.argv.as_slice(),
                command.cwd.as_ref().map_or(".", |cwd| cwd.as_str()),
                command
                    .timeout_seconds
                    .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECONDS),
                &SandboxFilesystemMode::WorkspaceWrite,
                &network,
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
            smoke_command_payload(command)
        ));
    }
    sections.push(
        "Finish with a concise answer describing the change and the verification actually run."
            .to_string(),
    );
    sections.join("\n\n")
}

fn smoke_command_payload(command: &CommandSpec) -> Value {
    json!({
        "argv": command.argv.as_slice(),
        "cwd": command.cwd.as_ref().map(|cwd| cwd.as_str()).unwrap_or("."),
        "timeout_seconds": command.timeout_seconds.unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECONDS),
        "sandbox_mode": "workspace_write",
        "network_access": match command.network_access {
            NetworkAccess::Denied => "denied",
            NetworkAccess::Allowed => "allowed",
        },
    })
}

fn smoke_commands_satisfied(projection: &AgentTaskProjection, result: &AgentLoopResult) -> bool {
    let first_eligible_result = result
        .tool_results
        .iter()
        .rposition(|tool_result| matches!(tool_result.tool_name.as_str(), TOOL_EDIT | TOOL_PATCH))
        .map_or(0, |index| index + 1);
    let eligible_results = &result.tool_results[first_eligible_result..];
    let mut matched_results = vec![false; eligible_results.len()];
    projection.smoke_commands.iter().all(|command| {
        let network = sandbox_network_mode(command.network_access);
        let expected = command_scope_digest(
            command.argv.as_slice(),
            command.cwd.as_ref().map_or(".", |cwd| cwd.as_str()),
            command
                .timeout_seconds
                .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECONDS),
            &SandboxFilesystemMode::WorkspaceWrite,
            &network,
        );
        let Some(index) = eligible_results
            .iter()
            .enumerate()
            .position(|(index, tool_result)| {
                !matched_results[index]
                    && tool_result.tool_name == TOOL_COMMAND
                    && tool_result.ok
                    && tool_result.result_id.as_deref() == Some(expected.as_str())
            })
        else {
            return false;
        };
        matched_results[index] = true;
        true
    })
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
    let kind = match error.error.category() {
        ModelErrorCategory::Authentication => BlockerKind::ProviderAuthentication,
        ModelErrorCategory::Network | ModelErrorCategory::ProviderUnavailable => {
            BlockerKind::Network
        }
        ModelErrorCategory::SandboxPermission => BlockerKind::Sandbox,
        _ => BlockerKind::ProviderConfiguration,
    };
    evaluation_blocker(kind, error.message.clone())
}

fn agent_blocker_kind(error: Option<&str>) -> BlockerKind {
    let error = error.unwrap_or_default().to_ascii_lowercase();
    if error.contains("auth") || error.contains("api key") {
        BlockerKind::ProviderAuthentication
    } else if error.contains("network") || error.contains("base_url") || error.contains("base url")
    {
        BlockerKind::Network
    } else if error.contains("sandbox") || error.contains("permission") {
        BlockerKind::Sandbox
    } else if error.contains("provider") || error.contains("model") || error.contains("config") {
        BlockerKind::ProviderConfiguration
    } else {
        BlockerKind::AgentRuntime
    }
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

fn publish_evaluation_artifacts(
    result_path: &Path,
    result: &impl Serialize,
    report_path: &Path,
    report: &impl Serialize,
) -> Result<(), String> {
    let report_temp = write_json_temp(report_path, report)?;
    let result_temp = match write_json_temp(result_path, result) {
        Ok(temp) => temp,
        Err(error) => {
            let _ = fs::remove_file(&report_temp);
            return Err(error);
        }
    };
    if let Err(error) = publish_json_temp(&report_temp, report_path) {
        let _ = fs::remove_file(&result_temp);
        return Err(error);
    }
    if let Err(error) = publish_json_temp(&result_temp, result_path) {
        return match fs::remove_file(report_path) {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(format!(
                "{error}; failed to remove incomplete report {}: {cleanup_error}",
                report_path.display()
            )),
        };
    }
    Ok(())
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

    fn successful_command_result(
        tool_call_id: &str,
        command: &CommandSpec,
    ) -> singularity_tools::ToolResult {
        let mut result = singularity_tools::ToolResult::summary(
            tool_call_id,
            TOOL_COMMAND,
            true,
            "ok",
            "digest",
        );
        result.result_id = Some(command_scope_digest(
            command.argv.as_slice(),
            command.cwd.as_ref().map_or(".", |cwd| cwd.as_str()),
            command
                .timeout_seconds
                .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECONDS),
            &SandboxFilesystemMode::WorkspaceWrite,
            &sandbox_network_mode(command.network_access),
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
            tool_repairs: Vec::new(),
            verification: singularity_agent::AgentVerification::default(),
            error: None,
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
        assert!(prompt.contains("\"sandbox_mode\":\"workspace_write\""));
        assert!(prompt.contains("\"network_access\":\"denied\""));
        assert!(!prompt.contains("evaluator"));
        assert!(!prompt.contains("test_patch"));
    }

    #[test]
    fn duplicate_smoke_commands_require_distinct_successful_tool_results() {
        let smoke = command(&["cargo", "test"]);
        let projection = AgentTaskProjection {
            task_id: TaskId::new("task-1").expect("task id"),
            description: "description".to_string(),
            instructions: "fix".to_string(),
            allowed_paths: vec![RelativePath::new("src/lib.rs").expect("path")],
            allowed_tools: vec![ToolName::new(TOOL_COMMAND).expect("tool")],
            smoke_commands: vec![smoke.clone(), smoke.clone()],
        };
        let tool_result = successful_command_result("call-1", &smoke);
        let result = completed_agent_result(vec![tool_result.clone()]);
        assert!(!smoke_commands_satisfied(&projection, &result));

        let result = completed_agent_result(vec![tool_result.clone(), tool_result]);
        assert!(smoke_commands_satisfied(&projection, &result));
    }

    #[test]
    fn smoke_commands_must_run_after_the_last_workspace_mutation() {
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
        let mutation = singularity_tools::ToolResult::summary(
            "call-edit",
            TOOL_EDIT,
            true,
            "changed",
            "digest",
        );
        let smoke_result = successful_command_result("call-smoke", &smoke);

        let stale = completed_agent_result(vec![smoke_result.clone(), mutation.clone()]);
        assert!(!smoke_commands_satisfied(&projection, &stale));

        let current = completed_agent_result(vec![mutation, smoke_result]);
        assert!(smoke_commands_satisfied(&projection, &current));
    }

    #[test]
    fn registry_exposes_only_manifest_allowed_tools() {
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
        assert!(registry.get(TOOL_COMMAND).is_none());
        assert!(registry.get(TOOL_EDIT).is_none());
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
    fn workspace_snapshot_detects_add_modify_and_delete() {
        let temp = tempfile::tempdir().expect("temp");
        fs::write(temp.path().join("a.txt"), "before").expect("write a");
        fs::write(temp.path().join("b.txt"), "delete").expect("write b");
        let before = snapshot_workspace(temp.path()).expect("before");
        fs::write(temp.path().join("a.txt"), "after").expect("modify a");
        fs::remove_file(temp.path().join("b.txt")).expect("delete b");
        fs::write(temp.path().join("c.txt"), "add").expect("add c");
        let after = snapshot_workspace(temp.path()).expect("after");
        assert_eq!(changed_paths(&before, &after), ["a.txt", "b.txt", "c.txt"]);
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
            }
            CommandResult::completed(&request.command_id, "ok")
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
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
    fn result_commit_marker_is_not_published_without_a_complete_report_pair() {
        let temp = tempfile::tempdir().expect("temp");
        let result_path = temp.path().join("result.json");
        let report_path = temp.path().join("report.json");
        fs::create_dir(&result_path).expect("blocking result directory");

        let error = publish_evaluation_artifacts(
            &result_path,
            &json!({"status": "completed"}),
            &report_path,
            &json!({"runner": RUNNER_NAME}),
        )
        .expect_err("publish must fail");

        assert!(error.contains("failed to publish artifact"));
        assert!(!report_path.exists());
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

        assert_eq!(
            execution.report["diagnostics"]["local_process_fallback_count"],
            1
        );
        assert_eq!(execution.result.status, EvaluationStatus::Failed);
        assert!(!execution.result.evaluation_passed);
    }
}
