from __future__ import annotations

import ast
import errno
import importlib
from pathlib import Path

import singularity.sandbox.windows as windows
from singularity.sandbox.windows_common import (
    ACCESS_DENIED_ERRNO_VALUES,
    WINDOWS_ERROR_ACCESS_DENIED,
    _is_create_process_with_logon_access_denied,
)


def test_windows_sandbox_capability_modules_own_private_helpers() -> None:
    module_names = (
        "singularity.sandbox.windows_identity",
        "singularity.sandbox.windows_acl",
        "singularity.sandbox.windows_firewall",
        "singularity.sandbox.windows_runtime",
        "singularity.sandbox.windows_doctor",
        "singularity.sandbox.windows_cleanup",
    )

    modules = {name: importlib.import_module(name) for name in module_names}

    expected_exports = {
        "singularity.sandbox.windows_identity": "_ensure_sandbox_identity",
        "singularity.sandbox.windows_acl": "_apply_sandbox_control_dir_acl",
        "singularity.sandbox.windows_firewall": "_network_state",
        "singularity.sandbox.windows_runtime": "_runner_smoke_state",
        "singularity.sandbox.windows_doctor": "probe_windows_sandbox",
        "singularity.sandbox.windows_cleanup": "cleanup_windows_sandbox_assets",
    }

    for module_name, symbol_name in expected_exports.items():
        moved_symbol = getattr(modules[module_name], symbol_name)
        assert moved_symbol.__module__ == module_name
        if symbol_name.startswith("_"):
            assert symbol_name not in windows.__all__
            assert not hasattr(windows, symbol_name)

    assert windows.__name__ == "singularity.sandbox.windows"


def test_windows_sandbox_platform_checks_are_patchable_without_mutating_os_name() -> None:
    production_files = (
        Path("src/singularity/sandbox/windows.py"),
        Path("src/singularity/sandbox/windows_runner.py"),
        Path("src/singularity/sandbox/windows_acl.py"),
        Path("src/singularity/sandbox/windows_cleanup.py"),
        Path("src/singularity/sandbox/windows_firewall.py"),
        Path("src/singularity/sandbox/windows_runtime.py"),
    )
    forbidden_production_patterns = ('os.name == "nt"', 'os.name != "nt"', "_windows.os.name")
    for path in production_files:
        text = path.read_text(encoding="utf-8")
        for pattern in forbidden_production_patterns:
            assert pattern not in text, f"{path} contains bare platform check {pattern!r}"

    tests_text = Path("tests/test_sandbox_backend_windows.py").read_text(encoding="utf-8")
    forbidden_test_patterns = (
        'setattr(windows.os, "name"',
        'setattr(sandbox.windows_runner.os, "name"',
        'setattr(windows_runner.os, "name"',
    )
    for pattern in forbidden_test_patterns:
        assert pattern not in tests_text, f"test patches shared os module via {pattern!r}"


def test_windows_sandbox_owner_modules_do_not_call_back_into_facade() -> None:
    owner_paths = (
        Path("src/singularity/sandbox/windows_identity.py"),
        Path("src/singularity/sandbox/windows_acl.py"),
        Path("src/singularity/sandbox/windows_firewall.py"),
        Path("src/singularity/sandbox/windows_runtime.py"),
        Path("src/singularity/sandbox/windows_doctor.py"),
        Path("src/singularity/sandbox/windows_cleanup.py"),
    )
    forbidden_imports = (
        "import singularity.sandbox.windows as _windows",
        "from singularity.sandbox import windows",
    )
    for path in owner_paths:
        text = path.read_text(encoding="utf-8")
        for forbidden in forbidden_imports:
            assert forbidden not in text, f"{path} imports the windows facade"


def test_windows_sandbox_facade_is_thin() -> None:
    path = Path("src/singularity/sandbox/windows.py")
    text = path.read_text(encoding="utf-8")
    assert sum(1 for _ in text.splitlines()) < 1200
    assert "sys.modules[__name__]" not in text

    tree = ast.parse(text)
    defined = {node.name for node in tree.body if isinstance(node, ast.FunctionDef)}
    classes = {node.name for node in tree.body if isinstance(node, ast.ClassDef)}
    assert "WindowsSandboxBackend" in classes
    assert windows.WindowsSandboxBackend.__module__ == "singularity.sandbox.windows"
    assert "_probe_windows_sandbox_uncached" not in defined
    assert "_ensure_sandbox_identity" not in defined
    assert "_network_state" not in defined
    assert "_apply_sandbox_control_dir_acl" not in defined
    assert "_runner_smoke_state" not in defined
    assert "cleanup_windows_sandbox_assets" not in defined

    exported = windows.__all__
    assert "_probe_windows_sandbox_uncached" not in exported
    assert "_windows_state_dir_path" not in exported

    common_text = Path("src/singularity/sandbox/windows_common.py").read_text(encoding="utf-8")
    common_tree = ast.parse(common_text)
    common_classes = {node.name for node in common_tree.body if isinstance(node, ast.ClassDef)}
    assert "WindowsSandboxBackend" not in common_classes


def test_create_process_with_logon_access_denied_detection_uses_named_constants() -> None:
    assert WINDOWS_ERROR_ACCESS_DENIED == 5
    assert errno.EACCES in ACCESS_DENIED_ERRNO_VALUES

    access_denied = OSError("CreateProcessWithLogonW failed")
    access_denied.winerror = WINDOWS_ERROR_ACCESS_DENIED  # type: ignore[attr-defined]
    assert _is_create_process_with_logon_access_denied(access_denied) is True

    errno_denied = OSError("CreateProcessWithLogonW failed")
    errno_denied.errno = errno.EACCES  # type: ignore[attr-defined]
    assert _is_create_process_with_logon_access_denied(errno_denied) is True

    english_denied = OSError("CreateProcessWithLogonW failed: access is denied")
    assert _is_create_process_with_logon_access_denied(english_denied) is True

    chinese_denied = OSError("CreateProcessWithLogonW failed: 拒绝访问")
    assert _is_create_process_with_logon_access_denied(chinese_denied) is True


def test_windows_common_source_avoids_bare_access_denied_literals() -> None:
    common_text = Path("src/singularity/sandbox/windows_common.py").read_text(encoding="utf-8")

    assert 'getattr(exc, "winerror", None) == 5' not in common_text
    assert 'getattr(exc, "errno", None) in {5, 13}' not in common_text
