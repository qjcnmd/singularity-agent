from __future__ import annotations

import json
from pathlib import Path

from singularity.observability import TraceRuntime
from singularity.plugins.discovery import discover_plugins
from singularity.plugins.runtime import PluginRuntime
from singularity.plugins.status import PluginStatusStore
from singularity.tools import ToolOriginKind, ToolPolicy, ToolRegistry, ToolRuntime

from tests.tool_runtime_helpers import make_test_policy_runtime


def test_enabled_tool_plugin_registers_exports_executes_and_traces(tmp_path: Path) -> None:
    plugin_dir = tmp_path / ".singularity" / "plugins" / "echo_plugin"
    plugin_dir.mkdir(parents=True)
    _write_manifest(plugin_dir, require_prefix=True)
    (plugin_dir / "plugin.py").write_text(
        """
def register(host):
    config = host.read_config()

    def echo(args):
        return {"text": config.get("prefix", "") + args.text}

    host.register_tool(
        name="echo",
        description="Echo configured text.",
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
""".strip()
        + "\n",
        encoding="utf-8",
    )
    discovered = discover_plugins(tmp_path)[0]
    PluginStatusStore(tmp_path).enable(discovered, config={"prefix": ">"})
    trace = TraceRuntime.create(tmp_path, trace_dir=tmp_path / "traces")
    registry = ToolRegistry(tmp_path, include_default_tools=False)

    diagnostics = PluginRuntime(tmp_path, trace=trace).activate(
        registry=registry,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )

    assert diagnostics == []
    assert registry.get("echo_plugin__echo") is not None
    record = registry.get_record("echo_plugin__echo")
    assert record is not None
    assert record.origin.kind == ToolOriginKind.PLUGIN
    assert record.origin.plugin_id == "echo_plugin"
    assert record.origin.local_tool_name == "echo"
    assert record.origin.exposed_name == "echo_plugin__echo"
    assert record.origin.manifest_hash == discovered.manifest_hash
    assert record.origin.required_permissions == ("read_workspace",)
    exported = registry.to_openai_tools()
    assert exported[0]["function"]["name"] == "echo_plugin__echo"
    assert set(exported[0]["function"]) == {"name", "description", "parameters"}
    assert "plugin_id" not in json.dumps(exported)
    assert "manifest_hash" not in json.dumps(exported)
    assert "approved_permissions" not in json.dumps(exported)

    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=trace,
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )
    result = runtime.execute_tool_call(
        {
            "id": "call_1",
            "function": {
                "name": "echo_plugin__echo",
                "arguments": json.dumps({"text": "hello"}),
            },
        }
    )

    assert result.ok is True
    assert result.content == {"text": ">hello"}
    event_types = [event.event_type.value for event in trace.store.query_events()]
    assert "plugin.load_started" in event_types
    assert "plugin.load_completed" in event_types
    assert "plugin.tool_registered" in event_types
    assert "plugin.activated" in event_types
    assert "tool.dispatch.completed" in event_types


def test_plugin_host_custom_trace_event_uses_stable_trace_type(tmp_path: Path) -> None:
    plugin_dir = tmp_path / ".singularity" / "plugins" / "trace_plugin"
    plugin_dir.mkdir(parents=True)
    _write_manifest(plugin_dir, plugin_id="trace_plugin")
    (plugin_dir / "plugin.py").write_text(
        """
def register(host):
    host.emit_trace("plugin.custom_event", {"status": "ok"})
""".strip()
        + "\n",
        encoding="utf-8",
    )
    discovered = discover_plugins(tmp_path)[0]
    PluginStatusStore(tmp_path).enable(discovered)
    trace = TraceRuntime.create(tmp_path, trace_dir=tmp_path / "traces")

    diagnostics = PluginRuntime(tmp_path, trace=trace).activate(
        registry=ToolRegistry(tmp_path, include_default_tools=False),
        policy_runtime=make_test_policy_runtime(tmp_path),
    )

    assert diagnostics == []
    events = [
        event
        for event in trace.store.query_events()
        if event.event_type.value == "plugin.event"
    ]
    assert events[0].payload["plugin_event"] == "plugin.custom_event"


def _write_manifest(
    plugin_dir: Path,
    *,
    plugin_id: str = "echo_plugin",
    require_prefix: bool = False,
) -> None:
    required = 'required = ["prefix"]' if require_prefix else ""
    prefix_schema = (
        """

[config_schema.properties.prefix]
type = "string"
"""
        if require_prefix
        else ""
    )
    (plugin_dir / "plugin.toml").write_text(
        f"""
id = "{plugin_id}"
name = "Echo Plugin"
version = "0.1.0"
api_version = "1"
entrypoint = "plugin.py:register"
type = "tool"
capabilities = ["echo"]
permissions = ["read_workspace"]

[activation]
mode = "manual"

[compatibility]
min_python = "3.11"

[config_schema]
type = "object"
{required}
additionalProperties = false
{prefix_schema}
""".strip()
        + "\n",
        encoding="utf-8",
    )
