from __future__ import annotations

import json
import sys
from pathlib import Path

from singularity.sandbox import (
    SandboxManager,
    SandboxProfileName,
    SandboxRequest,
    SandboxStatus,
    default_sandbox_profile,
)


def test_default_os_sandbox_fails_closed_without_executing_process(tmp_path: Path) -> None:
    marker = tmp_path / "must-not-exist.txt"
    request = SandboxRequest(
        sandbox_id="sandbox_integration",
        session_id="session",
        task_id="task",
        action_id="action",
        command=[
            sys.executable,
            "-c",
            f"from pathlib import Path; Path({str(marker)!r}).write_text('ran')",
        ],
        cwd=tmp_path,
        workspace_root=tmp_path,
        profile=default_sandbox_profile(
            SandboxProfileName.ISOLATED_VERIFICATION,
            workspace_root=tmp_path,
        ),
    )

    result = SandboxManager(tmp_path).run(request)

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert result.metadata["error_code"] == "backend_unavailable"
    assert marker.exists() is False

    trace_path = tmp_path / ".singularity" / "sandbox" / "trace.jsonl"
    event = json.loads(trace_path.read_text(encoding="utf-8").splitlines()[-1])
    assert event["status"] == "backend_unavailable"
    assert event["cleanup_status"] == "not_started"


def test_unavailable_os_sandbox_does_not_create_workspace_projection(tmp_path: Path) -> None:
    request = SandboxRequest(
        sandbox_id="sandbox_no_projection",
        session_id="session",
        task_id="task",
        action_id="action",
        command=[sys.executable, "-c", "print('must-not-run')"],
        cwd=tmp_path,
        workspace_root=tmp_path,
        profile=default_sandbox_profile(
            SandboxProfileName.READONLY_ANALYSIS,
            workspace_root=tmp_path,
        ),
    )

    result = SandboxManager(tmp_path).run(request)

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert not (tmp_path / "work" / "sandboxes" / request.sandbox_id).exists()
