import json
import sys
from pathlib import Path

from miniharness.sandbox import (
    SandboxNetworkMode,
    SandboxNetworkPolicy,
    SandboxProfileName,
    SandboxRequest,
    SandboxRuntime,
    SandboxStatus,
    default_sandbox_profile,
)


def sandbox_request(tmp_path: Path) -> SandboxRequest:
    return SandboxRequest(
        sandbox_id="sandbox_runtime",
        session_id="session",
        task_id="task",
        action_id="action",
        command=[sys.executable, "-c", "print('runtime')"],
        cwd=tmp_path,
        workspace_root=tmp_path,
        profile=default_sandbox_profile(
            SandboxProfileName.ISOLATED_VERIFICATION,
            workspace_root=tmp_path,
        ),
    )


def test_runtime_selects_local_backend_and_writes_trace(tmp_path: Path) -> None:
    runtime = SandboxRuntime(tmp_path)

    result = runtime.run(sandbox_request(tmp_path))

    assert result.status == SandboxStatus.SUCCESS
    assert result.backend_name == "local_staging"
    trace_path = tmp_path / ".miniharness" / "sandbox" / "trace.jsonl"
    events = [json.loads(line) for line in trace_path.read_text(encoding="utf-8").splitlines()]
    assert events[-1]["sandbox_id"] == "sandbox_runtime"
    assert events[-1]["status"] == "success"


def test_runtime_returns_backend_unavailable_when_capability_missing(tmp_path: Path) -> None:
    request = sandbox_request(tmp_path)
    request.profile.network = SandboxNetworkPolicy(
        mode=SandboxNetworkMode.DENIED,
        require_hard_isolation=True,
    )
    runtime = SandboxRuntime(tmp_path)

    result = runtime.run(request)

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert result.exit_code is None
    assert result.metadata["error_code"] == "sandbox_unavailable"

    trace_path = tmp_path / ".miniharness" / "sandbox" / "trace.jsonl"
    events = [json.loads(line) for line in trace_path.read_text(encoding="utf-8").splitlines()]
    assert events[-1]["session_id"] == "session"
    assert events[-1]["task_id"] == "task"
    assert events[-1]["action_id"] == "action"
    assert events[-1]["profile"] == "isolated_verification"
