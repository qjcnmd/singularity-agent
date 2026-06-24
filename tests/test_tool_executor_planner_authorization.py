import json
from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict

from singularity.planner import (
    ActionKind,
    AgentAction,
    AuthorizationDecision,
    RiskLevel,
)
from singularity.tools import PermissionLevel, ToolPolicy, ToolRegistry, ToolExecutor, ToolSpec
from singularity.jsonl_trace import JsonlTraceRecorder
from tests.tool_executor_helpers import make_test_policy_engine


class EmptyInput(BaseModel):
    model_config = ConfigDict(extra="forbid")


def make_tool_call(name: str, arguments: dict[str, Any] | None = None) -> dict[str, Any]:
    return {
        "id": "call_planner",
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments or {})},
    }


class DenyingPlanner:
    def __init__(self, *, allowed: bool = True, error_code: str = "action_not_allowed") -> None:
        self.allowed = allowed
        self.error_code = error_code
        self.calls: list[dict[str, Any]] = []
        self.observations: list[dict[str, Any]] = []

    def authorize_tool_call(self, **kwargs: Any) -> AuthorizationDecision:
        self.calls.append(kwargs)
        if self.allowed:
            return AuthorizationDecision(
                allowed=True,
                action=AgentAction(
                    kind=ActionKind.READ_RELEVANT_FILES,
                    intent="read",
                    phase_id="inspecting_workspace",
                    preconditions=[],
                    allowed_tools=[kwargs["tool_name"]],
                    expected_evidence=["inspected_files"],
                    risk_level=RiskLevel.LOW,
                ),
            )
        return AuthorizationDecision(allowed=False, error_code=self.error_code, reason="denied")

    def update_from_tool_result(self, **kwargs: Any) -> None:
        self.calls.append({"update": kwargs})

    def record_policy_observation(self, observation: dict[str, Any]) -> None:
        self.observations.append(observation)


def make_component(
    tmp_path: Path,
    *,
    planner: Any | None,
    handler_calls: list[str],
) -> ToolExecutor:
    def handler(_args: EmptyInput) -> dict[str, str]:
        handler_calls.append("called")
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
        planner=planner,
        policy_engine=make_test_policy_engine(tmp_path),
    )


def test_planner_denial_prevents_handler_execution(tmp_path: Path) -> None:
    calls: list[str] = []
    component = make_component(tmp_path, planner=DenyingPlanner(allowed=False), handler_calls=calls)

    result = component.execute_tool_call(make_tool_call("read_policy"))

    assert result.ok is False
    assert result.error_code == "action_not_allowed"
    assert calls == []


def test_planner_absent_standalone_mode_blocks_write_tools(tmp_path: Path) -> None:
    calls: list[str] = []

    class WriteInput(BaseModel):
        model_config = ConfigDict(extra="forbid")
        path: str

    def handler(_args: WriteInput) -> dict[str, str]:
        calls.append("called")
        return {"ok": "yes"}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="write_file",
            description="write",
            input_model=WriteInput,
            handler=handler,
            permission_level=PermissionLevel.WRITE,
            risk_tags=("write",),
            uses_mutation_manager=True,
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(make_tool_call("write_file", {"path": "x.txt"}))

    assert result.ok is False
    assert result.error_code in {"policy_denied", "permission_denied"}
    assert calls == []


def test_planner_absent_standalone_mode_allows_read_tools(tmp_path: Path) -> None:
    readme = tmp_path / "README.md"
    readme.write_text("hello", encoding="utf-8")
    component = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(make_tool_call("read_file", {"path": "README.md"}))

    assert result.ok is True


def test_planner_denial_records_policy_observation_when_available(tmp_path: Path) -> None:
    calls: list[str] = []
    planner = DenyingPlanner(allowed=False)
    component = make_component(tmp_path, planner=planner, handler_calls=calls)

    result = component.execute_tool_call(make_tool_call("read_policy"))

    assert result.ok is False
    assert planner.observations

