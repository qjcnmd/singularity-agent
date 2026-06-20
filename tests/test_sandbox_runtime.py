import json
import sys
from pathlib import Path

from miniharness.sandbox import (
    SandboxArtifactCollector,
    SandboxNetworkMode,
    SandboxNetworkPolicy,
    SandboxProfileName,
    SandboxRequest,
    SandboxResourceLimits,
    SandboxRuntime,
    SandboxStatus,
    default_sandbox_profile,
)
from miniharness.policy import SecurityMode


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


def test_strict_policy_sandbox_requires_real_network_isolation(tmp_path: Path) -> None:
    runtime = SandboxRuntime(tmp_path, security_mode=SecurityMode.STRICT)
    request = sandbox_request(tmp_path)
    request.profile.network = SandboxNetworkPolicy(mode=SandboxNetworkMode.DENIED)
    request.policy_constraints = type(
        "Constraints",
        (),
        {
            "sandbox_required": True,
            "to_dict": lambda self: {"sandbox_required": True},
        },
    )()
    request.profile.network.require_hard_isolation = True

    result = runtime.run(request)

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert result.metadata["error_code"] == "sandbox_unavailable"


def test_sandbox_output_and_log_artifacts_are_redacted(tmp_path: Path) -> None:
    request = sandbox_request(tmp_path)
    request.command = [
        sys.executable,
        "-c",
        "print('OPENAI_API_KEY=sk-sandbox-secret')",
    ]
    runtime = SandboxRuntime(tmp_path)

    result = runtime.run(request)

    assert result.status == SandboxStatus.SUCCESS
    assert "sk-sandbox-secret" not in result.stdout
    assert "OPENAI_API_KEY=<redacted>" in result.stdout
    stdout_artifact = next(item for item in result.artifacts if item.relative_path.endswith("stdout.log"))
    assert stdout_artifact.size_bytes == len(result.stdout.encode("utf-8"))


def test_sandbox_artifact_collector_writes_redacted_output_bytes(tmp_path: Path) -> None:
    collector = SandboxArtifactCollector()
    stdout = "OPENAI_API_KEY=<redacted>\n"

    artifacts = collector.collect(
        sandbox_id="sandbox_runtime",
        workspace_root=tmp_path,
        artifact_root=tmp_path / "artifacts",
        artifact_paths=[],
        limits=SandboxResourceLimits(),
        stdout=stdout,
    )

    stdout_artifact = next(item for item in artifacts if item.relative_path.endswith("stdout.log"))
    artifact_text = stdout_artifact.path.read_text(encoding="utf-8")
    assert stdout_artifact.size_bytes == len(stdout.encode("utf-8"))
    assert "sk-sandbox-secret" not in artifact_text
    assert "OPENAI_API_KEY=<redacted>" in artifact_text
