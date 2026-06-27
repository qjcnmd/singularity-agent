import json
import subprocess
import sys
from datetime import UTC, datetime
from pathlib import Path

import pytest

import singularity.sandbox as sandbox
import singularity.sandbox.windows as windows
from singularity.cli import app
from singularity.sandbox import (
    PreparedSandbox,
    SandboxCapabilityError,
    SandboxProfileName,
    SandboxRequest,
    SandboxResult,
    SandboxStatus,
)
from typer.testing import CliRunner


def _request(tmp_path: Path) -> SandboxRequest:
    return SandboxRequest(
        sandbox_id="sandbox_windows",
        session_id="session",
        task_id="task",
        action_id="action",
        command=["python", "-c", "print('must-not-run')"],
        cwd=tmp_path,
        workspace_root=tmp_path,
        profile=sandbox.default_sandbox_profile(
            SandboxProfileName.ISOLATED_VERIFICATION,
            workspace_root=tmp_path,
        ),
    )


def test_windows_doctor_reports_primitives_and_setup_separately() -> None:
    report = sandbox.probe_windows_sandbox()
    payload = report.to_dict()

    assert payload["schema_version"] == "sandbox.windows.doctor/v1"
    assert payload["implementation"] == "elevated"
    assert payload["enforcement_status"] in {
        "available",
        "backend_unavailable",
        "not_supported",
    }
    assert set(payload["primitives"]) == {
        "restricted_token",
        "job_object",
        "low_integrity",
        "acl",
        "firewall",
        "private_desktop",
    }
    for item in payload["primitives"].values():
        assert set(item) == {"status", "checked", "reason", "evidence"}
        assert item["status"] in {"available", "missing", "not_supported"}
        assert item["checked"] is True
        assert "secret" not in str(item["evidence"]).lower()
    assert set(payload["setup"]) == {
        "sandbox_account",
        "acl_boundary",
        "network_filter",
        "private_desktop",
        "execution_backend",
    }
    assert "execution" in payload
    assert isinstance(payload["blocking_requirements"], list)
    assert "recommended_action" in payload
    assert report.available == (payload["enforcement_status"] == "available")
    assert report.missing_requirements == tuple(payload["blocking_requirements"])


def test_windows_backend_is_unavailable_until_all_enforcement_is_configured() -> None:
    backend = sandbox.WindowsSandboxBackend()
    report = backend.doctor()

    if report.available:
        assert backend.is_available() is True
        assert backend.capabilities().filesystem_isolation is True
        assert backend.capabilities().network_isolation is True
    else:
        assert report.setup.execution_backend.ready is False
        assert backend.is_available() is False
        assert backend.capabilities().filesystem_isolation is False
        assert backend.capabilities().network_isolation is False


def test_windows_setup_reports_machine_readable_status() -> None:
    backend = sandbox.WindowsSandboxBackend()

    report = backend.setup()
    payload = report.to_dict()

    assert payload["schema_version"] == "sandbox.windows.setup/v1"
    assert payload["status"] in {
        "not_supported",
        "requires_elevation",
        "partial",
        "ready",
        "failed",
    }
    assert isinstance(payload["requires_elevation"], bool)
    assert isinstance(payload["changed"], bool)
    assert isinstance(payload["completed_steps"], list)
    assert isinstance(payload["pending_steps"], list)
    assert isinstance(payload["failed_steps"], list)
    assert isinstance(payload["available_after_setup"], bool)
    assert payload["message"]


def test_sandbox_setup_cli_does_not_rewrite_partial_status(monkeypatch: pytest.MonkeyPatch) -> None:
    report = sandbox.WindowsSandboxSetupReport(
        status="requires_elevation",
        requested_operation="setup",
        requires_elevation=True,
        changed=False,
        completed_steps=(),
        pending_steps=("sandbox_account",),
        failed_steps=(),
        available_after_setup=False,
        message="elevation required",
    )
    monkeypatch.setattr(sandbox.WindowsSandboxBackend, "setup", lambda self: report)

    result = CliRunner().invoke(app, ["sandbox", "setup", "--json"])

    assert result.exit_code == 1
    payload = __import__("json").loads(result.stdout)
    assert payload["schema_version"] == "sandbox.windows.setup/v1"
    assert payload["status"] == "requires_elevation"
    assert payload["available_after_setup"] is False


def test_windows_backend_never_prepares_without_completed_setup(tmp_path: Path) -> None:
    backend = sandbox.WindowsSandboxBackend()

    if backend.is_available():
        prepared = backend.prepare(_request(tmp_path))
        assert prepared.backend_name == "windows"
        backend.cleanup(prepared)
    else:
        with pytest.raises(SandboxCapabilityError, match="backend_unavailable"):
            backend.prepare(_request(tmp_path))


class _FakeReadyWindowsBackend(sandbox.WindowsSandboxBackend):
    def __init__(self, **kwargs) -> None:
        super().__init__(acl_applier=lambda _path: None, **kwargs)

    def doctor(self) -> sandbox.WindowsSandboxDoctorReport:
        return sandbox.WindowsSandboxDoctorReport.ready_for_tests()

    def setup(self) -> sandbox.WindowsSandboxSetupReport:
        return sandbox.WindowsSandboxSetupReport.ready_for_tests()


class _FakeRunner:
    def __init__(
        self,
        *,
        exit_code: int = 0,
        stdout: str = "ok\n",
        stderr: str = "",
        timed_out: bool = False,
        network_denied: bool = True,
        metadata: dict[str, object] | None = None,
    ) -> None:
        self.exit_code = exit_code
        self.stdout = stdout
        self.stderr = stderr
        self.timed_out = timed_out
        self.network_denied = network_denied
        self.metadata = metadata or {
            "runner": "fake",
            "backend": "windows",
            "restricted_token": True,
            "low_integrity": True,
            "private_desktop": True,
            "job_object": True,
        }
        self.calls: list[PreparedSandbox] = []

    def run(self, prepared: PreparedSandbox) -> sandbox.WindowsRunnerResult:
        self.calls.append(prepared)
        now = datetime.now(UTC).isoformat()
        return sandbox.WindowsRunnerResult(
            exit_code=None if self.timed_out else self.exit_code,
            stdout=self.stdout,
            stderr=self.stderr,
            timed_out=self.timed_out,
            started_at=now,
            ended_at=now,
            duration_ms=5,
            output_truncated=False,
            job_killed=self.timed_out,
            network_denied_verified=self.network_denied,
            metadata=self.metadata,
        )


def test_windows_backend_runs_low_risk_verification_with_ready_runner(tmp_path: Path) -> None:
    runner = _FakeRunner(stdout="pytest ok\n")
    backend = _FakeReadyWindowsBackend(runner=runner)
    request = _request(tmp_path)
    request.command = [sys.executable, "-m", "compileall", "."]

    prepared = backend.prepare(request)
    result = backend.run(prepared)

    assert runner.calls == [prepared]
    assert result.status == SandboxStatus.SUCCESS
    assert result.backend_name == "windows"
    assert result.exit_code == 0
    assert result.stdout == "pytest ok\n"
    assert result.metadata["execution_backend"] == "account_restricted_token"
    assert result.metadata["restricted_token"] is True
    assert result.metadata["low_integrity"] is True
    assert result.metadata["private_desktop"] is True
    assert result.metadata["process_tree_kill"] is True
    assert result.metadata["network_denied_verified"] is True


def test_windows_backend_does_not_forge_runner_enforcement_evidence(tmp_path: Path) -> None:
    backend = _FakeReadyWindowsBackend(
        runner=_FakeRunner(
            metadata={
                "runner": "fake",
                "backend": "windows",
                "restricted_token": False,
                "low_integrity": False,
                "private_desktop": False,
                "job_object": False,
            }
        )
    )
    prepared = backend.prepare(_request(tmp_path))

    result = backend.run(prepared)

    assert result.status == SandboxStatus.VIOLATION
    assert result.metadata["error_code"] == "sandbox_enforcement_failed"
    assert result.metadata["restricted_token"] is False
    assert result.metadata["low_integrity"] is False
    assert result.metadata["private_desktop"] is False
    assert result.metadata["process_tree_kill"] is False
    assert result.violations[0].violation_type == "process_isolation"


def test_windows_backend_timeout_maps_to_sandbox_timeout(tmp_path: Path) -> None:
    backend = _FakeReadyWindowsBackend(runner=_FakeRunner(timed_out=True))
    prepared = backend.prepare(_request(tmp_path))

    result = backend.run(prepared)

    assert result.status == SandboxStatus.TIMEOUT
    assert result.exit_code is None
    assert result.metadata["job_killed"] is True


def test_windows_backend_network_denied_self_test_fails_closed(tmp_path: Path) -> None:
    backend = _FakeReadyWindowsBackend(runner=_FakeRunner(network_denied=False))
    prepared = backend.prepare(_request(tmp_path))

    result = backend.run(prepared)

    assert result.status == SandboxStatus.VIOLATION
    assert result.exit_code is None
    assert result.metadata["error_code"] == "network_isolation_failed"
    assert result.violations[0].violation_type == "network"


def test_windows_backend_network_denied_requires_verified_firewall_probe(tmp_path: Path) -> None:
    class NoNetworkProofBackend(_FakeReadyWindowsBackend):
        def doctor(self) -> sandbox.WindowsSandboxDoctorReport:
            available = sandbox.WindowsSandboxDoctorReport.ready_for_tests()
            missing = sandbox.WindowsCapabilityState(
                "missing",
                True,
                "network probe missing",
                {"probe": "runtime"},
            )
            execution = sandbox.WindowsSandboxExecution(
                account_sid=available.execution.account_sid,
                credential=available.execution.credential,
                launcher=available.execution.launcher,
                runner_smoke=available.execution.runner_smoke,
                network_probe=missing,
            )
            return sandbox.WindowsSandboxDoctorReport(
                implementation=available.implementation,
                platform_supported=available.platform_supported,
                platform_status=available.platform_status,
                primitives=available.primitives,
                setup=available.setup,
                execution=execution,
                available=True,
                enforcement_status=available.enforcement_status,
                blocking_requirements=(),
                recommended_action=available.recommended_action,
            )

    backend = NoNetworkProofBackend(runner=_FakeRunner(network_denied=True))
    prepared = backend.prepare(_request(tmp_path))

    result = backend.run(prepared)

    assert result.status == SandboxStatus.VIOLATION
    assert result.metadata["error_code"] == "network_isolation_failed"
    assert result.metadata["network_filter_verified"] is True
    assert result.metadata["network_probe_verified"] is False
    assert result.metadata["network_denied_verified"] is False


def test_windows_backend_rechecks_enforcement_before_launching_runner(tmp_path: Path) -> None:
    class FlappingBackend(_FakeReadyWindowsBackend):
        def __init__(self, **kwargs) -> None:
            super().__init__(**kwargs)
            self.doctor_calls = 0

        def doctor(self) -> sandbox.WindowsSandboxDoctorReport:
            self.doctor_calls += 1
            if self.doctor_calls == 1:
                return sandbox.WindowsSandboxDoctorReport.ready_for_tests()
            available = sandbox.WindowsSandboxDoctorReport.ready_for_tests()
            return sandbox.WindowsSandboxDoctorReport(
                implementation=available.implementation,
                platform_supported=available.platform_supported,
                platform_status=available.platform_status,
                primitives=available.primitives,
                setup=available.setup,
                execution=available.execution,
                available=False,
                enforcement_status="backend_unavailable",
                blocking_requirements=("setup:network_filter",),
                recommended_action="rerun setup",
            )

    runner = _FakeRunner()
    backend = FlappingBackend(runner=runner)
    prepared = backend.prepare(_request(tmp_path))

    result = backend.run(prepared)

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert result.metadata["error_code"] == "backend_unavailable"
    assert runner.calls == []


def test_windows_backend_blocks_external_writable_paths_until_acl_projection_exists(
    tmp_path: Path,
) -> None:
    backend = _FakeReadyWindowsBackend()
    request = _request(tmp_path)
    request.profile.filesystem.writable_paths = [str(tmp_path), str(tmp_path.parent / "shared")]

    with pytest.raises(SandboxCapabilityError, match="additional writable directories"):
        backend.prepare(request)


def test_windows_backend_blocks_path_specific_readonly_until_acl_leases_exist(
    tmp_path: Path,
) -> None:
    backend = _FakeReadyWindowsBackend()
    request = _request(tmp_path)
    request.profile.filesystem.readonly_paths = ["src"]

    with pytest.raises(SandboxCapabilityError, match="readonly leases"):
        backend.prepare(request)


def test_windows_backend_redacts_output_and_respects_output_limit(tmp_path: Path) -> None:
    secret = "sk-test-secret-value"
    backend = _FakeReadyWindowsBackend(
        runner=_FakeRunner(stdout=f"{secret}\n" + ("A" * 100), stderr="")
    )
    request = _request(tmp_path)
    request.profile.resources.max_output_chars = 40
    prepared = backend.prepare(request)

    result = backend.run(prepared)

    assert secret not in result.stdout
    assert "redacted" in result.stdout.lower()
    assert len(result.stdout) <= 40
    assert result.metadata["output_truncated"] is True


def test_apply_account_acl_scopes_low_integrity_to_workspace(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    commands: list[list[str]] = []

    def fake_run_command(command: list[str]) -> subprocess.CompletedProcess[str]:
        commands.append(command)
        return subprocess.CompletedProcess(command, 0, "", "")

    monkeypatch.setattr(windows.shutil, "which", lambda _name: "icacls.exe")
    monkeypatch.setattr(windows, "_run_command", fake_run_command)
    run_root = tmp_path / "run"
    workspace_root = run_root / "workspace"
    workspace_root.mkdir(parents=True)

    result = windows._apply_account_acl(run_root, low_integrity_root=workspace_root)

    assert result.ok
    assert commands[0][1] == str(run_root)
    assert commands[1][1] == str(workspace_root)


def test_network_probe_requires_host_connectivity_baseline(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    launched = False

    def fake_run(_self, _prepared):
        nonlocal launched
        launched = True
        raise AssertionError("runner should not launch without host network baseline")

    monkeypatch.setattr(
        windows,
        "_network_state",
        lambda _sid: sandbox.WindowsCapabilityState(
            "available",
            True,
            "firewall configured",
            {},
        ),
    )
    monkeypatch.setattr(
        windows,
        "_host_network_baseline_state",
        lambda: sandbox.WindowsCapabilityState(
            "missing",
            True,
            "Host outbound connectivity baseline failed.",
            {"probe": "host"},
        ),
    )
    monkeypatch.setattr(windows.WindowsSandboxRunner, "run", fake_run)

    state = windows._network_probe_state("S-1-5-21-123")

    assert state.status == "missing"
    assert "baseline" in state.reason.lower()
    assert launched is False


def test_windows_runner_result_serialization_redacts_output() -> None:
    secret = "sk-test-secret-value"
    result = sandbox.WindowsRunnerResult(
        exit_code=0,
        stdout=f"{secret}\n",
        stderr=f"token={secret}\n",
        timed_out=False,
        started_at="2026-06-27T00:00:00+00:00",
        ended_at="2026-06-27T00:00:01+00:00",
        duration_ms=1,
        metadata={"api_key": secret},
    )

    payload = result.to_dict()

    encoded = json.dumps(payload, sort_keys=True)
    assert secret not in encoded
    assert "<redacted>" in encoded


def test_child_output_removes_raw_temp_files(tmp_path: Path) -> None:
    stdout_path = tmp_path / "child.stdout"
    stderr_path = tmp_path / "child.stderr"
    stdout_path.write_text("stdout", encoding="utf-8")
    stderr_path.write_text("stderr", encoding="utf-8")
    process = sandbox.windows_runner._WindowsChildProcess(
        command=["python"],
        process_handle=0,
        thread_handle=0,
        process_id=123,
        job_handle=None,
        job_assigned=False,
        desktop_handle=None,
        stdout_path=stdout_path,
        stderr_path=stderr_path,
        streams=[],
    )

    stdout, stderr, _truncated = sandbox.windows_runner._child_output(process, None)

    assert stdout == "stdout"
    assert stderr == "stderr"
    assert not stdout_path.exists()
    assert not stderr_path.exists()


def test_windows_sandbox_runner_removes_account_runner_logs(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    run_root = tmp_path / "run"
    run_root.mkdir()
    result_path = run_root / "runner-result.json"
    result = sandbox.WindowsRunnerResult(
        exit_code=0,
        stdout="ok",
        stderr="",
        timed_out=False,
        started_at="2026-06-27T00:00:00+00:00",
        ended_at="2026-06-27T00:00:01+00:00",
        duration_ms=1,
    )
    result_path.write_text(json.dumps(result.to_dict()), encoding="utf-8")
    stdout_path = run_root / "account-runner.stdout"
    stderr_path = run_root / "account-runner.stderr"
    stdout_path.write_text("sk-test-secret-value", encoding="utf-8")
    stderr_path.write_text("stderr", encoding="utf-8")
    class FakeAccountProcess:
        args = ["python"]
        returncode = 0

        def wait(self, timeout=None):
            return 0

        def kill(self):
            self.returncode = 1

        def stdout_text(self):
            return sandbox.windows_runner._read_and_unlink_text(stdout_path)

        def stderr_text(self):
            return sandbox.windows_runner._read_and_unlink_text(stderr_path)

    fake_process = FakeAccountProcess()
    prepared = type(
        "Prepared",
        (),
        {
            "baseline": {"runner_spec": str(run_root / "runner-spec.json"), "runner_result": str(result_path)},
            "sandbox_root": run_root,
            "request": type(
                "Request",
                (),
                {"profile": type("Profile", (), {"resources": type("Resources", (), {"timeout_seconds": 1})()})()},
            )(),
        },
    )()

    monkeypatch.setattr(sandbox.windows_runner.os, "name", "nt")
    monkeypatch.setattr(
        sandbox.windows_runner,
        "_read_generic_credential",
        lambda _target: ("SingularitySandboxRunner", "secret"),
    )
    monkeypatch.setattr(
        sandbox.windows_runner,
        "_start_account_process",
        lambda *args, **kwargs: fake_process,
    )

    runner_result = sandbox.WindowsSandboxRunner().run(prepared)

    assert runner_result.exit_code == 0
    assert not stdout_path.exists()
    assert not stderr_path.exists()
