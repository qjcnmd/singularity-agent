from __future__ import annotations

import json
import sys
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

from singularity.policy.permissions import PermissionProfile, PermissionProfileName
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
    backend_name: str = "test_native"
    available: bool = True
    network: bool = True
    readonly: bool = True
    doctor_reason: str | None = None
    run_status: SandboxStatus = SandboxStatus.SUCCESS
    run_reason: str = ""
    capability_calls: int = 0
    prepare_calls: int = 0
    run_calls: int = 0

    def name(self) -> str:
        return self.backend_name

    def capabilities(self) -> SandboxCapabilities:
        self.capability_calls += 1
        return _capabilities(network=self.network, readonly=self.readonly)

    def is_available(self) -> bool:
        return self.available

    def doctor(self):
        return type(
            "Doctor",
            (),
            {
                "reason": self.doctor_reason
                or f"backend_unavailable: {self.backend_name} is unavailable.",
                "diagnostics": (),
                "available": self.available,
            },
        )()

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
            status=self.run_status,
            exit_code=0 if self.run_status == SandboxStatus.SUCCESS else None,
            stdout="ok",
            stderr=self.run_reason,
            started_at=now,
            ended_at=now,
            duration_ms=0,
            metadata={
                "error_code": "backend_unavailable"
                if self.run_status == SandboxStatus.BACKEND_UNAVAILABLE
                else None,
                "reason": self.run_reason,
            },
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


def _manager_with_profile(
    tmp_path: Path,
    profile: PermissionProfileName,
    *,
    backends: list[_Backend] | None = None,
) -> SandboxManager:
    return SandboxManager(
        tmp_path,
        backends=backends if backends is not None else [],
        permission_profile=PermissionProfile.default_for_workspace(
            tmp_path,
            profile=profile,
        ),
    )


def test_default_backends_contain_only_native_os_backend() -> None:
    names = [backend.name() for backend in default_sandbox_backends()]

    assert "docker" not in names
    assert names in ([], ["windows_elevated", "windows_unelevated"])


def test_manager_workspace_write_prefers_elevated_backend(tmp_path: Path) -> None:
    elevated = _Backend(tmp_path, backend_name="windows_elevated")
    unelevated = _Backend(tmp_path, backend_name="windows_unelevated", network=False)
    component = _manager_with_profile(
        tmp_path,
        PermissionProfileName.WORKSPACE_WRITE,
        backends=[elevated, unelevated],
    )

    result = component.run(_request(tmp_path))

    assert result.status == SandboxStatus.SUCCESS
    assert result.backend_name == "windows_elevated"
    assert result.metadata["sandbox_mode"] == "workspace-write"
    assert result.metadata["sandbox_backend"] == "windows_elevated"
    assert result.metadata["sandbox_enforcement"] == "strict"
    assert result.metadata["enforcement_status"] == "available"
    assert result.metadata["fallback_used"] is False
    assert result.metadata["elevated_available"] is True
    assert elevated.run_calls == 1
    assert unelevated.run_calls == 0


def test_manager_workspace_write_falls_back_to_unelevated_for_elevated_runtime_blocker(
    tmp_path: Path,
) -> None:
    reason = (
        "backend_unavailable: python_c_extension_low_integrity_runtime_initialization_failed"
    )
    elevated = _Backend(
        tmp_path,
        backend_name="windows_elevated",
        available=False,
        doctor_reason=reason,
    )
    unelevated = _Backend(tmp_path, backend_name="windows_unelevated", network=False)
    component = _manager_with_profile(
        tmp_path,
        PermissionProfileName.WORKSPACE_WRITE,
        backends=[elevated, unelevated],
    )

    result = component.run(_request(tmp_path))

    assert result.status == SandboxStatus.SUCCESS
    assert result.backend_name == "windows_unelevated"
    assert result.metadata["sandbox_mode"] == "workspace-write"
    assert result.metadata["sandbox_backend"] == "windows_unelevated"
    assert result.metadata["sandbox_enforcement"] == "reduced"
    assert result.metadata["enforcement_status"] == "degraded"
    assert result.metadata["fallback_used"] is True
    assert result.metadata["fallback_reason"] == reason
    assert result.metadata["elevated_available"] is False
    assert "python_c_extension_low_integrity_runtime_initialization_failed" in result.metadata[
        "elevated_blocker_summary"
    ]
    assert elevated.run_calls == 0
    assert unelevated.run_calls == 1


def test_manager_workspace_write_falls_back_to_unelevated_after_elevated_runtime_recheck_blocker(
    tmp_path: Path,
) -> None:
    reason = "backend_unavailable: elevated_python_runtime_blocker"
    elevated = _Backend(
        tmp_path,
        backend_name="windows_elevated",
        run_status=SandboxStatus.BACKEND_UNAVAILABLE,
        run_reason=reason,
    )
    unelevated = _Backend(tmp_path, backend_name="windows_unelevated", network=False)
    component = _manager_with_profile(
        tmp_path,
        PermissionProfileName.WORKSPACE_WRITE,
        backends=[elevated, unelevated],
    )

    result = component.run(_request(tmp_path))

    assert result.status == SandboxStatus.SUCCESS
    assert result.backend_name == "windows_unelevated"
    assert result.metadata["sandbox_enforcement"] == "reduced"
    assert result.metadata["enforcement_status"] == "degraded"
    assert result.metadata["fallback_used"] is True
    assert result.metadata["fallback_reason"] == reason
    assert result.metadata["elevated_available"] is False
    assert result.metadata["elevated_blocker_summary"] == reason
    assert elevated.run_calls == 1
    assert unelevated.run_calls == 1


def test_manager_workspace_write_blocks_when_elevated_and_unelevated_unavailable(
    tmp_path: Path,
) -> None:
    elevated = _Backend(
        tmp_path,
        backend_name="windows_elevated",
        available=False,
        doctor_reason="backend_unavailable: elevated unavailable",
    )
    unelevated = _Backend(tmp_path, backend_name="windows_unelevated", available=False)
    component = _manager_with_profile(
        tmp_path,
        PermissionProfileName.WORKSPACE_WRITE,
        backends=[elevated, unelevated],
    )

    result = component.run(_request(tmp_path))

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert result.metadata["error_code"] == "backend_unavailable"
    assert result.metadata["sandbox_mode"] == "workspace-write"
    assert result.metadata["sandbox_backend"] == "unavailable"
    assert result.metadata["sandbox_enforcement"] == "strict"
    assert result.metadata["enforcement_status"] == "blocked"
    assert result.metadata["fallback_used"] is False
    assert result.metadata["elevated_available"] is False
    assert result.metadata["elevated_blocker_summary"]


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


def test_manager_read_only_mode_fails_closed_without_native_backend(
    tmp_path: Path,
) -> None:
    component = _manager_with_profile(tmp_path, PermissionProfileName.READ_ONLY)

    result = component.run(_request(tmp_path))

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert result.backend_name == "unavailable"
    assert result.metadata["error_code"] == "backend_unavailable"
    assert result.metadata.get("used_local_process_fallback") is not True


def test_manager_read_only_uses_unelevated_without_relaxing_to_local_process(
    tmp_path: Path,
) -> None:
    elevated = _Backend(tmp_path, backend_name="windows_elevated", available=False)
    unelevated = _Backend(tmp_path, backend_name="windows_unelevated", network=False)
    component = _manager_with_profile(
        tmp_path,
        PermissionProfileName.READ_ONLY,
        backends=[elevated, unelevated],
    )
    request = _request(tmp_path)
    request.profile = default_sandbox_profile(
        SandboxProfileName.READONLY_ANALYSIS,
        workspace_root=tmp_path,
    )

    result = component.run(request)

    assert result.status == SandboxStatus.SUCCESS
    assert result.backend_name == "windows_unelevated"
    assert result.metadata["sandbox_mode"] == "read-only"
    assert result.metadata["sandbox_enforcement"] == "reduced"
    assert result.metadata["enforcement_status"] == "degraded"
    assert result.metadata["fallback_used"] is True
    assert result.metadata.get("used_local_process_fallback") is not True


def test_manager_workspace_write_mode_fails_closed_without_native_backend(
    tmp_path: Path,
) -> None:
    component = _manager_with_profile(tmp_path, PermissionProfileName.WORKSPACE_WRITE)

    result = component.run(_request(tmp_path))

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert result.backend_name == "unavailable"
    assert result.metadata["error_code"] == "backend_unavailable"
    assert result.metadata.get("used_local_process_fallback") is not True


def test_manager_danger_full_access_runs_local_process_when_backend_missing(
    tmp_path: Path,
) -> None:
    component = _manager_with_profile(
        tmp_path,
        PermissionProfileName.DANGER_FULL_ACCESS,
    )
    request = _request(tmp_path)
    request.command = [sys.executable, "-c", "print('relaxed sandbox')"]

    result = component.run(request)

    assert result.status == SandboxStatus.SUCCESS
    assert result.backend_name == "local_process"
    assert result.exit_code == 0
    assert result.stdout.strip() == "relaxed sandbox"
    assert result.metadata["sandbox_mode"] == "danger-full-access"
    assert result.metadata["sandbox_backend"] == "local_process"
    assert result.metadata["sandbox_enforcement"] == "relaxed"
    assert result.metadata["enforcement_status"] == "relaxed"
    assert result.metadata["execution_backend"] == "local_process"
    assert result.metadata["fallback_used"] is True
    assert result.metadata["fallback_reason"] == "danger-full-access sandbox mode"
    assert result.metadata["backend_is_local_process"] is True
    assert result.metadata["used_local_process_fallback"] is True
    assert result.metadata["local_process_fallback_reason"] == "danger-full-access sandbox mode"


def test_manager_danger_full_access_records_relaxed_trace(tmp_path: Path) -> None:
    component = _manager_with_profile(
        tmp_path,
        PermissionProfileName.DANGER_FULL_ACCESS,
    )
    request = _request(tmp_path)
    request.command = [
        sys.executable,
        "-c",
        "print('trace ok')",
        "--token",
        "plain-secret-value",
    ]

    result = component.run(request)

    assert result.status == SandboxStatus.SUCCESS
    trace_path = tmp_path / ".singularity" / "sandbox" / "trace.jsonl"
    trace_entries = [
        json.loads(line)
        for line in trace_path.read_text(encoding="utf-8").splitlines()
    ]
    entry = trace_entries[-1]
    assert entry["backend_name"] == "local_process"
    assert entry["sandbox_mode"] == "danger-full-access"
    assert entry["sandbox_enforcement"] == "relaxed"
    assert entry["enforcement_status"] == "relaxed"
    assert entry["used_local_process_fallback"] is True
    assert entry["local_process_fallback_reason"] == "danger-full-access sandbox mode"
    assert "plain-secret-value" not in trace_path.read_text(encoding="utf-8")


def test_manager_danger_full_access_still_blocks_protected_paths(
    tmp_path: Path,
) -> None:
    component = _manager_with_profile(
        tmp_path,
        PermissionProfileName.DANGER_FULL_ACCESS,
    )
    request = _request(tmp_path)
    request.command = [sys.executable, ".env"]

    result = component.run(request)

    assert result.status == SandboxStatus.POLICY_BLOCKED
    assert result.backend_name == "policy"
    assert result.metadata["error_code"] == "protected_path_denied"
    assert result.metadata.get("used_local_process_fallback") is not True


def test_manager_danger_full_access_blocks_protected_executable_path(
    tmp_path: Path,
) -> None:
    component = _manager_with_profile(
        tmp_path,
        PermissionProfileName.DANGER_FULL_ACCESS,
    )
    request = _request(tmp_path)
    request.command = [".env"]

    result = component.run(request)

    assert result.status == SandboxStatus.POLICY_BLOCKED
    assert result.backend_name == "policy"
    assert result.metadata["error_code"] == "protected_path_denied"
    assert result.metadata.get("used_local_process_fallback") is not True


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
