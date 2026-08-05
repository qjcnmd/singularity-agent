//! Evaluation 结果的脱敏 command 观察、artifact 摘要和 evidence 构造。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::{
    CommandSpec, EvaluationEvidence, EvaluationEvidenceSchemaVersion, EvaluationResult,
    EvaluationSandboxPreflight, EvaluationScopeEvidence, EvaluationTaskEvidence,
    EvaluationTrialEvidence, EvidenceVerdict, PlannedWorkspaceSource, RunId, WorkspacePlan,
    task_selection_digest,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use singularity_agent::AgentLoopResult;

use super::command::{
    AgentCommandDiagnosticProjection, CommandDiagnostic, command_scope_digest_for_spec,
};
use super::{
    AGENT_DIR, DEFAULT_COMMAND_TIMEOUT_SECONDS, StageDiagnostics, TOOL_COMMAND, TaskEvaluation,
    TaskExecution,
};

/// 从任务计划和执行诊断构造脱敏 evidence。
pub(super) fn build_evaluation_evidence(
    run_id: &RunId,
    manifest_digest: String,
    plans: &[WorkspacePlan],
    executions: &[TaskEvaluation],
    run_dir: &Path,
    preflight: EvaluationSandboxPreflight,
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
    let task_count = u32::try_from(tasks.len()).unwrap_or(u32::MAX);
    let evidence = EvaluationEvidence {
        schema_version: EvaluationEvidenceSchemaVersion::V4,
        run_id: run_id.clone(),
        manifest_digest,
        task_selection_digest: task_selection_digest(&task_ids),
        denominator_task_count: u32::try_from(tasks.len()).unwrap_or(u32::MAX),
        trials_per_task: executions.first().map_or(0, |execution| {
            u32::try_from(execution.trials.len()).unwrap_or(u32::MAX)
        }),
        denominator_trial_count: executions
            .iter()
            .map(|execution| u32::try_from(execution.trials.len()).unwrap_or(u32::MAX))
            .sum(),
        tasks,
        configured_trial_count: executions
            .first()
            .map_or(0, |execution| {
                u32::try_from(execution.trials.len()).unwrap_or(u32::MAX)
            })
            .saturating_mul(task_count),
        sampled_trial_count: executions
            .iter()
            .map(|execution| u32::try_from(execution.trials.len()).unwrap_or(u32::MAX))
            .sum(),
        sandbox_preflight: Some(preflight),
    };
    evidence
        .validate()
        .map_err(|error| format!("invalid evaluation evidence: {error}"))?;
    Ok(evidence)
}

/// 构造未采样 run 的脱敏 evidence；每个 task 只保留选择身份，不生成 trial 证据。
pub(super) fn build_zero_sampling_evidence(
    run_id: &RunId,
    manifest_digest: String,
    plans: &[WorkspacePlan],
    trials_per_task: u32,
    preflight: EvaluationSandboxPreflight,
    result: &EvaluationResult,
) -> Result<EvaluationEvidence, String> {
    let task_ids = plans
        .iter()
        .map(|plan| plan.task_id.clone())
        .collect::<Vec<_>>();
    if let Some(task_id) = result
        .blocker
        .as_ref()
        .and_then(|blocker| blocker.task_id.as_ref())
        && !task_ids.contains(task_id)
    {
        return Err(format!(
            "zero-sampling blocker task {} is outside the selected task set",
            task_id.as_str()
        ));
    }
    let tasks = plans
        .iter()
        .map(|plan| EvaluationTaskEvidence {
            task_id: plan.task_id.clone(),
            source_tree_digest: None,
            source_commit: match &plan.source {
                PlannedWorkspaceSource::RemoteGit { commit, .. } => Some(commit.clone()),
                PlannedWorkspaceSource::Local { .. } => None,
            },
            trials: Vec::new(),
        })
        .collect::<Vec<_>>();
    let task_count = u32::try_from(tasks.len()).unwrap_or(u32::MAX);
    let evidence = EvaluationEvidence {
        schema_version: EvaluationEvidenceSchemaVersion::V4,
        run_id: run_id.clone(),
        manifest_digest,
        task_selection_digest: task_selection_digest(&task_ids),
        denominator_task_count: u32::try_from(plans.len()).unwrap_or(u32::MAX),
        trials_per_task,
        configured_trial_count: task_count.saturating_mul(trials_per_task),
        sampled_trial_count: 0,
        denominator_trial_count: 0,
        tasks,
        sandbox_preflight: Some(preflight),
    };
    evidence
        .validate_against_result(result)
        .map_err(|error| format!("invalid evaluation zero-sampling evidence: {error}"))?;
    Ok(evidence)
}

/// 计算产物内容摘要。
pub(super) fn content_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// 对 JSON 对象键排序后计算稳定摘要，避免 map 插入顺序影响指纹。
pub(super) fn canonical_json_digest(value: &impl Serialize) -> Result<String, String> {
    let value = serde_json::to_value(value)
        .map_err(|error| format!("failed to serialize canonical JSON fingerprint: {error}"))?;
    let mut bytes = Vec::new();
    write_canonical_json(&value, &mut bytes)?;
    Ok(content_digest(&bytes))
}

fn write_canonical_json(value: &Value, output: &mut Vec<u8>) -> Result<(), String> {
    match value {
        Value::Object(object) => {
            output.push(b'{');
            let mut entries = object.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key)
                    .map_err(|error| format!("failed to serialize canonical JSON key: {error}"))?;
                output.push(b':');
                write_canonical_json(value, output)?;
            }
            output.push(b'}');
        }
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_canonical_json(value, output)?;
            }
            output.push(b']');
        }
        _ => serde_json::to_writer(output, value)
            .map_err(|error| format!("failed to serialize canonical JSON value: {error}"))?,
    }
    Ok(())
}

/// Agent command 的 producer-owned typed projection；普通 audit 仅作为追踪输出。
#[derive(Debug, Default)]
pub(super) struct AgentCommandProjection {
    pub(super) diagnostics: Vec<CommandDiagnostic>,
    pub(super) unknown_count: usize,
    pub(super) strict_sandbox_command_count: usize,
    pub(super) local_process_fallback_count: usize,
}

pub(super) fn agent_command_projection(result: &AgentLoopResult) -> AgentCommandProjection {
    let mut projection = AgentCommandProjection::default();
    for tool_result in result
        .tool_results
        .iter()
        .filter(|tool_result| tool_result.tool_name == TOOL_COMMAND)
    {
        match CommandDiagnostic::from_agent_tool_result(tool_result) {
            AgentCommandDiagnosticProjection::Executed {
                diagnostic,
                strict_sandboxed,
                local_process_fallback,
            } => {
                projection.diagnostics.push(diagnostic);
                if strict_sandboxed {
                    projection.strict_sandbox_command_count =
                        projection.strict_sandbox_command_count.saturating_add(1);
                }
                if local_process_fallback {
                    projection.local_process_fallback_count =
                        projection.local_process_fallback_count.saturating_add(1);
                }
            }
            AgentCommandDiagnosticProjection::Unknown => {
                projection.unknown_count = projection.unknown_count.saturating_add(1);
            }
            AgentCommandDiagnosticProjection::NotExecuted => {}
        }
    }
    projection
}

fn build_task_evidence(
    plan: &WorkspacePlan,
    execution: &TaskEvaluation,
    run_dir: &Path,
) -> Result<EvaluationTaskEvidence, String> {
    let source_commit = match &plan.source {
        PlannedWorkspaceSource::RemoteGit { commit, .. } => Some(commit.clone()),
        PlannedWorkspaceSource::Local { .. } => None,
    };
    let trials = execution
        .trials
        .iter()
        .map(|trial| build_trial_evidence(plan, trial, run_dir))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(EvaluationTaskEvidence {
        task_id: plan.task_id.clone(),
        source_tree_digest: execution
            .trials
            .first()
            .and_then(|trial| trial.diagnostics.source_tree_digest.clone()),
        source_commit,
        trials,
    })
}

fn build_trial_evidence(
    plan: &WorkspacePlan,
    execution: &TaskExecution,
    run_dir: &Path,
) -> Result<EvaluationTrialEvidence, String> {
    let diagnostics = &execution.diagnostics;
    let task_dir = run_dir
        .join(plan.task_id.as_str())
        .join(format!("trial-{:04}", execution.result.trial));
    let changed_paths_digest = diagnostics
        .patch_evidence_path
        .as_ref()
        .map(|_| set_strings_digest("evaluation.changed_paths/v1", &diagnostics.changed_files));
    let trace_digest = diagnostics
        .trace_path
        .as_deref()
        .map(|path| artifact_digest(Path::new(path)))
        .transpose()?;
    Ok(EvaluationTrialEvidence {
        trial: execution.result.trial,
        changed_paths_digest,
        baseline: scope_evidence(
            &task_dir.join(AGENT_DIR),
            &plan.baseline.commands,
            &diagnostics.baseline,
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
        ),
        public: scope_evidence(
            &task_dir.join(AGENT_DIR),
            &plan.public.commands,
            &diagnostics.public,
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
        ),
        hidden: scope_evidence(
            &task_dir.join(AGENT_DIR),
            &plan.hidden.commands,
            &diagnostics.hidden,
            DEFAULT_COMMAND_TIMEOUT_SECONDS,
        ),
        trace_digest,
        patch_digest: diagnostics.patch_digest.clone(),
        prompt_structure: diagnostics.prompt_structure.clone(),
        prompt_fingerprint: diagnostics.prompt_fingerprint.clone(),
        tool_schema_fingerprint: diagnostics.tool_schema_fingerprint.clone(),
        provider: diagnostics.provider_evidence.clone(),
        local_process_fallback_count: u32::try_from(diagnostics.local_process_fallback_count)
            .unwrap_or(u32::MAX),
        local_process_fallback_unknown_count: u32::try_from(
            diagnostics.local_process_fallback_unknown_count,
        )
        .unwrap_or(u32::MAX),
    })
}

fn observed_verification_scopes(
    workspace: &Path,
    commands: &[CommandSpec],
    diagnostics: &StageDiagnostics,
    default_timeout_seconds: u64,
) -> Vec<String> {
    diagnostics
        .commands
        .iter()
        .filter_map(|diagnostic| {
            diagnostic
                .phase
                .strip_prefix("verification.command.")
                .and_then(|index| index.parse::<usize>().ok())
        })
        .filter_map(|index| commands.get(index))
        .filter_map(|command| {
            command_scope_digest_for_spec(workspace, command, default_timeout_seconds).ok()
        })
        .collect()
}

fn scope_evidence(
    workspace: &Path,
    commands: &[CommandSpec],
    diagnostics: &StageDiagnostics,
    default_timeout_seconds: u64,
) -> EvaluationScopeEvidence {
    let observed_scope_digests =
        observed_verification_scopes(workspace, commands, diagnostics, default_timeout_seconds);
    scope_evidence_with(commands, &observed_scope_digests, |command| {
        command_scope_digest_for_spec(workspace, command, default_timeout_seconds)
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

pub(super) fn is_sha256_digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}
