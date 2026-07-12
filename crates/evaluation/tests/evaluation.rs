use std::path::Path;

use serde_json::{Value, json};
use singularity_evaluation::{
    CommandExpectation, EvaluationError, EvaluationManifest, EvaluationResult, EvaluationStage,
    EvaluationStatus, PlannedWorkspaceSource, StageStatus, TaskId, WorkspaceSeed,
};

const IMMUTABLE_COMMIT: &str = "f1dba0e1dd764ae72d67c3d5e1471cf14d3db030";
const PUBLIC_TEST_PATCH_MARKER: &str = "PUBLIC_EVALUATOR_ONLY_PATCH_MARKER";
const HIDDEN_TEST_PATCH_MARKER: &str = "HIDDEN_EVALUATOR_ONLY_PATCH_MARKER";

fn valid_manifest() -> Value {
    json!({
        "schema_version": "evaluation.task_set/v4",
        "tasks": [
            {
                "task_id": "sqlfluff__sqlfluff-2419",
                "description": "Fix the public representative SQLFluff task.",
                "capabilities": ["single_file_fix", "python", "required_verification"],
                "workspace": {
                    "source": {
                        "type": "remote_git",
                        "repository": "https://github.com/sqlfluff/sqlfluff.git",
                        "commit": IMMUTABLE_COMMIT
                    },
                    "setup_commands": [
                        {"argv": ["cargo", "fetch"]}
                    ]
                },
                "agent": {
                    "instructions": "Apply the smallest focused fix.",
                    "allowed_paths": ["src/sqlfluff/rules/L060.py"],
                    "allowed_tools": [
                        "builtin.read",
                        "builtin.grep",
                        "builtin.edit",
                        "builtin.command"
                    ],
                    "smoke_commands": [
                        {
                            "argv": [
                                "cargo",
                                "check"
                            ],
                            "timeout_seconds": 60
                        }
                    ]
                },
                "evaluator": {
                    "public_test_patch": {
                        "format": "unified_diff",
                        "content": PUBLIC_TEST_PATCH_MARKER
                    },
                    "hidden_test_patch": {
                        "format": "unified_diff",
                        "content": HIDDEN_TEST_PATCH_MARKER
                    },
                    "baseline": {
                        "commands": [
                            {
                                "argv": [
                                    "cargo",
                                    "test",
                                    "--workspace"
                                ]
                            }
                        ]
                    },
                    "public": {
                        "commands": [
                            {
                                "argv": [
                                    "cargo",
                                    "test",
                                    "--workspace"
                                ]
                            }
                        ]
                    },
                    "hidden": {
                        "commands": [
                            {
                                "argv": [
                                    "cargo",
                                    "test",
                                    "--workspace"
                                ]
                            }
                        ]
                    }
                }
            }
        ]
    })
}

fn valid_result() -> Value {
    json!({
        "schema_version": "evaluation.result/v4",
        "run_id": "public-representative-20260710",
        "status": "completed",
        "evaluation_passed": false,
        "summary": {
            "task_count": 1,
            "scored_task_count": 1,
            "agent_completed_count": 1,
            "tests_passed_count": 0,
            "evaluation_passed_count": 0,
            "blocked_count": 0,
            "task_success_rate_basis_points": 0,
            "meets_core_task_success_threshold": false
        },
        "tasks": [
            {
                "task_id": "sqlfluff__sqlfluff-2419",
                "capabilities": ["single_file_fix", "python", "required_verification"],
                "status": "completed",
                "stages": {
                    "baseline": {"status": "passed"},
                    "agent": {"status": "passed"},
                    "public": {"status": "failed"},
                    "hidden": {"status": "passed"}
                },
                "agent_completed": true,
                "tests_passed": false,
                "evaluation_passed": false,
                "evidence": {
                    "workspace_change_count": 1,
                    "patch_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "tool_calls": 2,
                    "model_turns": 2,
                    "approval_count": 0,
                    "invalid_tool_call_count": 0,
                    "repeated_tool_call_count": 0,
                    "repair_attempt_count": 0,
                    "completion_rejection_count": 0,
                    "compaction_count": 0,
                    "provider_attempt_count": 2,
                    "provider_retry_count": 0,
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "cached_input_tokens": 0,
                    "reasoning_tokens": 0,
                    "total_tokens": 120,
                    "provider_latency_ms": 500,
                    "agent_duration_ms": 700,
                    "smoke_command_satisfied": true,
                    "strict_sandbox_command_count": 3,
                    "local_process_fallback_count": 0
                }
            }
        ]
    })
}

fn parse_manifest(value: &Value) -> Result<EvaluationManifest, EvaluationError> {
    EvaluationManifest::from_json_str(
        &serde_json::to_string(value).expect("serialize manifest fixture"),
        env!("CARGO_MANIFEST_DIR"),
    )
}

fn parse_result(value: &Value) -> Result<EvaluationResult, EvaluationError> {
    EvaluationResult::from_json_str(
        &serde_json::to_string(value).expect("serialize result fixture"),
    )
}

#[test]
fn task_set_rejects_unknown_fields_at_every_schema_boundary() {
    let mut top_level = valid_manifest();
    top_level["legacy_mode"] = json!(true);
    let error = parse_manifest(&top_level).expect_err("reject unknown top-level field");
    assert!(error.to_string().contains("unknown field"));

    let mut nested = valid_manifest();
    nested["tasks"][0]["agent"]["hidden_hint"] = json!("do not expose");
    let error = parse_manifest(&nested).expect_err("reject unknown nested field");
    assert!(error.to_string().contains("unknown field"));

    let mut workspace_source = valid_manifest();
    workspace_source["tasks"][0]["workspace"]["source"]["branch"] = json!("main");
    let error = parse_manifest(&workspace_source).expect_err("reject unknown source field");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn result_rejects_unknown_fields() {
    let mut result = valid_result();
    result["tasks"][0]["legacy_status"] = json!("success");

    let error = parse_result(&result).expect_err("reject unknown result field");
    assert!(error.to_string().contains("unknown field"));

    let mut stage = valid_result();
    stage["tasks"][0]["stages"]["public"]["exit_code"] = json!(1);
    let error = parse_result(&stage).expect_err("reject unknown stage result field");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn legacy_task_set_versions_and_legacy_task_fields_are_rejected() {
    let mut manifest = valid_manifest();
    manifest["schema_version"] = json!("evaluation.task_set/v1");
    assert!(matches!(
        parse_manifest(&manifest),
        Err(EvaluationError::UnsupportedSchemaVersion { .. })
    ));

    manifest["schema_version"] = json!("evaluation.task_set/v2");
    assert!(matches!(
        parse_manifest(&manifest),
        Err(EvaluationError::UnsupportedSchemaVersion { .. })
    ));

    let mut result = valid_result();
    result["schema_version"] = json!("evaluation.result/v1");
    let error = parse_result(&result).expect_err("reject result v1");
    assert!(matches!(
        error,
        EvaluationError::UnsupportedSchemaVersion { .. }
    ));

    let mut legacy_field = valid_manifest();
    legacy_field["tasks"][0]["user_task"] = json!("legacy prompt field");
    let error = parse_manifest(&legacy_field).expect_err("reject legacy task field");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn duplicate_task_ids_are_rejected() {
    let mut manifest = valid_manifest();
    let duplicate = manifest["tasks"][0].clone();
    manifest["tasks"]
        .as_array_mut()
        .expect("tasks array")
        .push(duplicate);

    let error = parse_manifest(&manifest).expect_err("reject duplicate task ids");
    assert!(matches!(error, EvaluationError::DuplicateTaskId(_)));
}

#[test]
fn task_and_run_ids_use_strict_portable_syntax() {
    for invalid in [
        "",
        " has-space",
        "contains space",
        "contains/slash",
        "contains:colon",
        ".leading-dot",
        "trailing-dot.",
    ] {
        let mut manifest = valid_manifest();
        manifest["tasks"][0]["task_id"] = json!(invalid);
        assert!(parse_manifest(&manifest).is_err(), "task id {invalid:?}");

        let mut result = valid_result();
        result["run_id"] = json!(invalid);
        assert!(parse_result(&result).is_err(), "run id {invalid:?}");
    }
}

#[test]
fn local_workspace_paths_are_manifest_relative_and_lexically_safe() {
    let temp = tempfile::tempdir().expect("create manifest directory");
    let mut manifest = valid_manifest();
    manifest["tasks"][0]["workspace"]["source"] = json!({
        "type": "local",
        "path": "fixtures/sqlfluff"
    });

    let loaded = EvaluationManifest::from_json_str(
        &serde_json::to_string(&manifest).expect("serialize manifest"),
        temp.path(),
    )
    .expect("parse local workspace");
    let task_id = TaskId::new("sqlfluff__sqlfluff-2419").expect("valid task id");
    let plan = loaded
        .workspace_plan(&task_id)
        .expect("build workspace plan");

    let expected = std::fs::canonicalize(temp.path())
        .expect("canonical manifest directory")
        .join("fixtures")
        .join("sqlfluff");
    assert_eq!(
        plan.source,
        PlannedWorkspaceSource::Local { path: expected }
    );

    for invalid in [
        "../repo",
        "fixtures/../repo",
        "/absolute/repo",
        "C:/absolute/repo",
        "fixtures/repo:stream",
        r"fixtures\repo",
        "fixtures//repo",
    ] {
        let mut invalid_manifest = manifest.clone();
        invalid_manifest["tasks"][0]["workspace"]["source"]["path"] = json!(invalid);
        assert!(
            EvaluationManifest::from_json_str(
                &serde_json::to_string(&invalid_manifest).expect("serialize invalid manifest"),
                temp.path(),
            )
            .is_err(),
            "workspace path {invalid:?}"
        );
    }
}

#[test]
fn task_paths_reject_parent_absolute_ads_and_backslash_forms() {
    for invalid in [
        "../secret.txt",
        "src/../secret.txt",
        "/secret.txt",
        "C:/secret.txt",
        "src/file.txt:stream",
        r"src\file.txt",
    ] {
        let mut manifest = valid_manifest();
        manifest["tasks"][0]["agent"]["allowed_paths"] = json!([invalid]);
        assert!(
            parse_manifest(&manifest).is_err(),
            "allowed path {invalid:?}"
        );
    }
}

#[test]
fn commands_require_nonempty_argv_arrays_and_reject_shell_string_wrappers() {
    let mut raw_string = valid_manifest();
    raw_string["tasks"][0]["evaluator"]["public"]["commands"] = json!(["cargo test"]);
    assert!(parse_manifest(&raw_string).is_err());

    let mut empty_argv = valid_manifest();
    empty_argv["tasks"][0]["evaluator"]["public"]["commands"] = json!([{"argv": []}]);
    assert!(parse_manifest(&empty_argv).is_err());

    for argv in [
        json!(["sh", "-c", "cargo test"]),
        json!(["bash", "-c", "cargo test"]),
        json!(["cmd.exe", "/C", "cargo test"]),
        json!(["powershell.exe", "-Command", "cargo test"]),
    ] {
        let mut shell = valid_manifest();
        shell["tasks"][0]["evaluator"]["public"]["commands"] = json!([{"argv": argv}]);
        assert!(parse_manifest(&shell).is_err(), "shell argv {argv}");
    }

    parse_manifest(&valid_manifest()).expect("direct argv commands are valid");
}

#[test]
fn remote_git_workspace_requires_remote_url_and_full_immutable_commit() {
    for invalid_commit in [
        "main",
        "v1.2.3",
        "f1dba0e",
        "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
    ] {
        let mut manifest = valid_manifest();
        manifest["tasks"][0]["workspace"]["source"]["commit"] = json!(invalid_commit);
        assert!(
            parse_manifest(&manifest).is_err(),
            "commit {invalid_commit:?}"
        );
    }

    for invalid_repo in ["../sqlfluff", "C:/repos/sqlfluff", "file:///tmp/sqlfluff"] {
        let mut manifest = valid_manifest();
        manifest["tasks"][0]["workspace"]["source"]["repository"] = json!(invalid_repo);
        assert!(parse_manifest(&manifest).is_err(), "repo {invalid_repo:?}");
    }

    parse_manifest(&valid_manifest()).expect("full commit is immutable");
}

#[test]
fn evaluation_commands_default_to_denied_network_and_allow_explicit_opt_in() {
    let manifest = parse_manifest(&valid_manifest()).expect("parse manifest");
    let task_id = TaskId::new("sqlfluff__sqlfluff-2419").expect("valid task id");
    let plan = manifest.workspace_plan(&task_id).expect("build plan");
    let default_command =
        serde_json::to_value(&plan.public.commands[0]).expect("serialize command");
    assert_eq!(default_command.get("network_access"), None);

    let mut explicit = valid_manifest();
    explicit["tasks"][0]["workspace"]["setup_commands"][0]["network_access"] = json!("allowed");
    let manifest = parse_manifest(&explicit).expect("parse explicit network command");
    let plan = manifest
        .workspace_plan(&task_id)
        .expect("build explicit plan");
    let setup = serde_json::to_value(&plan.agent.setup_commands[0]).expect("serialize setup");
    assert_eq!(setup["network_access"], "allowed");
}

#[test]
fn evaluation_rejects_unimplemented_tool_namespaces() {
    for unsupported in ["plugin.server.tool", "mcp.server.tool", "builtin.unknown"] {
        let mut manifest = valid_manifest();
        manifest["tasks"][0]["agent"]["allowed_tools"] = json!([unsupported]);
        let error = parse_manifest(&manifest).expect_err("reject unsupported tool");
        assert!(
            error
                .to_string()
                .contains("unsupported evaluation tool name"),
            "{unsupported}: {error}"
        );
    }
}

#[test]
fn smoke_commands_require_the_command_tool() {
    let mut manifest = valid_manifest();
    manifest["tasks"][0]["agent"]["allowed_tools"] = json!(["builtin.read"]);
    let error = parse_manifest(&manifest).expect_err("smoke command without command tool");
    assert!(
        error
            .to_string()
            .contains("agent.smoke_commands requires builtin.command"),
        "{error}"
    );
}

#[test]
fn command_timeout_has_a_bounded_upper_limit() {
    let mut manifest = valid_manifest();
    manifest["tasks"][0]["agent"]["smoke_commands"][0]["timeout_seconds"] = json!(3_601);
    let error = parse_manifest(&manifest).expect_err("reject excessive command timeout");
    assert!(
        error.to_string().contains("must not exceed 3600"),
        "{error}"
    );
}

#[test]
fn workspace_plan_has_typed_isolated_baseline_agent_public_and_hidden_stages() {
    let manifest = parse_manifest(&valid_manifest()).expect("parse manifest");
    let task_id = TaskId::new("sqlfluff__sqlfluff-2419").expect("valid task id");
    let plan = manifest.workspace_plan(&task_id).expect("build plan");

    assert_eq!(plan.baseline.stage, EvaluationStage::Baseline);
    assert_eq!(plan.baseline.seed, WorkspaceSeed::TaskSource);
    assert_eq!(plan.baseline.expectation, CommandExpectation::Failure);
    assert_eq!(plan.agent.stage, EvaluationStage::Agent);
    assert_eq!(plan.agent.seed, WorkspaceSeed::TaskSource);
    assert_eq!(plan.public.stage, EvaluationStage::Public);
    assert_eq!(plan.public.seed, WorkspaceSeed::AgentOutput);
    assert_eq!(plan.public.expectation, CommandExpectation::Success);
    assert_eq!(plan.hidden.stage, EvaluationStage::Hidden);
    assert_eq!(plan.hidden.seed, WorkspaceSeed::AgentOutput);
    assert_eq!(plan.hidden.expectation, CommandExpectation::Success);
    assert_eq!(plan.baseline.setup_commands.len(), 1);
    assert_eq!(plan.agent.setup_commands.len(), 1);
    assert_eq!(plan.public.setup_commands.len(), 1);
    assert_eq!(plan.hidden.setup_commands.len(), 1);
    assert_eq!(plan.baseline.commands.len(), 1);
    assert_eq!(plan.public.commands.len(), 1);
    assert_eq!(plan.hidden.commands.len(), 1);
}

#[test]
fn evaluator_only_test_patches_never_enter_agent_visible_projection() {
    let manifest = parse_manifest(&valid_manifest()).expect("parse manifest");
    let task = &manifest.task_set().tasks[0];
    let projection = task.agent_projection();
    let projection_json = serde_json::to_string(&projection).expect("serialize projection");

    assert!(!projection_json.contains(PUBLIC_TEST_PATCH_MARKER));
    assert!(!projection_json.contains(HIDDEN_TEST_PATCH_MARKER));
    assert!(!projection_json.contains("test_patch"));
    assert!(!projection_json.contains("evaluator"));

    let task_id = TaskId::new("sqlfluff__sqlfluff-2419").expect("valid task id");
    let plan = manifest.workspace_plan(&task_id).expect("build plan");
    let agent_plan_json = serde_json::to_string(&plan.agent).expect("serialize agent plan");
    assert!(!agent_plan_json.contains(PUBLIC_TEST_PATCH_MARKER));
    assert!(!agent_plan_json.contains(HIDDEN_TEST_PATCH_MARKER));
    assert!(
        plan.baseline
            .test_patch
            .as_ref()
            .is_some_and(|patch| patch.content() == PUBLIC_TEST_PATCH_MARKER)
    );
    assert!(
        plan.public
            .test_patch
            .as_ref()
            .is_some_and(|patch| patch.content() == PUBLIC_TEST_PATCH_MARKER)
    );
    assert!(
        plan.hidden
            .test_patch
            .as_ref()
            .is_some_and(|patch| patch.content() == HIDDEN_TEST_PATCH_MARKER)
    );
}

#[test]
fn manifest_rejects_duplicate_public_and_hidden_verification_evidence() {
    let mut value = valid_manifest();
    value["tasks"][0]["evaluator"]["hidden_test_patch"] =
        value["tasks"][0]["evaluator"]["public_test_patch"].clone();
    value["tasks"][0]["evaluator"]["hidden"] = value["tasks"][0]["evaluator"]["public"].clone();

    let error = parse_manifest(&value).expect_err("duplicate evidence must be rejected");
    assert!(error.to_string().contains("independent public and hidden"));
}

#[test]
fn manifest_rejects_verification_evidence_that_only_differs_in_execution_settings() {
    for (field, setting) in [
        ("timeout_seconds", json!(60)),
        ("network_access", json!("allowed")),
    ] {
        let mut value = valid_manifest();
        value["tasks"][0]["evaluator"]["hidden_test_patch"] =
            value["tasks"][0]["evaluator"]["public_test_patch"].clone();
        value["tasks"][0]["evaluator"]["hidden"]["commands"][0][field] = setting;

        let error =
            parse_manifest(&value).expect_err("execution-setting-only evidence must be rejected");
        assert!(error.to_string().contains("independent public and hidden"));
    }
}

#[test]
fn manifest_accepts_independent_patch_or_command_scope_evidence() {
    let mut different_patch = valid_manifest();
    different_patch["tasks"][0]["evaluator"]["hidden"]["commands"] =
        different_patch["tasks"][0]["evaluator"]["public"]["commands"].clone();
    parse_manifest(&different_patch).expect("different patches are independent evidence");

    let mut different_command_scope = valid_manifest();
    different_command_scope["tasks"][0]["evaluator"]["hidden_test_patch"] =
        different_command_scope["tasks"][0]["evaluator"]["public_test_patch"].clone();
    different_command_scope["tasks"][0]["evaluator"]["hidden"]["commands"][0]["argv"] =
        json!(["cargo", "test", "--all-targets"]);
    parse_manifest(&different_command_scope)
        .expect("different command argv is independent evidence");
}

#[test]
fn agent_completion_test_success_and_evaluation_success_remain_separate() {
    let result = parse_result(&valid_result()).expect("parse result");
    let task = &result.tasks[0];

    assert_eq!(result.status, EvaluationStatus::Completed);
    assert_eq!(task.status, EvaluationStatus::Completed);
    assert_eq!(task.stages.agent.status, StageStatus::Passed);
    assert!(task.agent_completed);
    assert!(!task.tests_passed);
    assert!(!task.evaluation_passed);
    assert!(!result.evaluation_passed);

    let mut invalid = valid_result();
    invalid["tasks"][0]["evaluation_passed"] = json!(true);
    invalid["evaluation_passed"] = json!(true);
    let error = parse_result(&invalid).expect_err("evaluation pass requires agent and tests");
    assert!(error.to_string().contains("evaluation_passed"));

    let mut baseline_not_satisfied = valid_result();
    baseline_not_satisfied["tasks"][0]["stages"]["baseline"]["status"] = json!("failed");
    baseline_not_satisfied["tasks"][0]["stages"]["public"]["status"] = json!("passed");
    baseline_not_satisfied["tasks"][0]["tests_passed"] = json!(true);
    baseline_not_satisfied["tasks"][0]["evaluation_passed"] = json!(true);
    baseline_not_satisfied["evaluation_passed"] = json!(true);
    let error = parse_result(&baseline_not_satisfied)
        .expect_err("evaluation pass requires a satisfied baseline contract");
    assert!(error.to_string().contains("baseline"));
}

#[test]
fn blocked_status_requires_a_typed_blocker() {
    let mut blocked = valid_result();
    blocked["status"] = json!("blocked");
    blocked["evaluation_passed"] = json!(false);
    blocked["tasks"][0]["status"] = json!("blocked");
    blocked["tasks"][0]["stages"]["agent"] = json!({
        "status": "blocked",
        "blocker": {
            "kind": "sandbox",
            "message": "restricted-token sandbox is unavailable"
        }
    });
    blocked["tasks"][0]["blocker"] = json!({
        "kind": "sandbox",
        "message": "restricted-token sandbox is unavailable"
    });
    blocked["blocker"] = json!({
        "kind": "sandbox",
        "message": "restricted-token sandbox is unavailable"
    });
    blocked["tasks"][0]["agent_completed"] = json!(false);
    blocked["summary"]["scored_task_count"] = json!(0);
    blocked["summary"]["agent_completed_count"] = json!(0);
    blocked["summary"]["blocked_count"] = json!(1);

    parse_result(&blocked).expect("typed blocker is valid");

    blocked
        .as_object_mut()
        .expect("result object")
        .remove("blocker");
    let error = parse_result(&blocked).expect_err("blocked run needs blocker");
    assert!(error.to_string().contains("blocker"));
}

#[test]
fn public_representative_manifest_is_validated_by_the_crate() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("docs")
        .join("evaluation")
        .join("public-representative-task.json");
    let manifest = EvaluationManifest::load(&path).expect("load public manifest");

    assert_eq!(manifest.task_set().tasks.len(), 3);
    for task in &manifest.task_set().tasks {
        let projection = task.agent_projection();
        let projection_json = serde_json::to_string(&projection).expect("serialize projection");
        assert!(!projection_json.contains("test_patch"));
        assert!(!projection_json.contains(PUBLIC_TEST_PATCH_MARKER));
        assert!(!projection_json.contains(HIDDEN_TEST_PATCH_MARKER));
    }

    let task = manifest
        .task_set()
        .tasks
        .iter()
        .find(|task| task.task_id.as_str() == "receipt_calculator__multi_line_receipt")
        .expect("local receipt task");
    let projection = task.agent_projection();
    assert_eq!(
        projection
            .allowed_paths
            .iter()
            .map(|path| path.as_str())
            .collect::<Vec<_>>(),
        ["pricing.py", "receipt.py"]
    );
    assert_eq!(
        projection.smoke_commands[0].argv.as_slice(),
        ["python", "-B", "smoke_test.py"]
    );
    let projection_json = serde_json::to_string(&projection).expect("serialize receipt projection");
    assert!(!projection_json.contains("test_public_receipt.py"));
    assert!(!projection_json.contains("test_hidden_receipt.py"));
    assert!(!projection_json.contains("evaluator"));

    let plan = manifest
        .workspace_plan(&task.task_id)
        .expect("build local receipt plan");
    let expected_fixture = manifest
        .manifest_dir()
        .join("fixtures")
        .join("receipt-calculator");
    assert_eq!(
        plan.source,
        PlannedWorkspaceSource::Local {
            path: expected_fixture
        }
    );
    let public_patch = plan.public.test_patch.as_ref().expect("public patch");
    let hidden_patch = plan.hidden.test_patch.as_ref().expect("hidden patch");
    assert!(public_patch.content().contains("test_public_receipt.py"));
    assert!(hidden_patch.content().contains("test_hidden_receipt.py"));
    assert_ne!(public_patch.content(), hidden_patch.content());
    assert_ne!(plan.public.commands, plan.hidden.commands);

    let cross_language = manifest
        .task_set()
        .tasks
        .iter()
        .find(|task| task.task_id.as_str() == "rust_node_calculator__multi_line_total")
        .expect("cross-language task");
    let projection = cross_language.agent_projection();
    assert_eq!(projection.smoke_commands.len(), 2);
    assert_eq!(
        projection.smoke_commands[0].argv.as_slice(),
        ["cargo", "test", "--locked", "--lib"]
    );
    assert_eq!(
        projection.smoke_commands[1].argv.as_slice(),
        ["node", "smoke_test.mjs"]
    );
}

#[test]
fn run_summary_reports_success_rate_without_weakening_evaluation_passed() {
    let result = parse_result(&valid_result()).expect("parse result");

    assert_eq!(result.summary.task_count, 1);
    assert_eq!(result.summary.scored_task_count, 1);
    assert_eq!(result.summary.agent_completed_count, 1);
    assert_eq!(result.summary.tests_passed_count, 0);
    assert_eq!(result.summary.evaluation_passed_count, 0);
    assert_eq!(result.summary.task_success_rate_basis_points, 0);
    assert!(!result.summary.meets_core_task_success_threshold);
    assert!(!result.evaluation_passed);

    let mut forged = valid_result();
    forged["summary"]["task_success_rate_basis_points"] = json!(10_000);
    forged["summary"]["meets_core_task_success_threshold"] = json!(true);
    let error = parse_result(&forged).expect_err("summary must be derived from task results");
    assert!(error.to_string().contains("summary"));
}

#[test]
fn task_capabilities_are_required_unique_and_evaluator_owned() {
    let manifest = parse_manifest(&valid_manifest()).expect("parse manifest");
    assert!(
        manifest.task_set().tasks[0]
            .capabilities
            .contains(&singularity_evaluation::EvaluationCapability::RequiredVerification)
    );
    let projection = serde_json::to_value(manifest.task_set().tasks[0].agent_projection())
        .expect("serialize projection");
    assert!(projection.get("capabilities").is_none());

    let mut missing = valid_manifest();
    missing["tasks"][0]
        .as_object_mut()
        .expect("task object")
        .remove("capabilities");
    assert!(parse_manifest(&missing).is_err());

    let mut duplicate = valid_manifest();
    duplicate["tasks"][0]["capabilities"] = json!(["python", "python"]);
    let error = parse_manifest(&duplicate).expect_err("duplicate capabilities fail closed");
    assert!(error.to_string().contains("duplicates"));
}

#[test]
fn manifest_path_resolution_uses_the_manifest_directory_not_process_cwd() {
    let temp = tempfile::tempdir().expect("create temp directory");
    let manifest_dir = temp.path().join("nested").join("manifests");
    std::fs::create_dir_all(&manifest_dir).expect("create manifest directory");
    let manifest_path = manifest_dir.join("task-set.json");
    let mut manifest = valid_manifest();
    manifest["tasks"][0]["workspace"]["source"] = json!({
        "type": "local",
        "path": "workspace/repo"
    });
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let loaded = EvaluationManifest::load(&manifest_path).expect("load manifest");
    let task_id = TaskId::new("sqlfluff__sqlfluff-2419").expect("valid task id");
    let plan = loaded.workspace_plan(&task_id).expect("build plan");
    let expected = std::fs::canonicalize(&manifest_dir)
        .expect("canonical manifest dir")
        .join("workspace")
        .join("repo");

    assert_eq!(
        plan.source,
        PlannedWorkspaceSource::Local { path: expected }
    );
}

#[test]
fn workspace_plan_source_is_remote_for_remote_git_tasks() {
    let manifest = parse_manifest(&valid_manifest()).expect("parse manifest");
    let task_id = TaskId::new("sqlfluff__sqlfluff-2419").expect("valid task id");
    let plan = manifest.workspace_plan(&task_id).expect("build plan");

    match plan.source {
        PlannedWorkspaceSource::RemoteGit { repository, commit } => {
            assert_eq!(
                repository.as_str(),
                "https://github.com/sqlfluff/sqlfluff.git"
            );
            assert_eq!(commit.as_str(), IMMUTABLE_COMMIT);
        }
        PlannedWorkspaceSource::Local { path } => {
            panic!("expected remote source, got {}", path.display());
        }
    }
}
