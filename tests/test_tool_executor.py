import json
import time
from pathlib import Path
from typing import Any

import pytest
from pydantic import BaseModel, Field

from singularity.edit import EditExecutor
from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.planner import (
    ActionKind,
    AgentAction,
    AuthorizationDecision,
    Planner,
    RiskLevel,
    TaskStatus,
)
from singularity.tools import (
    PermissionLevel,
    ToolExecutionRequest,
    ToolExecutor,
    ToolPolicy,
    ToolRegistry,
    ToolSpec,
)
from singularity.tools.edit import register_edit_tools
from singularity.workspace import WorkspaceMutationManager
from tests.tool_executor_helpers import default_policy_engine, make_test_policy_engine


def make_tool_call(
    name: str, arguments: dict[str, Any], *, tool_call_id: str = "call_1"
) -> dict[str, Any]:
    return {
        "id": tool_call_id,
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }


def make_raw_tool_call(
    name: str, arguments: str, *, tool_call_id: str = "call_1"
) -> dict[str, Any]:
    return {
        "id": tool_call_id,
        "type": "function",
        "function": {"name": name, "arguments": arguments},
    }


def make_component(tmp_path: Path) -> ToolExecutor:
    return ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )


def test_tool_executor_successfully_calls_registered_read_only_tool(tmp_path: Path) -> None:
    readme = tmp_path / "README.md"
    readme.write_text("hello from component", encoding="utf-8")
    component = make_component(tmp_path)

    result = component.execute_tool_call(
        make_tool_call("read_file", {"path": "README.md", "max_bytes": 100})
    )

    assert result.ok is True
    assert result.error_code is None
    assert result.content["path"] == "README.md"
    assert result.content["content"] == "hello from component"


def test_tool_executor_rejects_unknown_tool(tmp_path: Path) -> None:
    component = make_component(tmp_path)

    result = component.execute_tool_call(make_tool_call("missing_tool", {}))

    assert result.ok is False
    assert result.error_code == "tool_not_found"


def test_tool_executor_rejects_bad_arguments_json(tmp_path: Path) -> None:
    component = make_component(tmp_path)

    result = component.execute_tool_call(make_raw_tool_call("read_file", "{not json"))

    assert result.ok is False
    assert result.error_code == "bad_arguments_json"


def test_tool_executor_rejects_pydantic_validation_errors(tmp_path: Path) -> None:
    component = make_component(tmp_path)

    result = component.execute_tool_call(
        make_tool_call("read_file", {"path": "README.md", "max_bytes": 0})
    )

    assert result.ok is False
    assert result.error_code == "validation_error"


class WriteInput(BaseModel):
    path: str


def test_tool_executor_rejects_disabled_tool_like_unknown_tool(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="disabled_tool",
            description="disabled",
            input_model=WriteInput,
            handler=lambda _args: {"ok": True},
            enabled=False,
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(make_tool_call("disabled_tool", {"path": "README.md"}))

    assert result.ok is False
    assert result.error_code == "tool_not_found"


def test_tool_executor_policy_denies_write_tools_by_default(tmp_path: Path) -> None:
    called = False

    def write_handler(_args: WriteInput) -> dict[str, str]:
        nonlocal called
        called = True
        return {"status": "wrote"}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="write_file",
            version="0.0.1",
            description="Pretend to write a file.",
            input_model=WriteInput,
            handler=write_handler,
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
    assert called is False


class EmptyInput(BaseModel):
    pass


class TimeoutWriteInput(BaseModel):
    path: str


def slow_process_write_handler(args: TimeoutWriteInput) -> str:
    time.sleep(0.2)
    Path(args.path).write_text("late", encoding="utf-8")
    return "done"


def test_tool_executor_timeout_returns_timeout_error(tmp_path: Path) -> None:
    def slow_handler(_args: EmptyInput) -> str:
        time.sleep(0.2)
        return "done"

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="slow_read",
            version="0.0.1",
            description="Slow read.",
            input_model=EmptyInput,
            handler=slow_handler,
            permission_level=PermissionLevel.READ_ONLY,
            risk_tags=("read",),
            timeout_seconds=0.01,
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(make_tool_call("slow_read", {}))

    assert result.ok is False
    assert result.error_code == "timeout"


def test_tool_executor_timeout_does_not_wait_for_started_thread_handler(tmp_path: Path) -> None:
    marker = tmp_path / "late.txt"

    def slow_handler(_args: EmptyInput) -> str:
        time.sleep(0.25)
        marker.write_text("settled", encoding="utf-8")
        return "done"

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="slow_read",
            version="0.0.1",
            description="Slow read.",
            input_model=EmptyInput,
            handler=slow_handler,
            permission_level=PermissionLevel.READ_ONLY,
            risk_tags=("read",),
            timeout_seconds=0.01,
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    started = time.perf_counter()
    result = component.execute_tool_call(make_tool_call("slow_read", {}))

    assert result.error_code == "timeout"
    assert time.perf_counter() - started < 0.20
    assert marker.exists() is False
    assert result.metadata["timeout_untrusted_state"] is True
    time.sleep(0.30)
    assert marker.exists() is True


def test_tool_executor_timeout_terminates_process_isolated_handler(tmp_path: Path) -> None:
    marker = tmp_path / "late-process.txt"
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="slow_process_read",
            version="0.0.1",
            description="Slow process-isolated read.",
            input_model=TimeoutWriteInput,
            handler=slow_process_write_handler,
            permission_level=PermissionLevel.READ_ONLY,
            risk_tags=("read",),
            timeout_seconds=0.01,
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(make_tool_call("slow_process_read", {"path": str(marker)}))
    time.sleep(0.25)

    assert result.error_code == "timeout"
    assert marker.exists() is False
    assert result.metadata["handler_isolation"] == "process"
    assert result.metadata["timeout_terminated"] is True
    assert result.metadata["timeout_untrusted_state"] is False


def test_tool_executor_truncates_oversized_output_with_head_and_tail(tmp_path: Path) -> None:
    def large_handler(_args: EmptyInput) -> str:
        return "A" * 80 + "Z" * 80

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="large_read",
            version="0.0.1",
            description="Large read.",
            input_model=EmptyInput,
            handler=large_handler,
            permission_level=PermissionLevel.READ_ONLY,
            risk_tags=("read",),
            max_output_chars=60,
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(make_tool_call("large_read", {}))

    assert result.ok is True
    assert result.truncated is True
    assert result.metadata["original_chars"] == 160
    assert result.metadata["returned_chars"] <= 60
    assert str(result.content).startswith("A")
    assert str(result.content).endswith("Z")


def test_tool_executor_writes_audit_trace(tmp_path: Path) -> None:
    trace = JsonlTraceRecorder.create(tmp_path)
    component = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=trace,
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    component.execute_tool_call(
        make_tool_call("list_files", {"path": ".", "max_depth": 1}, tool_call_id="call_list")
    )

    events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    tool_events = [event for event in events if event["event"] == "tool_call"]
    assert len(tool_events) == 1
    audit = tool_events[0]["data"]
    assert audit["tool_call_id"] == "call_list"
    assert audit["tool_name"] == "list_files"
    assert audit["argument_summary"]["shape"] == "object"
    assert audit["argument_summary"]["keys"] == ["max_depth", "path"]
    assert audit["permission_level"] == "read_only"
    assert audit["status"] == "ok"
    assert audit["error_code"] is None
    assert audit["cache_hit"] is False
    assert "duration_seconds" in audit
    assert "output_digest" in audit


def test_tool_executor_execute_request_records_protocol_trace_ids(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("hello from request", encoding="utf-8")
    trace = JsonlTraceRecorder.create(tmp_path)
    component = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=trace,
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )
    request = ToolExecutionRequest(
        tool_call_id="call_request",
        tool_name="read_file",
        raw_arguments='{"path":"README.md"}',
        normalized_arguments={"path": "README.md"},
        batch_id="batch_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="inspection",
        model_request_id="model_req_1",
        model_response_id="model_resp_1",
        argument_digest="argument_digest_1",
    )

    result = component.execute_request(request)

    assert result.ok is True
    events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    audit = [event["data"] for event in events if event["event"] == "tool_call"][-1]
    assert audit["tool_call_id"] == "call_request"
    assert audit["batch_id"] == "batch_1"
    assert audit["model_request_id"] == "model_req_1"
    assert audit["model_response_id"] == "model_resp_1"
    assert audit["argument_digest"] == "argument_digest_1"
    assert audit["policy_decision_id"] == result.metadata["policy_decision_id"]
    assert audit["output_digest"] == result.metadata["output_digest"]


def test_tool_executor_execute_request_keeps_final_schema_validation(tmp_path: Path) -> None:
    component = make_component(tmp_path)
    request = ToolExecutionRequest(
        tool_call_id="call_invalid_request",
        tool_name="read_file",
        raw_arguments='{"path":"README.md","max_bytes":0}',
        normalized_arguments={"path": "README.md", "max_bytes": 0},
        batch_id="batch_1",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="inspection",
        model_request_id="model_req_1",
        model_response_id="model_resp_1",
    )

    result = component.execute_request(request)

    assert result.ok is False
    assert result.error_code == "validation_error"


class EchoInput(BaseModel):
    value: str = Field(..., min_length=1)


def test_tool_executor_uses_run_cache_for_cacheable_read_only_tools(tmp_path: Path) -> None:
    calls: list[str] = []

    def echo_handler(args: EchoInput) -> dict[str, str]:
        calls.append(args.value)
        return {"value": args.value}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="echo_read",
            version="0.0.1",
            description="Echo input.",
            input_model=EchoInput,
            handler=echo_handler,
            permission_level=PermissionLevel.READ_ONLY,
            risk_tags=("read",),
            cacheable=True,
        )
    )
    trace = JsonlTraceRecorder.create(tmp_path)
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=trace,
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    first = component.execute_tool_call(
        make_tool_call("echo_read", {"value": "same"}, tool_call_id="call_cache_first")
    )
    second = component.execute_tool_call(
        make_tool_call("echo_read", {"value": "same"}, tool_call_id="call_cache_second")
    )

    assert first.ok is True
    assert second.ok is True
    assert second.metadata["cache_hit"] is True
    assert calls == ["same"]

    events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    assert [event["data"]["cache_hit"] for event in events if event["event"] == "tool_call"] == [
        False,
        True,
    ]


def test_tool_executor_asks_planner_before_executing_tool(tmp_path: Path) -> None:
    called = False

    def write_handler(_args: WriteInput) -> dict[str, str]:
        nonlocal called
        called = True
        return {"status": "wrote"}

    class DenyingPlanner:
        def authorize_tool_call(self, **_kwargs: Any) -> AuthorizationDecision:
            return AuthorizationDecision(
                allowed=False,
                error_code="action_not_allowed",
                reason="write not allowed in current phase",
            )

        def update_from_tool_result(self, **_kwargs: Any) -> None:
            raise AssertionError("denied tool results should not be recorded as executed")

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="write_file",
            version="0.0.1",
            description="Pretend to write a file.",
            input_model=WriteInput,
            handler=write_handler,
            permission_level=PermissionLevel.WRITE,
            risk_tags=("write",),
            uses_mutation_manager=True,
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        planner=DenyingPlanner(),
        policy_engine=default_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(make_tool_call("write_file", {"path": "x.txt"}))

    assert result.ok is False
    assert result.error_code == "action_not_allowed"
    assert result.error.details["planner_reason"] == "write not allowed in current phase"
    assert called is False


def test_tool_executor_reports_executed_tool_result_to_planner(tmp_path: Path) -> None:
    class RecordingPlanner:
        def __init__(self) -> None:
            self.updates: list[dict[str, Any]] = []

        def authorize_tool_call(self, **_kwargs: Any) -> AuthorizationDecision:
            return AuthorizationDecision(
                allowed=True,
                action=AgentAction(
                    kind=ActionKind.READ_RELEVANT_FILES,
                    intent="read",
                    phase_id="inspecting_workspace",
                    preconditions=[],
                    allowed_tools=["read_file"],
                    expected_evidence=["inspected_files"],
                    risk_level=RiskLevel.LOW,
                ),
            )

        def update_from_tool_result(self, **kwargs: Any) -> None:
            self.updates.append(kwargs)

    readme = tmp_path / "README.md"
    readme.write_text("planner bridge", encoding="utf-8")
    planner = RecordingPlanner()
    component = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        planner=planner,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(
        make_tool_call("read_file", {"path": "README.md"}, tool_call_id="call_read")
    )

    assert result.ok is True
    assert len(planner.updates) == 1
    assert planner.updates[0]["tool_call_id"] == "call_read"
    assert planner.updates[0]["tool_name"] == "read_file"
    assert planner.updates[0]["result"].ok is True
    assert planner.updates[0]["action_id"].startswith("action_")


def test_tool_executor_fails_closed_when_planner_update_fails(tmp_path: Path) -> None:
    class BrokenPlanner:
        session_id = "session_1"
        task_id = "task_1"
        state = None

        def authorize_tool_call(self, **_kwargs: Any) -> None:
            return None

        def update_from_tool_result(self, **_kwargs: Any) -> None:
            raise RuntimeError("planner store unavailable")

    class RecordingTrace:
        def __init__(self) -> None:
            self.events: list[dict[str, Any]] = []

        def emit(self, event_type, **kwargs: Any) -> None:
            self.events.append({"event_type": event_type, **kwargs})

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="read_ok",
            version="0.0.1",
            description="Read-only success.",
            input_model=EmptyInput,
            handler=lambda _args: {"ok": True},
            permission_level=PermissionLevel.READ_ONLY,
        )
    )
    trace = RecordingTrace()
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=trace,
        workspace_root=tmp_path,
        planner=BrokenPlanner(),
        policy_engine=make_test_policy_engine(tmp_path),
    )

    with pytest.raises(RuntimeError, match="planner observation update failed"):
        component.execute_tool_call(make_tool_call("read_ok", {}, tool_call_id="call_read"))

    assert any(
        "Planner observation update failed" in event.get("summary", "")
        and str(event.get("severity")).lower().endswith("error")
        for event in trace.events
    )


def test_write_file_facade_with_planner_observer_records_one_planner_change(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Add file")
    planner.state.status = TaskStatus.APPLYING_CHANGES
    planner.state.current_phase = "applying_changes"
    planner.plan.current_phase = "applying_changes"
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    mutation = WorkspaceMutationManager(tmp_path, planner=planner)
    register_edit_tools(
        registry,
        EditExecutor(tmp_path, mutation_manager=mutation, planner=planner),
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        planner=planner,
        policy_engine=default_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(
        make_tool_call(
            "write_file",
            {
                "path": "app.py",
                "content": "print('ok')\n",
                "mode": "create",
                "reason": "add app",
            },
            tool_call_id="call_mutate",
        )
    )

    assert result.ok is True
    assert len(planner.evidence.applied_changes) == 1
    assert planner.evidence.applied_changes[0]["transaction_id"]
    assert planner.evidence.applied_changes[0]["diff_digest"]
