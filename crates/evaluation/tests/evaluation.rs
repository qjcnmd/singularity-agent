#![recursion_limit = "256"]

//! Evaluation v5/v6/v2 schema、trial 分母和证据绑定合同测试。

use serde_json::{Value, json};
use singularity_evaluation::{
    BlockerKind, EvaluationBlocker, EvaluationCapability, EvaluationError, EvaluationEvidence,
    EvaluationEvidenceSchemaVersion, EvaluationEvidenceSummary, EvaluationManifest,
    EvaluationPromptStructure, EvaluationProviderEvidence, EvaluationResult,
    EvaluationResultSchemaVersion, EvaluationRunSummary, EvaluationScopeEvidence,
    EvaluationStageResults, EvaluationStatus, EvaluationTaskEvidence, EvaluationTaskResult,
    EvaluationTrialEvidence, EvaluationTrialResult, EvidenceVerdict, RunId, StageResult,
    StageStatus, TaskId, ToolCapabilityName, ToolCapabilityRequirement, task_selection_digest,
};

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const IMMUTABLE_COMMIT: &str = "f1dba0e1dd764ae72d67c3d5e1471cf14d3db030";

fn requirement(name: &str) -> ToolCapabilityRequirement {
    ToolCapabilityRequirement {
        capability: ToolCapabilityName::new(name).expect("capability name"),
        minimum_version: 1,
    }
}

fn valid_manifest() -> Value {
    json!({
        "schema_version": "evaluation.task_set/v5",
        "trial_count": 2,
        "tasks": [{
            "task_id": "representative-task",
            "description": "Fix a representative task.",
            "capabilities": ["single_file_fix", "required_verification"],
            "workspace": {
                "source": {
                    "type": "remote_git",
                    "repository": "https://github.com/example/repository.git",
                    "commit": IMMUTABLE_COMMIT
                },
                "setup_commands": [{"argv": ["cargo", "fetch"]}]
            },
            "agent": {
                "instructions": "Apply a focused fix.",
                "allowed_paths": ["src/lib.rs"],
                "required_tool_capabilities": [
                    {"capability": "workspace_read", "minimum_version": 1},
                    {"capability": "workspace_write", "minimum_version": 1},
                    {"capability": "command_execution", "minimum_version": 1}
                ],
                "smoke_commands": [{"argv": ["cargo", "check"], "timeout_seconds": 60}]
            },
            "evaluator": {
                "public_test_patch": {"format": "unified_diff", "content": "public patch"},
                "hidden_test_patch": {"format": "unified_diff", "content": "hidden patch"},
                "baseline": {"commands": [{"argv": ["cargo", "test", "--lib"]}]},
                "public": {"commands": [{"argv": ["cargo", "test", "--lib"]}]},
                "hidden": {"commands": [{"argv": ["cargo", "test", "--tests"]}]}
            }
        }]
    })
}

fn parse_manifest(value: &Value) -> Result<EvaluationManifest, EvaluationError> {
    EvaluationManifest::from_json_str(
        &serde_json::to_string(value).expect("manifest JSON"),
        env!("CARGO_MANIFEST_DIR"),
    )
}

fn stages(agent: StageResult, public: StageResult, hidden: StageResult) -> EvaluationStageResults {
    EvaluationStageResults {
        baseline: StageResult {
            status: StageStatus::Passed,
            blocker: None,
        },
        agent,
        public,
        hidden,
    }
}

fn passed_stage() -> StageResult {
    StageResult {
        status: StageStatus::Passed,
        blocker: None,
    }
}

fn passed_trial(trial: u32) -> EvaluationTrialResult {
    EvaluationTrialResult {
        trial,
        status: EvaluationStatus::Completed,
        blocker: None,
        stages: stages(passed_stage(), passed_stage(), passed_stage()),
        agent_completed: true,
        tests_passed: true,
        evaluation_passed: true,
        evidence: EvaluationEvidenceSummary {
            patch_digest: Some(DIGEST.to_string()),
            smoke_command_satisfied: true,
            strict_sandbox_command_count: 4,
            model_turns: trial + 1,
            tool_calls: trial + 2,
            agent_duration_ms: u64::from(trial) * 100,
            provider_latency_ms: u64::from(trial) * 25,
            provider_attempt_count: trial,
            provider_retry_count: trial - 1,
            total_tokens: u64::from(trial) * 200,
            ..EvaluationEvidenceSummary::default()
        },
    }
}

fn failed_trial(trial: u32) -> EvaluationTrialResult {
    EvaluationTrialResult {
        trial,
        status: EvaluationStatus::Failed,
        blocker: None,
        stages: stages(
            passed_stage(),
            StageResult {
                status: StageStatus::Failed,
                blocker: None,
            },
            passed_stage(),
        ),
        agent_completed: true,
        tests_passed: false,
        evaluation_passed: false,
        evidence: EvaluationEvidenceSummary {
            model_turns: 2,
            tool_calls: 3,
            agent_duration_ms: 100,
            ..EvaluationEvidenceSummary::default()
        },
    }
}

fn blocked_trial(trial: u32) -> EvaluationTrialResult {
    let blocker = EvaluationBlocker {
        kind: BlockerKind::Network,
        message: "provider unavailable".to_string(),
    };
    EvaluationTrialResult {
        trial,
        status: EvaluationStatus::Blocked,
        blocker: Some(blocker.clone()),
        stages: EvaluationStageResults {
            baseline: StageResult {
                status: StageStatus::Skipped,
                blocker: None,
            },
            agent: StageResult {
                status: StageStatus::Blocked,
                blocker: Some(blocker),
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
        evaluation_passed: false,
        evidence: EvaluationEvidenceSummary::default(),
    }
}

fn task_result(trials: Vec<EvaluationTrialResult>) -> EvaluationTaskResult {
    EvaluationTaskResult::from_trials(
        TaskId::new("representative-task").expect("task id"),
        vec![EvaluationCapability::RequiredVerification],
        vec![requirement("workspace_read")],
        trials,
    )
}

fn result_for(task: EvaluationTaskResult, trials_per_task: u32) -> EvaluationResult {
    let status = task.status;
    let blocker = task.blocker.clone();
    let evaluation_passed = task.evaluation_passed;
    let tasks = vec![task];
    EvaluationResult {
        schema_version: EvaluationResultSchemaVersion::V6,
        run_id: RunId::new("run-1").expect("run id"),
        status,
        blocker,
        evaluation_passed,
        summary: EvaluationRunSummary::from_tasks(&tasks, trials_per_task),
        tasks,
    }
}

fn empty_scope() -> EvaluationScopeEvidence {
    EvaluationScopeEvidence {
        expectation_known: true,
        expected_scope_digests: Vec::new(),
        observed_scope_digests: Vec::new(),
        required_scopes_satisfied: EvidenceVerdict::Passed,
    }
}

fn trial_evidence(trial: &EvaluationTrialResult, complete: bool) -> EvaluationTrialEvidence {
    EvaluationTrialEvidence {
        trial: trial.trial,
        changed_paths_digest: complete.then(|| DIGEST.to_string()),
        allowlist: if complete {
            EvidenceVerdict::Passed
        } else {
            EvidenceVerdict::Unknown
        },
        smoke: empty_scope(),
        baseline: empty_scope(),
        public: empty_scope(),
        hidden: empty_scope(),
        trace_digest: complete.then(|| DIGEST.to_string()),
        patch_digest: trial.evidence.patch_digest.clone(),
        prompt_structure: complete.then(|| EvaluationPromptStructure {
            contract: "evaluation.agent_prompt/v1".to_string(),
            model_message_roles: vec!["developer".to_string(), "user".to_string()],
            section_kinds: vec!["task_instructions".to_string()],
            allowed_path_count: 1,
            resolved_tool_count: 1,
            smoke_command_count: 0,
            project_instructions_fingerprint: None,
        }),
        prompt_fingerprint: complete.then(|| DIGEST.to_string()),
        tool_schema_fingerprint: complete.then(|| DIGEST.to_string()),
        provider: complete.then(|| EvaluationProviderEvidence {
            provider_fingerprint: DIGEST.to_string(),
            model_fingerprint: DIGEST.to_string(),
            negotiation_fingerprint: Some(DIGEST.to_string()),
            api_protocol: Some("responses".to_string()),
            protocol_contract_fingerprint: Some(DIGEST.to_string()),
            capability_metadata_fingerprint: Some(DIGEST.to_string()),
        }),
        local_process_fallback_count: trial.evidence.local_process_fallback_count,
        local_process_fallback_unknown_count: trial.evidence.local_process_fallback_unknown_count,
    }
}

fn evidence_for(result: &EvaluationResult, complete: bool) -> EvaluationEvidence {
    let task = &result.tasks[0];
    EvaluationEvidence {
        schema_version: EvaluationEvidenceSchemaVersion::V2,
        run_id: result.run_id.clone(),
        manifest_digest: DIGEST.to_string(),
        task_selection_digest: task_selection_digest(std::slice::from_ref(&task.task_id)),
        denominator_task_count: 1,
        trials_per_task: result.summary.trials_per_task,
        denominator_trial_count: result.summary.trial_count,
        tasks: vec![EvaluationTaskEvidence {
            task_id: task.task_id.clone(),
            source_tree_digest: complete.then(|| DIGEST.to_string()),
            source_commit: None,
            allowed_paths_digest: DIGEST.to_string(),
            tool_capability_requirements_digest: DIGEST.to_string(),
            trials: task
                .trials
                .iter()
                .map(|trial| trial_evidence(trial, complete))
                .collect(),
        }],
    }
}

#[test]
fn task_set_v5_uses_explicit_versioned_capability_requirements() {
    let manifest = parse_manifest(&valid_manifest()).expect("valid v5 manifest");
    assert_eq!(manifest.task_set().trial_count, 2);
    let projection = manifest.task_set().tasks[0].agent_projection();
    assert_eq!(projection.required_tool_capabilities.len(), 3);
    assert_eq!(
        projection.required_tool_capabilities[0].capability.as_str(),
        "workspace_read"
    );
    let serialized = serde_json::to_string(&projection).expect("projection JSON");
    assert!(!serialized.contains("allowed_tools"));
}

#[test]
fn old_schema_and_tool_name_artifacts_fail_closed() {
    let mut old = valid_manifest();
    old["schema_version"] = json!("evaluation.task_set/v4");
    assert!(matches!(
        parse_manifest(&old),
        Err(EvaluationError::UnsupportedSchemaVersion { .. })
    ));

    let mut legacy_field = valid_manifest();
    legacy_field["tasks"][0]["agent"]["allowed_tools"] = json!(["read"]);
    let error = parse_manifest(&legacy_field).expect_err("legacy field must be rejected");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn trial_count_and_capability_versions_are_bounded() {
    for invalid in [0, 33] {
        let mut manifest = valid_manifest();
        manifest["trial_count"] = json!(invalid);
        assert!(parse_manifest(&manifest).is_err());
    }
    let mut zero_version = valid_manifest();
    zero_version["tasks"][0]["agent"]["required_tool_capabilities"][0]["minimum_version"] =
        json!(0);
    assert!(parse_manifest(&zero_version).is_err());
}

#[test]
fn duplicate_capability_names_are_rejected_even_with_different_versions() {
    let mut manifest = valid_manifest();
    manifest["tasks"][0]["agent"]["required_tool_capabilities"] = json!([
        {"capability": "workspace_read", "minimum_version": 1},
        {"capability": "workspace_read", "minimum_version": 2}
    ]);
    let error = parse_manifest(&manifest).expect_err("duplicate capabilities");
    assert!(error.to_string().contains("duplicate capability"));
}

#[test]
fn capability_names_are_syntax_checked_without_a_local_whitelist() {
    let mut manifest = valid_manifest();
    manifest["tasks"][0]["agent"]["required_tool_capabilities"][0]["capability"] =
        json!("future_registry_capability");
    parse_manifest(&manifest).expect("registry owns semantic capability validation");

    manifest["tasks"][0]["agent"]["required_tool_capabilities"][0]["capability"] =
        json!("Invalid-Capability");
    assert!(parse_manifest(&manifest).is_err());
}

#[test]
fn command_and_path_trust_boundaries_remain_fail_closed() {
    for invalid in ["../secret", "C:/secret", "src\\secret", "src/file:stream"] {
        let mut manifest = valid_manifest();
        manifest["tasks"][0]["agent"]["allowed_paths"] = json!([invalid]);
        assert!(parse_manifest(&manifest).is_err(), "{invalid}");
    }
    let mut shell = valid_manifest();
    shell["tasks"][0]["evaluator"]["public"]["commands"][0]["argv"] =
        json!(["sh", "-c", "cargo test"]);
    assert!(parse_manifest(&shell).is_err());
}

#[test]
fn result_v6_derives_blocked_excluding_agent_denominators() {
    let task = task_result(vec![passed_trial(1), blocked_trial(2)]);
    assert_eq!(task.summary.completed_trial_count, 1);
    assert_eq!(task.summary.failed_trial_count, 0);
    assert_eq!(task.summary.blocked_trial_count, 1);
    assert_eq!(task.summary.agent_scored_trial_count, 1);
    assert_eq!(task.summary.agent_completed_count, 1);
    assert_eq!(task.summary.agent_failed_count, 0);

    let result = result_for(task, 2);
    result.validate().expect("valid blocked-denominator result");
    assert_eq!(result.summary.blocked_trial_count, 1);
    assert_eq!(result.summary.agent_scored_trial_count, 1);
}

#[test]
fn single_trial_is_unstable_and_multi_trial_statistics_are_finite() {
    let single = task_result(vec![failed_trial(1)]);
    assert!(!single.stability.stable);
    let single_result = result_for(single, 1);
    single_result.validate().expect("single trial result");

    let multiple = task_result(vec![passed_trial(1), passed_trial(2)]);
    assert!(multiple.stability.stable);
    for statistics in [
        multiple.stability.model_turns.as_ref(),
        multiple.stability.tool_calls.as_ref(),
        multiple.stability.agent_duration_ms.as_ref(),
        multiple.stability.provider_latency_ms.as_ref(),
        multiple.stability.provider_retries.as_ref(),
        multiple.stability.total_tokens.as_ref(),
    ] {
        let statistics = statistics.expect("finite statistics");
        assert!(statistics.mean.is_finite());
        assert!(statistics.population_variance.is_finite());
        assert_eq!(statistics.sample_count, 2);
    }
    result_for(multiple, 2)
        .validate()
        .expect("multi trial result");
}

#[test]
fn result_v5_and_evidence_v1_are_typed_rejections() {
    let result = result_for(task_result(vec![failed_trial(1)]), 1);
    let mut value = serde_json::to_value(result).expect("result JSON");
    value["schema_version"] = json!("evaluation.result/v5");
    assert!(matches!(
        EvaluationResult::from_json_str(&serde_json::to_string(&value).expect("JSON")),
        Err(EvaluationError::UnsupportedSchemaVersion { .. })
    ));

    let result = result_for(task_result(vec![failed_trial(1)]), 1);
    let mut value = serde_json::to_value(evidence_for(&result, false)).expect("evidence JSON");
    value["schema_version"] = json!("evaluation.evidence/v1");
    assert!(matches!(
        EvaluationEvidence::from_json_str(&serde_json::to_string(&value).expect("JSON")),
        Err(EvaluationError::UnsupportedSchemaVersion { .. })
    ));
}

#[test]
fn evidence_v2_binds_every_trial_and_safe_reproducibility_identity() {
    let result = result_for(task_result(vec![passed_trial(1), passed_trial(2)]), 2);
    let evidence = evidence_for(&result, true);
    evidence
        .validate_against_result(&result)
        .expect("complete per-trial evidence");

    let mut missing_provider = evidence.clone();
    missing_provider.tasks[0].trials[1].provider = None;
    let error = missing_provider
        .validate_against_result(&result)
        .expect_err("passed trial requires provider negotiation identity");
    assert!(error.to_string().contains("trial 2"));

    let mut mismatched_trial = evidence;
    mismatched_trial.tasks[0].trials[1].trial = 3;
    assert!(mismatched_trial.validate().is_err());
}

#[test]
fn evidence_rejects_partial_negotiation_and_unknown_raw_fields() {
    let result = result_for(task_result(vec![failed_trial(1)]), 1);
    let mut evidence = evidence_for(&result, true);
    evidence.tasks[0].trials[0]
        .provider
        .as_mut()
        .expect("provider")
        .api_protocol = None;
    assert!(evidence.validate().is_err());

    let evidence = evidence_for(&result, false);
    let mut value = serde_json::to_value(evidence).expect("evidence JSON");
    value["tasks"][0]["trials"][0]["raw_prompt"] = json!("secret");
    let error =
        EvaluationEvidence::from_json_str(&serde_json::to_string(&value).expect("evidence JSON"))
            .expect_err("unknown raw evidence field");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn summary_and_stability_cannot_be_forged() {
    let result = result_for(task_result(vec![passed_trial(1), passed_trial(2)]), 2);
    let mut value = serde_json::to_value(result).expect("result JSON");
    value["summary"]["blocked_trial_count"] = json!(2);
    let error =
        EvaluationResult::from_json_str(&serde_json::to_string(&value).expect("result JSON"))
            .expect_err("summary must be derived");
    assert!(error.to_string().contains("summary"));

    let result = result_for(task_result(vec![failed_trial(1)]), 1);
    let mut value = serde_json::to_value(result).expect("result JSON");
    value["tasks"][0]["stability"]["stable"] = json!(true);
    let error =
        EvaluationResult::from_json_str(&serde_json::to_string(&value).expect("result JSON"))
            .expect_err("single trial cannot claim stability");
    assert!(error.to_string().contains("stability") || error.to_string().contains("stable"));
}
