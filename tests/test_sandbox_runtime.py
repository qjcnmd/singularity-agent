import json
import sys
from pathlib import Path

from singularity.sandbox import (
    DockerSandboxBackend,
    LocalStagingBackend,
    SandboxArtifactCollector,
    SandboxCapabilities,
    SandboxNetworkMode,
    SandboxNetworkPolicy,
    SandboxProfileName,
    SandboxRequest,
    SandboxResourceLimits,
    SandboxRuntime,
    SandboxStatus,
    default_sandbox_profile,
)
from singularity.policy import SecurityMode


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
    runtime = SandboxRuntime(tmp_path, backends=[LocalStagingBackend()])

    result = runtime.run(sandbox_request(tmp_path))

    assert result.status == SandboxStatus.SUCCESS
    assert result.backend_name == "local_staging"
    trace_path = tmp_path / ".singularity" / "sandbox" / "trace.jsonl"
    events = [json.loads(line) for line in trace_path.read_text(encoding="utf-8").splitlines()]
    assert events[-1]["sandbox_id"] == "sandbox_runtime"
    assert events[-1]["status"] == "success"
    serialized = json.dumps(events)
    assert str(tmp_path) not in serialized
    assert events[-1]["workspace_handle"] == "."
    assert events[-1]["sandbox_handle"].endswith("sandbox_runtime")


def test_runtime_capability_summary_names_hard_soft_and_no_isolation(
    tmp_path: Path,
) -> None:
    runtime = SandboxRuntime(tmp_path, backends=[LocalStagingBackend()])

    summary = runtime.capability_summary(approval_mode="non_interactive")

    assert summary["hard_isolation"] is False
    assert summary["soft_workspace_isolation"] is True
    assert summary["no_isolation"] is False
    assert summary["network_blocked"] is False
    assert summary["write_scope"] == "copy_on_write_workspace"
    assert summary["approval_mode"] == "non_interactive"
    assert summary["available_backends"] == ["local_staging"]
    assert summary["capabilities"]["local_staging"]["network_isolation"] is False


def test_runtime_defaults_to_docker_when_available(
    tmp_path: Path,
    monkeypatch,
) -> None:
    monkeypatch.setattr("singularity.sandbox.backends.docker_backend_available", lambda: True)

    runtime = SandboxRuntime(tmp_path)

    assert runtime.backends[0].name() == "docker"
    assert runtime.backends[1].name() == "local_staging"


def test_runtime_falls_back_to_local_when_docker_unavailable(
    tmp_path: Path,
    monkeypatch,
) -> None:
    monkeypatch.setattr("singularity.sandbox.backends.docker_backend_available", lambda: False)

    runtime = SandboxRuntime(tmp_path)

    assert [backend.name() for backend in runtime.backends] == ["local_staging"]


def test_runtime_selects_later_backend_when_first_lacks_required_capability(tmp_path: Path) -> None:
    request = sandbox_request(tmp_path)
    request.profile.network = SandboxNetworkPolicy(
        mode=SandboxNetworkMode.DENIED,
        require_hard_isolation=True,
    )
    docker = DockerSandboxBackend()
    docker.is_available = lambda: True  # type: ignore[method-assign]
    runtime = SandboxRuntime(tmp_path, backends=[LocalStagingBackend(), docker])

    selected = runtime._select_backend(request)

    assert selected is docker


def test_runtime_skips_default_docker_for_unsupported_project_toolchain(tmp_path: Path) -> None:
    request = sandbox_request(tmp_path)
    request.command = ["node", "--version"]
    docker = DockerSandboxBackend()
    docker.is_available = lambda: True  # type: ignore[method-assign]
    runtime = SandboxRuntime(tmp_path, backends=[docker, LocalStagingBackend()])

    selected = runtime._select_backend(request)

    assert isinstance(selected, LocalStagingBackend)


def test_runtime_fails_closed_when_unsupported_toolchain_needs_hard_isolation(tmp_path: Path) -> None:
    request = sandbox_request(tmp_path)
    request.command = ["node", "--version"]
    request.profile.network = SandboxNetworkPolicy(
        mode=SandboxNetworkMode.DENIED,
        require_hard_isolation=True,
    )
    docker = DockerSandboxBackend()
    docker.is_available = lambda: True  # type: ignore[method-assign]
    runtime = SandboxRuntime(tmp_path, backends=[docker, LocalStagingBackend()])

    result = runtime.run(request)

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert result.metadata["error_code"] == "sandbox_unavailable"


def test_runtime_skips_backend_when_availability_probe_fails(tmp_path: Path) -> None:
    class BrokenAvailabilityBackend(LocalStagingBackend):
        def name(self) -> str:
            return "broken"

        def is_available(self) -> bool:
            raise OSError("probe failed")

    runtime = SandboxRuntime(
        tmp_path,
        backends=[BrokenAvailabilityBackend(), LocalStagingBackend()],
    )

    result = runtime.run(sandbox_request(tmp_path))

    assert result.status == SandboxStatus.SUCCESS
    assert result.backend_name == "local_staging"


def test_runtime_returns_structured_failure_when_backend_setup_raises(tmp_path: Path) -> None:
    class SetupFailureBackend(LocalStagingBackend):
        def name(self) -> str:
            return "setup_failure"

        def capabilities(self) -> SandboxCapabilities:
            return super().capabilities()

        def prepare(self, request: SandboxRequest):
            raise RuntimeError("setup boom")

    runtime = SandboxRuntime(tmp_path, backends=[SetupFailureBackend()])

    result = runtime.run(sandbox_request(tmp_path))

    assert result.status == SandboxStatus.SETUP_FAILED
    assert result.backend_name == "setup_failure"
    assert result.metadata["error_code"] == "sandbox_setup_failed"


def test_runtime_returns_backend_unavailable_when_capability_missing(tmp_path: Path) -> None:
    request = sandbox_request(tmp_path)
    request.profile.network = SandboxNetworkPolicy(
        mode=SandboxNetworkMode.DENIED,
        require_hard_isolation=True,
    )
    runtime = SandboxRuntime(tmp_path, backends=[LocalStagingBackend()])

    result = runtime.run(request)

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert result.exit_code is None
    assert result.metadata["error_code"] == "sandbox_unavailable"

    trace_path = tmp_path / ".singularity" / "sandbox" / "trace.jsonl"
    events = [json.loads(line) for line in trace_path.read_text(encoding="utf-8").splitlines()]
    assert events[-1]["session_id"] == "session"
    assert events[-1]["task_id"] == "task"
    assert events[-1]["action_id"] == "action"
    assert events[-1]["profile"] == "isolated_verification"


def test_strict_policy_sandbox_requires_real_network_isolation(tmp_path: Path) -> None:
    runtime = SandboxRuntime(
        tmp_path,
        backends=[LocalStagingBackend()],
        security_mode=SecurityMode.STRICT,
    )
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


def test_policy_hard_isolation_constraint_fails_closed_on_local_backend(tmp_path: Path) -> None:
    runtime = SandboxRuntime(tmp_path, backends=[LocalStagingBackend()])
    request = sandbox_request(tmp_path)
    request.policy_constraints = type(
        "Constraints",
        (),
        {
            "sandbox_required": True,
            "hard_isolation_required": True,
            "to_dict": lambda self: {
                "sandbox_required": True,
                "hard_isolation_required": True,
            },
        },
    )()

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
    runtime = SandboxRuntime(tmp_path, backends=[LocalStagingBackend()])

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
