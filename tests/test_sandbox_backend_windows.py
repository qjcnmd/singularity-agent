import os
import sys
from pathlib import Path

import pytest

from singularity.sandbox import (
    SandboxCapabilityError,
    SandboxNetworkMode,
    SandboxNetworkPolicy,
    SandboxProfileName,
    SandboxRequest,
    SandboxStatus,
    WindowsRestrictedTokenBackend,
    default_sandbox_profile,
    windows_restricted_token_available,
)


def _request(tmp_path: Path) -> SandboxRequest:
    return SandboxRequest(
        sandbox_id="sandbox_windows",
        session_id="session",
        task_id="task",
        action_id="action",
        command=[
            sys.executable,
            "-c",
            "from pathlib import Path; Path('windows.txt').write_text('ok', encoding='utf-8'); print('windows')",
        ],
        cwd=tmp_path,
        workspace_root=tmp_path,
        profile=default_sandbox_profile(
            SandboxProfileName.ISOLATED_VERIFICATION,
            workspace_root=tmp_path,
        ),
    )


def _absolute_write_request(tmp_path: Path, target: Path) -> SandboxRequest:
    request = _request(tmp_path)
    request.command = [
        sys.executable,
        "-c",
        (
            "from pathlib import Path\n"
            f"target = Path({str(target)!r})\n"
            "try:\n"
            "    target.write_text('escape', encoding='utf-8')\n"
            "    print('wrote')\n"
            "except OSError as exc:\n"
            "    print(type(exc).__name__)\n"
        ),
    ]
    return request


def test_windows_restricted_backend_capabilities_are_honest() -> None:
    capabilities = WindowsRestrictedTokenBackend().capabilities()

    assert capabilities.filesystem_isolation is True
    assert capabilities.copy_on_write is True
    assert capabilities.network_isolation is False
    assert capabilities.process_tree_kill is True
    assert capabilities.memory_limit is False


def test_windows_restricted_backend_fails_closed_for_hard_network(tmp_path: Path) -> None:
    backend = WindowsRestrictedTokenBackend()
    request = _request(tmp_path)
    request.profile.network = SandboxNetworkPolicy(
        mode=SandboxNetworkMode.DENIED,
        require_hard_isolation=True,
    )

    with pytest.raises(SandboxCapabilityError):
        backend.prepare(request)


def test_real_windows_restricted_backend_smoke(tmp_path: Path) -> None:
    if os.name != "nt":
        pytest.skip("Windows restricted token backend is Windows-only.")
    if not windows_restricted_token_available(use_cache=False):
        pytest.skip("Windows restricted token APIs are unavailable.")
    backend = WindowsRestrictedTokenBackend()
    prepared = backend.prepare(_request(tmp_path))
    try:
        result = backend.run(prepared)
    finally:
        backend.cleanup(prepared)

    assert result.status == SandboxStatus.SUCCESS
    assert result.backend_name == "windows_restricted_token"
    assert result.stdout.strip() == "windows"
    assert not (tmp_path / "windows.txt").exists()
    assert result.filesystem_changes.created_files == ["windows.txt"]
    assert result.metadata["network_isolation_enforced"] is False
    assert result.metadata["restricted_token"] is True
    assert result.metadata["integrity_level"] == "low"


def test_real_windows_restricted_backend_blocks_absolute_host_write(tmp_path: Path) -> None:
    if os.name != "nt":
        pytest.skip("Windows restricted token backend is Windows-only.")
    if not windows_restricted_token_available(use_cache=False):
        pytest.skip("Windows restricted token APIs are unavailable.")
    escape_target = tmp_path / "escape.txt"
    backend = WindowsRestrictedTokenBackend()
    prepared = backend.prepare(_absolute_write_request(tmp_path, escape_target))
    try:
        result = backend.run(prepared)
    finally:
        backend.cleanup(prepared)

    assert result.status == SandboxStatus.SUCCESS
    assert "wrote" not in result.stdout
    assert not escape_target.exists()
