"""Tests for the repair contract / verification contract subsystem.

Covers structured VerificationContract, ContractSatisfaction, edge cases for
invalid contracts, policy-blocked categories, low confidence, step-level
evidence backfill, and Planner authorization of verification commands.
"""
from __future__ import annotations

import shlex
from pathlib import Path
from typing import Any

from pydantic import BaseModel

from singularity.command import CommandRequest, SemanticStatus
from singularity.failure_analysis import (
    BLOCKED_FAILURE_CATEGORIES,
    MIN_REPAIR_CONFIDENCE,
    ContractSatisfaction,
    FailureAnalysisRequest,
    FailureAnalysisResult,
    RepairActionCandidate,
    RepairContract,
    RepairPlan,
    RepairPlanner,
    RepairReplanSignal,
    StepEvidence,
    VerificationContract,
    VerificationStep,
)
from singularity.planner import Planner, TaskStatus
from singularity.planner.models import AuthorizationDecision, EvidenceLedger
from singularity.tools.models import ToolSpec
from singularity.verification import VerificationRunner
from singularity.verification.models import CheckKind, VerificationCheck
from tests.test_verification_runner import FakeCommandExecutor, command_result


class _EmptyInput(BaseModel):
    pass


# ---------------------------------------------------------------------------
# VerificationStep / VerificationContract basics
# ---------------------------------------------------------------------------


class TestVerificationStep:
    def test_round_trip(self) -> None:
        step = VerificationStep(step_id="vstep_0", command="pytest tests/", kind="unit_test", required=True)
        payload = step.to_dict()
        restored = VerificationStep.from_dict(payload)
        assert restored == step

    def test_defaults(self) -> None:
        step = VerificationStep(step_id="s1", command="echo ok")
        assert step.kind == "smoke"
        assert step.required is True

    def test_command_argv(self) -> None:
        step = VerificationStep(step_id="s1", command="pytest tests/test_app.py -x")
        assert step.command_argv == ["pytest", "tests/test_app.py", "-x"]

    def test_matches_command_prefix(self) -> None:
        step = VerificationStep(step_id="s1", command="pytest tests/")
        assert step.matches_command(["pytest", "tests/"])
        assert step.matches_command(["pytest", "tests/", "-x", "--verbose"])
        assert not step.matches_command(["pytest"])
        assert not step.matches_command(["ruff", "check", "."])
        assert not step.matches_command([])

    def test_to_dict_includes_command_argv(self) -> None:
        step = VerificationStep(step_id="s1", command="pytest tests/")
        payload = step.to_dict()
        assert "command_argv" in payload
        assert payload["command_argv"] == ["pytest", "tests/"]


class TestVerificationContract:
    def test_from_plan_strings_skips_empty_and_internal(self) -> None:
        plan = ["pytest tests/", "", "final_review", "ruff check ."]
        contract = VerificationContract.from_plan_strings(plan)
        assert len(contract.steps) == 2
        assert contract.steps[0].command == "pytest tests/"
        assert contract.steps[1].command == "ruff check ."
        assert contract.is_valid

    def test_empty_contract_is_not_valid(self) -> None:
        contract = VerificationContract.empty()
        assert not contract.is_valid
        assert contract.steps == []

    def test_contract_with_validation_errors_is_not_valid(self) -> None:
        contract = VerificationContract(
            contract_id="c1",
            steps=[VerificationStep(step_id="s1", command="echo ok")],
            validation_errors=["some_error"],
        )
        assert not contract.is_valid

    def test_from_dict_round_trip(self) -> None:
        original = VerificationContract.from_plan_strings(["pytest -x", "ruff check ."])
        payload = original.to_dict()
        restored = VerificationContract.from_dict(payload)
        assert restored.contract_id == original.contract_id
        assert len(restored.steps) == len(original.steps)
        assert restored.steps[0].command == original.steps[0].command

    def test_is_command_allowed(self) -> None:
        contract = VerificationContract.from_plan_strings(["pytest tests/", "ruff check ."])
        assert contract.is_command_allowed(["pytest", "tests/"])
        assert contract.is_command_allowed(["pytest", "tests/", "-x"])
        assert contract.is_command_allowed(["ruff", "check", "."])
        assert not contract.is_command_allowed(["python", "-m", "black", "."])
        assert not contract.is_command_allowed(["rm", "-rf", "/"])

    def test_empty_contract_allows_all(self) -> None:
        contract = VerificationContract.empty()
        assert contract.is_command_allowed(["anything"])

    def test_step_for_command(self) -> None:
        contract = VerificationContract.from_plan_strings(["pytest tests/", "ruff check ."])
        step = contract.step_for_command(["pytest", "tests/"])
        assert step is not None
        assert step.command == "pytest tests/"
        assert contract.step_for_command(["unknown", "cmd"]) is None


class TestContractSatisfaction:
    def test_satisfied_when_no_failures(self) -> None:
        s = ContractSatisfaction(
            contract_id="c1", satisfied=True,
            completed_steps=["s1", "s2"], failed_steps=[], skipped_steps=[],
        )
        assert s.satisfied
        payload = s.to_dict()
        assert payload["satisfied"] is True

    def test_not_satisfied_when_failures(self) -> None:
        s = ContractSatisfaction(
            contract_id="c1", satisfied=False,
            completed_steps=["s1"], failed_steps=["s2"], skipped_steps=[],
            reason="failed_steps=1",
        )
        assert not s.satisfied

    def test_step_evidence_in_to_dict(self) -> None:
        s = ContractSatisfaction(
            contract_id="c1", satisfied=True,
            completed_steps=["s1"], failed_steps=[], skipped_steps=[],
            step_evidence=[StepEvidence(step_id="s1", check_id="c1", command_id="cmd1", status="passed")],
        )
        payload = s.to_dict()
        assert len(payload["step_evidence"]) == 1
        assert payload["step_evidence"][0]["step_id"] == "s1"


# ---------------------------------------------------------------------------
# FailureAnalysisResult with verification_contract
# ---------------------------------------------------------------------------


def _make_request(
    *,
    evidence_refs: list[str] | None = None,
    changed_files: list[str] | None = None,
    workspace_root: str = "/workspace",
) -> FailureAnalysisRequest:
    return FailureAnalysisRequest(
        request_id="req_1",
        run_id="run_1",
        session_id="sess_1",
        task_id="task_1",
        phase_id="repairing_failures",
        workspace_root=workspace_root,
        failure_source="verification",
        failure_summary="test failed",
        failure_sources=[],
        evidence_refs=evidence_refs if evidence_refs is not None else ["ev_1"],
        changed_files=changed_files if changed_files is not None else ["src/app.py"],
    )


def _make_analysis(
    *,
    verification_plan: list[str] | None = None,
    confidence: float = 0.8,
    needs_user_input: bool = False,
    blocked_reason: str | None = None,
    affected_files: list[str] | None = None,
    evidence_refs: list[str] | None = None,
    failure_category: str = "unit_test_failure",
) -> FailureAnalysisResult:
    _vplan = verification_plan if verification_plan is not None else ["pytest tests/test_app.py"]
    _erefs = evidence_refs if evidence_refs is not None else ["ev_1"]
    _afiles = affected_files if affected_files is not None else ["src/app.py"]
    return FailureAnalysisResult(
        analysis_id="analysis_1",
        request_id="req_1",
        root_cause="assertion failed",
        failure_category=failure_category,
        affected_files=_afiles,
        evidence_refs=_erefs,
        repair_strategy="repair_then_verify",
        next_actions=["fix the bug"],
        verification_plan=_vplan,
        confidence=confidence,
        needs_user_input=needs_user_input,
        blocked_reason=blocked_reason,
        verification_contract=VerificationContract.from_plan_strings(_vplan),
    )


class TestFailureAnalysisResultContract:
    def test_analysis_carries_verification_contract(self) -> None:
        analysis = _make_analysis()
        assert analysis.verification_contract.is_valid
        assert len(analysis.verification_contract.steps) == 1

    def test_analysis_to_dict_includes_contract(self) -> None:
        analysis = _make_analysis()
        payload = analysis.to_dict()
        assert "verification_contract" in payload


# ---------------------------------------------------------------------------
# RepairContract with structured verification
# ---------------------------------------------------------------------------


class TestRepairContract:
    def test_from_analysis_includes_contract(self) -> None:
        analysis = _make_analysis()
        candidates = [
            RepairActionCandidate(
                candidate_id="c1", action_type="edit",
                target_file="src/app.py", rationale="fix the bug",
                tool_hints=["read_file", "apply_patch"],
            )
        ]
        contract = RepairContract.from_analysis(analysis, action_candidates=candidates)
        assert contract.verification_contract.is_valid
        assert "run_verification" in contract.allowed_tool_names

    def test_blocked_contract_has_no_allowed_tools(self) -> None:
        analysis = _make_analysis(needs_user_input=True, blocked_reason="policy_blocked")
        contract = RepairContract.blocked(analysis, reason="policy_blocked")
        assert contract.needs_user_input
        assert contract.allowed_tool_names == []


class TestRepairPlanContract:
    def test_plan_carries_contract(self) -> None:
        planner = RepairPlanner()
        analysis = _make_analysis()
        plan = planner.plan(analysis)
        assert plan.verification_contract.is_valid
        assert plan.repair_contract is not None


class TestRepairReplanSignalContract:
    def test_signal_carries_contract(self) -> None:
        planner = RepairPlanner()
        request = _make_request()
        analysis = _make_analysis()
        plan = planner.plan(analysis)
        assert plan.repair_contract is not None
        signal = planner.to_replan_signal(request=request, analysis=analysis, plan=plan)
        assert signal.verification_contract.is_valid


# ---------------------------------------------------------------------------
# Policy-blocked / low-confidence / missing-info edge cases
# ---------------------------------------------------------------------------


class TestBlockedCategories:
    def test_all_blocked_categories_route_to_blocked(self) -> None:
        planner = RepairPlanner()
        for category in BLOCKED_FAILURE_CATEGORIES:
            analysis = _make_analysis(
                needs_user_input=True,
                blocked_reason=f"category={category}",
                failure_category=category,
            )
            plan = planner.plan(analysis)
            assert plan.needs_user_input, f"category {category} should be blocked"
            assert plan.strategy == "blocked"

    def test_low_confidence_rejected_by_contract(self) -> None:
        analysis = _make_analysis(confidence=MIN_REPAIR_CONFIDENCE - 0.1)
        candidates = [
            RepairActionCandidate(
                candidate_id="c1", action_type="edit",
                target_file="src/app.py", rationale="fix",
                tool_hints=["read_file", "apply_patch"],
            )
        ]
        contract = RepairContract.from_analysis(analysis, action_candidates=candidates)
        assert contract.needs_user_input
        assert any("low_confidence" in e for e in contract.validation_errors)

    def test_missing_evidence_refs_rejected(self) -> None:
        analysis = _make_analysis(evidence_refs=[])
        candidates = [
            RepairActionCandidate(
                candidate_id="c1", action_type="edit",
                target_file="src/app.py", rationale="fix",
                tool_hints=["read_file", "apply_patch"],
            )
        ]
        contract = RepairContract.from_analysis(analysis, action_candidates=candidates)
        assert contract.needs_user_input

    def test_missing_target_files_rejected(self) -> None:
        analysis = _make_analysis(affected_files=[])
        candidates = [
            RepairActionCandidate(
                candidate_id="c1", action_type="edit",
                target_file=None, rationale="fix",
                tool_hints=["read_file"],
            )
        ]
        contract = RepairContract.from_analysis(analysis, action_candidates=candidates)
        assert contract.needs_user_input


# ---------------------------------------------------------------------------
# Planner integration: verification contract satisfaction
# ---------------------------------------------------------------------------


class TestPlannerVerificationContract:
    def _make_repair_planner(self, tmp_path: Path) -> Planner:
        planner = Planner(tmp_path, session_id="s1", task_id="t1")
        planner.start_task("fix the bug")
        planner.state.status = TaskStatus.REPAIRING_FAILURES
        planner.state.current_phase = "repairing_failures"
        planner.plan.current_phase = "repairing_failures"
        return planner

    def _inject_repair_plan(self, planner: Planner) -> VerificationContract:
        contract = VerificationContract.from_plan_strings(["pytest tests/"])
        plan = RepairPlan(
            plan_id="rp_1",
            analysis_id="a1",
            strategy="repair_then_verify",
            summary="fix",
            action_candidates=[],
            next_actions=["fix"],
            verification_plan=["pytest tests/"],
            evidence_refs=["ev_1"],
            confidence=0.8,
            repair_contract=RepairContract(
                contract_id="rc_1",
                analysis_id="a1",
                failure_category="unit_test_failure",
                target_files=["src/app.py"],
                evidence_refs=["ev_1"],
                action_candidates=[],
                verification_plan=["pytest tests/"],
                confidence=0.8,
                allowed_tool_names=["run_verification"],
                verification_contract=contract,
            ),
            verification_contract=contract,
        )
        planner.evidence.repair_plans.append(plan.to_dict())
        return contract

    def test_satisfaction_no_steps_returns_satisfied(self, tmp_path: Path) -> None:
        planner = self._make_repair_planner(tmp_path)
        satisfaction = planner.assess_verification_contract_satisfaction()
        assert satisfaction.satisfied
        assert satisfaction.reason == "no_verification_steps"

    def test_satisfaction_no_results_returns_not_satisfied(self, tmp_path: Path) -> None:
        planner = self._make_repair_planner(tmp_path)
        self._inject_repair_plan(planner)
        satisfaction = planner.assess_verification_contract_satisfaction()
        assert not satisfaction.satisfied
        assert satisfaction.reason == "no_verification_results"

    def test_satisfaction_without_step_evidence_is_not_satisfied(self, tmp_path: Path) -> None:
        """When verification results lack step_evidence, satisfaction is blocked."""
        planner = self._make_repair_planner(tmp_path)
        self._inject_repair_plan(planner)
        planner.evidence.verification_results.append({
            "completion_assessment": {"status": "ready"},
            "check_status": [{"check_id": "check_1", "status": "passed"}],
            "results": [],
        })
        satisfaction = planner.assess_verification_contract_satisfaction()
        assert not satisfaction.satisfied
        assert satisfaction.reason == "step_evidence_missing"

    def test_satisfaction_with_step_evidence(self, tmp_path: Path) -> None:
        """When step_evidence shows all steps passed, satisfaction is true."""
        planner = self._make_repair_planner(tmp_path)
        contract = self._inject_repair_plan(planner)
        planner.evidence.verification_results.append({
            "completion_assessment": {"status": "ready"},
            "check_status": [{"check_id": "check_1", "status": "passed"}],
            "results": [],
            "step_evidence": [
                {
                    "step_id": contract.steps[0].step_id,
                    "check_id": "check_1",
                    "command_id": "cmd_1",
                    "status": "passed",
                    "artifact_ref": "work/artifact.txt",
                }
            ],
        })
        satisfaction = planner.assess_verification_contract_satisfaction()
        assert satisfaction.satisfied
        assert contract.steps[0].step_id in satisfaction.completed_steps
        assert len(satisfaction.step_evidence) == 1
        assert satisfaction.step_evidence[0].check_id == "check_1"

    def test_satisfaction_with_failed_step_evidence(self, tmp_path: Path) -> None:
        """When step_evidence shows a step failed, satisfaction is false."""
        planner = self._make_repair_planner(tmp_path)
        contract = self._inject_repair_plan(planner)
        planner.evidence.verification_results.append({
            "completion_assessment": {"status": "failed"},
            "check_status": [{"check_id": "check_1", "status": "failed"}],
            "results": [],
            "step_evidence": [
                {
                    "step_id": contract.steps[0].step_id,
                    "check_id": "check_1",
                    "command_id": "cmd_1",
                    "status": "failed",
                    "artifact_ref": None,
                }
            ],
        })
        satisfaction = planner.assess_verification_contract_satisfaction()
        assert not satisfaction.satisfied
        assert contract.steps[0].step_id in satisfaction.failed_steps

    def test_assess_completion_includes_contract_satisfaction(self, tmp_path: Path) -> None:
        planner = Planner(tmp_path, session_id="s1", task_id="t1")
        planner.start_task("fix the bug")
        assessment = planner.assess_completion(mark_blocked=False)
        assert "verification_contract_satisfaction" in assessment

    def test_blocked_signal_blocks_planner(self, tmp_path: Path) -> None:
        planner = Planner(tmp_path, session_id="s1", task_id="t1")
        planner.start_task("fix the bug")
        contract = VerificationContract.empty()
        signal = {
            "signal_id": "sig_1",
            "repair_plan_id": "rp_1",
            "analysis_id": "a1",
            "contract_id": "rc_1",
            "failure_fingerprint": "fp_1",
            "failure_category": "permission_denied",
            "target_files": [],
            "action_candidates": [],
            "verification_plan": [],
            "verification_contract": contract.to_dict(),
            "confidence": 0.8,
            "needs_user_input": True,
            "blocked_reason": "permission_denied cannot be repaired automatically",
            "repair_contract": {
                "contract_id": "rc_1",
                "analysis_id": "a1",
                "failure_category": "permission_denied",
                "target_files": [],
                "evidence_refs": ["ev_1"],
                "action_candidates": [],
                "verification_plan": [],
                "verification_contract": contract.to_dict(),
                "confidence": 0.8,
                "allowed_tool_names": [],
                "needs_user_input": True,
                "blocked_reason": "permission_denied cannot be repaired automatically",
                "validation_errors": [],
            },
            "error_code": "permission_denied",
            "verification_failed": True,
        }
        planner.replan(signal)
        assert planner.state.status == TaskStatus.BLOCKED

    def test_repeated_failure_budget_exceeded(self, tmp_path: Path) -> None:
        planner = Planner(tmp_path, session_id="s1", task_id="t1")
        planner.start_task("fix the bug")
        planner.budget.max_repeated_failures = 1
        contract = VerificationContract.from_plan_strings(["pytest tests/"])
        signal = {
            "signal_id": "sig_1",
            "repair_plan_id": "rp_1",
            "analysis_id": "a1",
            "contract_id": "rc_1",
            "failure_fingerprint": "same_fp",
            "failure_category": "unit_test_failure",
            "target_files": ["src/app.py"],
            "action_candidates": [],
            "verification_plan": ["pytest tests/"],
            "verification_contract": contract.to_dict(),
            "confidence": 0.8,
            "needs_user_input": False,
            "blocked_reason": None,
            "repair_contract": {
                "contract_id": "rc_1",
                "analysis_id": "a1",
                "failure_category": "unit_test_failure",
                "target_files": ["src/app.py"],
                "evidence_refs": ["ev_1"],
                "action_candidates": [],
                "verification_plan": ["pytest tests/"],
                "verification_contract": contract.to_dict(),
                "confidence": 0.8,
                "allowed_tool_names": ["run_verification"],
                "needs_user_input": False,
                "blocked_reason": None,
                "validation_errors": [],
            },
            "error_code": "unit_test_failure",
            "verification_failed": True,
        }
        planner.replan(signal)
        decision = planner.replan(signal)
        assert planner.state.status == TaskStatus.BLOCKED
        assert "repeated_failure" in planner.state.blocked_reasons


# ---------------------------------------------------------------------------
# Planner authorization: run_verification constrained by contract
# ---------------------------------------------------------------------------


class TestPlannerVerificationAuthorization:
    def _make_spec(self, name: str) -> ToolSpec:
        """Create a minimal ToolSpec for authorization testing."""
        return ToolSpec(
            name=name,
            version="0.0.1",
            description=f"Test {name}",
            input_model=_EmptyInput,
            handler=lambda args: {},
            permission_level="shell",
            capabilities=(),
            operation="verification",
            resource_resolver=lambda _a, _r: [],
            side_effects="execute_command",
            sensitivity="workspace",
            risk_tags=("verification_runner",),
            uses_command_executor=True,
        )

    def _setup_repair_planner(self, tmp_path: Path) -> tuple[Planner, VerificationContract]:
        planner = Planner(tmp_path, session_id="s1", task_id="t1")
        planner.start_task("fix the bug")
        planner.state.status = TaskStatus.REPAIRING_FAILURES
        planner.state.current_phase = "repairing_failures"
        planner.plan.current_phase = "repairing_failures"
        contract = VerificationContract.from_plan_strings(["pytest tests/"])
        plan = RepairPlan(
            plan_id="rp_1",
            analysis_id="a1",
            strategy="repair_then_verify",
            summary="fix",
            action_candidates=[],
            next_actions=["fix"],
            verification_plan=["pytest tests/"],
            evidence_refs=["ev_1"],
            confidence=0.8,
            repair_contract=RepairContract(
                contract_id="rc_1",
                analysis_id="a1",
                failure_category="unit_test_failure",
                target_files=["src/app.py"],
                evidence_refs=["ev_1"],
                action_candidates=[],
                verification_plan=["pytest tests/"],
                confidence=0.8,
                allowed_tool_names=["run_verification", "get_verification_result"],
                verification_contract=contract,
            ),
            verification_contract=contract,
        )
        planner.evidence.repair_plans.append(plan.to_dict())
        return planner, contract

    def test_contract_command_allowed(self, tmp_path: Path) -> None:
        """run_verification with contract-matching command is allowed."""
        planner, contract = self._setup_repair_planner(tmp_path)
        spec = self._make_spec("run_verification")
        decision = planner.authorize_tool_call(
            tool_name="run_verification",
            tool_call_id="tc_1",
            spec=spec,
            arguments={"smoke_commands": [["pytest", "tests/"]]},
        )
        assert decision.allowed

    def test_arbitrary_command_rejected(self, tmp_path: Path) -> None:
        """run_verification with a command NOT in the contract is rejected."""
        planner, contract = self._setup_repair_planner(tmp_path)
        spec = self._make_spec("run_verification")
        decision = planner.authorize_tool_call(
            tool_name="run_verification",
            tool_call_id="tc_2",
            spec=spec,
            arguments={"smoke_commands": [["python", "-m", "black", "--check", "."]]},
        )
        assert not decision.allowed
        assert decision.error_code == "verification_contract_command_not_allowed"

    def test_no_smoke_commands_allowed(self, tmp_path: Path) -> None:
        """run_verification without smoke_commands (uses defaults) is allowed."""
        planner, contract = self._setup_repair_planner(tmp_path)
        spec = self._make_spec("run_verification")
        decision = planner.authorize_tool_call(
            tool_name="run_verification",
            tool_call_id="tc_3",
            spec=spec,
            arguments={},
        )
        assert decision.allowed

    def test_get_verification_result_allowed(self, tmp_path: Path) -> None:
        """get_verification_result is always allowed during repair."""
        planner, contract = self._setup_repair_planner(tmp_path)
        spec = self._make_spec("get_verification_result")
        decision = planner.authorize_tool_call(
            tool_name="get_verification_result",
            tool_call_id="tc_4",
            spec=spec,
            arguments={},
        )
        assert decision.allowed

    def test_multiple_commands_one_disallowed_rejects_all(self, tmp_path: Path) -> None:
        """If one of multiple smoke_commands is not in the contract, reject."""
        planner, contract = self._setup_repair_planner(tmp_path)
        spec = self._make_spec("run_verification")
        decision = planner.authorize_tool_call(
            tool_name="run_verification",
            tool_call_id="tc_5",
            spec=spec,
            arguments={
                "smoke_commands": [
                    ["pytest", "tests/"],
                    ["python", "-m", "black", "--check", "."],
                ]
            },
        )
        assert not decision.allowed
        assert decision.error_code == "verification_contract_command_not_allowed"


# ---------------------------------------------------------------------------
# VerificationRunner: step_evidence in observation
# ---------------------------------------------------------------------------


class TestVerificationRunnerStepEvidence:
    def test_smoke_command_gets_contract_step_id(self, tmp_path: Path) -> None:
        """When contract is active, smoke checks carry contract_step_id."""
        contract = VerificationContract.from_plan_strings(["pytest tests/test_app.py"])
        runner = VerificationRunner(tmp_path, command_executor=FakeCommandExecutor([]))
        plan = runner.plan_verification(
            changed_files=["src/app.py"],
            task_intent="fix test",
            smoke_commands=[["pytest", "tests/test_app.py"]],
            verification_contract=contract,
        )
        # Find the smoke check
        smoke_checks = [c for c in plan.required_checks if c.kind == CheckKind.VERIFICATION_SMOKE]
        assert len(smoke_checks) == 1
        assert smoke_checks[0].contract_step_id == contract.steps[0].step_id

    def test_observation_includes_step_evidence(self, tmp_path: Path) -> None:
        """run_plan observation includes step_evidence with step→check mapping."""
        contract = VerificationContract.from_plan_strings(["pytest tests/test_app.py"])
        smoke_request = CommandRequest(argv=["python", "-m", "pytest", "tests/test_app.py"])
        syntax_request = CommandRequest(argv=["python", "-m", "py_compile", "src/app.py"])
        fake = FakeCommandExecutor([
            command_result(
                smoke_request,
                command_id="cmd_smoke",
                exit_code=0,
                semantic_status=SemanticStatus.SUCCEEDED,
                output="1 passed",
            ),
            command_result(
                syntax_request,
                command_id="cmd_syntax",
                exit_code=0,
                semantic_status=SemanticStatus.SUCCEEDED,
                output="",
            ),
        ])
        runner = VerificationRunner(tmp_path, command_executor=fake)
        plan = runner.plan_verification(
            changed_files=["src/app.py"],
            task_intent="fix test",
            smoke_commands=[["pytest", "tests/test_app.py"]],
            verification_contract=contract,
        )
        observation = runner.run_plan(plan.id)
        step_evidence = observation["verification"]["step_evidence"]
        assert len(step_evidence) >= 1
        # Find the evidence for our contract step
        our_step = next(
            (se for se in step_evidence if se["step_id"] == contract.steps[0].step_id),
            None,
        )
        assert our_step is not None
        assert our_step["status"] == "passed"
        assert our_step["check_id"] is not None
        assert our_step["command_id"] == "cmd_smoke"

    def test_failed_step_evidence(self, tmp_path: Path) -> None:
        """Failed check produces step_evidence with status=failed."""
        contract = VerificationContract.from_plan_strings(["pytest tests/test_app.py"])
        smoke_request = CommandRequest(argv=["python", "-m", "pytest", "tests/test_app.py"])
        syntax_request = CommandRequest(argv=["python", "-m", "py_compile", "src/app.py"])
        fake = FakeCommandExecutor([
            command_result(
                smoke_request,
                command_id="cmd_fail",
                exit_code=1,
                semantic_status=SemanticStatus.TESTS_FAILED,
                output="FAILED tests/test_app.py::test_bad - AssertionError",
                error_code="semantic_failure",
            ),
            command_result(
                syntax_request,
                command_id="cmd_syntax",
                exit_code=0,
                semantic_status=SemanticStatus.SUCCEEDED,
                output="",
            ),
        ])
        runner = VerificationRunner(tmp_path, command_executor=fake)
        plan = runner.plan_verification(
            changed_files=["src/app.py"],
            task_intent="fix test",
            smoke_commands=[["pytest", "tests/test_app.py"]],
            verification_contract=contract,
        )
        observation = runner.run_plan(plan.id)
        step_evidence = observation["verification"]["step_evidence"]
        our_step = next(
            (se for se in step_evidence if se["step_id"] == contract.steps[0].step_id),
            None,
        )
        assert our_step is not None
        assert our_step["status"] == "failed"

    def test_no_contract_no_step_evidence(self, tmp_path: Path) -> None:
        """Without a contract, step_evidence is empty (no contract_step_ids)."""
        smoke_request = CommandRequest(argv=["python", "-m", "pytest", "tests/test_app.py"])
        syntax_request = CommandRequest(argv=["python", "-m", "py_compile", "src/app.py"])
        fake = FakeCommandExecutor([
            command_result(
                smoke_request,
                command_id="cmd_1",
                exit_code=0,
                semantic_status=SemanticStatus.SUCCEEDED,
                output="1 passed",
            ),
            command_result(
                syntax_request,
                command_id="cmd_syntax",
                exit_code=0,
                semantic_status=SemanticStatus.SUCCEEDED,
                output="",
            ),
        ])
        runner = VerificationRunner(tmp_path, command_executor=fake)
        plan = runner.plan_verification(
            changed_files=["src/app.py"],
            task_intent="fix test",
            smoke_commands=[["pytest", "tests/test_app.py"]],
        )
        observation = runner.run_plan(plan.id)
        step_evidence = observation["verification"]["step_evidence"]
        assert len(step_evidence) == 0


# ---------------------------------------------------------------------------
# Integration: VerificationRunner → Planner satisfaction loop
# ---------------------------------------------------------------------------


class TestVerificationSatisfactionLoop:
    def test_runner_step_evidence_drives_satisfaction(self, tmp_path: Path) -> None:
        """Full loop: VerificationRunner produces step_evidence → stored in planner evidence."""
        contract = VerificationContract.from_plan_strings(["pytest tests/test_app.py"])
        planner = Planner(tmp_path, session_id="s1", task_id="t1")
        planner.start_task("fix the bug")
        planner.state.status = TaskStatus.REPAIRING_FAILURES
        planner.state.current_phase = "repairing_failures"
        planner.plan.current_phase = "repairing_failures"
        plan = RepairPlan(
            plan_id="rp_1", analysis_id="a1", strategy="repair_then_verify",
            summary="fix", action_candidates=[], next_actions=["fix"],
            verification_plan=["pytest tests/test_app.py"], evidence_refs=["ev_1"],
            confidence=0.8,
            repair_contract=RepairContract(
                contract_id="rc_1", analysis_id="a1",
                failure_category="unit_test_failure", target_files=["src/app.py"],
                evidence_refs=["ev_1"], action_candidates=[],
                verification_plan=["pytest tests/test_app.py"], confidence=0.8,
                allowed_tool_names=["run_verification"],
                verification_contract=contract,
            ),
            verification_contract=contract,
        )
        planner.evidence.repair_plans.append(plan.to_dict())
        smoke_req = CommandRequest(argv=["python", "-m", "pytest", "tests/test_app.py"])
        syntax_req = CommandRequest(argv=["python", "-m", "py_compile", "src/app.py"])
        fake = FakeCommandExecutor([
            command_result(smoke_req, command_id="cmd_pass", exit_code=0,
                           semantic_status=SemanticStatus.SUCCEEDED, output="1 passed"),
            command_result(syntax_req, command_id="cmd_syntax", exit_code=0,
                           semantic_status=SemanticStatus.SUCCEEDED, output=""),
        ])
        runner = VerificationRunner(tmp_path, command_executor=fake, planner=planner)
        vplan = runner.plan_verification(
            changed_files=["src/app.py"], task_intent="fix test",
            smoke_commands=[["pytest", "tests/test_app.py"]],
            verification_contract=contract,
        )
        runner.run_plan(vplan.id)
        # Verify step_evidence was stored in planner evidence
        assert planner.evidence.verification_results
        latest = planner.evidence.verification_results[-1]
        step_evidence = latest.get("step_evidence") or latest.get("verification", {}).get("step_evidence") or []
        assert len(step_evidence) >= 1
        our_step = next((se for se in step_evidence if se["step_id"] == contract.steps[0].step_id), None)
        assert our_step is not None
        assert our_step["status"] == "passed"

    def test_failed_runner_keeps_repairing_phase(self, tmp_path: Path) -> None:
        """Failed verification keeps state in repairing_failures → satisfaction works."""
        contract = VerificationContract.from_plan_strings(["pytest tests/test_app.py"])
        planner = Planner(tmp_path, session_id="s1", task_id="t1")
        planner.start_task("fix the bug")
        planner.state.status = TaskStatus.REPAIRING_FAILURES
        planner.state.current_phase = "repairing_failures"
        planner.plan.current_phase = "repairing_failures"
        plan = RepairPlan(
            plan_id="rp_1", analysis_id="a1", strategy="repair_then_verify",
            summary="fix", action_candidates=[], next_actions=["fix"],
            verification_plan=["pytest tests/test_app.py"], evidence_refs=["ev_1"],
            confidence=0.8,
            repair_contract=RepairContract(
                contract_id="rc_1", analysis_id="a1",
                failure_category="unit_test_failure", target_files=["src/app.py"],
                evidence_refs=["ev_1"], action_candidates=[],
                verification_plan=["pytest tests/test_app.py"], confidence=0.8,
                allowed_tool_names=["run_verification"],
                verification_contract=contract,
            ),
            verification_contract=contract,
        )
        planner.evidence.repair_plans.append(plan.to_dict())
        smoke_req = CommandRequest(argv=["python", "-m", "pytest", "tests/test_app.py"])
        syntax_req = CommandRequest(argv=["python", "-m", "py_compile", "src/app.py"])
        fake = FakeCommandExecutor([
            command_result(smoke_req, command_id="cmd_fail", exit_code=1,
                           semantic_status=SemanticStatus.TESTS_FAILED,
                           output="FAILED - AssertionError", error_code="semantic_failure"),
            command_result(syntax_req, command_id="cmd_syntax", exit_code=0,
                           semantic_status=SemanticStatus.SUCCEEDED, output=""),
        ])
        runner = VerificationRunner(tmp_path, command_executor=fake, planner=planner)
        vplan = runner.plan_verification(
            changed_files=["src/app.py"], task_intent="fix test",
            smoke_commands=[["pytest", "tests/test_app.py"]],
            verification_contract=contract,
        )
        runner.run_plan(vplan.id)
        # Failed verification keeps state in repairing_failures
        assert planner.state.status == TaskStatus.REPAIRING_FAILURES
        # Satisfaction check works because we're still in repairing phase
        satisfaction = planner.assess_verification_contract_satisfaction()
        assert not satisfaction.satisfied
        assert len(satisfaction.step_evidence) == 1
        assert satisfaction.step_evidence[0].status == "failed"


# ---------------------------------------------------------------------------
# Finalizer includes verification contract in summary
# ---------------------------------------------------------------------------


class TestFinalizerContractSummary:
    def test_failure_repair_summary_has_contract_fields(self, tmp_path: Path) -> None:
        from singularity.planner.finalizer import Finalizer

        planner = Planner(tmp_path, session_id="s1", task_id="t1")
        planner.start_task("fix the bug")
        contract = VerificationContract.from_plan_strings(["pytest tests/"])
        plan_dict = RepairPlan(
            plan_id="rp_1",
            analysis_id="a1",
            strategy="repair_then_verify",
            summary="fix",
            action_candidates=[],
            next_actions=["fix"],
            verification_plan=["pytest tests/"],
            evidence_refs=["ev_1"],
            confidence=0.8,
            repair_contract=RepairContract(
                contract_id="rc_1",
                analysis_id="a1",
                failure_category="unit_test_failure",
                target_files=["src/app.py"],
                evidence_refs=["ev_1"],
                action_candidates=[],
                verification_plan=["pytest tests/"],
                confidence=0.8,
                allowed_tool_names=["run_verification"],
                verification_contract=contract,
            ),
            verification_contract=contract,
        ).to_dict()
        planner.evidence.repair_plans.append(plan_dict)

        finalizer = Finalizer()
        summary = finalizer._failure_repair_summary(planner.evidence)
        assert summary["verification_contract_id"] is not None
        assert summary["verification_contract_step_count"] == 1
