#![recursion_limit = "256"]

//! Evaluation v6/v9/v4 schema、task/trial 指标和证据绑定合同测试。

use serde_json::{Value, json};
use singularity_evaluation::{
    BlockerKind, EvaluationBlocker, EvaluationCapability, EvaluationError, EvaluationEvidence,
    EvaluationEvidenceSchemaVersion, EvaluationEvidenceSummary, EvaluationManifest,
    EvaluationPromptStructure, EvaluationProviderEvidence, EvaluationResult, EvaluationRunSummary,
    EvaluationSandboxPreflight, EvaluationSandboxPreflightFact, EvaluationSandboxPreflightOutcome,
    EvaluationScopeEvidence, EvaluationStageResults, EvaluationStatus, EvaluationTaskEvidence,
    EvaluationTaskResult, EvaluationTrialEvidence, EvaluationTrialResult, EvidenceVerdict, RunId,
    StageResult, StageStatus, TaskId, TaskSetSchemaVersion, task_selection_digest,
};

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const IMMUTABLE_COMMIT: &str = "f1dba0e1dd764ae72d67c3d5e1471cf14d3db030";

fn valid_manifest() -> Value {
    json!({
        "schema_version": "evaluation.task_set/v6",
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
                "instructions": "Apply a focused fix."
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

#[test]
fn public_representative_task_uses_the_current_runtime_contract() {
    let manifest_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/evaluation/public-representative-task.json");
    let manifest = EvaluationManifest::load(manifest_path).expect("public manifest is runnable");

    assert_eq!(manifest.task_set().schema_version, TaskSetSchemaVersion::V6);
    assert_eq!(manifest.task_set().trial_count, 2);
    assert_eq!(manifest.task_set().tasks.len(), 5);
    assert!(
        manifest
            .task_set()
            .tasks
            .iter()
            .all(|task| !task.agent.instructions.trim().is_empty())
    );
}

#[test]
fn workspace_setup_is_one_trial_level_plan_step() {
    let manifest = parse_manifest(&valid_manifest()).expect("manifest");
    let task_id = TaskId::new("representative-task").expect("task id");
    let plan = manifest.workspace_plan(&task_id).expect("workspace plan");

    assert_eq!(plan.setup_commands.len(), 1);
    assert_eq!(plan.setup_commands[0].argv.as_slice(), ["cargo", "fetch"]);
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
        functional_task_success: true,
        agent_protocol_success: true,
        sandbox_security_success: true,
        evaluation_passed: true,
        evidence: EvaluationEvidenceSummary {
            patch_digest: Some(DIGEST.to_string()),
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

fn dimension_trial(
    trial: u32,
    functional: bool,
    protocol: bool,
    sandbox: bool,
) -> EvaluationTrialResult {
    let mut result = passed_trial(trial);
    result.functional_task_success = functional;
    result.agent_protocol_success = protocol;
    result.sandbox_security_success = sandbox;
    result.evaluation_passed = functional && protocol && sandbox;
    result.status = if result.evaluation_passed {
        EvaluationStatus::Completed
    } else {
        EvaluationStatus::Failed
    };
    result
}

fn failed_trial(trial: u32) -> EvaluationTrialResult {
    EvaluationTrialResult {
        trial,
        status: EvaluationStatus::Failed,
        blocker: None,
        stages: stages(
            StageResult {
                status: StageStatus::Failed,
                blocker: None,
            },
            passed_stage(),
            passed_stage(),
        ),
        agent_completed: false,
        tests_passed: false,
        functional_task_success: false,
        agent_protocol_success: false,
        sandbox_security_success: false,
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
        code: None,
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
        functional_task_success: false,
        agent_protocol_success: false,
        sandbox_security_success: false,
        evaluation_passed: false,
        evidence: EvaluationEvidenceSummary::default(),
    }
}

fn task_result_for(task_id: &str, trials: Vec<EvaluationTrialResult>) -> EvaluationTaskResult {
    EvaluationTaskResult::from_trials(
        TaskId::new(task_id).expect("task id"),
        vec![EvaluationCapability::RequiredVerification],
        trials,
    )
}

fn task_result(trials: Vec<EvaluationTrialResult>) -> EvaluationTaskResult {
    task_result_for("representative-task", trials)
}

fn result_for(task: EvaluationTaskResult, trials_per_task: u32) -> EvaluationResult {
    result_for_tasks(vec![task], trials_per_task)
}

fn result_for_tasks(tasks: Vec<EvaluationTaskResult>, trials_per_task: u32) -> EvaluationResult {
    let mut result =
        EvaluationResult::from_tasks(RunId::new("run-1").expect("run id"), trials_per_task, tasks);
    result.sandbox_preflight = Some(supported_preflight());
    result
}

fn supported_preflight() -> EvaluationSandboxPreflight {
    EvaluationSandboxPreflight {
        outcome: EvaluationSandboxPreflightOutcome::Supported,
        error_code: None,
        profile: "workspace_write_network_denied".to_string(),
        backend: "test".to_string(),
        missing_capabilities: Vec::new(),
        os: "test".to_string(),
        arch: "test".to_string(),
        kernel: None,
        filesystem: None,
        overlayfs: EvaluationSandboxPreflightFact::Passed,
        user_namespace: EvaluationSandboxPreflightFact::NotApplicable,
        mount_namespace: EvaluationSandboxPreflightFact::NotApplicable,
        pid_namespace: EvaluationSandboxPreflightFact::NotApplicable,
        network_namespace: EvaluationSandboxPreflightFact::NotApplicable,
        no_new_privs: EvaluationSandboxPreflightFact::NotApplicable,
        seccomp: EvaluationSandboxPreflightFact::NotApplicable,
        landlock: EvaluationSandboxPreflightFact::NotApplicable,
        transactional_workspace: EvaluationSandboxPreflightFact::Passed,
        network_denied: EvaluationSandboxPreflightFact::Passed,
        protected_paths: EvaluationSandboxPreflightFact::Passed,
    }
}

fn unsupported_preflight() -> EvaluationSandboxPreflight {
    let mut preflight = supported_preflight();
    preflight.outcome = EvaluationSandboxPreflightOutcome::Unsupported;
    preflight.error_code = Some("sandbox_preflight_test_unsupported".to_string());
    preflight.missing_capabilities = vec!["transactional_workspace".to_string()];
    preflight.transactional_workspace = EvaluationSandboxPreflightFact::Failed;
    preflight
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
        baseline: empty_scope(),
        public: empty_scope(),
        hidden: empty_scope(),
        trace_digest: complete.then(|| DIGEST.to_string()),
        patch_digest: trial.evidence.patch_digest.clone(),
        prompt_structure: complete.then(|| EvaluationPromptStructure {
            contract: "evaluation.agent_prompt/v1".to_string(),
            model_message_roles: vec!["developer".to_string(), "user".to_string()],
            section_kinds: vec!["task_instructions".to_string()],
            resolved_tool_count: 1,
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
        schema_version: EvaluationEvidenceSchemaVersion::V4,
        run_id: result.run_id.clone(),
        manifest_digest: DIGEST.to_string(),
        task_selection_digest: task_selection_digest(std::slice::from_ref(&task.task_id)),
        denominator_task_count: 1,
        trials_per_task: result.summary.trials_per_task,
        configured_trial_count: result.summary.configured_trial_count,
        sampled_trial_count: result.summary.sampled_trial_count,
        denominator_trial_count: result.summary.trial_count,
        tasks: vec![EvaluationTaskEvidence {
            task_id: task.task_id.clone(),
            source_tree_digest: complete.then(|| DIGEST.to_string()),
            source_commit: None,
            trials: task
                .trials
                .iter()
                .map(|trial| trial_evidence(trial, complete))
                .collect(),
        }],
        sandbox_preflight: result.sandbox_preflight.clone(),
    }
}

#[test]
fn task_set_v6_exposes_only_instructions_to_the_agent() {
    let manifest = parse_manifest(&valid_manifest()).expect("valid v6 manifest");
    assert_eq!(manifest.task_set().trial_count, 2);
    let projection = manifest.task_set().tasks[0].agent_projection();
    assert_eq!(projection.instructions, "Apply a focused fix.");
    let serialized = serde_json::to_string(&projection).expect("projection JSON");
    assert!(!serialized.contains("allowed_paths"));
    assert!(!serialized.contains("required_tool_capabilities"));
    assert!(!serialized.contains("smoke_commands"));
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

    let mut retired_field = valid_manifest();
    retired_field["tasks"][0]["agent"]["allowed_paths"] = json!(["src/lib.rs"]);
    let error = parse_manifest(&retired_field).expect_err("retired field must be rejected");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn trial_count_is_bounded() {
    for invalid in [0, 33] {
        let mut manifest = valid_manifest();
        manifest["trial_count"] = json!(invalid);
        assert!(parse_manifest(&manifest).is_err());
    }
}

#[test]
fn duplicate_task_capabilities_are_rejected() {
    let mut manifest = valid_manifest();
    manifest["tasks"][0]["capabilities"] = json!(["rust", "rust"]);
    let error = parse_manifest(&manifest).expect_err("duplicate capabilities");
    assert!(error.to_string().contains("duplicates"));
}

#[test]
fn task_capability_taxonomy_rejects_unknown_names() {
    let mut manifest = valid_manifest();
    manifest["tasks"][0]["capabilities"] = json!(["future_registry_capability"]);
    assert!(parse_manifest(&manifest).is_err());
}

#[test]
fn command_and_path_trust_boundaries_remain_fail_closed() {
    for invalid in ["../secret", "C:/secret", "src\\secret", "src/file:stream"] {
        let mut manifest = valid_manifest();
        manifest["tasks"][0]["workspace"]["source"] = json!({
            "type": "local",
            "path": invalid
        });
        assert!(parse_manifest(&manifest).is_err(), "{invalid}");
    }
    let mut shell = valid_manifest();
    shell["tasks"][0]["evaluator"]["public"]["commands"][0]["argv"] =
        json!(["sh", "-c", "cargo test"]);
    assert!(parse_manifest(&shell).is_err());
}

#[test]
fn result_v9_keeps_blocked_trials_out_of_trial_diagnostics() {
    let task = task_result(vec![passed_trial(1), blocked_trial(2)]);
    assert_eq!(task.summary.completed_trial_count, 1);
    assert_eq!(task.summary.failed_trial_count, 0);
    assert_eq!(task.summary.blocked_trial_count, 1);
    assert_eq!(task.summary.agent_scored_trial_count, 1);
    assert_eq!(task.summary.agent_completed_count, 1);
    assert_eq!(task.summary.agent_failed_count, 0);
    assert_eq!(task.summary.functional_task_success_count, 1);
    assert_eq!(task.summary.agent_protocol_success_count, 1);
    assert_eq!(task.summary.sandbox_security_success_count, 1);

    let result = result_for(task, 2);
    result.validate().expect("valid blocked-denominator result");
    assert_eq!(result.summary.blocked_trial_count, 1);
    assert_eq!(result.summary.agent_scored_trial_count, 1);
    assert_eq!(result.summary.functional_task_success_count, 0);
    assert_eq!(result.summary.agent_protocol_success_count, 0);
    assert_eq!(result.summary.sandbox_security_success_count, 0);
}

#[test]
fn run_gate_uses_four_of_five_functional_and_protocol_tasks_and_all_sandbox_tasks() {
    let tasks = vec![
        task_result_for("task-a", vec![passed_trial(1)]),
        task_result_for("task-b", vec![passed_trial(1)]),
        task_result_for("task-c", vec![passed_trial(1)]),
        task_result_for("task-d", vec![dimension_trial(1, false, true, true)]),
        task_result_for("task-e", vec![dimension_trial(1, true, false, true)]),
    ];
    let mut result = result_for_tasks(tasks, 1);
    result.validate().expect("dimension gate result");

    assert_eq!(result.summary.functional_task_success_count, 4);
    assert_eq!(result.summary.agent_protocol_success_count, 4);
    assert_eq!(result.summary.sandbox_security_success_count, 5);
    assert!(result.summary.meets_functional_task_success_threshold);
    assert!(result.summary.meets_agent_protocol_success_threshold);
    assert!(result.summary.meets_sandbox_security_success_threshold);
    assert!(result.evaluation_passed);

    result.tasks[4] = task_result_for("task-e", vec![dimension_trial(1, true, false, false)]);
    result.summary = EvaluationRunSummary::from_tasks(&result.tasks, 1);
    result.evaluation_passed = false;
    result.validate().expect("sandbox gate result");
    assert!(!result.summary.meets_sandbox_security_success_threshold);
    assert!(!result.evaluation_passed);
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
fn preflight_blocker_binds_one_zero_sample_summary_to_the_same_error_code() {
    let preflight = unsupported_preflight();
    let blocker = EvaluationBlocker {
        code: preflight.error_code.clone(),
        kind: BlockerKind::Environment,
        message: "sandbox preflight unsupported".to_string(),
    };
    let result = EvaluationResult::blocked_by_sandbox_preflight(
        RunId::new("preflight-blocked").expect("run id"),
        2,
        5,
        blocker,
        preflight,
    );
    result.validate().expect("valid zero-sample blocker");

    let mut forged_count = result.clone();
    forged_count.summary.completed_trial_count = 1;
    assert!(forged_count.validate().is_err());

    let mut mismatched_code = result;
    mismatched_code.blocker.as_mut().expect("blocker").code =
        Some("sandbox_preflight_other".to_string());
    assert!(mismatched_code.validate().is_err());
}

#[test]
fn source_preparation_blocker_binds_zero_sampling_with_supported_preflight() {
    let task_ids = [
        TaskId::new("source-ok").expect("task id"),
        TaskId::new("source-blocked").expect("task id"),
    ];
    let blocker = EvaluationBlocker {
        code: Some("workspace_preparation_failed".to_string()),
        kind: BlockerKind::WorkspacePreparation,
        message: "source could not be materialized".to_string(),
    };
    let result = EvaluationResult::blocked_before_sampling(
        RunId::new("source-blocked-run").expect("run id"),
        2,
        3,
        blocker,
        supported_preflight(),
    );
    result
        .validate()
        .expect("source blocker is valid before sampling");

    let evidence = EvaluationEvidence {
        schema_version: EvaluationEvidenceSchemaVersion::V4,
        run_id: result.run_id.clone(),
        manifest_digest: DIGEST.to_string(),
        task_selection_digest: task_selection_digest(&task_ids),
        denominator_task_count: 2,
        trials_per_task: 3,
        denominator_trial_count: 0,
        configured_trial_count: 6,
        sampled_trial_count: 0,
        tasks: task_ids
            .iter()
            .cloned()
            .map(|task_id| EvaluationTaskEvidence {
                task_id,
                source_tree_digest: None,
                source_commit: None,
                trials: Vec::new(),
            })
            .collect(),
        sandbox_preflight: Some(supported_preflight()),
    };
    evidence
        .validate()
        .expect("zero-sampling evidence has a valid structural projection");
    evidence
        .validate_against_result(&result)
        .expect("zero-sampling evidence binds to source blocker");
}

#[test]
fn zero_sampling_result_rejects_post_sampling_blocker_categories_and_missing_code() {
    for kind in [
        BlockerKind::ProviderResponse,
        BlockerKind::ProviderAuthentication,
        BlockerKind::AgentRuntime,
    ] {
        let result = EvaluationResult::blocked_before_sampling(
            RunId::new("invalid-zero-sampling").expect("run id"),
            1,
            1,
            EvaluationBlocker {
                code: Some("provider_response_invalid".to_string()),
                kind,
                message: "post-sampling blocker".to_string(),
            },
            supported_preflight(),
        );
        assert!(result.validate().is_err(), "{kind:?} must not be run-level");
    }
    let missing_code = EvaluationResult::blocked_before_sampling(
        RunId::new("missing-zero-sampling-code").expect("run id"),
        1,
        1,
        EvaluationBlocker {
            code: None,
            kind: BlockerKind::WorkspacePreparation,
            message: "source preparation failed".to_string(),
        },
        supported_preflight(),
    );
    assert!(missing_code.validate().is_err());
}

#[test]
fn unsupported_past_and_future_schemas_fail_closed() {
    let result = result_for(task_result(vec![failed_trial(1)]), 1);
    let mut value = serde_json::to_value(result).expect("result JSON");
    value["schema_version"] = json!("evaluation.result/v5");
    assert!(matches!(
        EvaluationResult::from_json_str(&serde_json::to_string(&value).expect("JSON")),
        Err(EvaluationError::UnsupportedSchemaVersion { .. })
    ));

    let result = result_for(task_result(vec![failed_trial(1)]), 1);
    let mut value = serde_json::to_value(evidence_for(&result, false)).expect("evidence JSON");
    value["schema_version"] = json!("evaluation.evidence/v3");
    assert!(matches!(
        EvaluationEvidence::from_json_str(&serde_json::to_string(&value).expect("JSON")),
        Err(EvaluationError::UnsupportedSchemaVersion { .. })
    ));

    let result = result_for(task_result(vec![failed_trial(1)]), 1);
    let mut future = serde_json::to_value(result).expect("result JSON");
    future["schema_version"] = json!("evaluation.result/v10");
    assert!(matches!(
        EvaluationResult::from_json_str(&serde_json::to_string(&future).expect("JSON")),
        Err(EvaluationError::UnsupportedSchemaVersion { .. })
    ));

    let result = result_for(task_result(vec![failed_trial(1)]), 1);
    let mut future = serde_json::to_value(evidence_for(&result, false)).expect("evidence JSON");
    future["schema_version"] = json!("evaluation.evidence/v5");
    assert!(matches!(
        EvaluationEvidence::from_json_str(&serde_json::to_string(&future).expect("JSON")),
        Err(EvaluationError::UnsupportedSchemaVersion { .. })
    ));
}

#[test]
fn previous_result_is_rejected_and_current_v4_evidence_binds_to_v9_result() {
    let result = result_for(task_result(vec![passed_trial(1), failed_trial(2)]), 2);
    let mut legacy_result = serde_json::to_value(&result).expect("result JSON");
    legacy_result["schema_version"] = json!("evaluation.result/v6");
    assert!(matches!(
        EvaluationResult::from_json_str(
            &serde_json::to_string(&legacy_result).expect("legacy result JSON"),
        ),
        Err(EvaluationError::UnsupportedSchemaVersion { .. })
    ));

    let evidence = evidence_for(&result, true);
    let parsed_evidence = EvaluationEvidence::from_json_str(
        &serde_json::to_string(&evidence).expect("current evidence JSON"),
    )
    .expect("current v4 evidence parses directly");
    parsed_evidence
        .validate_against_result(&result)
        .expect("current v4 evidence binds to v9 result");
    assert_eq!(
        serde_json::to_value(parsed_evidence).expect("current evidence JSON")["schema_version"],
        json!("evaluation.evidence/v4")
    );
}

#[test]
fn evidence_v4_binds_every_trial_and_safe_reproducibility_identity() {
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
