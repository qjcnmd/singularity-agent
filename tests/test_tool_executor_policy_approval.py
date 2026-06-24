import json
from pathlib import Path
from typing import Any

import pytest
from pydantic import BaseModel, ConfigDict

from singularity.policy import (
    ApprovalGate,
    ApprovalGrant,
    ApprovalMode,
    DecisionOutcome,
    PolicyConfig,
    PolicyDecision,
    PolicyRequest,
    RiskLevel,
)
from singularity.interaction import InteractionController, UserDecision
from singularity.tools import ToolPolicy, ToolRegistry, ToolExecutor, ToolSpec
from singularity.jsonl_trace import JsonlTraceRecorder


class EmptyInput(BaseModel):
    model_config = ConfigDict(extra="forbid")


def make_tool_call(name: str, arguments: dict[str, Any] | None = None) -> dict[str, Any]:
    return {
        "id": "call_policy",
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments or {})},
    }


class SequencedPolicyEngine:
    def __init__(self, outcomes: list[DecisionOutcome]) -> None:
        self.outcomes = outcomes
        self.requests: list[PolicyRequest] = []
        self.registered: list[ApprovalGrant] = []

    @property
    def config(self) -> PolicyConfig:
        return PolicyConfig(approval_mode=ApprovalMode.INTERACTIVE)

    def evaluate(self, request: PolicyRequest) -> PolicyDecision:
        self.requests.append(request)
        outcome = self.outcomes.pop(0) if self.outcomes else DecisionOutcome.ALLOW
        return PolicyDecision(
            request_id=request.request_id,
            outcome=outcome,
            reason=f"{outcome.value} from test",
            risk_level=RiskLevel.MEDIUM,
        )

    def enforce(self, request: PolicyRequest) -> PolicyDecision:
        return self.evaluate(request)

    def register_grant(self, grant: ApprovalGrant) -> None:
        self.registered.append(grant)


class ApprovingProvider:
    def request_decision(self, prompt):
        return UserDecision(
            prompt_id=prompt.prompt_id,
            decision="approve",
            reason="approved in tool executor test",
            decided_by="test-user",
        )

    def request_clarification(self, request):
        raise AssertionError("not used")


def component_with_policy(
    tmp_path: Path,
    policy_engine: SequencedPolicyEngine,
    *,
    approval_gate: Any | None = None,
    handler_calls: list[str] | None = None,
) -> ToolExecutor:
    calls = handler_calls if handler_calls is not None else []

    def handler(_args: EmptyInput) -> dict[str, str]:
        calls.append("called")
        return {"ok": "yes"}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="read_policy",
            description="policy",
            input_model=EmptyInput,
            handler=handler,
        )
    )
    return ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=policy_engine,  # type: ignore[arg-type]
        approval_gate=approval_gate,
    )


def test_require_review_uses_approval_gate_and_consumes_grant(tmp_path: Path) -> None:
    calls: list[str] = []
    policy_engine = SequencedPolicyEngine(
        [DecisionOutcome.REQUIRE_REVIEW, DecisionOutcome.ALLOW]
    )
    gate = ApprovalGate(
        PolicyConfig(workspace_root=tmp_path, approval_mode=ApprovalMode.INTERACTIVE),
        interaction=InteractionController(provider=ApprovingProvider()),
    )
    component = component_with_policy(
        tmp_path, policy_engine, approval_gate=gate, handler_calls=calls
    )

    result = component.execute_tool_call(make_tool_call("read_policy"))

    assert result.ok is True
    assert calls == ["called"]
    assert len(policy_engine.registered) == 1
    assert result.metadata["approval_grant_id"] == policy_engine.registered[0].grant_id


def test_tool_executor_requires_injected_policy_engine(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="read_policy",
            description="policy",
            input_model=EmptyInput,
            handler=lambda _args: {"ok": "yes"},
        )
    )

    with pytest.raises(ValueError, match="policy_engine is required"):
        ToolExecutor(
            registry=registry,
            policy=ToolPolicy.read_only(),
            trace=JsonlTraceRecorder.create(tmp_path),
            workspace_root=tmp_path,
        )


def test_non_interactive_require_review_fails_closed(tmp_path: Path) -> None:
    calls: list[str] = []
    component = component_with_policy(
        tmp_path,
        SequencedPolicyEngine([DecisionOutcome.REQUIRE_REVIEW]),
        approval_gate=None,
        handler_calls=calls,
    )

    result = component.execute_tool_call(make_tool_call("read_policy"))

    assert result.ok is False
    assert result.error_code == "approval_required"
    assert calls == []


def test_non_allow_policy_outcomes_do_not_call_handler(tmp_path: Path) -> None:
    for outcome, code in [
        (DecisionOutcome.DENY, "policy_denied"),
        (DecisionOutcome.ASK_USER, "policy_ask_user_required"),
        (DecisionOutcome.ESCALATE, "policy_escalation_required"),
        (DecisionOutcome.SANDBOX_REQUIRED, "sandbox_required"),
    ]:
        calls: list[str] = []
        component = component_with_policy(
            tmp_path,
            SequencedPolicyEngine([outcome]),
            handler_calls=calls,
        )

        result = component.execute_tool_call(make_tool_call("read_policy"))

        assert result.ok is False
        assert result.error_code == code
        assert calls == []
