import json
from pathlib import Path

from singularity.planner import (
    ActionKind,
    PlannerRuntime,
    ReplanDecisionKind,
    RiskDecisionKind,
    TaskContract,
    TaskContractBuilder,
    TaskContractSchemaError,
    TaskStatus,
)
from singularity.tools.models import ToolExecutionBackendKind, ToolResult, ToolSpec, PermissionLevel
from pydantic import BaseModel
from singularity.workspace import CreateFile, MutationRuntime
from singularity.command import CommandRequest, CommandRuntime
from singularity.policy import PolicyConfig, PolicyRuntime, SecurityMode
from singularity.review import (
    ReviewDecision,
    ReviewDecisionAction,
    ReviewReport,
    ReviewStage,
    ReviewTarget,
)
from singularity.verification import VerificationRuntime


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
            ToolExecutionBackendKind.DELEGATED_EDIT_RUNTIME
            if name in {"write_file", "apply_patch", "edit_apply"}
            else ToolExecutionBackendKind.IN_PROCESS
        ),
        uses_edit_runtime=name in {"write_file", "apply_patch", "edit_apply"},
        uses_mutation_runtime=permission == PermissionLevel.WRITE,
        uses_command_runtime=permission == PermissionLevel.SHELL,
    )


def test_start_task_builds_state_plan_and_persists(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")

    state = planner.start_task("Add planner runtime")

    assert state.task_id == "task_1"
    assert state.session_id == "session_1"
    assert state.user_goal == "Add planner runtime"
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
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
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
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
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
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
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
    edit_denied = planner.authorize_tool_call(
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
    assert edit_denied.allowed is False
    assert edit_denied.error_code == "action_not_allowed"
    assert low_level_denied.allowed is False
    assert low_level_denied.error_code == "action_not_allowed"


def test_finalizing_phase_allows_read_only_evidence_tools(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
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
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
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
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
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
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Change code")
    planner.evidence.inspected_files.append("README.md")

    assessment = planner.assess_completion()

    assert assessment["status"] == TaskStatus.BLOCKED.value
    assert "required_changes_applied" in assessment["unmet"]
    assert planner.state.status != TaskStatus.COMPLETED


def test_final_report_is_generated_from_evidence(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
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


def test_final_review_rejects_before_completed(tmp_path: Path) -> None:
    class RejectingReviewRuntime:
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

    planner = PlannerRuntime(
        tmp_path,
        session_id="session_1",
        task_id="task_1",
        review_runtime=RejectingReviewRuntime(),
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
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Repair")

    assert planner.replan({"error_code": "patch_context_not_found"}).decision == ReplanDecisionKind.READ_FRESH_FILE
    assert planner.replan({"error_code": "snapshot_mismatch"}).decision == ReplanDecisionKind.READ_FRESH_FILE
    assert planner.replan({"verification_failed": True}).decision == ReplanDecisionKind.REPAIR_FAILURE

    planner.budget.repeated_failures["same"] = planner.budget.max_repeated_failures
    decision = planner.replan({"failure_fingerprint": "same"})
    assert decision.decision == ReplanDecisionKind.ASK_USER
    assert planner.state.status == TaskStatus.BLOCKED


def test_risk_escalation_requires_review_for_high_risk_actions(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
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
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Resume")
    planner.evidence.inspected_files.append("README.md")
    planner.interrupt("pause")

    resumed = PlannerRuntime(tmp_path).resume("session_1")

    assert resumed.state.status == TaskStatus.RECOVERING
    assert resumed.evidence.inspected_files == ["README.md"]

    conflicted = resumed.resume(
        "session_1",
        workspace_health={"status": "conflicted", "external_changes": ["README.md"]},
    )
    assert conflicted.state.status == TaskStatus.NEEDS_REVIEW
    assert "workspace conflict on resume" in conflicted.state.blocked_reasons


def test_planner_trace_records_phase_action_budget_and_risk(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
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


def test_mutation_runtime_observer_updates_planner_with_rich_result(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Create file")
    runtime = MutationRuntime(tmp_path, planner=planner)

    result = runtime.apply_operations(
        [CreateFile(path="app.py", content="print('ok')\n")],
        intent="create app",
        created_by="test",
        tool_call_id="call_mutate",
    )

    assert result.ok is True
    assert planner.evidence.applied_changes[0]["changed_files"] == ["app.py"]
    assert planner.state.linked_transactions == [result.transaction_id]


def test_command_runtime_observer_updates_planner_with_rich_result(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Run command")
    runtime = CommandRuntime(
        tmp_path,
        planner=planner,
        policy_runtime=PolicyRuntime(
            PolicyConfig(workspace_root=tmp_path, security_mode=SecurityMode.COMPAT)
        ),
    )

    result = runtime.run(CommandRequest(argv=["python", "-c", "print('ok')"]), tool_call_id="call_cmd")

    assert result.command_id in planner.state.linked_commands
    assert planner.evidence.command_results[0]["command_id"] == result.command_id


def test_verification_runtime_observer_updates_planner_with_assessment(tmp_path: Path) -> None:
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
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Verify")
    runtime = VerificationRuntime(tmp_path, planner=planner)

    plan = runtime.plan_verification(changed_files=["tests/test_sample.py"], task_intent="tests")
    runtime.run_plan(plan.id)

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

    planner = PlannerRuntime(
        tmp_path,
        session_id="session_1",
        task_id="task_1",
        project_index_runtime=FakeProjectIndex(),
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

    planner = PlannerRuntime(
        tmp_path,
        session_id="session_1",
        task_id="task_1",
        project_index_runtime=FakeProjectIndex(),
    )
    planner.start_task("Change app")

    planner.record_diff_observation({"changed_files": ["src/app.py"]})

    latest = planner.evidence.retrieval_results[-1]
    assert latest["trigger"] == "diff_observation"
    assert latest["files_to_read"] == ["src/app.py", "src/caller.py", "tests/test_app.py"]
    assert latest["evidence_sources"] == ["project_index:impact", "project_index:test_impact"]


def test_lesson_extraction_only_ingests_verified_completed_report(tmp_path: Path) -> None:
    class FakeMemoryRuntime:
        def __init__(self) -> None:
            self.final_reports = []

        def ingest_final_report(self, final_report, *, accept: bool = False):
            self.final_reports.append((final_report, accept))
            return [{"candidate_id": "cand_verified"}]

    memory = FakeMemoryRuntime()
    planner = PlannerRuntime(
        tmp_path,
        session_id="session_1",
        task_id="task_1",
        memory_runtime=memory,
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
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
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
