import json
from pathlib import Path
from typing import Any

import pytest
from pydantic import BaseModel, ConfigDict

from singularity.policy import (
    ApprovalGate,
    ApprovalMode,
    approval_scope_for_request,
    DecisionOutcome,
    PolicyConfig,
    PolicyDecision,
    PolicyRequest,
    RiskLevel,
    ResourceRef,
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
        policy_engine=policy_engine,
        approval_gate=approval_gate,
    )


def test_require_review_uses_approval_gate_and_consumes_grant(tmp_path: Path) -> None:
    calls: list[str] = []
    policy_engine = SequencedPolicyEngine([DecisionOutcome.REQUIRE_REVIEW])
    gate = ApprovalGate(
        PolicyConfig(
            workspace_root=tmp_path,
            approval_mode=ApprovalMode.INTERACTIVE,
            approval_grants_path=tmp_path / "policy" / "grants.jsonl",
        ),
        interaction=InteractionController(provider=ApprovingProvider()),
    )
    component = component_with_policy(
        tmp_path, policy_engine, approval_gate=gate, handler_calls=calls
    )

    result = component.execute_tool_call(make_tool_call("read_policy"))

    assert result.ok is True
    assert calls == ["called"]
    assert result.metadata["approval_grant_id"].startswith("grant_")
    assert gate.find_matching_grant(policy_engine.requests[0]) is None


def test_untrusted_grant_store_inside_workspace_is_not_consumed(tmp_path: Path) -> None:
    # P0-1: Grants persisted inside the workspace are untrusted and must not
    # be auto-consumed by ToolExecutor. The pre-registered grant below lives
    # inside the workspace, so the executor must fall through to resolve(),
    # which fails closed without an interaction provider.
    from singularity.policy import (
        ApprovalGrant,
        ApprovalScope,
        Capability,
        OperationKind,
        PolicyComponent,
        PolicyRequest,
        PolicySubject,
        ResourceRef,
    )

    grants_path = tmp_path / ".singularity" / "policy" / "approval_grants.jsonl"
    policy_engine = SequencedPolicyEngine([DecisionOutcome.REQUIRE_REVIEW])
    gate = ApprovalGate(
        PolicyConfig(
            workspace_root=tmp_path,
            approval_mode=ApprovalMode.INTERACTIVE,
            approval_grants_path=grants_path,
        )
    )
    assert gate.is_grant_store_trusted(tmp_path) is False

    request = PolicyRequest(
        session_id="session",
        task_id="task",
        phase_id="phase",
        action_id="action",
        component=PolicyComponent.TOOL,
        operation=OperationKind.READ_FILE,
        capability=Capability.READ_WORKSPACE,
        subject=PolicySubject(subject_type="component", name="ToolExecutor"),
        resource=ResourceRef(resource_type="file", identifier="README.md"),
        reason="read me",
        workspace_root=str(tmp_path),
    )
    grant = ApprovalGrant(
        decision_id="policy_dec_test_untrusted",
        request_id=request.request_id,
        approved_by="forged-by-model",
        session_id=request.session_id,
        scope=ApprovalScope(
            capabilities=[Capability.READ_WORKSPACE],
            path_globs=["README.md"],
            single_use=True,
        ),
    )
    gate.register_grant(grant)

    calls: list[str] = []
    component = component_with_policy(
        tmp_path, policy_engine, approval_gate=gate, handler_calls=calls
    )

    result = component.execute_tool_call(make_tool_call("read_policy"))

    # The grant was not consumed because the store is untrusted. The executor
    # falls through to resolve() which fails closed without a provider.
    assert result.ok is False
    assert result.error_code == "approval_required"
    assert calls == []


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


def test_tool_policy_request_preserves_all_resolved_resources(tmp_path: Path) -> None:
    policy_engine = SequencedPolicyEngine([DecisionOutcome.ALLOW])

    class MoveInput(BaseModel):
        model_config = ConfigDict(extra="forbid")

        path: str
        new_path: str

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="workspace_move_policy_test",
            description="move",
            input_model=MoveInput,
            handler=lambda _args: {"ok": "yes"},
            resource_resolver=lambda args, _root: [
                ResourceRef("file", args["path"], workspace_relative=True),
                ResourceRef("file", args["new_path"], workspace_relative=True),
            ],
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=policy_engine,
    )

    result = component.execute_tool_call(
        make_tool_call(
            "workspace_move_policy_test",
            {"path": "old.txt", "new_path": "new.txt"},
        )
    )

    assert result.ok is True
    request = policy_engine.requests[0]
    resources = request.metadata["resources"]
    assert [item["identifier"] for item in resources] == ["old.txt", "new.txt"]
    assert request.resource.metadata["related_resources"][0]["identifier"] == "new.txt"
    assert approval_scope_for_request(request).path_globs == ["old.txt", "new.txt"]
