from __future__ import annotations

import importlib

import singularity.sandbox.windows as windows


def test_windows_sandbox_capability_modules_keep_facade_exports() -> None:
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
        assert moved_symbol is getattr(windows, symbol_name)
        assert moved_symbol.__module__ == module_name
