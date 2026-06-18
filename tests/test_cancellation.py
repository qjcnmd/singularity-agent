from __future__ import annotations

import sys
from pathlib import Path

import pytest

from miniharness.command import CommandPurpose, CommandRequest, CommandRuntime
from miniharness.kernel.cancellation import CancellationManager, CancellationToken
from miniharness.kernel.exceptions import CancellationError
from miniharness.kernel.models import CancellationReason
from miniharness.model import MockModelProvider, ModelRuntime, ModelTurnRequest
from miniharness.planner import PlannerRuntime
from miniharness.sandbox import SandboxRuntime
from miniharness.tools import ToolRegistry
from miniharness.verification import VerificationRuntime
from tests.test_sandbox_runtime import sandbox_request


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


def _cancelled_token() -> CancellationToken:
    token = CancellationToken()
    token.cancel(CancellationReason.USER_INTERRUPTED, "stop")
    return token
