from __future__ import annotations

from pathlib import Path

import pytest

from singularity.plugins.diagnostics import check_entrypoint_path
from singularity.plugins.discovery import discover_plugins


def test_manifest_discovery_reads_toml_without_importing_plugin(tmp_path: Path) -> None:
    plugin_dir = _plugin_dir(tmp_path, "demo_plugin")
    sentinel = tmp_path / "imported.txt"
    _write_manifest(plugin_dir, plugin_id="demo_plugin")
    (plugin_dir / "plugin.py").write_text(
        f"from pathlib import Path\nPath({str(sentinel)!r}).write_text('imported')\n",
        encoding="utf-8",
    )

    discovered = discover_plugins(tmp_path)

    assert [plugin.manifest.id for plugin in discovered] == ["demo_plugin"]
    assert not sentinel.exists()


def test_manifest_discovery_supports_both_manifest_names(tmp_path: Path) -> None:
    first = _plugin_dir(tmp_path, "first_plugin")
    second = _plugin_dir(tmp_path, "second_plugin")
    _write_manifest(first, plugin_id="first_plugin", filename="plugin.toml")
    _write_manifest(second, plugin_id="second_plugin", filename="singularity-plugin.toml")

    discovered = discover_plugins(tmp_path)

    assert [plugin.manifest.id for plugin in discovered] == ["first_plugin", "second_plugin"]


def test_manifest_discovery_order_project_env_user(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    project_plugin = _plugin_dir(tmp_path, "project_plugin")
    env_root = tmp_path / "env_plugins"
    user_home = tmp_path / "user_data_home"
    env_plugin = env_root / "env_plugin"
    user_plugin = user_home / "config" / "plugins" / "user_plugin"
    _write_manifest(project_plugin, plugin_id="project_plugin")
    _write_manifest(env_plugin, plugin_id="env_plugin")
    _write_manifest(user_plugin, plugin_id="user_plugin")
    monkeypatch.setenv("SINGULARITY_PLUGIN_PATH", str(env_root))

    discovered = discover_plugins(tmp_path, home=user_home)

    assert [plugin.manifest.id for plugin in discovered] == [
        "project_plugin",
        "env_plugin",
        "user_plugin",
    ]


def test_duplicate_plugin_ids_are_diagnostic(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    project_plugin = _plugin_dir(tmp_path, "demo_project")
    env_root = tmp_path / "env_plugins"
    env_plugin = env_root / "demo_env"
    _write_manifest(project_plugin, plugin_id="demo_plugin")
    _write_manifest(env_plugin, plugin_id="demo_plugin")
    monkeypatch.setenv("SINGULARITY_PLUGIN_PATH", str(env_root))

    discovered = discover_plugins(tmp_path)

    assert len(discovered) == 2
    assert all(
        any(diagnostic.code == "duplicate_plugin_id" for diagnostic in plugin.diagnostics)
        for plugin in discovered
    )


def test_entrypoint_escape_is_rejected_at_manifest_validation(tmp_path: Path) -> None:
    plugin_dir = _plugin_dir(tmp_path, "escape_plugin")
    _write_manifest(plugin_dir, plugin_id="escape_plugin", entrypoint="../plugin.py:register")

    discovered = discover_plugins(tmp_path)

    assert any(diagnostic.code == "manifest_invalid" for diagnostic in discovered[0].diagnostics)


def test_symlink_entrypoint_escape_is_rejected_when_supported(tmp_path: Path) -> None:
    outside = tmp_path / "outside.py"
    outside.write_text("def register(host):\n    pass\n", encoding="utf-8")
    plugin_dir = _plugin_dir(tmp_path, "link_plugin")
    _write_manifest(plugin_dir, plugin_id="link_plugin", entrypoint="link.py:register")
    try:
        (plugin_dir / "link.py").symlink_to(outside)
    except OSError:
        pytest.skip("symlink creation is not available in this environment")

    discovered = discover_plugins(tmp_path)
    diagnostics = check_entrypoint_path(discovered[0])

    assert any(diagnostic.code == "entrypoint_invalid" for diagnostic in diagnostics)


def _plugin_dir(root: Path, name: str) -> Path:
    path = root / ".singularity" / "plugins" / name
    path.mkdir(parents=True, exist_ok=True)
    return path


def _write_manifest(
    plugin_dir: Path,
    *,
    plugin_id: str,
    filename: str = "plugin.toml",
    entrypoint: str = "plugin.py:register",
) -> None:
    plugin_dir.mkdir(parents=True, exist_ok=True)
    (plugin_dir / filename).write_text(
        f"""
id = "{plugin_id}"
name = "{plugin_id}"
version = "0.1.0"
api_version = "1"
entrypoint = "{entrypoint}"
type = "tool"
capabilities = ["echo"]
permissions = ["read_workspace"]

[activation]
mode = "manual"

[compatibility]
min_python = "3.11"

[config_schema]
type = "object"
additionalProperties = false
""".strip()
        + "\n",
        encoding="utf-8",
    )
