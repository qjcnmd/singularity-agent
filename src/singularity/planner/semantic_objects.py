"""Semantic Planner structured objects.

This module defines the structured runtime objects used by the model-driven
Semantic Planner producers (``semantic_producers.py``). These dataclasses are
**internal structured objects** — they are NOT directly projected into model
context. The model-visible projection of planner state (including summaries of
the objects defined here) is owned by ``PlannerContextRenderer`` in
``context.py``, which is the single chokepoint for what the main task model
sees.

Model-visible vs internal trace/debug boundary:
- Model-visible: ``PlannerContextRenderer.render()`` projects *summaries* of
  ``RiskPoint`` / ``VerificationStrategy`` / ``RepairPolicy`` (a few fields
  each, not the full objects).
- Internal trace/debug: the full ``SemanticPlan`` / ``PlannerDecision`` /
  ``RiskPoint`` / ``VerificationStrategy`` / ``RepairPolicy`` objects are
  persisted into ``TaskState`` (as dicts) and recorded in trace events.

Producer origin tagging: every object carries ``producer_source`` ("model"
when produced by a real model call, "rules" / "rules_fallback" when produced
by the rule fallback path) so downstream consumers can tell whether a given
plan/decision was model-driven or rule-driven.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from singularity.planner.models import ActionKind, ReplanDecisionKind
from singularity.planner.semantic import RollingPlan


@dataclass(frozen=True)
class RiskPoint:
    """A foreseeable risk the planner should account for.

    ``trigger_conditions`` are short human-readable phrases describing when
    the risk is likely to materialize; ``mitigation_strategy`` is the planned
    response. ``severity`` is one of ``"low" | "medium" | "high" | "critical"``.
    ``acceptance_criterion_id`` optionally binds the risk to a specific
    acceptance criterion from ``TaskContract``.
    """

    risk_id: str
    description: str
    trigger_conditions: list[str]
    mitigation_strategy: str
    severity: str = "medium"
    acceptance_criterion_id: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "risk_id": self.risk_id,
            "description": self.description,
            "trigger_conditions": list(self.trigger_conditions),
            "mitigation_strategy": self.mitigation_strategy,
            "severity": self.severity,
            "acceptance_criterion_id": self.acceptance_criterion_id,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> RiskPoint:
        return cls(
            risk_id=str(payload["risk_id"]),
            description=str(payload.get("description") or ""),
            trigger_conditions=[str(item) for item in payload.get("trigger_conditions") or []],
            mitigation_strategy=str(payload.get("mitigation_strategy") or ""),
            severity=str(payload.get("severity") or "medium"),
            acceptance_criterion_id=payload.get("acceptance_criterion_id"),
        )


@dataclass(frozen=True)
class VerificationStrategy:
    """How a specific acceptance criterion will be verified.

    ``command`` is the primary verification command (argv list, executable
    by ``run_command``). ``fallback_commands`` are alternative commands to try
    if the primary fails. ``expected_outcome`` is a short human-readable
    description of what success looks like. ``evidence_key`` binds the strategy
    to an ``EvidenceRequirement`` from ``TaskContract`` when applicable.
    """

    strategy_id: str
    acceptance_criterion_id: str | None
    command: list[str] | None
    expected_outcome: str
    fallback_commands: list[list[str]] = field(default_factory=list)
    evidence_key: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "strategy_id": self.strategy_id,
            "acceptance_criterion_id": self.acceptance_criterion_id,
            "command": list(self.command) if self.command is not None else None,
            "expected_outcome": self.expected_outcome,
            "fallback_commands": [list(cmd) for cmd in self.fallback_commands],
            "evidence_key": self.evidence_key,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> VerificationStrategy:
        raw_command = payload.get("command")
        command = [str(item) for item in raw_command] if raw_command is not None else None
        return cls(
            strategy_id=str(payload["strategy_id"]),
            acceptance_criterion_id=payload.get("acceptance_criterion_id"),
            command=command,
            expected_outcome=str(payload.get("expected_outcome") or ""),
            fallback_commands=[
                [str(item) for item in cmd]
                for cmd in payload.get("fallback_commands") or []
            ],
            evidence_key=payload.get("evidence_key"),
        )


@dataclass(frozen=True)
class RepairPolicy:
    """Policy governing how failures of a given category are repaired.

    ``failure_category_pattern`` matches ``FailureAnalysisResult.failure_category``.
    ``allowed_repair_actions`` are ``ActionKind`` names permitted under this
    policy. ``max_attempts`` bounds total repair attempts for this category;
    ``escalation_threshold`` is the failure count at which the planner escalates
    to ``ASK_USER`` instead of continuing to repair.
    """

    failure_category_pattern: str
    allowed_repair_actions: list[str]
    max_attempts: int = 3
    escalation_threshold: int = 2
    verification_strategy_id: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "failure_category_pattern": self.failure_category_pattern,
            "allowed_repair_actions": list(self.allowed_repair_actions),
            "max_attempts": self.max_attempts,
            "escalation_threshold": self.escalation_threshold,
            "verification_strategy_id": self.verification_strategy_id,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> RepairPolicy:
        return cls(
            failure_category_pattern=str(payload["failure_category_pattern"]),
            allowed_repair_actions=[str(item) for item in payload.get("allowed_repair_actions") or []],
            max_attempts=int(payload.get("max_attempts") or 3),
            escalation_threshold=int(payload.get("escalation_threshold") or 2),
            verification_strategy_id=payload.get("verification_strategy_id"),
        )


@dataclass(frozen=True)
class SemanticPlan:
    """A semantic plan: a ``RollingPlan`` plus risk/verification/repair policy.

    This wraps the existing ``RollingPlan`` (which is what the rest of the
    planner/runtime already consumes) and attaches the new structured objects
    produced by the model-driven Semantic Planner. ``producer_source`` records
    whether this plan came from the model ("model") or the rule fallback
    ("rules" / "rules_fallback").
    """

    rolling_plan: RollingPlan
    risk_points: list[RiskPoint] = field(default_factory=list)
    verification_strategies: list[VerificationStrategy] = field(default_factory=list)
    repair_policy: RepairPolicy | None = None
    producer_source: str = "rules"

    def to_dict(self) -> dict[str, Any]:
        return {
            "rolling_plan": self.rolling_plan.to_dict(),
            "risk_points": [item.to_dict() for item in self.risk_points],
            "verification_strategies": [item.to_dict() for item in self.verification_strategies],
            "repair_policy": self.repair_policy.to_dict() if self.repair_policy else None,
            "producer_source": self.producer_source,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> SemanticPlan:
        rolling_plan = RollingPlan.from_dict(payload.get("rolling_plan") or {})
        return cls(
            rolling_plan=rolling_plan,
            risk_points=[RiskPoint.from_dict(item) for item in payload.get("risk_points") or []],
            verification_strategies=[
                VerificationStrategy.from_dict(item)
                for item in payload.get("verification_strategies") or []
            ],
            repair_policy=RepairPolicy.from_dict(payload["repair_policy"])
            if payload.get("repair_policy")
            else None,
            producer_source=str(payload.get("producer_source") or "rules"),
        )


@dataclass(frozen=True)
class PlannerDecision:
    """A planner replan decision enriched with model-driven rationale.

    This wraps the existing ``ReplanDecision`` fields (``decision``,
    ``reason``, ``next_action``) and adds the risk points that triggered the
    decision and the verification strategy the planner selected. ``producer_source``
    records whether this decision came from the model ("model") or the rule
    fallback ("rules" / "rules_fallback").
    """

    decision: ReplanDecisionKind
    reason: str
    next_action: ActionKind | None = None
    risk_points_triggered: list[str] = field(default_factory=list)
    verification_strategy_selected: str | None = None
    producer_source: str = "rules"

    def to_dict(self) -> dict[str, Any]:
        return {
            "decision": self.decision.value,
            "reason": self.reason,
            "next_action": self.next_action.value if self.next_action else None,
            "risk_points_triggered": list(self.risk_points_triggered),
            "verification_strategy_selected": self.verification_strategy_selected,
            "producer_source": self.producer_source,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> PlannerDecision:
        next_action_raw = payload.get("next_action")
        next_action = ActionKind(next_action_raw) if next_action_raw else None
        return cls(
            decision=ReplanDecisionKind(payload["decision"]),
            reason=str(payload.get("reason") or ""),
            next_action=next_action,
            risk_points_triggered=[str(item) for item in payload.get("risk_points_triggered") or []],
            verification_strategy_selected=payload.get("verification_strategy_selected"),
            producer_source=str(payload.get("producer_source") or "rules"),
        )
