"""Model-driven Semantic Planner producers.

Each producer always tries the model first and falls back to the existing
rule-based path when the model call fails, returns invalid JSON, or fails
schema validation. The rule objects (``TaskContractBuilder.from_rules``,
``SemanticPlanner``, ``Replanner``) are NOT deleted — they are kept as
fallback implementations inside the producers.

This module is the capability-layer entry point for the Semantic Planner. It
is wired into ``Planner`` via ``PlannerProducerBundle`` and into the real
``AgentLoop`` via ``AgentGraphBuilder._wire_planner`` (see ``kernel/graph.py``).

Producer trace events (emitted via ``trace.emit`` with ``component="semantic_planner"``):
- ``semantic_planner.task_contract.model_ok`` — model produced a valid contract.
- ``semantic_planner.task_contract.fallback`` — fell back to rules (with reason).
- ``semantic_planner.semantic_plan.model_ok`` / ``.fallback`` — same for plans.
- ``semantic_planner.planner_decision.model_ok`` / ``.fallback`` — same for decisions.

The producer context passed to ``produce(...)`` is a compact dict built by
``Planner._producer_context()`` — it is intentionally separate from
``PlannerContextRenderer.render()`` (which projects to the main task model) so
producer-internal model calls do not pollute the main task model's context.
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass
from typing import Any
from uuid import uuid4

from singularity.model.models import (
    ContentBlock,
    ModelBudget,
    ModelMessage,
    ModelPreferences,
    ModelPurpose,
    ModelRole,
    ModelTurnRequest,
    ModelTurnStatus,
    ToolChoiceMode,
    ToolChoicePolicy,
)
from singularity.planner.contract import TaskContract, TaskContractBuilder
from singularity.planner.models import ReplanDecision
from singularity.planner.replanner import Replanner
from singularity.planner.semantic import RollingPlan, SemanticPlanner
from singularity.planner.semantic_objects import (
    PlannerDecision,
    RepairPolicy,
    RiskPoint,
    SemanticPlan,
    VerificationStrategy,
)


def _json_payload(text: str) -> dict[str, Any]:
    """Parse a JSON object from model text, with a regex fallback."""
    try:
        value = json.loads(text)
    except json.JSONDecodeError:
        match = re.search(r"\{.*\}", text, flags=re.DOTALL)
        if not match:
            raise ValueError("model response did not contain a JSON object")
        value = json.loads(match.group(0))
    if not isinstance(value, dict):
        raise ValueError("model response JSON was not an object")
    return value


def _emit(
    trace: Any,
    event: str,
    *,
    summary: str,
    ids: dict[str, Any],
    payload: dict[str, Any] | None = None,
    severity: str = "info",
) -> None:
    """Emit a producer trace event, tolerating recorders without ``emit``."""
    if trace is None:
        return
    if hasattr(trace, "emit"):
        trace.emit(
            event,
            component="semantic_planner",
            summary=summary,
            payload=payload or {},
            ids=ids,
            severity=severity,
        )
    elif hasattr(trace, "record"):
        trace.record(event, {**(payload or {}), "summary": summary, **ids})


def _request_ids(context_payload: dict[str, Any]) -> dict[str, str]:
    """Extract run/session/task/phase/action ids for ModelTurnRequest."""
    return {
        "run_id": str(context_payload.get("run_id") or context_payload.get("task_id") or "producer"),
        "session_id": str(context_payload.get("session_id") or "producer"),
        "task_id": str(context_payload.get("task_id") or "producer"),
        "phase_id": str(context_payload.get("phase_id") or "semantic_planner"),
        "action_id": str(context_payload.get("action_id") or uuid4().hex[:12]),
    }


class TaskContractProducer:
    """Produces a ``TaskContract`` via model, falling back to rules."""

    def __init__(
        self,
        *,
        model_runner: Any | None,
        rule_builder: TaskContractBuilder,
        trace: Any | None = None,
    ) -> None:
        self.model_runner = model_runner
        self.rule_builder = rule_builder
        self.trace = trace

    def produce(self, user_goal: str, *, context_payload: dict[str, Any]) -> TaskContract:
        if self.model_runner is None:
            return self._fallback(user_goal, reason="model_runner unavailable")
        try:
            payload = self._call_model(user_goal, context_payload)
            contract = self.rule_builder.from_structured_output(
                payload, fallback_goal=user_goal
            )
            # If from_structured_output fell back internally (empty criteria),
            # it sets source="rules"; only tag "model" when criteria exist.
            if contract.acceptance_criteria:
                contract = TaskContract(
                    user_goal=contract.user_goal,
                    acceptance_criteria=contract.acceptance_criteria,
                    deliverables=contract.deliverables,
                    constraints=contract.constraints,
                    verification_requirements=contract.verification_requirements,
                    report_requirements=contract.report_requirements,
                    evidence_requirements=contract.evidence_requirements,
                    source="model",
                    version=contract.version,
                )
            self._emit_ok("task_contract", context_payload)
            return contract
        except Exception as exc:  # noqa: BLE001 - any failure falls back
            return self._fallback(user_goal, reason=f"model error: {type(exc).__name__}: {exc}")

    def _call_model(self, user_goal: str, context_payload: dict[str, Any]) -> dict[str, Any]:
        ids = _request_ids(context_payload)
        prompt = (
            "Extract a task contract from the user goal below. Return JSON only "
            "with keys: user_goal, acceptance_criteria (list of {criterion_id, "
            "description, evidence (list of str), required}), deliverables (list "
            "of {kind, description, path}), verification_requirements (list of "
            "{description, command (list of str or null), required}), "
            "constraints (list of {description, source}).\n\nUser goal:\n"
            + user_goal
        )
        request = self._model_request(
            prompt, ModelPurpose.TASK_CONTRACT_EXTRACTION, ids, context_payload
        )
        runner = self.model_runner
        if runner is None:
            raise RuntimeError("model_runner unavailable")
        result = runner.run_turn(request)
        if result.status != ModelTurnStatus.SUCCESS or result.assistant_message is None:
            raise RuntimeError(f"model turn failed: {result.status}")
        return _json_payload(result.assistant_message.text)

    def _fallback(self, user_goal: str, *, reason: str) -> TaskContract:
        contract = self.rule_builder.from_rules(user_goal)
        self._emit_fallback("task_contract", reason=reason)
        return contract

    def _model_request(
        self,
        prompt: str,
        purpose: ModelPurpose,
        ids: dict[str, str],
        context_payload: dict[str, Any],
    ) -> ModelTurnRequest:
        return ModelTurnRequest(
            request_id=f"req_{uuid4().hex[:12]}",
            run_id=ids["run_id"],
            session_id=ids["session_id"],
            task_id=ids["task_id"],
            phase_id=ids["phase_id"],
            action_id=ids["action_id"],
            purpose=purpose,
            messages=[
                ModelMessage(
                    role=ModelRole.USER,
                    content=[ContentBlock.from_text(prompt)],
                )
            ],
            tools=[],
            tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.NONE, max_tool_calls=0),
            model_preferences=ModelPreferences(json_mode=True, max_output_tokens=1500),
            budget=ModelBudget(max_retries=1, max_output_tokens=1500),
            context_metadata={"producer": "task_contract_extraction"},
        )

    def _emit_ok(self, name: str, context_payload: dict[str, Any]) -> None:
        _emit(
            self.trace,
            f"semantic_planner.{name}.model_ok",
            summary=f"{name} produced by model",
            ids=_request_ids(context_payload),
        )

    def _emit_fallback(self, name: str, *, reason: str) -> None:
        _emit(
            self.trace,
            f"semantic_planner.{name}.fallback",
            summary=f"{name} fell back to rules: {reason[:200]}",
            ids={},
            severity="warning",
        )


class SemanticPlanProducer:
    """Produces a ``SemanticPlan`` via model, falling back to rules."""

    def __init__(
        self,
        *,
        model_runner: Any | None,
        rule_planner: SemanticPlanner,
        trace: Any | None = None,
    ) -> None:
        self.model_runner = model_runner
        self.rule_planner = rule_planner
        self.trace = trace

    def produce_initial(
        self,
        task_contract: TaskContract,
        *,
        context_payload: dict[str, Any],
    ) -> SemanticPlan:
        if self.model_runner is None:
            return self._fallback_initial(
                task_contract, reason="model_runner unavailable"
            )
        try:
            payload = self._call_model_initial(task_contract, context_payload)
            plan = SemanticPlan.from_dict(payload)
            if not plan.rolling_plan.steps:
                raise ValueError("model returned empty plan steps")
            plan = SemanticPlan(
                rolling_plan=plan.rolling_plan,
                risk_points=plan.risk_points,
                verification_strategies=plan.verification_strategies,
                repair_policy=plan.repair_policy,
                producer_source="model",
            )
            _emit(
                self.trace,
                "semantic_planner.semantic_plan.model_ok",
                summary="semantic_plan (initial) produced by model",
                ids=_request_ids(context_payload),
            )
            return plan
        except Exception as exc:  # noqa: BLE001
            return self._fallback_initial(
                task_contract, reason=f"model error: {type(exc).__name__}: {exc}"
            )

    def produce_repair(
        self,
        failure_analysis: dict[str, Any],
        *,
        task_contract: TaskContract,
        context_payload: dict[str, Any],
    ) -> SemanticPlan:
        if self.model_runner is None:
            return self._fallback_repair(
                failure_analysis, task_contract, reason="model_runner unavailable"
            )
        try:
            payload = self._call_model_repair(
                failure_analysis, task_contract, context_payload
            )
            plan = SemanticPlan.from_dict(payload)
            if not plan.rolling_plan.steps:
                raise ValueError("model returned empty repair plan steps")
            plan = SemanticPlan(
                rolling_plan=plan.rolling_plan,
                risk_points=plan.risk_points,
                verification_strategies=plan.verification_strategies,
                repair_policy=plan.repair_policy,
                producer_source="model",
            )
            _emit(
                self.trace,
                "semantic_planner.semantic_plan.model_ok",
                summary="semantic_plan (repair) produced by model",
                ids=_request_ids(context_payload),
            )
            return plan
        except Exception as exc:  # noqa: BLE001
            return self._fallback_repair(
                failure_analysis, task_contract, reason=f"model error: {type(exc).__name__}: {exc}"
            )

    def _call_model_initial(
        self, task_contract: TaskContract, context_payload: dict[str, Any]
    ) -> dict[str, Any]:
        ids = _request_ids(context_payload)
        contract_json = json.dumps(task_contract.to_dict(), ensure_ascii=False, sort_keys=True, default=str)
        prompt = (
            "Produce a semantic plan for the task contract below. Return JSON "
            "only with keys: rolling_plan ({plan_id, user_goal, current_step_id, "
            "version, steps (list of {step_id, title, kind, acceptance_criterion_id, "
            "dependencies (list of {step_id, reason}), allowed_capabilities, "
            "expected_evidence (list of {evidence_key, description}), "
            "fallback_steps (list of {reason, next_action, allowed_capabilities}), "
            "status})}), risk_points (list of {risk_id, description, "
            "trigger_conditions, mitigation_strategy, severity, "
            "acceptance_criterion_id}), verification_strategies (list of "
            "{strategy_id, acceptance_criterion_id, command (list of str or null), "
            "expected_outcome, fallback_commands, evidence_key}), repair_policy "
            "({failure_category_pattern, allowed_repair_actions, max_attempts, "
            "escalation_threshold, verification_strategy_id} or null).\n\n"
            "Task contract:\n" + contract_json
        )
        return self._run_turn(prompt, ModelPurpose.SEMANTIC_PLANNING, ids, context_payload)

    def _call_model_repair(
        self,
        failure_analysis: dict[str, Any],
        task_contract: TaskContract,
        context_payload: dict[str, Any],
    ) -> dict[str, Any]:
        ids = _request_ids(context_payload)
        analysis_json = json.dumps(failure_analysis, ensure_ascii=False, sort_keys=True, default=str)
        contract_json = json.dumps(task_contract.to_dict(), ensure_ascii=False, sort_keys=True, default=str)
        prompt = (
            "Produce a repair plan based on the failure analysis and task contract "
            "below. Return JSON only with keys: rolling_plan, risk_points, "
            "verification_strategies, repair_policy (same schema as initial plan).\n\n"
            "Failure analysis:\n" + analysis_json + "\n\nTask contract:\n" + contract_json
        )
        return self._run_turn(prompt, ModelPurpose.SEMANTIC_PLANNING, ids, context_payload)

    def _run_turn(
        self,
        prompt: str,
        purpose: ModelPurpose,
        ids: dict[str, str],
        context_payload: dict[str, Any],
    ) -> dict[str, Any]:
        request = ModelTurnRequest(
            request_id=f"req_{uuid4().hex[:12]}",
            run_id=ids["run_id"],
            session_id=ids["session_id"],
            task_id=ids["task_id"],
            phase_id=ids["phase_id"],
            action_id=ids["action_id"],
            purpose=purpose,
            messages=[
                ModelMessage(
                    role=ModelRole.USER,
                    content=[ContentBlock.from_text(prompt)],
                )
            ],
            tools=[],
            tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.NONE, max_tool_calls=0),
            model_preferences=ModelPreferences(json_mode=True, max_output_tokens=2000),
            budget=ModelBudget(max_retries=1, max_output_tokens=2000),
            context_metadata={"producer": "semantic_planning"},
        )
        runner = self.model_runner
        if runner is None:
            raise RuntimeError("model_runner unavailable")
        result = runner.run_turn(request)
        if result.status != ModelTurnStatus.SUCCESS or result.assistant_message is None:
            raise RuntimeError(f"model turn failed: {result.status}")
        return _json_payload(result.assistant_message.text)

    def _fallback_initial(
        self, task_contract: TaskContract, *, reason: str
    ) -> SemanticPlan:
        rolling = self.rule_planner.initial_plan(task_contract)
        _emit(
            self.trace,
            "semantic_planner.semantic_plan.fallback",
            summary=f"semantic_plan (initial) fell back to rules: {reason[:200]}",
            ids={},
            severity="warning",
        )
        return SemanticPlan(
            rolling_plan=rolling,
            risk_points=[],
            verification_strategies=[],
            repair_policy=None,
            producer_source="rules_fallback",
        )

    def _fallback_repair(
        self,
        failure_analysis: dict[str, Any],
        task_contract: TaskContract,
        *,
        reason: str,
    ) -> SemanticPlan:
        rolling = self.rule_planner.repair_plan(
            failure_analysis, task_contract=task_contract
        )
        _emit(
            self.trace,
            "semantic_planner.semantic_plan.fallback",
            summary=f"semantic_plan (repair) fell back to rules: {reason[:200]}",
            ids={},
            severity="warning",
        )
        return SemanticPlan(
            rolling_plan=rolling,
            risk_points=[],
            verification_strategies=[],
            repair_policy=None,
            producer_source="rules_fallback",
        )


class PlannerDecisionProducer:
    """Produces a ``PlannerDecision`` via model, falling back to rules."""

    def __init__(
        self,
        *,
        model_runner: Any | None,
        rule_replanner: Replanner,
        trace: Any | None = None,
    ) -> None:
        self.model_runner = model_runner
        self.rule_replanner = rule_replanner
        self.trace = trace

    def produce(
        self,
        signal: dict[str, Any],
        *,
        context_payload: dict[str, Any],
        risk_points: list[RiskPoint],
        verification_strategies: list[VerificationStrategy],
        repair_policy: RepairPolicy | None,
    ) -> PlannerDecision:
        if self.model_runner is None:
            return self._fallback(signal, reason="model_runner unavailable")
        try:
            payload = self._call_model(
                signal, context_payload, risk_points, verification_strategies, repair_policy
            )
            decision = PlannerDecision.from_dict(payload)
            decision = PlannerDecision(
                decision=decision.decision,
                reason=decision.reason,
                next_action=decision.next_action,
                risk_points_triggered=decision.risk_points_triggered,
                verification_strategy_selected=decision.verification_strategy_selected,
                producer_source="model",
            )
            _emit(
                self.trace,
                "semantic_planner.planner_decision.model_ok",
                summary=f"planner_decision ({decision.decision.value}) produced by model",
                ids=_request_ids(context_payload),
            )
            return decision
        except Exception as exc:  # noqa: BLE001
            return self._fallback(signal, reason=f"model error: {type(exc).__name__}: {exc}")

    def _call_model(
        self,
        signal: dict[str, Any],
        context_payload: dict[str, Any],
        risk_points: list[RiskPoint],
        verification_strategies: list[VerificationStrategy],
        repair_policy: RepairPolicy | None,
    ) -> dict[str, Any]:
        ids = _request_ids(context_payload)
        signal_json = json.dumps(signal, ensure_ascii=False, sort_keys=True, default=str)
        risks_json = json.dumps(
            [r.to_dict() for r in risk_points], ensure_ascii=False, sort_keys=True, default=str
        )
        strategies_json = json.dumps(
            [s.to_dict() for s in verification_strategies],
            ensure_ascii=False,
            sort_keys=True,
            default=str,
        )
        policy_json = json.dumps(
            repair_policy.to_dict() if repair_policy else None,
            ensure_ascii=False,
            sort_keys=True,
            default=str,
        )
        prompt = (
            "Decide the next planner action given the signal, risk points, "
            "verification strategies, and repair policy below. Return JSON only "
            "with keys: decision (one of continue, retry_with_new_context, "
            "read_fresh_file, repair_failure, rerun_verification, ask_user, "
            "require_review, abort, finalize_with_warnings), reason (str), "
            "next_action (one of InspectWorkspace, ReadRelevantFiles, SearchCode, "
            "AnalyzeIssue, ProposeChangeSet, ApplyMutation, RunVerification, "
            "ParseFailure, RepairChange, AskUser, RequireReview, Finalize or null), "
            "risk_points_triggered (list of risk_id), "
            "verification_strategy_selected (strategy_id or null).\n\n"
            "Signal:\n" + signal_json + "\n\nRisk points:\n" + risks_json
            + "\n\nVerification strategies:\n" + strategies_json
            + "\n\nRepair policy:\n" + policy_json
        )
        request = ModelTurnRequest(
            request_id=f"req_{uuid4().hex[:12]}",
            run_id=ids["run_id"],
            session_id=ids["session_id"],
            task_id=ids["task_id"],
            phase_id=ids["phase_id"],
            action_id=ids["action_id"],
            purpose=ModelPurpose.PLANNER_DECISION,
            messages=[
                ModelMessage(
                    role=ModelRole.USER,
                    content=[ContentBlock.from_text(prompt)],
                )
            ],
            tools=[],
            tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.NONE, max_tool_calls=0),
            model_preferences=ModelPreferences(json_mode=True, max_output_tokens=800),
            budget=ModelBudget(max_retries=1, max_output_tokens=800),
            context_metadata={"producer": "planner_decision"},
        )
        runner = self.model_runner
        if runner is None:
            raise RuntimeError("model_runner unavailable")
        result = runner.run_turn(request)
        if result.status != ModelTurnStatus.SUCCESS or result.assistant_message is None:
            raise RuntimeError(f"model turn failed: {result.status}")
        return _json_payload(result.assistant_message.text)

    def _fallback(self, signal: dict[str, Any], *, reason: str) -> PlannerDecision:
        rule_decision: ReplanDecision = self.rule_replanner.decide(signal)
        _emit(
            self.trace,
            "semantic_planner.planner_decision.fallback",
            summary=f"planner_decision fell back to rules: {reason[:200]}",
            ids={},
            severity="warning",
        )
        return PlannerDecision(
            decision=rule_decision.decision,
            reason=rule_decision.reason,
            next_action=rule_decision.next_action,
            risk_points_triggered=[],
            verification_strategy_selected=None,
            producer_source="rules_fallback",
        )


@dataclass
class PlannerProducerBundle:
    """Bundle of the three semantic producers wired into ``Planner``."""

    task_contract: TaskContractProducer
    semantic_plan: SemanticPlanProducer
    planner_decision: PlannerDecisionProducer

    @classmethod
    def with_rule_fallback(
        cls,
        *,
        rule_builder: TaskContractBuilder,
        rule_planner: SemanticPlanner,
        rule_replanner: Replanner,
        model_runner: Any | None = None,
        trace: Any | None = None,
    ) -> "PlannerProducerBundle":
        """Build a bundle; when ``model_runner`` is None all producers fall back."""
        return cls(
            task_contract=TaskContractProducer(
                model_runner=model_runner,
                rule_builder=rule_builder,
                trace=trace,
            ),
            semantic_plan=SemanticPlanProducer(
                model_runner=model_runner,
                rule_planner=rule_planner,
                trace=trace,
            ),
            planner_decision=PlannerDecisionProducer(
                model_runner=model_runner,
                rule_replanner=rule_replanner,
                trace=trace,
            ),
        )
