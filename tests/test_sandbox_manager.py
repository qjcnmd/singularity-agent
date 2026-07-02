from __future__ import annotations

from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

from singularity.sandbox import (
    PreparedSandbox,
    SandboxCapabilities,
    SandboxManager,
    SandboxNetworkMode,
    SandboxProfileName,
    SandboxRequest,
    SandboxResult,
    SandboxStatus,
    default_sandbox_backends,
    default_sandbox_profile,
)


def _capabilities(*, network: bool = True, readonly: bool = True) -> SandboxCapabilities:
    return SandboxCapabilities(
        filesystem_isolation=True,
        copy_on_write=True,
        readonly_mount=readonly,
        network_isolation=network,
        env_isolation=True,
        process_tree_kill=True,
        timeout=True,
        output_limit=True,
        memory_limit=True,
        process_limit=True,
        artifact_capture=True,
        change_detection=True,
    )


@dataclass
class _Backend:
    root: Path
    available: bool = True
    network: bool = True
    capability_calls: int = 0
    prepare_calls: int = 0
    run_calls: int = 0

    def name(self) -> str:
        return "test_native"

    def capabilities(self) -> SandboxCapabilities:
        self.capability_calls += 1
        return _capabilities(network=self.network)

    def is_available(self) -> bool:
        return self.available

    def prepare(self, request: SandboxRequest) -> PreparedSandbox:
        self.prepare_calls += 1
        return PreparedSandbox(
            sandbox_id=request.sandbox_id,
            backend_name=self.name(),
            sandbox_root=self.root / "sandbox",
            workspace_copy_root=self.root / "sandbox" / "workspace",
            execution_cwd=self.root / "sandbox" / "workspace",
            env={},
            request=request,
            created_at=datetime.now(UTC).isoformat(),
            trace_id="trace",
        )

    def run(self, prepared: PreparedSandbox) -> SandboxResult:
        self.run_calls += 1
        now = datetime.now(UTC).isoformat()
        return SandboxResult(
            sandbox_id=prepared.sandbox_id,
            backend_name=self.name(),
            status=SandboxStatus.SUCCESS,
            exit_code=0,
            stdout="ok",
            stderr="",
            started_at=now,
            ended_at=now,
            duration_ms=0,
        )

    def cleanup(self, prepared: PreparedSandbox) -> None:
        return None


def _request(tmp_path: Path) -> SandboxRequest:
    return SandboxRequest(
        sandbox_id="sandbox_manager",
        session_id="session",
        task_id="task",
        action_id="action",
        command=["python", "-c", "print('component')"],
        cwd=tmp_path,
        workspace_root=tmp_path,
        profile=default_sandbox_profile(
            SandboxProfileName.ISOLATED_VERIFICATION,
            workspace_root=tmp_path,
        ),
    )


def test_default_backends_contain_only_native_os_backend() -> None:
    names = [backend.name() for backend in default_sandbox_backends()]

    assert "docker" not in names
    assert names in ([], ["windows"])


def test_manager_fails_closed_without_available_backend_and_does_not_prepare(
    tmp_path: Path,
) -> None:
    backend = _Backend(tmp_path, available=False)
    component = SandboxManager(tmp_path, backends=[backend])

    result = component.run(_request(tmp_path))

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert result.metadata["error_code"] == "backend_unavailable"
    assert backend.prepare_calls == 0
    assert backend.run_calls == 0


def test_manager_enforces_resolved_request_capabilities_before_prepare(tmp_path: Path) -> None:
    backend = _Backend(tmp_path, network=False)
    component = SandboxManager(tmp_path, backends=[backend])
    request = _request(tmp_path)
    assert request.profile.network.mode == SandboxNetworkMode.DENIED

    result = component.run(request)

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert "network" in result.stderr.lower()
    assert backend.prepare_calls == 0


def test_manager_blocks_protected_command_path_before_prepare(tmp_path: Path) -> None:
    backend = _Backend(tmp_path, available=True)
    component = SandboxManager(tmp_path, backends=[backend])
    request = _request(tmp_path)
    request.command = ["python", ".env"]

    result = component.run(request)

    assert result.status == SandboxStatus.POLICY_BLOCKED
    assert result.metadata["error_code"] == "protected_path_denied"
    assert backend.prepare_calls == 0
    assert backend.run_calls == 0


def test_manager_consumes_request_without_reinterpreting_policy_constraints(
    tmp_path: Path,
) -> None:
    backend = _Backend(tmp_path)
    component = SandboxManager(tmp_path, backends=[backend])
    request = _request(tmp_path)
    request.profile.network.mode = SandboxNetworkMode.ALLOWED
    request.policy_constraints = type(
        "LegacyConstraints",
        (),
        {"hard_isolation_required": True, "to_dict": lambda self: {}},
    )()
    before = request.profile.to_dict()

    result = component.run(request)

    assert result.status == SandboxStatus.SUCCESS
    assert request.profile.to_dict() == before
    assert backend.prepare_calls == 1
    assert backend.run_calls == 1


def test_manager_reuses_selected_backend_capabilities_for_run_trace(tmp_path: Path) -> None:
    backend = _Backend(tmp_path)
    component = SandboxManager(tmp_path, backends=[backend])

    result = component.run(_request(tmp_path))

    assert result.status == SandboxStatus.SUCCESS
    assert backend.capability_calls == 1
    assert backend.prepare_calls == 1
    assert backend.run_calls == 1


def test_capability_summary_only_reports_available_enforcement(tmp_path: Path) -> None:
    unavailable = _Backend(tmp_path, available=False)
    available = _Backend(tmp_path, available=True)
    component = SandboxManager(tmp_path, backends=[unavailable, available])

    summary = component.capability_summary()

    assert summary["available_backends"] == ["test_native"]
    assert summary["backend_status"] == "available"


def test_capability_summary_reports_backend_unavailable(tmp_path: Path) -> None:
    component = SandboxManager(tmp_path, backends=[_Backend(tmp_path, available=False)])

    summary = component.capability_summary()

    assert summary["available_backends"] == []
    assert summary["backend_status"] == "backend_unavailable"
