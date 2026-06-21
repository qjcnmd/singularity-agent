from __future__ import annotations

from pathlib import Path

from miniharness.observability import TraceRuntime
from miniharness.plugins.diagnostics import validate_config
from miniharness.plugins.discovery import discover_plugins
from miniharness.plugins.runtime import PluginRuntime
from miniharness.plugins.status import PluginStatusStore
from miniharness.tools import ToolRegistry


def test_disabled_plugin_is_not_loaded(tmp_path: Path) -> None:
    plugin_dir = _plugin_dir(tmp_path, "disabled_plugin")
    sentinel = tmp_path / "imported.txt"
    _write_manifest(plugin_dir, plugin_id="disabled_plugin")
    _write_plugin(plugin_dir, f"Path({str(sentinel)!r}).write_text('imported')\n")

    runtime = PluginRuntime(tmp_path)
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    diagnostics = runtime.activate(registry=registry)

    assert diagnostics == []
    assert not sentinel.exists()
    assert registry.list() == []


def test_enable_disable_status_and_hash_mismatch(tmp_path: Path) -> None:
    plugin_dir = _plugin_dir(tmp_path, "hash_plugin")
    _write_manifest(plugin_dir, plugin_id="hash_plugin")
    _write_plugin(plugin_dir)
    discovered = discover_plugins(tmp_path)[0]
    store = PluginStatusStore(tmp_path)

    enabled = store.enable(discovered)
    assert enabled.enabled is True

    manifest_path = plugin_dir / "plugin.toml"
    manifest_path.write_text(
        manifest_path.read_text(encoding="utf-8").replace('version = "0.1.0"', 'version = "0.1.1"'),
        encoding="utf-8",
    )
    runtime = PluginRuntime(tmp_path)
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    diagnostics = runtime.activate(registry=registry)

    assert any(diagnostic.code == "manifest_hash_mismatch" for diagnostic in diagnostics)
    assert registry.list() == []

    disabled = store.disable("hash_plugin")
    assert disabled.enabled is False


def test_permission_refusal_is_isolated_to_plugin(tmp_path: Path) -> None:
    plugin_dir = _plugin_dir(tmp_path, "permission_plugin")
    _write_manifest(plugin_dir, plugin_id="permission_plugin", permissions="[]")
    _write_plugin(plugin_dir)
    discovered = discover_plugins(tmp_path)[0]
    PluginStatusStore(tmp_path).enable(discovered)

    runtime = PluginRuntime(tmp_path)
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    diagnostics = runtime.activate(registry=registry)

    assert any("did not declare required permissions" in diagnostic.message for diagnostic in diagnostics)
    assert registry.list() == []


def test_config_schema_validation_rejects_bad_config() -> None:
    diagnostics = validate_config(
        {
            "type": "object",
            "properties": {"prefix": {"type": "string"}},
            "required": ["prefix"],
            "additionalProperties": False,
        },
        {"prefix": 1, "extra": True},
    )

    assert {diagnostic.code for diagnostic in diagnostics} == {
        "config_type_mismatch",
        "config_unknown_key",
    }


def test_plugin_exception_does_not_crash_runtime(tmp_path: Path) -> None:
    plugin_dir = _plugin_dir(tmp_path, "broken_plugin")
    _write_manifest(plugin_dir, plugin_id="broken_plugin")
    _write_plugin(plugin_dir, "raise RuntimeError('boom')\n")
    discovered = discover_plugins(tmp_path)[0]
    PluginStatusStore(tmp_path).enable(discovered)
    trace = TraceRuntime.create(tmp_path, trace_dir=tmp_path / "traces")

    runtime = PluginRuntime(tmp_path, trace=trace)
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    diagnostics = runtime.activate(registry=registry)

    assert any(diagnostic.code == "plugin_load_failed" for diagnostic in diagnostics)
    assert registry.list() == []
    assert any(event.event_type.value == "plugin.load_failed" for event in trace.store.query_events())


def _plugin_dir(root: Path, name: str) -> Path:
    path = root / ".miniharness" / "plugins" / name
    path.mkdir(parents=True, exist_ok=True)
    return path


def _write_manifest(
    plugin_dir: Path,
    *,
    plugin_id: str,
    permissions: str = '["read_workspace"]',
) -> None:
    (plugin_dir / "plugin.toml").write_text(
        f"""
id = "{plugin_id}"
name = "{plugin_id}"
version = "0.1.0"
api_version = "1"
entrypoint = "plugin.py:register"
type = "tool"
capabilities = ["echo"]
permissions = {permissions}

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


def _write_plugin(plugin_dir: Path, register_body: str | None = None) -> None:
    body = register_body or """
def echo(args):
    return {"text": args.text}
host.register_tool(
    name="echo",
    description="Echo text.",
    input_schema={
        "type": "object",
        "properties": {"text": {"type": "string"}},
        "required": ["text"],
        "additionalProperties": False,
    },
    handler=echo,
    risk_level="low",
    required_permissions=["read_workspace"],
)
"""
    (plugin_dir / "plugin.py").write_text(
        "from pathlib import Path\n\n"
        "def register(host):\n"
        + "".join(f"    {line}\n" if line else "\n" for line in body.splitlines()),
        encoding="utf-8",
    )
