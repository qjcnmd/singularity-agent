import json
from pathlib import Path

from pydantic import BaseModel

from singularity.command import CommandExecutor, CommandRequest
from singularity.planner import (
    ActionKind,
    FinalReport,
    FinalReportRenderer,
    Planner,
    ReplanDecisionKind,
    RiskDecisionKind,
    TaskContract,
    TaskContractBuilder,
    TaskContractSchemaError,
    TaskStatus,
)
from singularity.policy import PolicyConfig, PolicyEngine
from singularity.policy.permissions import PermissionProfile, PermissionProfileName
from singularity.review import (
    ReviewDecision,
    ReviewDecisionAction,
    ReviewReport,
    ReviewStage,
    ReviewTarget,
)
from singularity.tools.models import PermissionLevel, ToolExecutionBackendKind, ToolResult, ToolSpec
from singularity.verification import VerificationRunner
from singularity.workspace import CreateFile, WorkspaceMutationManager


class EmptyInput(BaseModel):
    pass


def spec(name: str, *, permission: PermissionLevel = PermissionLevel.READ_ONLY) -> ToolSpec:
    return ToolSpec(
        name=name,
        version="test",
        description=name,
        input_model=EmptyInput,
        handler=lambda _args: {"ok": True},
        permission_level=permission,
        execution_backend=(
            ToolExecutionBackendKind.DELEGATED_EDIT_EXECUTOR
            if name in {"write_file", "apply_patch", "edit_apply"}
            else ToolExecutionBackendKind.IN_PROCESS
        ),
        uses_edit_executor=name in {"write_file", "apply_patch", "edit_apply"},
        uses_mutation_manager=permission == PermissionLevel.WRITE,
        uses_command_executor=permission == PermissionLevel.SHELL,
    )


def test_start_task_builds_state_plan_and_persists(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")

    state = planner.start_task("Add planner component")

    assert state.task_id == "task_1"
    assert state.session_id == "session_1"
    assert state.user_goal == "Add planner component"
    assert state.status == TaskStatus.UNDERSTANDING_TASK
    assert planner.plan is not None
    assert [phase.phase_id for phase in planner.plan.phases] == [
        "understanding_task",
        "inspecting_workspace",
        "planning_changes",
        "applying_changes",
        "running_verification",
        "repairing_failures",
        "finalizing",
    ]
    assert (tmp_path / ".singularity" / "planner" / "session_1" / "state.json").exists()
    assert (tmp_path / ".singularity" / "planner" / "session_1" / "planner_events.jsonl").exists()


def test_benchmark_constraints_limit_tool_exposure_and_authorization(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.apply_benchmark_constraints(
        {
            "task_id": "bench.task",
            "allowed_tools": ["read_file"],
            "expected_file_changes": ["app.py"],
            "completion_standard": "Only read during this benchmark step.",
            "risk_tags": ["policy-blocked"],
        }
    )
    state = planner.start_task("Exercise benchmark constraints")
    state.current_phase = "applying_changes"

    tools = [
        {"function": {"name": "read_file"}},
        {"function": {"name": "write_file"}},
    ]

    assert planner.filtered_tools(tools) == [{"function": {"name": "read_file"}}]
    decision = planner.authorize_tool_call(
        tool_name="write_file",
        tool_call_id="call_write",
        spec=spec("write_file", permission=PermissionLevel.WRITE),
        arguments={"path": "app.py"},
    )

    assert decision.allowed is False
    assert decision.error_code == "benchmark_tool_not_allowed"
    assert state.task_contract["benchmark_constraints"]["allowed_tools"] == ["read_file"]


def test_benchmark_expected_file_changes_delay_verification_phase(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.apply_benchmark_constraints(
        {
            "task_id": "bench.multi_file",
            "allowed_tools": ["read_file", "write_file", "run_verification"],
            "expected_file_changes": ["cart.py", "policy.py"],
            "verification_command": "python -m pytest tests/test_shipping.py",
        }
    )
    state = planner.start_task("Update policy and cart")
    state.status = TaskStatus.APPLYING_CHANGES
    state.current_phase = "applying_changes"
    planner.plan.current_phase = "applying_changes"

    planner.update_from_tool_result(
        tool_call_id="call_policy",
        tool_name="write_file",
        result={
            "ok": True,
            "content": {
                "mutation_status": "applied",
                "transaction_id": "tx_policy",
                "changed_files": ["policy.py"],
            },
        },
    )

    assert planner.state.current_phase == "applying_changes"
    assessment = planner.assess_completion(mark_blocked=False)
    assert "benchmark_expected_file_changes" in assessment["unmet"]

    planner.update_from_tool_result(
        tool_call_id="call_cart",
        tool_name="write_file",
        result={
            "ok": True,
            "content": {
                "mutation_status": "applied",
                "transaction_id": "tx_cart",
                "changed_files": ["cart.py"],
            },
        },
    )

    assert planner.state.current_phase == "running_verification"


def test_sandbox_required_policy_observation_is_not_unresolved_failure(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Run verification")

    planner.record_policy_observation(
        {
            "outcome": "sandbox_required",
            "component": "verification",
            "operation": "run_verification",
            "reason": "Verification command execution requires an isolated sandbox.",
            "risk_level": "medium",
            "resource": "python test.py",
            "decision_id": "decision_1",
        }
    )

    assert planner.evidence.policy_observations
    assert planner.evidence.unresolved_failures == []
    assert planner.state.blocked_reasons == []


def test_task_contract_builder_extracts_create_file_smoke_contract() -> None:
    contract = TaskContractBuilder().build("Create quicksort.py and run smoke verification")

    assert contract.deliverables[0].path == "quicksort.py"
    assert contract.acceptance_criteria[0].criterion_id == "deliver_quicksort_py"
    assert contract.acceptance_criteria[1].criterion_id == "verify_quicksort_py"
    assert contract.smoke_commands() == [["python", "quicksort.py"]]


def test_task_contract_builder_records_report_obligations() -> None:
    contract = TaskContractBuilder().build("生成一份实验报告，包含修改、验证和风险")

    assert contract.deliverables[0].kind == "report"
    assert contract.report_requirements
    assert {"goal", "requirements", "changes", "verification", "risks"} <= set(
        contract.report_requirements[0].sections
    )


def test_task_contract_accepts_model_structured_output() -> None:
    contract = TaskContractBuilder().build(
        "fallback",
        structured_output={
            "user_goal": "model goal",
            "acceptance_criteria": [
                {
                    "criterion_id": "model_criterion",
                    "description": "model criterion",
                    "evidence": ["inspected_files"],
                }
            ],
            "deliverables": [{"kind": "file", "description": "file", "path": "a.py"}],
        },
    )

    assert contract.source == "model"
    assert contract.user_goal == "model goal"
    assert contract.acceptance_criteria[0].criterion_id == "model_criterion"


def test_task_contract_schema_validation_rejects_unverifiable_required_criteria() -> None:
    try:
        TaskContract.validate_payload(
            {
                "user_goal": "model goal",
                "acceptance_criteria": [
                    {
                        "criterion_id": "bad",
                        "description": "missing evidence",
                        "evidence": [],
                    }
                ],
            }
        )
    except TaskContractSchemaError:
        pass
    else:
        raise AssertionError("invalid contract schema should fail validation")

    fallback = TaskContractBuilder().build(
        "Create fallback.py and run smoke verification",
        structured_output={
            "acceptance_criteria": [
                {
                    "criterion_id": "bad",
                    "description": "missing evidence",
                    "evidence": [],
                }
            ]
        },
    )

    assert fallback.source == "rules"
    assert fallback.deliverables[0].path == "fallback.py"
    assert fallback.smoke_commands() == [["python", "fallback.py"]]


def test_planner_injects_contract_and_completion_reports_missing_smoke_evidence(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Create quicksort.py and run smoke verification")
    planner.evidence.inspected_files.append("README.md")
    planner.evidence.applied_changes.append(
        {"changed_files": ["quicksort.py"], "transaction_id": "tx_1"}
    )

    context = json.loads(planner.planner_context_message()["content"])["planner"]
    assessment = planner.assess_completion(mark_blocked=False)

    assert context["task_contract"]["deliverables"][0]["path"] == "quicksort.py"
    assert planner.contract_smoke_commands() == [["python", "quicksort.py"]]
    assert "contract:verify_quicksort_py" in assessment["unmet"]
    assert assessment["criteria"]["deliver_quicksort_py"]["satisfied"] is True
    assert assessment["criteria"]["verify_quicksort_py"]["satisfied"] is False

    planner.evidence.verification_results.append(
        {
            "completion_assessment": {"status": "ready"},
            "check_status": [{"check_id": "check_1", "status": "passed"}],
        }
    )
    planner.state.final_assessment = {"status": "ready"}

    ready = planner.assess_completion(mark_blocked=False)

    assert "contract:verify_quicksort_py" not in ready["unmet"]
    assert ready["criteria"]["verify_quicksort_py"]["satisfied"] is True


def test_phase_policy_allows_read_tools_before_mutation_and_blocks_write(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Inspect first")
    planner.state.status = TaskStatus.INSPECTING_WORKSPACE
    planner.state.current_phase = "inspecting_workspace"

    allowed = planner.authorize_tool_call(
        tool_name="read_file",
        tool_call_id="call_read",
        spec=spec("read_file"),
        arguments={"path": "README.md"},
    )
    denied = planner.authorize_tool_call(
        tool_name="workspace_create_file",
        tool_call_id="call_write",
        spec=spec("workspace_create_file", permission=PermissionLevel.WRITE),
        arguments={"path": "x.txt"},
    )

    assert allowed.allowed is True
    assert allowed.action is not None
    assert allowed.action.kind == ActionKind.READ_RELEVANT_FILES
    assert denied.allowed is False
    assert denied.error_code == "action_not_allowed"


def test_applying_phase_defaults_to_facades_not_low_level_workspace_tools(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Change code")
    planner.state.status = TaskStatus.APPLYING_CHANGES
    planner.state.current_phase = "applying_changes"

    write_allowed = planner.authorize_tool_call(
        tool_name="write_file",
        tool_call_id="call_write",
        spec=spec("write_file", permission=PermissionLevel.WRITE),
        arguments={"path": "x.txt", "content": "x\n", "mode": "create"},
    )
    patch_allowed = planner.authorize_tool_call(
        tool_name="apply_patch",
        tool_call_id="call_patch",
        spec=spec("apply_patch", permission=PermissionLevel.WRITE),
        arguments={"patch": "--- /dev/null\n+++ b/x.txt\n@@ -0,0 +1 @@\n+x\n"},
    )
    edit_allowed = planner.authorize_tool_call(
        tool_name="edit_apply",
        tool_call_id="call_edit",
        spec=spec("edit_apply", permission=PermissionLevel.WRITE),
        arguments={"summary": "create", "operations": []},
    )
    low_level_denied = planner.authorize_tool_call(
        tool_name="workspace_create_file",
        tool_call_id="call_workspace",
        spec=spec("workspace_create_file", permission=PermissionLevel.WRITE),
        arguments={"path": "x.txt"},
    )

    assert write_allowed.allowed is True
    assert write_allowed.action.kind == ActionKind.APPLY_MUTATION
    assert patch_allowed.allowed is True
    assert patch_allowed.action.kind == ActionKind.APPLY_MUTATION
    assert edit_allowed.allowed is True
    assert edit_allowed.action.kind == ActionKind.APPLY_MUTATION
    assert low_level_denied.allowed is False
    assert low_level_denied.error_code == "action_not_allowed"


def test_user_write_constraint_blocks_tests_paths_at_authorization(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Change code")
    planner.state.status = TaskStatus.APPLYING_CHANGES
    planner.state.current_phase = "applying_changes"
    planner.state.constraints.append("不要修改 tests/")

    source_allowed = planner.authorize_tool_call(
        tool_name="write_file",
        tool_call_id="call_src",
        spec=spec("write_file", permission=PermissionLevel.WRITE),
        arguments={"path": "src/app.py", "content": "x\n", "mode": "create"},
    )
    tests_write_denied = planner.authorize_tool_call(
        tool_name="write_file",
        tool_call_id="call_tests",
        spec=spec("write_file", permission=PermissionLevel.WRITE),
        arguments={"path": "tests/test_sample.py", "content": "x\n", "mode": "create"},
    )
    tests_patch_denied = planner.authorize_tool_call(
        tool_name="apply_patch",
        tool_call_id="call_patch",
        spec=spec("apply_patch", permission=PermissionLevel.WRITE),
        arguments={"patch": "--- a/tests/test_sample.py\n+++ b/tests/test_sample.py\n@@ -1 +1 @@\n-old\n+new\n"},
    )

    assert source_allowed.allowed is True
    assert tests_write_denied.allowed is False
    assert tests_write_denied.error_code == "user_constraint_blocks_write_path"
    assert tests_patch_denied.allowed is False
    assert tests_patch_denied.error_code == "user_constraint_blocks_write_path"


def test_finalizing_phase_allows_read_only_evidence_tools(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Finalize report")
    planner.state.status = TaskStatus.FINALIZING
    planner.state.current_phase = "finalizing"

    read_allowed = planner.authorize_tool_call(
        tool_name="read_file",
        tool_call_id="call_read",
        spec=spec("read_file"),
        arguments={"path": "quicksort.py"},
    )
    verification_result_allowed = planner.authorize_tool_call(
        tool_name="get_verification_result",
        tool_call_id="call_verify_result",
        spec=spec("get_verification_result"),
        arguments={},
    )
    write_denied = planner.authorize_tool_call(
        tool_name="write_file",
        tool_call_id="call_write",
        spec=spec("write_file", permission=PermissionLevel.WRITE),
        arguments={"path": "quicksort.py", "content": "x", "mode": "overwrite"},
    )

    assert read_allowed.allowed is True
    assert read_allowed.action is not None
    assert read_allowed.action.kind == ActionKind.READ_RELEVANT_FILES
    assert verification_result_allowed.allowed is True
    assert verification_result_allowed.action is not None
    assert verification_result_allowed.action.kind == ActionKind.ANALYZE_ISSUE
    assert write_denied.allowed is False
    assert write_denied.error_code == "action_not_allowed"


def test_tool_result_updates_evidence_ledger_and_advances_phase(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Read README")
    planner.state.status = TaskStatus.INSPECTING_WORKSPACE
    planner.state.current_phase = "inspecting_workspace"
    decision = planner.authorize_tool_call(
        tool_name="read_file",
        tool_call_id="call_read",
        spec=spec("read_file"),
        arguments={"path": "README.md"},
    )

    planner.update_from_tool_result(
        tool_call_id="call_read",
        tool_name="read_file",
        result=ToolResult.success(
            content={"path": "README.md", "content": "hello", "bytes_read": 5}
        ),
        action_id=decision.action.action_id,
    )

    assert planner.evidence.inspected_files == ["README.md"]
    assert planner.state.current_phase == "planning_changes"
    assert planner.state.status == TaskStatus.PLANNING_CHANGES


def test_mutation_command_and_verification_results_update_evidence(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Change code")

    planner.update_from_mutation(
        {
            "mutation_status": "applied",
            "changed_files": ["src/app.py"],
            "changeset_id": "change_1",
            "transaction_id": "tx_1",
        },
        tool_call_id="call_mutate",
    )
    planner.update_from_command(
        {
            "command_result": {
                "command_id": "cmd_1",
                "semantic_status": "succeeded",
                "changed_files": [],
            }
        },
        tool_call_id="call_cmd",
    )
    planner.update_from_verification(
        {
            "verification": {
                "completion_assessment": {
                    "status": "ready",
                    "warnings": [],
                    "remaining_risks": [],
                },
                "check_status": [
                    {"check_id": "check_1", "kind": "unit_test", "status": "passed"}
                ],
            }
        },
        tool_call_id="call_verify",
    )

    assert planner.evidence.applied_changes[0]["transaction_id"] == "tx_1"
    assert planner.state.linked_transactions == ["tx_1"]
    assert planner.state.linked_commands == ["cmd_1"]
    assert planner.state.linked_verifications == ["check_1"]
    assert planner.state.final_assessment["status"] == "ready"


def test_completion_requires_evidence_before_completed(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Change code")
    planner.evidence.inspected_files.append("README.md")

    assessment = planner.assess_completion()

    assert assessment["status"] == TaskStatus.BLOCKED.value
    assert "required_changes_applied" in assessment["unmet"]
    assert planner.state.status != TaskStatus.COMPLETED


def test_final_report_is_generated_from_evidence(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Change code")
    planner.evidence.inspected_files.append("README.md")
    planner.evidence.applied_changes.append(
        {"changed_files": ["README.md"], "transaction_id": "tx_1"}
    )
    planner.state.linked_transactions.append("tx_1")
    planner.evidence.verification_results.append(
        {
            "completion_assessment": {
                "status": "ready",
                "warnings": [],
                "remaining_risks": [],
            },
            "check_status": [{"check_id": "check_1", "status": "passed"}],
        }
    )
    planner.state.final_assessment = {"status": "ready"}

    report = planner.finalize()

    assert report.status == TaskStatus.COMPLETED
    assert report.files_changed == ["README.md"]
    assert report.verification_summary["status"] == "ready"
    assert (tmp_path / ".singularity" / "planner" / "session_1" / "final_report.json").exists()
    markdown_artifacts = [artifact for artifact in report.artifacts if artifact.endswith("final_report.md")]
    assert markdown_artifacts
    markdown = (tmp_path / markdown_artifacts[0]).read_text(encoding="utf-8")
    assert "# Final Report" in markdown
    assert "## Objective" in markdown
    assert "## Implementation" in markdown
    assert "## Verification" in markdown
    assert "## Results" in markdown
    assert "## Final Review" in markdown
    assert "## Risks and Next Steps" in markdown
    assert "## Evidence Appendix" in markdown


def test_final_report_markdown_includes_context_usage_diagnostic(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Inspect context")
    report = FinalReport(
        user_goal="Inspect context",
        status=TaskStatus.COMPLETED,
        files_changed=[],
        agent_changes=[],
        command_side_effects=[],
        verification_summary={"status": "ready"},
        unresolved_issues=[],
        risks=[],
        rollback_status={},
        policy_approval_summary={},
        artifacts=[],
        next_steps=[],
        context_usage_diagnostic={
            "layer_token_usage": {"recent_dialogue": 12},
            "included_item_ids": ["included_1"],
            "excluded_item_ids": ["excluded_1"],
            "stale_item_ids": ["stale_1"],
            "summary_item_ids": ["summary_1"],
            "recent_tail_item_ids": ["tail_1"],
            "cache_hit_ratio": 0.25,
            "cache_attribution": {"source": "component_inferred"},
            "cache_miss_reasons": ["context_shape_change"],
        },
    )

    markdown = FinalReportRenderer().render_markdown(
        report=report,
        state=planner.state,
        evidence=planner.evidence,
    )

    assert "## Context Usage" in markdown
    assert "Layer token usage" in markdown
    assert "Included items: 1" in markdown
    assert "Excluded items: 1" in markdown
    assert "Stale items: 1" in markdown
    assert "Summary items: 1" in markdown
    assert "Recent tail items: 1" in markdown
    assert "Cache hit ratio: 0.25" in markdown
    assert "Cache attribution source: component_inferred" in markdown
    assert "context_shape_change" in markdown


def test_final_review_rejects_before_completed(tmp_path: Path) -> None:
    class RejectingReviewPipeline:
        def final_review(self, **_kwargs: object) -> ReviewReport:
            return ReviewReport(
                target=ReviewTarget(stage=ReviewStage.FINAL, task_id="task_1"),
                input_summary="final review rejected",
                decision=ReviewDecision(
                    action=ReviewDecisionAction.REPAIR,
                    reasons=["Report evidence is incomplete."],
                    repair_targets=["check_1"],
                ),
            )

    planner = Planner(
        tmp_path,
        session_id="session_1",
        task_id="task_1",
        review_pipeline=RejectingReviewPipeline(),
    )
    planner.start_task("Change code")
    planner.evidence.inspected_files.append("README.md")
    planner.evidence.applied_changes.append(
        {"changed_files": ["README.md"], "transaction_id": "tx_1"}
    )
    planner.evidence.verification_results.append(
        {
            "completion_assessment": {"status": "ready", "warnings": [], "remaining_risks": []},
            "check_status": [{"check_id": "check_1", "status": "passed"}],
        }
    )
    planner.state.final_assessment = {"status": "ready"}

    report = planner.finalize()

    assert report.status != TaskStatus.COMPLETED
    assert planner.state.status == TaskStatus.REPAIRING_FAILURES
    assert planner.evidence.review_results[-1]["decision"]["route"] == "repair"


def test_replanner_maps_required_failures(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Repair")

    assert planner.replan({"error_code": "patch_context_not_found"}).decision == ReplanDecisionKind.READ_FRESH_FILE
    assert planner.replan({"error_code": "snapshot_mismatch"}).decision == ReplanDecisionKind.READ_FRESH_FILE
    assert planner.replan({"verification_failed": True}).decision == ReplanDecisionKind.REPAIR_FAILURE

    planner.budget.repeated_failures["same"] = planner.budget.max_repeated_failures
    decision = planner.replan({"failure_fingerprint": "same"})
    assert decision.decision == ReplanDecisionKind.ASK_USER
    assert planner.state.status == TaskStatus.BLOCKED


def test_risk_escalation_requires_review_for_high_risk_actions(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Change CI")
    planner.state.status = TaskStatus.APPLYING_CHANGES
    planner.state.current_phase = "applying_changes"

    decision = planner.authorize_tool_call(
        tool_name="write_file",
        tool_call_id="call_ci",
        spec=spec("write_file", permission=PermissionLevel.WRITE),
        arguments={
            "path": ".github/workflows/ci.yml",
            "content": "name: ci\n",
            "mode": "create",
        },
    )

    assert decision.allowed is False
    assert decision.risk_decision == RiskDecisionKind.REQUIRE_REVIEW
    assert planner.state.status == TaskStatus.NEEDS_REVIEW


def test_interrupt_and_resume_restore_state_and_health_conflict(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Resume")
    planner.evidence.inspected_files.append("README.md")
    planner.interrupt("pause")

    resumed = Planner(tmp_path).resume("session_1")

    assert resumed.state.status == TaskStatus.RECOVERING
    assert resumed.evidence.inspected_files == ["README.md"]

    conflicted = resumed.resume(
        "session_1",
        workspace_health={"status": "conflicted", "external_changes": ["README.md"]},
    )
    assert conflicted.state.status == TaskStatus.NEEDS_REVIEW
    assert "workspace conflict on resume" in conflicted.state.blocked_reasons


def test_planner_trace_records_phase_action_budget_and_risk(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Trace")
    planner.step()

    events = [
        json.loads(line)
        for line in (tmp_path / ".singularity" / "planner" / "session_1" / "planner_events.jsonl")
        .read_text(encoding="utf-8")
        .splitlines()
    ]
    last = events[-1]

    assert last["event"] == "planner"
    assert last["task_id"] == "task_1"
    assert "phase" in last
    assert "budget_state" in last
    assert "risk_level" in last


def test_mutation_manager_observer_updates_planner_with_rich_result(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Create file")
    component = WorkspaceMutationManager(tmp_path, planner=planner)

    result = component.apply_operations(
        [CreateFile(path="app.py", content="print('ok')\n")],
        intent="create app",
        created_by="test",
        tool_call_id="call_mutate",
    )

    assert result.ok is True
    assert planner.evidence.applied_changes[0]["changed_files"] == ["app.py"]
    assert planner.state.linked_transactions == [result.transaction_id]


def test_command_executor_observer_updates_planner_with_rich_result(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Run command")
    component = CommandExecutor(
        tmp_path,
        planner=planner,
        policy_engine=PolicyEngine(
            PolicyConfig(
                workspace_root=tmp_path,
                permission_profile=PermissionProfile.default_for_workspace(
                    tmp_path,
                    profile=PermissionProfileName.DANGER_FULL_ACCESS,
                ),
            )
        ),
    )

    result = component.run(CommandRequest(argv=["python", "-c", "print('ok')"]), tool_call_id="call_cmd")

    assert result.command_id in planner.state.linked_commands
    assert planner.evidence.command_results[0]["command_id"] == result.command_id


def test_verification_runner_observer_updates_planner_with_assessment(tmp_path: Path) -> None:
    (tmp_path / "pyproject.toml").write_text(
        """
[project]
name = "sample"

[tool.pytest.ini_options]
testpaths = ["tests"]
""",
        encoding="utf-8",
    )
    tests_dir = tmp_path / "tests"
    tests_dir.mkdir()
    (tests_dir / "test_sample.py").write_text("def test_ok():\n    assert True\n", encoding="utf-8")
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Verify")
    component = VerificationRunner(tmp_path, planner=planner)

    plan = component.plan_verification(changed_files=["tests/test_sample.py"], task_intent="tests")
    component.run_plan(plan.id)

    assert planner.evidence.verification_results
    assert planner.state.final_assessment["status"] in {
        "ready",
        "ready_with_warnings",
        "blocked",
        "failed",
        "needs_review",
    }


def test_verification_failure_records_dynamic_retrieval_context(tmp_path: Path) -> None:
    class FakeProjectIndex:
        def get_test_impact(self, changed_files):
            return {
                "changed_files": list(changed_files),
                "likely_tests": ["tests/test_app.py"],
                "commands": ["python -m pytest tests/test_app.py"],
            }

    planner = Planner(
        tmp_path,
        session_id="session_1",
        task_id="task_1",
        project_index=FakeProjectIndex(),
    )
    planner.start_task("Fix failing app test")
    planner.update_from_mutation(
        {
            "mutation_status": "applied",
            "changed_files": ["src/app.py"],
            "transaction_id": "tx_1",
        }
    )

    planner.update_from_verification(
        {
            "verification": {
                "completion_assessment": {"status": "failed"},
                "failure_analysis": [
                    {
                        "analysis_id": "failure_1",
                        "check_id": "check_pytest",
                        "failure_type": "unit_test_failure",
                        "suspect_files": ["tests/test_app.py"],
                        "retrieval_queries": ["tests/test_app.py", "AssertionError"],
                    }
                ],
                "check_status": [{"check_id": "check_pytest", "status": "failed"}],
            }
        }
    )

    latest = planner.evidence.retrieval_results[-1]
    assert latest["trigger"] == "verification_failure"
    assert latest["files_to_read"] == ["src/app.py", "tests/test_app.py"]
    assert "AssertionError" in latest["index_queries"]
    context = json.loads(planner.planner_context_message()["content"])["planner"]
    assert context["evidence"]["dynamic_retrieval"]["files_to_read"] == [
        "src/app.py",
        "tests/test_app.py",
    ]


def test_diff_observation_records_project_index_retrieval(tmp_path: Path) -> None:
    class FakeProjectIndex:
        def analyze_impact(self, changed_files):
            return {
                "requested_paths": list(changed_files),
                "reverse_dependencies": ["src/caller.py"],
                "affected_tests": ["tests/test_app.py"],
            }

        def get_test_impact(self, changed_files):
            return {"changed_files": list(changed_files), "likely_tests": ["tests/test_app.py"]}

    planner = Planner(
        tmp_path,
        session_id="session_1",
        task_id="task_1",
        project_index=FakeProjectIndex(),
    )
    planner.start_task("Change app")

    planner.record_diff_observation({"changed_files": ["src/app.py"]})

    latest = planner.evidence.retrieval_results[-1]
    assert latest["trigger"] == "diff_observation"
    assert latest["files_to_read"] == ["src/app.py", "src/caller.py", "tests/test_app.py"]
    assert latest["evidence_sources"] == ["project_index:impact", "project_index:test_impact"]


def test_lesson_extraction_only_ingests_verified_completed_report(tmp_path: Path) -> None:
    class FakeMemoryLearningPipeline:
        def __init__(self) -> None:
            self.final_reports = []

        def ingest_final_report(self, final_report, *, accept: bool = False):
            self.final_reports.append((final_report, accept))
            return [{"candidate_id": "cand_verified"}]

    memory = FakeMemoryLearningPipeline()
    planner = Planner(
        tmp_path,
        session_id="session_1",
        task_id="task_1",
        memory_pipeline=memory,
    )
    planner.start_task("Change code")

    failed = {
        "status": "failed",
        "verification_summary": {"status": "failed"},
        "files_changed": ["src/app.py"],
    }
    assert planner.extract_lessons(failed) == []
    assert memory.final_reports == []

    completed = {
        "status": "completed",
        "verification_summary": {
            "status": "ready",
            "check_status": [{"check_id": "check_1", "status": "passed"}],
        },
        "files_changed": ["src/app.py"],
    }

    assert planner.extract_lessons(completed) == [{"candidate_id": "cand_verified"}]
    assert memory.final_reports == [(completed, False)]


def test_planner_records_review_observation_and_routes_decision(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Review")

    planner.record_review_observation(
        {
            "review_id": "review_1",
            "target": {"stage": "post_verification"},
            "findings": [
                {
                    "finding_id": "finding_1",
                    "title": "Verification failed",
                    "severity": "error",
                    "category": "bug_risk",
                    "blocking": True,
                }
            ],
            "decision": {
                "action": "repair",
                "reasons": ["Verification failed"],
                "repair_targets": ["check_1"],
                "replan_signal": {"verification_failed": True},
            },
        }
    )

    assert planner.evidence.review_results[-1]["review_id"] == "review_1"
    assert planner.state.status == TaskStatus.REPAIRING_FAILURES
    assert planner.state.current_phase == "repairing_failures"

    planner.record_review_observation(
        {
            "review_id": "review_2",
            "target": {"stage": "pre_edit"},
            "findings": [],
            "decision": {"action": "needs_human_approval", "reasons": ["policy"]},
        }
    )

    assert planner.state.status == TaskStatus.NEEDS_REVIEW


def test_planner_store_atomic_write_does_not_corrupt_existing_file(tmp_path: Path) -> None:
    from singularity.planner.store import PlannerStore

    store = PlannerStore(tmp_path)
    target = store.session_dir("session_atomic") / "state.json"
    original_payload = {"session_id": "session_atomic", "version": 1, "task_id": "task_1"}
    store._write_json(target, original_payload)
    assert target.exists()

    # Simulate an interrupted write: a leftover temp file should not corrupt the target.
    temp_path = target.with_name(f".{target.name}.stale.tmp")
    temp_path.write_text("PARTIAL_CORRUPT", encoding="utf-8")

    # The original file must still be readable and intact.
    loaded = store._read_json(target)
    assert loaded == original_payload

    # A fresh atomic write replaces the file completely; no partial content remains.
    new_payload = {"session_id": "session_atomic", "version": 2, "task_id": "task_2"}
    store._write_json(target, new_payload)
    loaded = store._read_json(target)
    assert loaded == new_payload
    assert "PARTIAL_CORRUPT" not in target.read_text(encoding="utf-8")


def test_planner_store_concurrent_append_events_do_not_interleave(tmp_path: Path) -> None:
    import threading

    from singularity.planner.store import PlannerStore

    store = PlannerStore(tmp_path)
    session_id = "session_concurrent"
    events_path = store.session_dir(session_id) / "planner_events.jsonl"

    errors: list[BaseException] = []

    def appender(thread_index: int) -> None:
        try:
            for index in range(25):
                store.append_event(
                    session_id,
                    task_id=f"task_{thread_index}",
                    phase="inspecting_workspace",
                    action_id=f"action_{thread_index}_{index}",
                    extra={"thread_index": thread_index, "index": index},
                )
        except BaseException as exc:
            errors.append(exc)

    threads = [threading.Thread(target=appender, args=(i,)) for i in range(4)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert errors == []
    lines = [line for line in events_path.read_text(encoding="utf-8").splitlines() if line.strip()]
    # Each append writes exactly one full line; no partial/interleaved lines.
    assert len(lines) == 100
    parsed = [json.loads(line) for line in lines]
    action_ids = [entry["action_id"] for entry in parsed]
    assert len(set(action_ids)) == 100
    for entry in parsed:
        assert entry["event"] == "planner"
        assert entry["session_id"] == session_id


def test_planner_store_save_is_atomic_across_files(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_save_atomic", task_id="task_save")
    planner.start_task("Atomic save task")

    # All four files should be present and valid after save (atomic per file).
    session_dir = tmp_path / ".singularity" / "planner" / "session_save_atomic"
    for filename in ("state.json", "plan.json", "evidence.json", "budget.json"):
        path = session_dir / filename
        assert path.exists()
        payload = json.loads(path.read_text(encoding="utf-8"))
        assert isinstance(payload, dict)
        # No leftover temp files after a successful save.
        siblings = [child.name for child in session_dir.iterdir() if child.name.startswith(f".{filename}.")]
        assert siblings == []
