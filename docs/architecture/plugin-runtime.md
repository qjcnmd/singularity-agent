# Plugin Runtime Architecture

Singularity supports local-only plugins through `singularity.plugins`. The runtime is manifest-first, host-controlled, permission-gated, compatible with the current API version, and traceable. It does not implement a Git runtime, a remote plugin marketplace, remote updates, or automatic dependency installation.

## Lifecycle

Plugin lifecycle is:

```text
discover manifest
-> validate manifest schema
-> check enabled status
-> check manifest hash and path
-> check API/Singularity/Python compatibility
-> validate permissions and config
-> policy gate local plugin load
-> import entrypoint from the discovered plugin directory
-> call register(host)
-> register declared contributions into Singularity registries
-> write lock/status and trace events
```

Discovery only reads `plugin.toml` or `singularity-plugin.toml`. It never imports plugin code. Import happens only in `PluginLoader` after an enabled plugin passes validation and policy gates.

## Directories

Discovery order is stable:

1. Project: `.singularity/plugins/`
2. Environment: each directory in `SINGULARITY_PLUGIN_PATH`, split by `os.pathsep`
3. User config: `resolve_runtime_paths(...).config_dir / "plugins"`

Project status is stored in:

```text
.singularity/plugin-status.json
.singularity/plugin-lock.json
```

Plugins are disabled by default. Enabling records plugin id, version, normalized path, manifest hash, approved permissions, config, and compatibility status. If the manifest hash or path changes, the enabled record no longer authorizes loading; the plugin must be enabled again after review.

## Manifest

Required manifest fields:

```toml
id = "local_echo"
name = "Local Echo"
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
additionalProperties = false
```

Supported plugin types are `tool`, `provider`, `prompt`, `memory`, `eval`, and `project_adapter`. v1 fully wires `tool` plugins into `ToolRegistry`; the other types are declarative contributions for manifest, discovery, status, compatibility, and diagnostics.

Plugin ids and tool local names must be safe slugs. A plugin tool named `echo` from plugin `local_echo` is exposed to the model as:

```text
local_echo__echo
```

This avoids collisions with built-in tools and keeps OpenAI-style function names valid.

## Security Model

The plugin runtime is a local trusted or semi-trusted extension mechanism. It provides control-plane safety, not full Python module sandboxing inside the main process.

The enforced boundaries are:

- manifest-first discovery; no code execution during discovery
- default disabled; status must explicitly enable a discovered id/path/hash
- normalized entrypoint path must stay inside the plugin directory, including symlink resolution
- absolute paths and `..` entrypoints are rejected
- API version, Singularity version, Python version, permissions, config, and policy are checked before load
- plugins receive only `PluginHost`, not internal Singularity runtime objects
- plugin failures become diagnostics and trace events instead of crashing the runtime graph
- high-risk tool execution still flows through `ToolRuntime`, `PolicyRuntime`, `ApprovalGate`, `CommandRuntime`, `SandboxRuntime`, and trace

## Host API

The stable plugin API is intentionally small:

```python
def register(host):
    config = host.read_config()
    host.emit_trace("plugin.custom_event", {"safe": True})
    host.register_tool(...)
```

`PluginHost.register_tool()` requires:

- `name`
- `description`
- `input_schema`
- `handler`
- `risk_level`
- `required_permissions`

The host converts the declaration into a `ToolSpec`. The plugin never receives `ToolRegistry`, `ToolRuntime`, policy, approval, command, sandbox, trace store, or planner objects.

## Trace

Plugin runtime emits:

- `plugin.discovered`
- `plugin.check_failed`
- `plugin.enabled`
- `plugin.disabled`
- `plugin.load_started`
- `plugin.load_completed`
- `plugin.load_failed`
- `plugin.activated`
- `plugin.tool_registered`
- `plugin.event`

Payloads include safe summaries such as plugin id, version, manifest hash, status, and diagnostic codes. Raw plugin source and secret config values are not written intentionally; normal trace redaction still applies.

## CLI

```text
singularity-agent plugin list --json
singularity-agent plugin inspect local_echo --json
singularity-agent plugin check local_echo --json
singularity-agent plugin enable local_echo --json
singularity-agent plugin disable local_echo --json
```

`--mode` and `--home` use the same runtime path resolution as the system commands for user-level discovery. Enabling is project-level and writes `.singularity/plugin-status.json`.

## Minimal Tool Plugin

Directory:

```text
.singularity/plugins/local_echo/
  plugin.toml
  plugin.py
```

`plugin.toml`:

```toml
id = "local_echo"
name = "Local Echo"
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
additionalProperties = false
```

`plugin.py`:

```python
def register(host):
    def echo(args):
        return {"text": args.text}

    host.register_tool(
        name="echo",
        description="Echo text through a local plugin.",
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
```

Enable and check:

```text
singularity-agent plugin check local_echo
singularity-agent plugin enable local_echo
singularity-agent plugin list
```
