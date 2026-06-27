from pathlib import Path

import pytest

import singularity.sandbox as sandbox
from singularity.sandbox import SandboxCapabilityError, SandboxProfileName, SandboxRequest


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

    assert payload["implementation"] == "elevated"
    assert set(payload["primitives"]) == {
        "restricted_token",
        "job_object",
        "low_integrity",
        "acl",
        "firewall",
        "private_desktop",
    }
    assert set(payload["setup"]) == {
        "sandbox_account",
        "acl_boundary",
        "network_filter",
        "private_desktop",
        "execution_backend",
    }
    assert report.available == (
        all(payload["primitives"].values()) and all(payload["setup"].values())
    )
    assert report.missing_requirements


def test_windows_backend_is_unavailable_until_all_enforcement_is_configured() -> None:
    backend = sandbox.WindowsSandboxBackend()
    report = backend.doctor()

    assert report.setup.execution_backend is False
    assert backend.is_available() is False
    assert backend.capabilities().filesystem_isolation is False
    assert backend.capabilities().network_isolation is False


def test_windows_setup_fails_explicitly_instead_of_claiming_success() -> None:
    backend = sandbox.WindowsSandboxBackend()

    with pytest.raises(sandbox.SandboxSetupError, match="elevated Windows sandbox setup"):
        backend.setup()


def test_windows_backend_never_prepares_without_completed_setup(tmp_path: Path) -> None:
    backend = sandbox.WindowsSandboxBackend()

    with pytest.raises(SandboxCapabilityError, match="backend_unavailable"):
        backend.prepare(_request(tmp_path))
