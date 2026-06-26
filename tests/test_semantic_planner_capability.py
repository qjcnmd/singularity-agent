"""Tests for the Semantic Planner capability layer.

Covers:
- TaskContractProducer (model path + fallback)
- SemanticPlanProducer (initial + repair, model path + fallback)
- PlannerDecisionProducer (model path + fallback)
- Planner integration (start_task/replan/record_failure_analysis use producers)
- Producer context separation from renderer context
- Round-trip serialization of semantic objects
- ModelPurpose enum extensions
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

from singularity.model.models import (
    ModelMessage,
    ModelPurpose,
    ModelTurnRequest,
    ModelTurnResult,
    ModelTurnStatus,
)
from singularity.planner.contract import TaskContract, TaskContractBuilder
from singularity.planner.context import PlannerContextRenderer
from singularity.planner.engine import Planner
from singularity.planner.models import (
    ActionKind,
    EvidenceLedger,
    ReplanDecision,
    ReplanDecisionKind,
    TaskPlan,
    TaskState,
    TaskStatus,
)
from singularity.planner.replanner import Replanner
from singularity.planner.semantic import RollingPlan, SemanticPlanner
from singularity.planner.semantic_objects import (
    PlannerDecision,
    RepairPolicy,
    RiskPoint,
    SemanticPlan,
    VerificationStrategy,
)
from singularity.planner.semantic_producers import (
    PlannerDecisionProducer,
    PlannerProducerBundle,
    SemanticPlanProducer,
    TaskContractProducer,
)


# ---------------------------------------------------------------------------
# Fakes
# ---------------------------------------------------------------------------


class FakeModelRunner:
    """Duck-typed fake that returns scripted JSON per ModelPurpose.

    Pass ``responses`` as a dict mapping ``ModelPurpose`` -> JSON string.
    If the mapped value is an ``Exception``, it is raised on ``run_turn``.
    """

    def __init__(self, responses: dict[Any, Any]) -> None:
        self.responses = responses
        self.requests: list[ModelTurnRequest] = []

    def run_turn(self, request: ModelTurnRequest) -> ModelTurnResult:
        self.requests.append(request)
        value = self.responses.get(request.purpose)
        if isinstance(value, Exception):
            raise value
        if value is None:
            return ModelTurnResult(
                request_id=request.request_id,
                response_id="resp_fake",
                status=ModelTurnStatus.FAILED,
            )
        return ModelTurnResult(
            request_id=request.request_id,
            response_id="resp_fake",
            status=ModelTurnStatus.SUCCESS,
            assistant_message=ModelMessage.assistant_text(value),
        )


class FakeTrace:
    """Minimal trace recorder that captures emitted events."""

    def __init__(self) -> None:
        self.events: list[tuple[str, dict[str, Any]]] = []

    def emit(
        self,
        event: str,
        *,
        component: str = "",
        summary: str = "",
        payload: dict[str, Any] | None = None,
        ids: dict[str, Any] | None = None,
        severity: str = "info",
    ) -> None:
        self.events.append(
            (
                event,
                {
                    "component": component,
                    "summary": summary,
                    "payload": payload or {},
                    "ids": ids or {},
                    "severity": severity,
                },
            )
        )

    def record(self, event: str, payload: dict[str, Any] | None = None) -> None:
        """Compat shim for ``TraceRecorder.record`` used by ``Planner._record_event``."""
        self.events.append((event, payload or {}))

    def has_event(self, substring: str) -> bool:
        return any(substring in event for event, _ in self.events)


# ---------------------------------------------------------------------------
# JSON payloads for model responses
# ---------------------------------------------------------------------------

VALID_CONTRACT_JSON = json.dumps(
    {
        "user_goal": "Create hello.py",
        "acceptance_criteria": [
            {
                "criterion_id": "deliver_hello",
                "description": "hello.py is created.",
                "evidence": ["applied_changes"],
                "required": True,
            }
        ],
        "deliverables": [
            {"kind": "artifact", "description": "hello.py file", "path": "hello.py"}
        ],
        "verification_requirements": [],
        "constraints": [],
    }
)

INVALID_CONTRACT_JSON = json.dumps({"user_goal": "test"})  # missing acceptance_criteria

VALID_PLAN_JSON = json.dumps(
    {
        "rolling_plan": {
            "plan_id": "plan_model_1",
            "user_goal": "Create hello.py",
            "current_step_id": "step_create",
            "version": 1,
            "steps": [
                {
                    "step_id": "step_create",
                    "title": "Create hello.py",
                    "kind": "change",
                    "allowed_capabilities": ["write_file"],
                    "expected_evidence": [
                        {"evidence_key": "applied_changes", "description": "File created."}
                    ],
                    "status": "pending",
                }
            ],
        },
        "risk_points": [
            {
                "risk_id": "risk_overwrite",
                "description": "Existing file may be overwritten.",
                "trigger_conditions": ["hello.py already exists"],
                "mitigation_strategy": "Check file existence before writing.",
                "severity": "medium",
                "acceptance_criterion_id": "deliver_hello",
            }
        ],
        "verification_strategies": [
            {
                "strategy_id": "vs_run_check",
                "acceptance_criterion_id": "deliver_hello",
                "command": ["python", "hello.py"],
                "expected_outcome": "exit code 0",
                "fallback_commands": [],
                "evidence_key": "verification_results",
            }
        ],
        "repair_policy": {
            "failure_category_pattern": "verification_failed",
            "allowed_repair_actions": ["RepairChange"],
            "max_attempts": 3,
            "escalation_threshold": 2,
            "verification_strategy_id": "vs_run_check",
        },
    }
)

VALID_DECISION_JSON = json.dumps(
    {
        "decision": "repair_failure",
        "reason": "Verification failed; repair needed.",
        "next_action": "RepairChange",
        "risk_points_triggered": ["risk_overwrite"],
        "verification_strategy_selected": "vs_run_check",
    }
)

VALID_REPAIR_PLAN_JSON = json.dumps(
    {
        "rolling_plan": {
            "plan_id": "plan_repair_1",
            "user_goal": "Create hello.py",
            "current_step_id": "step_repair",
            "version": 1,
            "steps": [
                {
                    "step_id": "step_repair",
                    "title": "Repair hello.py",
                    "kind": "repair",
                    "allowed_capabilities": ["apply_patch"],
                    "expected_evidence": [
                        {"evidence_key": "applied_changes", "description": "File repaired."}
                    ],
                    "status": "pending",
                }
            ],
        },
        "risk_points": [],
        "verification_strategies": [],
        "repair_policy": None,
    }
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _context() -> dict[str, Any]:
    return {"run_id": "test_run", "session_id": "test_session", "task_id": "test_task"}


def _bundle(runner: Any, trace: Any = None) -> PlannerProducerBundle:
    return PlannerProducerBundle.with_rule_fallback(
        model_runner=runner,
        rule_builder=TaskContractBuilder(),
        rule_planner=SemanticPlanner(),
        rule_replanner=Replanner(),
        trace=trace,
    )


def _default_plan(task_id: str) -> TaskPlan:
    from singularity.planner.models import ActionKind, TaskPhase

    return TaskPlan(
        plan_id="plan_test",
        task_id=task_id,
        phases=[
            TaskPhase(
                phase_id="understanding_task",
                name="Understanding",
                purpose="Understand the task.",
                allowed_tools=["read_file"],
                allowed_actions=[ActionKind.READ_RELEVANT_FILES],
            )
        ],
        current_phase="understanding_task",
    )


# ---------------------------------------------------------------------------
# 1-3: TaskContractProducer
# ---------------------------------------------------------------------------


def test_task_contract_producer_uses_model_when_available():
    runner = FakeModelRunner({ModelPurpose.TASK_CONTRACT_EXTRACTION: VALID_CONTRACT_JSON})
    producer = TaskContractProducer(
        model_runner=runner, rule_builder=TaskContractBuilder()
    )
    contract = producer.produce("Create hello.py", context_payload=_context())
    assert contract.acceptance_criteria
    assert contract.source == "model"
    assert len(runner.requests) == 1


def test_task_contract_producer_falls_back_to_rules_on_model_error():
    runner = FakeModelRunner(
        {ModelPurpose.TASK_CONTRACT_EXTRACTION: RuntimeError("boom")}
    )
    producer = TaskContractProducer(
        model_runner=runner, rule_builder=TaskContractBuilder()
    )
    contract = producer.produce("Create hello.py", context_payload=_context())
    assert contract.acceptance_criteria
    assert contract.source == "rules"


def test_task_contract_producer_falls_back_on_invalid_schema():
    runner = FakeModelRunner(
        {ModelPurpose.TASK_CONTRACT_EXTRACTION: INVALID_CONTRACT_JSON}
    )
    producer = TaskContractProducer(
        model_runner=runner, rule_builder=TaskContractBuilder()
    )
    contract = producer.produce("Create hello.py", context_payload=_context())
    assert contract.source == "rules"


# ---------------------------------------------------------------------------
# 4-6: SemanticPlanProducer
# ---------------------------------------------------------------------------


def test_semantic_plan_producer_initial_plan_model_path():
    runner = FakeModelRunner({ModelPurpose.SEMANTIC_PLANNING: VALID_PLAN_JSON})
    trace = FakeTrace()
    producer = SemanticPlanProducer(
        model_runner=runner, rule_planner=SemanticPlanner(), trace=trace
    )
    contract = TaskContractBuilder().from_rules("Create hello.py")
    plan = producer.produce_initial(contract, context_payload=_context())
    assert plan.producer_source == "model"
    assert len(plan.risk_points) == 1
    assert plan.risk_points[0].risk_id == "risk_overwrite"
    assert plan.rolling_plan.steps
    assert trace.has_event("semantic_plan.model_ok")


def test_semantic_plan_producer_repair_model_path():
    runner = FakeModelRunner({ModelPurpose.SEMANTIC_PLANNING: VALID_REPAIR_PLAN_JSON})
    producer = SemanticPlanProducer(
        model_runner=runner, rule_planner=SemanticPlanner()
    )
    contract = TaskContractBuilder().from_rules("Create hello.py")
    plan = producer.produce_repair(
        {"failure_category": "verification_failed"},
        task_contract=contract,
        context_payload=_context(),
    )
    assert plan.producer_source == "model"
    assert plan.rolling_plan.steps[0].step_id == "step_repair"


def test_semantic_plan_producer_fallback_on_model_failure():
    runner = FakeModelRunner(
        {ModelPurpose.SEMANTIC_PLANNING: RuntimeError("model down")}
    )
    producer = SemanticPlanProducer(
        model_runner=runner, rule_planner=SemanticPlanner()
    )
    contract = TaskContractBuilder().from_rules("Create hello.py")
    plan = producer.produce_initial(contract, context_payload=_context())
    assert plan.producer_source == "rules_fallback"
    assert plan.rolling_plan.steps  # rule-based plan still has steps


# ---------------------------------------------------------------------------
# 7-8: PlannerDecisionProducer
# ---------------------------------------------------------------------------


def test_planner_decision_producer_model_path():
    runner = FakeModelRunner({ModelPurpose.PLANNER_DECISION: VALID_DECISION_JSON})
    producer = PlannerDecisionProducer(
        model_runner=runner, rule_replanner=Replanner()
    )
    decision = producer.produce(
        {"failure_category": "verification_failed"},
        context_payload=_context(),
        risk_points=[],
        verification_strategies=[],
        repair_policy=None,
    )
    assert decision.producer_source == "model"
    assert decision.decision == ReplanDecisionKind.REPAIR_FAILURE
    assert decision.risk_points_triggered == ["risk_overwrite"]


def test_planner_decision_producer_fallback():
    runner = FakeModelRunner(
        {ModelPurpose.PLANNER_DECISION: RuntimeError("model down")}
    )
    producer = PlannerDecisionProducer(
        model_runner=runner, rule_replanner=Replanner()
    )
    decision = producer.produce(
        {"failure_category": "verification_failed"},
        context_payload=_context(),
        risk_points=[],
        verification_strategies=[],
        repair_policy=None,
    )
    assert decision.producer_source == "rules_fallback"


# ---------------------------------------------------------------------------
# 9-11: Planner integration
# ---------------------------------------------------------------------------


def test_planner_start_task_uses_producers(tmp_path):
    runner = FakeModelRunner(
        {
            ModelPurpose.TASK_CONTRACT_EXTRACTION: VALID_CONTRACT_JSON,
            ModelPurpose.SEMANTIC_PLANNING: VALID_PLAN_JSON,
        }
    )
    trace = FakeTrace()
    planner = Planner(tmp_path, trace=trace, model_runner=runner)
    state = planner.start_task("Create hello.py")
    assert state.task_contract
    assert state.task_contract.get("source") == "model"
    assert state.risk_points
    assert state.risk_points[0]["risk_id"] == "risk_overwrite"
    assert state.verification_strategies
    assert state.repair_policy is not None
    assert trace.has_event("task_contract.model_ok")
    assert trace.has_event("semantic_plan.model_ok")


def test_planner_replan_uses_producer_decision(tmp_path):
    runner = FakeModelRunner(
        {
            ModelPurpose.TASK_CONTRACT_EXTRACTION: VALID_CONTRACT_JSON,
            ModelPurpose.SEMANTIC_PLANNING: VALID_PLAN_JSON,
            ModelPurpose.PLANNER_DECISION: VALID_DECISION_JSON,
        }
    )
    trace = FakeTrace()
    planner = Planner(tmp_path, trace=trace, model_runner=runner)
    planner.start_task("Create hello.py")
    decision = planner.replan({"failure_category": "verification_failed"})
    assert decision.decision == ReplanDecisionKind.REPAIR_FAILURE
    assert trace.has_event("planner_decision.model_ok")


def test_planner_record_failure_analysis_uses_producer_repair_plan(tmp_path):
    runner = FakeModelRunner(
        {
            ModelPurpose.TASK_CONTRACT_EXTRACTION: VALID_CONTRACT_JSON,
            ModelPurpose.SEMANTIC_PLANNING: VALID_REPAIR_PLAN_JSON,
            ModelPurpose.PLANNER_DECISION: VALID_DECISION_JSON,
        }
    )
    trace = FakeTrace()
    planner = Planner(tmp_path, trace=trace, model_runner=runner)
    planner.start_task("Create hello.py")
    planner.record_failure_analysis(
        {"analysis_id": "a1", "failure_category": "verification_failed"},
        {"plan_id": "p1", "needs_user_input": False},
    )
    state = planner.state
    assert state is not None
    assert state.rolling_plan.get("plan_id") == "plan_repair_1"


# ---------------------------------------------------------------------------
# 12: Producer context separation
# ---------------------------------------------------------------------------


def test_producer_context_is_separate_from_renderer_context(tmp_path):
    planner = Planner(tmp_path)
    planner.start_task("Create hello.py")
    producer_ctx = planner._producer_context()
    renderer = PlannerContextRenderer()
    renderer_output = renderer.render(
        state=planner.state,
        plan=planner.plan,
        evidence=planner.evidence,
    )
    # _producer_context returns a compact dict; renderer returns a JSON string.
    assert isinstance(producer_ctx, dict)
    assert isinstance(renderer_output, str)
    # The producer context has a flat structure with task_contract as a dict;
    # the renderer output is a nested JSON payload with "planner" key.
    assert "planner" not in producer_ctx
    assert "planner" in json.loads(renderer_output)


# ---------------------------------------------------------------------------
# 13-14: Round-trip serialization
# ---------------------------------------------------------------------------


def test_risk_point_verification_strategy_repair_policy_round_trip():
    rp = RiskPoint(
        risk_id="r1",
        description="desc",
        trigger_conditions=["c1"],
        mitigation_strategy="mit",
        severity="high",
        acceptance_criterion_id="ac1",
    )
    rp2 = RiskPoint.from_dict(rp.to_dict())
    assert rp2 == rp

    vs = VerificationStrategy(
        strategy_id="vs1",
        acceptance_criterion_id="ac1",
        command=["python", "test.py"],
        expected_outcome="pass",
        fallback_commands=[["python", "test2.py"]],
        evidence_key="ev1",
    )
    vs2 = VerificationStrategy.from_dict(vs.to_dict())
    assert vs2 == vs

    policy = RepairPolicy(
        failure_category_pattern="verification_failed",
        allowed_repair_actions=["RepairChange"],
        max_attempts=5,
        escalation_threshold=3,
        verification_strategy_id="vs1",
    )
    policy2 = RepairPolicy.from_dict(policy.to_dict())
    assert policy2 == policy


def test_semantic_plan_to_dict_round_trip():
    rolling = SemanticPlanner().initial_plan(
        TaskContractBuilder().from_rules("Create hello.py")
    )
    plan = SemanticPlan(
        rolling_plan=rolling,
        risk_points=[
            RiskPoint("r1", "desc", ["c"], "mit", "medium", "ac1"),
        ],
        verification_strategies=[
            VerificationStrategy("vs1", "ac1", None, "pass", [], "ev1"),
        ],
        repair_policy=RepairPolicy("fail", ["RepairChange"], 3, 2, "vs1"),
        producer_source="model",
    )
    restored = SemanticPlan.from_dict(plan.to_dict())
    assert restored.producer_source == "model"
    assert len(restored.risk_points) == 1
    assert restored.risk_points[0].risk_id == "r1"
    assert len(restored.verification_strategies) == 1
    assert restored.repair_policy is not None
    assert restored.repair_policy.max_attempts == 3
    assert restored.rolling_plan.steps


# ---------------------------------------------------------------------------
# 15: ModelPurpose enum extensions
# ---------------------------------------------------------------------------


def test_model_purpose_new_values_exist():
    assert ModelPurpose.TASK_CONTRACT_EXTRACTION.value == "task_contract_extraction"
    assert ModelPurpose.SEMANTIC_PLANNING.value == "semantic_planning"
    assert ModelPurpose.PLANNER_DECISION.value == "planner_decision"
