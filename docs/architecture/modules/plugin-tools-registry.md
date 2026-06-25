# Plugin Tools Into ToolRegistry Runtime Flow

Runtime flow doc id: plugin-tools-registry
Source paths:
- src/singularity/kernel/graph.py
- src/singularity/plugins/manager.py
- src/singularity/plugins/status.py
- src/singularity/plugins/loader.py
- src/singularity/plugins/host.py
- src/singularity/plugins/models.py
- src/singularity/tools/models.py
- src/singularity/tools/registry.py

Symbols:
- AgentGraphBuilder
- AgentGraphBuilder._build_tools_protocol
- PluginManager
- PluginManager.activate
- PluginStatusStore
- PluginStatusStore.enabled_for
- PluginLockStore
- PluginLoader
- PluginLoader.load
- PluginHost
- PluginHost.register_tool
- DiscoveredPlugin
- PluginDiagnostic
- PluginToolContribution
- PluginLockEntry
- PluginStatus
- ToolOrigin
- ToolSpec
- ToolRegistry
- ToolRegistry.register

Field checks:
- DiscoveredPlugin: manifest, manifest_path, plugin_dir, source, manifest_hash, diagnostics
- PluginStatus: enabled, version, path, manifest_hash, approved_permissions, config, compatibility_status
- PluginToolContribution: plugin_id, local_name, exposed_name, required_permissions, spec
- PluginDiagnostic: plugin_id, severity, code, message, path, details
- PluginLockEntry: plugin_id, version, path, manifest_hash, compatibility_status, enabled
- ToolOrigin: kind, plugin_id, local_tool_name, exposed_name, manifest_hash, source_path, required_permissions, approved_permissions, activation_hash, schema_digest
- ToolSpec: name, version, description, input_model, output_model, handler, permission_level, risk_tags, timeout_seconds, max_output_chars, cacheable, idempotent, uses_edit_executor, uses_mutation_manager, uses_command_executor, delegates_policy_constraints, capabilities, operation, resource_resolver, side_effects, sensitivity, cache_policy, idempotency_policy, execution_backend, approval_profile, artifact_policy, enabled

## Module Boundary

This module owns the flow that admits plugin-contributed tools into the same `ToolRegistry` used by built-in tools.

It is responsible for plugin discovery status checks, manifest hash/path matching, plugin load, contribution permission checks, contribution schema checks, risk tag checks, approval profile checks, and registry registration with `ToolOriginKind.PLUGIN`.

It is not responsible for executing plugin tools differently after registration. Once admitted, plugin tools use the same `ToolExecutor`, policy, approval, tool protocol, context, and trace paths as built-in tools.

## Current Source Locations

- `src/singularity/kernel/graph.py`: `_build_tools_protocol()` creates `PluginManager` after built-in tool registration and before `ToolExecutor`.
- `src/singularity/plugins/manager.py`: `PluginManager.activate()`, `_admit_tool_contribution()`, and `_tool_origin()`.
- `src/singularity/plugins/status.py`: `PluginStatusStore.get()`, `PluginStatusStore.enabled_for()`, and `PluginLockStore`.
- `src/singularity/plugins/loader.py`: `PluginLoader.load()` imports and calls plugin registration entrypoints.
- `src/singularity/plugins/host.py`: `PluginHost.register_tool()` builds `PluginToolContribution`.
- `src/singularity/plugins/models.py`: plugin manifest/status/contribution models.
- `src/singularity/tools/models.py`: `ToolOrigin`, `ToolOriginKind`, `ToolSpec`.

## Runtime Call Chain

1. `AgentGraphBuilder._build_tools_protocol()` registers built-in tool groups into `ToolRegistry`.
2. It constructs `PluginManager(project_root, trace=trace)`.
3. `PluginManager.activate(registry=tools, policy_engine=policy_engine)` calls `discover()`.
4. For each discovered plugin, `PluginStatusStore.get(plugin_id)` reads the persisted status and skips absent or disabled entries.
5. `PluginStatusStore.enabled_for(plugin)` rechecks enabled status and rejects status records whose path or `manifest_hash` no longer matches the discovered plugin.
6. `check_plugin()` and duplicate id checks produce diagnostics.
7. `_policy_gate()` optionally calls `PolicyEngine.enforce()` before loading plugin code.
8. `PluginLoader.load()` calls the plugin registration entrypoint with `PluginHost`.
9. `PluginHost.register_tool()` creates `PluginToolContribution` values with an underlying `ToolSpec`.
10. `_admit_tool_contribution()` checks identity, declared and approved permissions, derived permission shape, schema root, root `additionalProperties`, plugin risk tags, approval profile, and high-risk gates.
11. `ToolRegistry.register(contribution.spec, origin=_tool_origin(...), admitted=True, admission_reason="plugin_contribution_admitted")` inserts the plugin tool.

## Runtime Objects Passed

- `DiscoveredPlugin`: manifest, manifest path, plugin directory, source, and `manifest_hash`.
- `PluginStatus`: `enabled`, `version`, `path`, `manifest_hash`, `approved_permissions`, `config`, `compatibility_status`.
- `PluginToolContribution`: `plugin_id`, `local_name`, `exposed_name`, `required_permissions`, `spec`.
- `ToolSpec`: same internal execution contract used by built-in tools.
- `ToolOrigin`: `kind=PLUGIN`, `plugin_id`, `local_tool_name`, `exposed_name`, `manifest_hash`, `source_path`, `required_permissions`, `approved_permissions`, `activation_hash`, `schema_digest`.
- `PluginDiagnostic`: plugin id, severity, code, message, path, details.

## Model-Visible Objects (模型实际可见对象)

After plugin admission, the model sees the plugin tool exactly like any other visible tool:

- provider function `name`;
- provider function `description`;
- provider function `parameters`;
- optional provider function `strict`.

Plugin identity is not emitted by `ModelToolRenderer.to_provider_tools()`. `ModelToolRenderer.render()` stores `origin` and optional `plugin_id` in `ModelToolSchema.metadata`, but the provider schema conversion does not include that metadata.

## Internal Trace Debug Audit Objects (内部 trace/debug/audit 对象)

Internal-only plugin data includes:

- `PluginStatus.path`, `manifest_hash`, `approved_permissions`, and `config`;
- `PluginLockEntry` records written by `PluginLockStore`;
- `PluginDiagnostic` details;
- `ToolOrigin` plugin metadata;
- plugin manager trace events: `PLUGIN_DISCOVERED`, `PLUGIN_CHECK_FAILED`, `PLUGIN_TOOL_REGISTERED`, and `PLUGIN_ACTIVATED`;
- plugin loader trace events: `PLUGIN_LOAD_STARTED`, `PLUGIN_LOAD_COMPLETED`, and `PLUGIN_LOAD_FAILED`;
- plugin host custom trace events: `PLUGIN_EVENT`;
- `_policy_gate()` policy request/decision ids.

## State Transitions And Failure Paths

- Disabled or absent plugin status skips activation.
- Path or manifest hash mismatch produces `plugin_status_mismatch` and prevents registration.
- Duplicate enabled plugin ids produce `duplicate_plugin_id_enabled`.
- Policy gate denial produces `plugin_policy_denied`.
- Policy gate exceptions produce `plugin_policy_gate_failed`.
- Loader failure keeps diagnostics and prevents contribution registration.
- Contribution identity/name mismatch prevents registration.
- Undeclared or unapproved permissions prevent registration.
- Schema root allowing extra properties prevents registration.
- Missing `plugin` and `plugin:<id>` risk tags prevents registration.
- Missing plugin approval profile prevents registration.
- High-risk permissions require a policy gate or explicit approval profile.
- `ToolRegistry.register()` can still reject duplicate or invalid `ToolSpec` values.

## Current Structure Assessment

The current structure is reasonable because plugins enter through a narrow admission path and then reuse the normal tool runtime. That avoids a parallel plugin execution path.

The main operational risk is that plugin governance spans manifest discovery, status store, loader, contribution admission, and registry records. The runtime flow doc must stay aligned with all of those files, not just `PluginManager`.

## Production-Grade Target Structure

Current code does not have a single durable `PluginToolAdmissionRecord` object. A production-grade target could add a proposed record that captures:

- proposed `admission_id`;
- proposed `plugin_status_snapshot`;
- proposed `policy_decision_id`;
- proposed `diagnostic_codes`;
- proposed `registered_tool_names`;
- proposed `provider_schema_hash`.

This is not current implementation. Today the durable pieces are plugin status, plugin lock, registry record, and trace events.

## Harness Usage Example

A local plugin contributes `format_markdown` with declared read/write permissions. The user enables it, producing `plugin-status.json` with a matching path and `manifest_hash`. During kernel graph build, `PluginManager.activate()` loads the plugin, validates the contribution, and registers its `ToolSpec` with `ToolOriginKind.PLUGIN`. In the next model turn, the model sees only a normal `format_markdown` function schema. The policy and trace layers still know it came from the plugin.

## Maintenance Rules

Update this document when changing:

- plugin manifest/status/lock fields;
- `PluginManager.activate()` or `_admit_tool_contribution()`;
- `PluginStatusStore.enabled_for()`;
- `PluginLoader.load()` or `PluginHost.register_tool()`;
- `ToolOrigin` plugin fields;
- the place in `AgentGraphBuilder._build_tools_protocol()` where plugins activate.

## Verification

- `python scripts/verify_runtime_docs.py`
- `python -m pytest tests/plugins tests/test_tool_registry_production.py --basetemp work/pytest-tmp`
- `python -m pytest tests/plugins/test_tool_plugin_integration.py --basetemp work/pytest-tmp`

## Last Verified Against

Last verified against commit `5f2202bd8cfcc2a4e4a66c025891550e52f3556e` on 2026-06-25.
