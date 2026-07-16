//! Evaluation 结果的脱敏 command 观察、artifact 摘要和 evidence 构造。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};
use singularity_agent::AgentLoopResult;
use singularity_evaluation::{
    CommandSpec, EvaluationEvidence, EvaluationEvidenceSchemaVersion, EvaluationScopeEvidence,
    EvaluationTaskEvidence, EvidenceVerdict, PlannedWorkspaceSource, RunId, WorkspacePlan,
    task_selection_digest,
};
use singularity_tools::ToolResult;

use super::command::command_scope_digest_for_spec;
use super::{
    AGENT_DIR, BASELINE_DIR, DEFAULT_COMMAND_TIMEOUT_SECONDS, HIDDEN_DIR, PUBLIC_DIR,
    StageDiagnostics, TOOL_COMMAND, TaskExecution,
};

/// 从任务计划和执行诊断构造脱敏 evidence。
pub(super) fn build_evaluation_evidence(
    run_id: &RunId,
    manifest_digest: String,
    plans: &[WorkspacePlan],
    executions: &[TaskExecution],
    run_dir: &Path,
) -> Result<EvaluationEvidence, String> {
    if plans.len() != executions.len() {
        return Err("evaluation evidence task plan/result count mismatch".to_string());
    }
    let tasks = plans
        .iter()
        .zip(executions)
        .map(|(plan, execution)| build_task_evidence(plan, execution, run_dir))
        .collect::<Result<Vec<_>, _>>()?;
    let task_ids = plans
        .iter()
        .map(|plan| plan.task_id.clone())
        .collect::<Vec<_>>();
    let evidence = EvaluationEvidence {
        schema_version: EvaluationEvidenceSchemaVersion::V1,
        run_id: run_id.clone(),
        manifest_digest,
        task_selection_digest: task_selection_digest(&task_ids),
        denominator_task_count: u32::try_from(tasks.len()).unwrap_or(u32::MAX),
        tasks,
    };
    evidence
        .validate()
        .map_err(|error| format!("invalid evaluation evidence: {error}"))?;
    Ok(evidence)
}

/// 计算产物内容摘要。
pub(super) fn content_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// 从 Agent 结果提取 command scope 与未知审计计数。
pub(super) fn agent_command_observation(result: &AgentLoopResult) -> (Vec<String>, usize) {
    let mut observed = Vec::new();
    let mut unknown_count = 0usize;
    for tool_result in result
        .tool_results
        .iter()
        .filter(|tool_result| tool_result.tool_name == TOOL_COMMAND)
    {
        let Some(scope_digest) = safe_command_scope_digest(tool_result) else {
            if tool_result.result_id.is_some() {
                unknown_count = unknown_count.saturating_add(1);
            }
            continue;
        };
        observed.push(scope_digest.to_string());
        let observation_complete = tool_result.audit_metadata().is_some_and(|audit| {
            audit
                .get("local_process_fallback")
                .and_then(Value::as_bool)
                .is_some()
                && audit
                    .get("sandbox_enforcement")
                    .and_then(Value::as_str)
                    .is_some()
        });
        if !observation_complete {
            unknown_count = unknown_count.saturating_add(1);
        }
    }
    (observed, unknown_count)
}

/// 只接受格式正确的 command scope digest。
pub(super) fn safe_command_scope_digest(tool_result: &ToolResult) -> Option<&str> {
    (tool_result.tool_name == TOOL_COMMAND)
        .then_some(tool_result.result_id.as_deref())
        .flatten()
        .filter(|digest| is_sha256_digest(digest))
}

fn build_task_evidence(
    plan: &WorkspacePlan,
    execution: &TaskExecution,
    run_dir: &Path,
) -> Result<EvaluationTaskEvidence, String> {
    let diagnostics = &execution.diagnostics;
    let task_dir = run_dir.join(plan.task_id.as_str());
    let allowed_paths = plan
        .agent
        .projection
        .allowed_paths
        .iter()
        .map(|path| path.as_str().to_string())
        .collect::<Vec<_>>();
    let changed_paths_digest = diagnostics
        .patch_evidence_path
        .as_ref()
        .map(|_| set_strings_digest("evaluation.changed_paths/v1", &diagnostics.changed_files));
    let allowlist = if diagnostics.patch_evidence_path.is_none() {
        EvidenceVerdict::Unknown
    } else if diagnostics.disallowed_changed_files.is_empty() {
        EvidenceVerdict::Passed
    } else {
        EvidenceVerdict::Failed
    };
    let trace_digest = diagnostics
        .trace_path
        .as_deref()
        .map(|path| artifact_digest(Path::new(path)))
        .transpose()?;
    let source_commit = match &plan.source {
        PlannedWorkspaceSource::RemoteGit { commit, .. } => Some(commit.clone()),
        PlannedWorkspaceSource::Local { .. } => None,
    };
    Ok(EvaluationTaskEvidence {
        task_id: plan.task_id.clone(),
        source_tree_digest: diagnostics
            .source
            .as_ref()
            .and_then(|source| source.tree_digest.clone()),
        source_commit,
        allowed_paths_digest: set_strings_digest("evaluation.allowed_paths/v1", &allowed_paths),
        changed_paths_digest,
        allowlist,
        smoke: smoke_scope_evidence(
            &task_dir.join(AGENT_DIR),
            &plan.agent.projection.smoke_commands,
            &diagnostics.observed_smoke_scope_digests,
        ),
        baseline: scope_evidence(
            &task_dir.join(BASELINE_DIR),
            &plan.baseline.commands,
            &observed_verification_scopes(&diagnostics.baseline),
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
        ),
        public: scope_evidence(
            &task_dir.join(PUBLIC_DIR),
            &plan.public.commands,
            &observed_verification_scopes(&diagnostics.public),
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
        ),
        hidden: scope_evidence(
            &task_dir.join(HIDDEN_DIR),
            &plan.hidden.commands,
            &observed_verification_scopes(&diagnostics.hidden),
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
        ),
        trace_digest,
        patch_digest: diagnostics.patch_digest.clone(),
        local_process_fallback_count: u32::try_from(diagnostics.local_process_fallback_count)
            .unwrap_or(u32::MAX),
        local_process_fallback_unknown_count: u32::try_from(
            diagnostics.local_process_fallback_unknown_count,
        )
        .unwrap_or(u32::MAX),
    })
}

fn observed_verification_scopes(diagnostics: &StageDiagnostics) -> Vec<String> {
    diagnostics
        .commands
        .iter()
        .filter(|command| command.phase.starts_with("verification.command."))
        .filter_map(|command| command.scope_digest.clone())
        .collect()
}

fn scope_evidence(
    workspace: &Path,
    commands: &[CommandSpec],
    observed_scope_digests: &[String],
    default_timeout_seconds: u64,
) -> EvaluationScopeEvidence {
    scope_evidence_with(commands, observed_scope_digests, |command| {
        command_scope_digest_for_spec(workspace, command, default_timeout_seconds)
    })
}

// smoke 收据必须复用 Agent completion 使用的 command string scope，避免产生第二事实源。
fn smoke_scope_evidence(
    workspace: &Path,
    commands: &[CommandSpec],
    observed_scope_digests: &[String],
) -> EvaluationScopeEvidence {
    scope_evidence_with(commands, observed_scope_digests, |command| {
        super::smoke_command_scope_digest(workspace, command)
    })
}

fn scope_evidence_with(
    commands: &[CommandSpec],
    observed_scope_digests: &[String],
    expected_scope_digest: impl Fn(&CommandSpec) -> Result<String, String>,
) -> EvaluationScopeEvidence {
    if commands.is_empty() {
        return EvaluationScopeEvidence {
            expectation_known: true,
            expected_scope_digests: Vec::new(),
            observed_scope_digests: observed_scope_digests.to_vec(),
            required_scopes_satisfied: EvidenceVerdict::Passed,
        };
    }
    let expected_scope_digests = commands
        .iter()
        .map(expected_scope_digest)
        .collect::<Result<Vec<_>, _>>();
    let Ok(expected_scope_digests) = expected_scope_digests else {
        return EvaluationScopeEvidence {
            expectation_known: false,
            expected_scope_digests: Vec::new(),
            observed_scope_digests: observed_scope_digests.to_vec(),
            required_scopes_satisfied: EvidenceVerdict::Unknown,
        };
    };
    let required_scopes_satisfied =
        if multiset_contains(observed_scope_digests, &expected_scope_digests) {
            EvidenceVerdict::Passed
        } else {
            EvidenceVerdict::Failed
        };
    EvaluationScopeEvidence {
        expectation_known: true,
        expected_scope_digests,
        observed_scope_digests: observed_scope_digests.to_vec(),
        required_scopes_satisfied,
    }
}

fn multiset_contains(observed: &[String], required: &[String]) -> bool {
    let mut counts = BTreeMap::new();
    for digest in observed {
        *counts.entry(digest.as_str()).or_insert(0usize) += 1;
    }
    required.iter().all(|digest| {
        let Some(count) = counts.get_mut(digest.as_str()) else {
            return false;
        };
        if *count == 0 {
            return false;
        }
        *count -= 1;
        true
    })
}

fn artifact_digest(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("failed to hash artifact {}: {error}", path.display()))?;
    Ok(content_digest(&bytes))
}

fn set_strings_digest(domain: &str, values: &[String]) -> String {
    let values = values.iter().cloned().collect::<BTreeSet<_>>();
    let mut digest = Sha256::new();
    update_digest_value(&mut digest, domain);
    for value in values {
        update_digest_value(&mut digest, &value);
    }
    format!("sha256:{:x}", digest.finalize())
}

fn update_digest_value(digest: &mut Sha256, value: &str) {
    let length = u64::try_from(value.len()).unwrap_or(u64::MAX);
    digest.update(length.to_le_bytes());
    digest.update(value.as_bytes());
}

fn is_sha256_digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use singularity_evaluation::{Argv, CommandSpec, EvidenceVerdict};
    use singularity_policy::NetworkAccess;
    use singularity_tools::{SandboxFilesystemMode, command_script_scope_digest_with_policy};

    use super::smoke_scope_evidence;
    use crate::evaluation::command::sandbox_network_mode;
    use crate::evaluation::{
        DEFAULT_COMMAND_TIMEOUT_SECONDS, command_script_from_argv, resolved_smoke_cwd,
    };

    #[test]
    fn smoke_scope_evidence_uses_the_agent_command_script_contract() {
        let workspace = tempfile::tempdir().expect("workspace");
        let command = CommandSpec {
            argv: Argv::new(vec!["python".to_string(), "smoke_test.py".to_string()]).expect("argv"),
            cwd: None,
            timeout_seconds: None,
            network_access: NetworkAccess::Denied,
        };
        let cwd = resolved_smoke_cwd(workspace.path(), &command).expect("smoke cwd");
        let observed = command_script_scope_digest_with_policy(
            &command_script_from_argv(command.argv.as_slice()),
            &cwd,
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
            SandboxFilesystemMode::WorkspaceWrite,
            sandbox_network_mode(command.network_access),
        );

        let evidence = smoke_scope_evidence(workspace.path(), &[command], &[observed]);

        assert_eq!(evidence.required_scopes_satisfied, EvidenceVerdict::Passed);
    }
}
