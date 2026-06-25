# Tool Registry / ToolSpec / Tool Exposure Runtime Flow

Runtime flow doc id: tool-registry-exposure
Source paths:
- src/singularity/kernel/graph.py
- src/singularity/tools/registry.py
- src/singularity/tools/models.py
- src/singularity/tools/router.py
- src/singularity/planner/engine.py
- src/singularity/model/tools.py

Symbols:
- AgentGraphBuilder
- AgentGraphBuilder._build_tools_protocol
- ToolRegistry
- ToolRegistry.register
- ToolRegistry.list_model_visible
- ToolRegistry.to_openai_tools
- ToolRegistry._validate_spec
- ToolSpec
- RegisteredToolRecord
- ToolRouter
- ToolRouter.decide
- ToolExposureDecision
- Planner
- Planner.decide_tool_exposure
- Planner.filtered_tools
- ModelToolRenderer
- ModelToolRenderer.render
- ModelToolRenderer.to_provider_tools

## Module Boundary

This module owns the registry-side definition of tools and the first model-facing exposure boundary.

It is responsible for accepting `ToolSpec` instances, retaining `RegisteredToolRecord` governance metadata, filtering enabled and admitted tools through `ToolRegistry.list_model_visible()`, and rendering model-facing schemas through `ToolRegistry.to_openai_tools()` or `ModelToolRenderer.render()`.

It is not responsible for executing a tool, approving a tool call, validating provider tool-call responses, or storing tool results. Those are owned by `ToolExecutor`, policy/approval, and `ToolProtocolEngine`.

## Current Source Locations

- `src/singularity/kernel/graph.py`: `AgentGraphBuilder._build_tools_protocol()` constructs `ToolRegistry`, registers built-in tool groups, activates plugins, then builds `ToolExecutor` and `ToolProtocolEngine`.
- `src/singularity/tools/registry.py`: `ToolRegistry.register()`, `list_model_visible()`, `to_openai_tools()`, and `_validate_spec()`.
- `src/singularity/tools/models.py`: `ToolSpec`, `RegisteredToolRecord`, `ToolOrigin`, `ToolExecutionRequest`, and `ToolResult`.
- `src/singularity/tools/router.py`: `ToolRouter.decide()` and `ToolExposureDecision`.
- `src/singularity/planner/engine.py`: `Planner.decide_tool_exposure()` and `filtered_tools()` apply task-phase exposure decisions.
- `src/singularity/model/tools.py`: `ModelToolRenderer.render()` and `to_provider_tools()`.

## Runtime Call Chain

1. `AgentGraphBuilder.build()` calls `_build_tools_protocol()`.
2. `_build_tools_protocol()` creates `ToolRegistry(project_root)`.
3. Built-in registration functions call `ToolRegistry.register(spec)`.
4. `ToolRegistry._validate_spec()` rejects enabled write/shell/delegated specs that do not declare the required execution backend or delegation flags.
5. Plugin activation may call `ToolRegistry.register()` with `ToolOrigin(kind=ToolOriginKind.PLUGIN, ...)`.
6. During `AgentLoop.run()`, the loop calls `self.tools.openai_tools(strict=self.strict)`.
7. `Planner.filtered_tools()` and `Planner.decide_tool_exposure()` use `ToolRouter.decide()` to narrow active tool exposure for the current task phase.
8. `ModelRunner.build_request_from_context()` delegates to `ModelTurnRequestBuilder.build_request()`.
9. `ModelToolRenderer.render()` calls `registry.list_model_visible()`, converts each visible `ToolSpec` into `ModelToolSchema`, and stores internal metadata on that schema.
10. `ModelToolRenderer.to_provider_tools()` converts `ModelToolSchema` into provider function schemas.

## Runtime Objects Passed

- `ToolSpec`: `name`, `version`, `description`, `input_model`, `output_model`, `handler`, `permission_level`, `risk_tags`, `timeout_seconds`, `max_output_chars`, `cacheable`, `idempotent`, `uses_edit_executor`, `uses_mutation_manager`, `uses_command_executor`, `delegates_policy_constraints`, `capabilities`, `operation`, `resource_resolver`, `side_effects`, `sensitivity`, `cache_policy`, `idempotency_policy`, `retry_policy`, `execution_backend`, `approval_profile`, `artifact_policy`, `streamable`, `enabled`.
- `RegisteredToolRecord`: `spec`, `origin`, `admitted`, `admission_reason`, `diagnostics`, `metadata`.
- `ToolOrigin`: `kind`, `plugin_id`, `local_tool_name`, `exposed_name`, `manifest_hash`, `source_path`, `required_permissions`, `approved_permissions`, `activation_hash`, `schema_digest`.
- `ModelToolSchema`: `name`, `description`, `parameters_schema`, `capability_tags`, `risk_tags`, `metadata`.

## Model-Visible Objects (模型实际可见对象)

The model-visible provider tool schema is restricted to:

- `type: "function"`;
- `function.name`;
- `function.description`;
- `function.parameters`;
- optional `function.strict`.

`ToolRegistry.to_openai_tools()` and `ModelToolRenderer.to_provider_tools()` both emit only that function schema. The model does not receive `ToolSpec.handler`, `permission_level`, `risk_tags`, `execution_backend`, `approval_profile`, `artifact_policy`, `ToolOrigin`, or `RegisteredToolRecord`.

`ModelToolRenderer.render()` creates `ModelToolSchema.metadata` with `version`, `permission_level`, `cacheable`, `idempotent`, `strict`, `origin`, and optional `plugin_id`, but `to_provider_tools()` does not copy that metadata into the provider tool schema.

## Internal Trace Debug Audit Objects (内部 trace/debug/audit 对象)

Internal-only governance data includes:

- `RegisteredToolRecord.origin`, `admitted`, `admission_reason`, `diagnostics`, and `metadata`;
- `ToolSpec.permission_level`, `risk_tags`, `capabilities`, `operation`, `side_effects`, `sensitivity`, `execution_backend`, `approval_profile`, and `artifact_policy`;
- `ToolExposureDecision` reasons and planner phase gating;
- `ToolRegistry.list_policy_shapes()` output for internal policy shape inspection;
- `ModelToolSchema.metadata`, which is used for request metadata and schema hashing but not provider schema emission.

## State Transitions And Failure Paths

- Registering after `ToolRegistry.freeze()` raises `RuntimeError`.
- Duplicate tool names raise `ValueError`.
- Enabled write tools without mutation manager delegation are rejected by `_validate_spec()`.
- Enabled shell tools without command executor or delegated backend are rejected.
- Verification tools that set `delegates_policy_constraints=True` must use `DELEGATED_VERIFICATION_RUNNER`.
- Disabled specs are stored but excluded from `list_model_visible()`.
- Non-admitted records are excluded from `list_model_visible()`.

## Current Structure Assessment

The current structure is reasonable because `ToolSpec` remains the rich internal contract while provider exposure is a narrow projection. `ToolRegistry.list_model_visible()` is the right place to separate enabled/admitted tools from all registered records.

The main weakness is that `ToolRegistry.to_openai_tools()` and `ModelToolRenderer.to_provider_tools()` are two provider-schema paths with similar output shapes. They currently agree on the key boundary, but future changes must update both or consolidate the conversion.

## Production-Grade Target Structure

Current code has no dedicated single `ToolExposurePolicy` object. A production-grade target could introduce one proposed object that owns:

- proposed `exposure_reason`;
- proposed `schema_visibility_level`;
- proposed `model_schema_version`;
- proposed invariant checks that metadata fields cannot leak into provider schemas.

This is not current implementation. Today the split is `ToolRegistry.list_model_visible()` plus `ModelToolRenderer`.

## Harness Usage Example

In a typical coding-agent turn, `AgentGraphBuilder._build_tools_protocol()` registers `read_file`, `search_text`, edit, command, workspace-state, code-index, and verification tools. `AgentLoop.run()` asks planner for the active phase, filters tool names, and `ModelToolRenderer.to_provider_tools()` exposes only the allowed function names and JSON schemas. The model can call `read_file` by name, but it cannot see the Python handler, the mutation backend, policy tags, or plugin origin metadata.

## Maintenance Rules

Update this document when changing:

- `ToolSpec` fields that affect exposure, policy, execution, or result handling;
- `RegisteredToolRecord` or `ToolOrigin`;
- `ToolRegistry.register()`, `_validate_spec()`, `list_model_visible()`, or `to_openai_tools()`;
- `ToolRouter.decide()`, `Planner.decide_tool_exposure()`, or `Planner.filtered_tools()`;
- built-in tool registration order in `AgentGraphBuilder._build_tools_protocol()`;
- `ModelToolRenderer.render()` or `to_provider_tools()`.

## Verification

- `python scripts/verify_runtime_docs.py`
- `python -m pytest tests/test_tool_registry_production.py tests/test_model_tools.py tests/test_tool_contract.py tests/test_tool_router.py --basetemp work/pytest-tmp`
- `python -m pytest tests/test_agent_graph.py --basetemp work/pytest-tmp`

## Last Verified Against

Last verified against commit `5f2202bd8cfcc2a4e4a66c025891550e52f3556e` on 2026-06-25.
