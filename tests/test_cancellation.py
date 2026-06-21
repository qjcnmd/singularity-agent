from __future__ import annotations

import sys
from pathlib import Path

import pytest
from pydantic import BaseModel

from singularity.command import CommandPurpose, CommandRequest, CommandRuntime
from singularity.context import ContextManager
from singularity.kernel.cancellation import CancellationManager, CancellationToken
from singularity.kernel.exceptions import CancellationError
from singularity.kernel.models import CancellationReason
from singularity.model import MockModelProvider, ModelRuntime, ModelToolCall, ModelToolParseStatus, ModelTurnRequest, ModelTurnResult, ModelTurnStatus
from singularity.planner import PlannerRuntime
from singularity.review import ReviewRuntime
from singularity.sandbox import SandboxRuntime
from singularity.tool_protocol.runtime import ToolCallingProtocolRuntime
from singularity.tools import ToolPolicy, ToolRegistry, ToolRuntime
from singularity.tools.models import PermissionLevel, ToolResult, ToolSpec
from singularity.verification import VerificationRuntime
from tests.test_sandbox_runtime import sandbox_request
from tests.tool_runtime_helpers import make_test_policy_runtime


def test_cancellation_token_raises_after_cancel() -> None:
    token = CancellationToken()

    token.cancel(CancellationReason.USER_INTERRUPTED, "Ctrl+C")

    assert token.cancelled
    assert token.reason == CancellationReason.USER_INTERRUPTED
    with pytest.raises(CancellationError) as exc_info:
        token.throw_if_cancelled()
    assert "Ctrl+C" in str(exc_info.value)


def test_child_token_follows_parent_cancellation() -> None:
    parent = CancellationToken()
    child = parent.child_token()

    parent.cancel(CancellationReason.SHUTDOWN_REQUESTED, "shutdown")

    assert child.cancelled
    with pytest.raises(CancellationError):
        child.throw_if_cancelled()


def test_cancellation_manager_cancels_root_and_children() -> None:
    manager = CancellationManager()
    child = manager.child_token()

    manager.cancel(CancellationReason.POLICY_ABORT, "policy denied")

    assert manager.token.cancelled
    assert child.cancelled
    assert child.reason == CancellationReason.POLICY_ABORT


def test_model_runtime_checks_cancellation_before_provider_call(tmp_path: Path) -> None:
    provider = MockModelProvider(text="should not run")
    runtime = ModelRuntime.with_mock_provider(provider, tool_registry=ToolRegistry(tmp_path))
    runtime.cancellation_token = _cancelled_token()

    with pytest.raises(CancellationError):
        runtime.run_turn(ModelTurnRequest.simple(messages=[]))

    assert provider.complete_calls == 0


def test_planner_runtime_checks_cancellation_before_step(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("inspect")
    planner.cancellation_token = _cancelled_token()

    with pytest.raises(CancellationError):
        planner.step()


def test_command_runtime_checks_cancellation_before_start(tmp_path: Path) -> None:
    runtime = CommandRuntime(tmp_path)
    runtime.cancellation_token = _cancelled_token()

    with pytest.raises(CancellationError):
        runtime.run(
            CommandRequest(
                argv=[sys.executable, "-c", "print('should not run')"],
                cwd=".",
                purpose=CommandPurpose.READ_ONLY_COMMAND,
            )
        )


def test_sandbox_runtime_checks_cancellation_before_backend(tmp_path: Path) -> None:
    runtime = SandboxRuntime(tmp_path)
    runtime.cancellation_token = _cancelled_token()

    with pytest.raises(CancellationError):
        runtime.run(sandbox_request(tmp_path))


def test_verification_runtime_checks_cancellation_before_running_plan(tmp_path: Path) -> None:
    runtime = VerificationRuntime(tmp_path)
    plan = runtime.plan_verification(changed_files=["README.md"], task_intent="docs")
    runtime.cancellation_token = _cancelled_token()

    with pytest.raises(CancellationError):
        runtime.run_plan(plan.id)


def test_tool_runtime_checks_cancellation_before_handler(tmp_path: Path) -> None:
    calls = []

    class EmptyInput(BaseModel):
        pass

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="read",
            description="read",
            input_model=EmptyInput,
            handler=lambda _args: calls.append("called") or ToolResult.success(content={"ok": True}),
            permission_level=PermissionLevel.READ_ONLY,
        )
    )
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=None,
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )
    runtime.cancellation_token = _cancelled_token()

    with pytest.raises(CancellationError):
        runtime.execute_tool_call(
            {"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{}"}}
        )

    assert calls == []


def test_tool_protocol_runtime_checks_cancellation_before_tool_handler(tmp_path: Path) -> None:
    calls = []

    class EmptyInput(BaseModel):
        pass

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="read",
            description="read",
            input_model=EmptyInput,
            handler=lambda _args: calls.append("called") or ToolResult.success(content={"ok": True}),
            permission_level=PermissionLevel.READ_ONLY,
        )
    )
    tool_runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=None,
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )
    protocol = ToolCallingProtocolRuntime(registry=registry, trace=None)
    protocol.cancellation_token = _cancelled_token()
    context = ContextManager(system_prompt="system", user_goal="inspect")
    result = ModelTurnResult(
        request_id="req_1",
        response_id="resp_1",
        status=ModelTurnStatus.SUCCESS,
        tool_calls=[
            ModelToolCall(
                tool_call_id="call_1",
                tool_name="read",
                arguments={},
                raw_arguments="{}",
                parse_status=ModelToolParseStatus.VALID,
            )
        ],
    )

    with pytest.raises(CancellationError):
        protocol.handle_model_turn_result(
            result,
            context=context,
            tool_runtime=tool_runtime,
            planner=None,
            policy_runtime=None,
        )

    assert calls == []


def test_review_runtime_checks_cancellation_before_review(tmp_path: Path) -> None:
    runtime = ReviewRuntime(tmp_path, enable_model_critic=False)
    runtime.cancellation_token = _cancelled_token()

    with pytest.raises(CancellationError):
        runtime.post_verification_review(verification={"completion_assessment": {"status": "ready"}})


def _cancelled_token() -> CancellationToken:
    token = CancellationToken()
    token.cancel(CancellationReason.USER_INTERRUPTED, "stop")
    return token
