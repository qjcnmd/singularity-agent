import json
import sys
from pathlib import Path

from singularity.sandbox import (
    DockerSandboxBackend,
    LocalStagingBackend,
    SandboxArtifactCollector,
    SandboxCapabilities,
    SandboxFilesystemMode,
    SandboxNetworkMode,
    SandboxNetworkPolicy,
    SandboxProfileName,
    SandboxRequest,
    SandboxResourceLimits,
    SandboxManager,
    SandboxStatus,
    WindowsRestrictedTokenBackend,
    default_sandbox_profile,
)
from singularity.policy import SecurityMode


def sandbox_request(tmp_path: Path) -> SandboxRequest:
    return SandboxRequest(
        sandbox_id="sandbox_manager",
        session_id="session",
        task_id="task",
        action_id="action",
        command=[sys.executable, "-c", "print('component')"],
        cwd=tmp_path,
        workspace_root=tmp_path,
        profile=default_sandbox_profile(
            SandboxProfileName.ISOLATED_VERIFICATION,
            workspace_root=tmp_path,
        ),
    )


def test_sandbox_manager_selects_local_backend_and_writes_trace(tmp_path: Path) -> None:
    component = SandboxManager(tmp_path, backends=[LocalStagingBackend()])

    result = component.run(sandbox_request(tmp_path))

    assert result.status == SandboxStatus.SUCCESS
    assert result.backend_name == "local_staging"
    trace_path = tmp_path / ".singularity" / "sandbox" / "trace.jsonl"
    events = [json.loads(line) for line in trace_path.read_text(encoding="utf-8").splitlines()]
    assert events[-1]["sandbox_id"] == "sandbox_manager"
    assert events[-1]["status"] == "success"
    serialized = json.dumps(events)
    assert str(tmp_path) not in serialized
    assert events[-1]["workspace_handle"] == "."
    assert events[-1]["sandbox_handle"].endswith("sandbox_manager")


def test_sandbox_capability_summary_names_hard_soft_and_no_isolation(
    tmp_path: Path,
) -> None:
    component = SandboxManager(tmp_path, backends=[LocalStagingBackend()])

    summary = component.capability_summary(approval_mode="non_interactive")

    assert summary["hard_isolation"] is False
    assert summary["soft_workspace_isolation"] is True
    assert summary["no_isolation"] is False
    assert summary["network_blocked"] is False
    assert summary["write_scope"] == "copy_on_write_workspace"
    assert summary["approval_mode"] == "non_interactive"
    assert summary["available_backends"] == ["local_staging"]
    assert summary["capabilities"]["local_staging"]["network_isolation"] is False


def test_sandbox_manager_defaults_to_docker_when_available(
    tmp_path: Path,
    monkeypatch,
) -> None:
    monkeypatch.setattr("singularity.sandbox.backends.docker_backend_available", lambda: True)
    monkeypatch.setattr(
        "singularity.sandbox.backends.windows_restricted_token_available",
        lambda: True,
    )

    component = SandboxManager(tmp_path)

    assert component.backends[0].name() == "docker"
    assert component.backends[1].name() == "windows_restricted_token"
    assert component.backends[2].name() == "local_staging"


def test_sandbox_manager_falls_back_to_windows_before_local_when_docker_unavailable(
    tmp_path: Path,
    monkeypatch,
) -> None:
    monkeypatch.setattr("singularity.sandbox.backends.docker_backend_available", lambda: False)
    monkeypatch.setattr(
        "singularity.sandbox.backends.windows_restricted_token_available",
        lambda: True,
    )

    component = SandboxManager(tmp_path)

    assert [backend.name() for backend in component.backends] == [
        "windows_restricted_token",
        "local_staging",
    ]


def test_sandbox_manager_falls_back_to_local_when_stronger_backends_unavailable(
    tmp_path: Path,
    monkeypatch,
) -> None:
    monkeypatch.setattr("singularity.sandbox.backends.docker_backend_available", lambda: False)
    monkeypatch.setattr(
        "singularity.sandbox.backends.windows_restricted_token_available",
        lambda: False,
    )

    component = SandboxManager(tmp_path)

    assert [backend.name() for backend in component.backends] == ["local_staging"]


def test_sandbox_manager_selects_later_backend_when_first_lacks_required_capability(tmp_path: Path) -> None:
    request = sandbox_request(tmp_path)
    request.profile.network = SandboxNetworkPolicy(
        mode=SandboxNetworkMode.DENIED,
        require_hard_isolation=True,
    )
    docker = DockerSandboxBackend()
    docker.is_available = lambda: True  # type: ignore[method-assign]
    component = SandboxManager(tmp_path, backends=[LocalStagingBackend(), docker])

    selected = component._select_backend(request)

    assert selected is docker


def test_sandbox_manager_skips_default_docker_for_unsupported_project_toolchain(tmp_path: Path) -> None:
    request = sandbox_request(tmp_path)
    request.command = ["node", "--version"]
    docker = DockerSandboxBackend()
    docker.is_available = lambda: True  # type: ignore[method-assign]
    windows = WindowsRestrictedTokenBackend()
    windows.is_available = lambda: True  # type: ignore[method-assign]
    component = SandboxManager(tmp_path, backends=[docker, windows, LocalStagingBackend()])

    selected = component._select_backend(request)

    assert isinstance(selected, WindowsRestrictedTokenBackend)


def test_sandbox_manager_fails_closed_when_unsupported_toolchain_needs_hard_isolation(tmp_path: Path) -> None:
    request = sandbox_request(tmp_path)
    request.command = ["node", "--version"]
    request.profile.network = SandboxNetworkPolicy(
        mode=SandboxNetworkMode.DENIED,
        require_hard_isolation=True,
    )
    docker = DockerSandboxBackend()
    docker.is_available = lambda: True  # type: ignore[method-assign]
    component = SandboxManager(tmp_path, backends=[docker, LocalStagingBackend()])

    result = component.run(request)

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert result.metadata["error_code"] == "sandbox_unavailable"


def test_sandbox_manager_skips_backend_when_availability_probe_fails(tmp_path: Path) -> None:
    class BrokenAvailabilityBackend(LocalStagingBackend):
        def name(self) -> str:
            return "broken"

        def is_available(self) -> bool:
            raise OSError("probe failed")

    component = SandboxManager(
        tmp_path,
        backends=[BrokenAvailabilityBackend(), LocalStagingBackend()],
    )

    result = component.run(sandbox_request(tmp_path))

    assert result.status == SandboxStatus.SUCCESS
    assert result.backend_name == "local_staging"


def test_sandbox_manager_returns_structured_failure_when_backend_setup_raises(tmp_path: Path) -> None:
    class SetupFailureBackend(LocalStagingBackend):
        def name(self) -> str:
            return "setup_failure"

        def capabilities(self) -> SandboxCapabilities:
            return super().capabilities()

        def prepare(self, request: SandboxRequest):
            raise RuntimeError("setup boom")

    component = SandboxManager(tmp_path, backends=[SetupFailureBackend()])

    result = component.run(sandbox_request(tmp_path))

    assert result.status == SandboxStatus.SETUP_FAILED
    assert result.backend_name == "setup_failure"
    assert result.metadata["error_code"] == "sandbox_setup_failed"


def test_sandbox_manager_returns_backend_unavailable_when_capability_missing(tmp_path: Path) -> None:
    request = sandbox_request(tmp_path)
    request.profile.network = SandboxNetworkPolicy(
        mode=SandboxNetworkMode.DENIED,
        require_hard_isolation=True,
    )
    component = SandboxManager(tmp_path, backends=[LocalStagingBackend()])

    result = component.run(request)

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
    component = SandboxManager(
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

    result = component.run(request)

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert result.metadata["error_code"] == "sandbox_unavailable"


def test_policy_hard_isolation_constraint_fails_closed_on_local_backend(tmp_path: Path) -> None:
    component = SandboxManager(tmp_path, backends=[LocalStagingBackend()])
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

    result = component.run(request)

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert result.metadata["error_code"] == "sandbox_unavailable"


def test_sandbox_output_and_log_artifacts_are_redacted(tmp_path: Path) -> None:
    request = sandbox_request(tmp_path)
    request.command = [
        sys.executable,
        "-c",
        "print('OPENAI_API_KEY=sk-sandbox-secret')",
    ]
    component = SandboxManager(tmp_path, backends=[LocalStagingBackend()])

    result = component.run(request)

    assert result.status == SandboxStatus.SUCCESS
    assert "sk-sandbox-secret" not in result.stdout
    assert "OPENAI_API_KEY=<redacted>" in result.stdout
    stdout_artifact = next(item for item in result.artifacts if item.relative_path.endswith("stdout.log"))
    assert stdout_artifact.size_bytes == len(result.stdout.encode("utf-8"))


def test_sandbox_artifact_collector_writes_redacted_output_bytes(tmp_path: Path) -> None:
    collector = SandboxArtifactCollector()
    stdout = "OPENAI_API_KEY=<redacted>\n"

    artifacts = collector.collect(
        sandbox_id="sandbox_manager",
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


def test_read_only_filesystem_mode_maps_to_read_only_workspace(tmp_path: Path) -> None:
    # Regression: "read_only"/"readonly" used to be mapped to
    # COPY_ON_WRITE_WORKSPACE (writable). It must now map to
    # READ_ONLY_WORKSPACE so the staged tree is actually read-only.
    component = SandboxManager(tmp_path, backends=[LocalStagingBackend()])
    command_request = type(
        "Cmd",
        (),
        {"argv": ["python", "-c", "print('ok')"], "command_id": "cmd_1", "purpose": None},
    )()
    constraints = type(
        "Constraints",
        (),
        {
            "filesystem_mode": "read_only",
            "network_allowed": False,
            "hard_isolation_required": False,
            "sandbox_required": True,
            "max_duration_seconds": None,
            "max_output_chars": None,
            "allowed_hosts": [],
            "to_dict": lambda self: {},
        },
    )()
    policy_decision = type(
        "Decision",
        (),
        {"decision_id": "dec_1", "constraints": constraints, "reason": "test"},
    )()

    request = component.build_request_from_policy(
        command_request,
        policy_decision,
        session_id="session",
        task_id="task",
        action_id="action",
        cwd=tmp_path,
    )

    assert request.profile.filesystem.mode == SandboxFilesystemMode.READ_ONLY_WORKSPACE


def test_network_denied_fail_closed_produces_violation_on_local_backend(tmp_path: Path) -> None:
    # LocalStagingBackend has network_isolation=False. When the profile
    # denies network access (mode=DENIED), the manager must fail-closed by
    # recording a SandboxViolation rather than silently running with an
    # unenforced denial.
    component = SandboxManager(tmp_path, backends=[LocalStagingBackend()])
    request = sandbox_request(tmp_path)
    # The default ISOLATED_VERIFICATION profile already sets network.mode=DENIED.
    assert request.profile.network.mode == SandboxNetworkMode.DENIED
    request.profile.network.require_hard_isolation = False

    result = component.run(request)

    violation_types = [violation.violation_type for violation in result.violations]
    assert "network_denial_unenforced" in violation_types


def test_capability_summary_reflects_selected_backend_not_union(tmp_path: Path) -> None:
    # capability_summary must report isolation based on the selected (first
    # available) backend, not the union of all backends. When Docker is
    # first and available, hard_isolation must be True (Docker has
    # network_isolation). When only LocalStaging is available, it must be
    # False.
    docker = DockerSandboxBackend()
    docker.is_available = lambda: True  # type: ignore[method-assign]
    component_with_docker = SandboxManager(tmp_path, backends=[docker, LocalStagingBackend()])

    summary = component_with_docker.capability_summary()

    assert summary["hard_isolation"] is True

    component_local_only = SandboxManager(tmp_path, backends=[LocalStagingBackend()])
    summary_local = component_local_only.capability_summary()

    assert summary_local["hard_isolation"] is False
