# Tool Execution / ToolRuntime / ToolResult Runtime Flow

Runtime flow doc id: tool-execution-runtime
Source paths:
- src/singularity/tool_protocol/engine.py
- src/singularity/tool_protocol/result.py
- src/singularity/tool_protocol/models.py
- src/singularity/tool_protocol/state.py
- src/singularity/tools/executor.py
- src/singularity/tools/models.py
- src/singularity/planner/engine.py
- src/singularity/context/manager.py
- src/singularity/sandbox/manager.py
- src/singularity/sandbox/artifacts.py
- src/singularity/sandbox/exceptions.py
- src/singularity/sandbox/filesystem.py
- src/singularity/sandbox/models.py

Symbols:
- ToolProtocolEngine
- ToolProtocolEngine.process_model_turn
- ToolProtocolEngine.handle_model_turn_result
- ToolProtocolEngine.execute_plan
- ToolProtocolEngine.append_results_to_context
- ToolProtocolResultBuilder
- ToolProtocolResultBuilder.build
- ToolProtocolStateStore
- ToolProtocolStateStore.save_batch
- ToolProtocolStateStore.upsert_record
- ToolProtocolStateStore.bind_result
- ToolProtocolStateStore.transition
- ToolProtocolStateStore.mark_result_appended
- ToolCallEnvelope
- ToolProtocolResultEnvelope
- ToolObservationView
- ToolExecutor
- ToolExecutor.execute_tool_call
- ToolExecutor.execute_request
- ToolExecutor._safe_update_planner
- ToolExecutionRequest
- ToolResult
- Planner
- Planner.update_from_tool_result
- ContextManager
- ContextManager.add_tool_protocol_result
- SandboxManager
- SandboxManager.ensure_capabilities
- SandboxArtifactCollector
- SandboxArtifactCollector.collect
- SandboxCapabilityError
- SandboxFilesystemManager
- SandboxFilesystemManager._make_readonly
- SandboxCapabilities

Field checks:
- ToolCallEnvelope: protocol_version, run_id, session_id, task_id, phase_id, model_request_id, model_response_id, assistant_message_id, tool_call_id, tool_name, raw_arguments, parsed_arguments, normalized_arguments, argument_digest, tool_schema_hash, allowed_tool_names, proposed_at, proposed_by_model, parse_status, validation_errors, metadata, phase
- ToolExecutionRequest: tool_call_id, tool_name, raw_arguments, normalized_arguments, batch_id, run_id, session_id, task_id, phase_id, model_request_id, model_response_id, argument_digest, metadata
- ToolResult: ok, content, error_code, error, truncated, metadata
- ToolProtocolResultEnvelope: tool_call_id, tool_name, ok, status, error_code, error_kind, content_preview, content_digest, raw_result_ref, artifact_refs, observation_id, policy_decision_id, approval_grant_id, truncated, redacted, metadata
- ToolObservationView: tool_call_id, tool_name, ok, status, visibility, content_preview, content_digest, result_ref, error_code, error_kind, reference_ids, observation_id, truncated, redacted
- ToolProtocolTurnResult: status, batch_id, executed_count, failed_count, rejected_count, pending_approval_count, appended_tool_message_count, next_action, recovery_report, metadata
- SandboxCapabilities: filesystem_isolation, copy_on_write, readonly_mount, network_isolation, env_isolation, process_tree_kill, timeout, output_limit, memory_limit, process_limit, artifact_capture, change_detection

## Module Boundary

This module owns model tool-call intake, protocol validation, scheduling, tool execution, result envelope construction, planner updates, and context binding.

It is responsible for converting provider tool calls into `ToolExecutionRequest`, calling `ToolExecutor`, converting `ToolResult` into `ToolProtocolResultEnvelope`, recording protocol state, updating planner evidence, and appending bounded tool messages back into context.

It is not responsible for exposing schemas to the model, building the model request, or rendering final prompt frames.

## Current Source Locations

- `src/singularity/tool_protocol/engine.py`: `ToolProtocolEngine` controls validation, schedule, execution, replay, and context append.
- `src/singularity/tool_protocol/result.py`: `ToolProtocolResultBuilder.build()` creates result envelopes.
- `src/singularity/tool_protocol/models.py`: `ToolCallEnvelope`, `ToolProtocolResultEnvelope`, `ToolObservationView`, `ToolProtocolTurnResult`.
- `src/singularity/tool_protocol/state.py`: `ToolProtocolStateStore` persists batches, call records, phase transitions, result binding, and appended context-message ids.
- `ToolProtocolEngine` creates its default `ToolProtocolStateStore` under the active `TraceStore.run_dir` when a structured trace store is available. For legacy trace recorders with only `trace.path` and `trace.run_id`, it derives a per-run directory next to the trace path and stores `tool_protocol.sqlite3` there. Without trace data, it falls back to `<project_root>/.singularity/runs/default/tool_protocol.sqlite3`.
- `src/singularity/tools/executor.py`: `ToolExecutor.execute_tool_call()`, `execute_request()`, and planner evidence update fail-closed handling.
- `src/singularity/tools/models.py`: `ToolExecutionRequest`, `ToolResult`, `ToolError`, `ToolSpec`.
- `src/singularity/planner/engine.py`: `Planner.update_from_tool_result()`.
- `src/singularity/context/manager.py`: `ContextManager.add_tool_protocol_result()`.
- `src/singularity/sandbox/manager.py`: `SandboxManager.ensure_capabilities()` enforces capability fail-closed checks before sandboxed execution.
- `src/singularity/sandbox/artifacts.py`: `SandboxArtifactCollector.collect()` captures stdout/stderr logs and workspace file artifacts with redaction.
- `src/singularity/sandbox/exceptions.py`: `SandboxCapabilityError` raised when a backend cannot enforce a required isolation capability.
- `src/singularity/sandbox/filesystem.py`: `SandboxFilesystemManager` prepares sandbox workspaces and fails closed when read-only chmod enforcement fails.
- `src/singularity/sandbox/models.py`: `SandboxCapabilities` (with `readonly_mount`, `network_isolation`), `SandboxNetworkMode.DENIED`, `SandboxFilesystemMode.READ_ONLY_WORKSPACE`.

## Runtime Call Chain

1. `AgentLoop.run()` receives a successful `ModelTurnResult` with `tool_calls`.
2. `ToolProtocolEngine.process_model_turn()` calls `handle_model_turn_result()`.
3. `handle_model_turn_result()` converts the assistant message and calls `validate_batch()`.
4. `ToolProtocolValidator.validate_assistant_message()` returns `ToolCallBatch` and validation information.
5. `ToolProtocolEngine.build_execution_plan()` calls `ToolProtocolScheduler.schedule()`.
6. `ToolProtocolEngine.execute_plan()` iterates scheduled `ToolCallEnvelope` values.
7. For each valid call, `ToolExecutionRequest.from_envelope()` carries trace ids, batch id, model request id, response id, and arguments to `ToolExecutor`.
8. `ToolExecutor.execute_request()` resolves `ToolSpec`, validates arguments, checks replay, boundaries, dry-run, delegated preflight, policy, approval, planner authorization, cache, backend, and handler execution.
9. `ToolExecutor` returns `ToolResult`.
10. `ToolProtocolResultBuilder.build()` creates `ToolProtocolResultEnvelope`.
11. `ToolProtocolStateStore` binds the result and transitions call phase.
12. `ToolProtocolEngine.append_results_to_context()` calls `ContextManager.add_tool_protocol_result()`.
13. If `AgentGraphBuilder._wire_planner()` has bound `planner` onto the shared `ToolExecutor`, `ToolExecutor._safe_update_planner()` or `_update_planner()` calls `Planner.update_from_tool_result()`. Evidence update failures emit an error trace and fail closed instead of silently treating the tool dispatch as successful.
14. `AgentLoop` reduces the `ToolProtocolTurnResult` and either continues, finalizes, blocks, or triggers failure analysis.

## Runtime Objects Passed

- `ToolCallEnvelope`: `tool_call_id`, `tool_name`, `raw_arguments`, `parsed_arguments`, `normalized_arguments`, `argument_digest`, `validation_errors`, model/run ids, and metadata.
- `ToolExecutionRequest`: `tool_call_id`, `tool_name`, `raw_arguments`, `normalized_arguments`, `batch_id`, `run_id`, `session_id`, `task_id`, `phase_id`, `model_request_id`, `model_response_id`, `argument_digest`, `metadata`.
- `ToolResult`: `ok`, `content`, `error_code`, `error`, `truncated`, `metadata`.
- `ToolProtocolResultEnvelope`: `tool_call_id`, `tool_name`, `ok`, `status`, `content_preview`, `content_digest`, `raw_result_ref`, `artifact_refs`, `observation_id`, `truncated`, `redacted`, `error_code`, `error_kind`, `policy_decision_id`, `approval_grant_id`, `metadata`.
- `ToolProtocolTurnResult`: status, batch id, executed/failed/rejected/pending counts, appended tool message count, next action, metadata.

## Model-Visible Objects (模型实际可见对象)

The model sees tool execution results only after `ContextManager.add_tool_protocol_result()` turns the result envelope into a tool message:

- message `role: "tool"`;
- `tool_call_id`;
- `name`;
- JSON `content` from `ToolObservationView.to_model_payload()`.

The model payload includes bounded/redacted result fields such as `tool_call_id`, `tool_name`, `ok`, `status`, `content`, `content_preview`, `content_digest`, `result_ref`, `reference_ids`, `truncated`, and `redacted`.

`ToolProtocolResultEnvelope.raw_result_ref` is projected as model-visible `result_ref`. `ToolProtocolResultEnvelope.artifact_refs` is projected as model-visible `reference_ids`; the model payload does not include an `artifact_refs` key.

Tests in `tests/test_context.py` and `tests/test_tool_protocol_models.py` confirm that policy decision ids, approval grant ids, raw arguments, internal debug metadata, raw metadata, and the envelope-level `artifact_refs` key do not enter the tool message content.

### Protocol Result Unified Redaction

`ContextManager.add_tool_protocol_result()` redacts the model-visible payload unconditionally across all sensitivity levels, matching `add_tool_result()`. Regardless of the `SensitivityClassifier.classify()` result (SECRET/SENSITIVE/WORKSPACE/PUBLIC), `ContextRedactor.redact_text()` is applied to `content_preview` to produce `rendered_preview`, which is then written back into both `model_payload["content"]` and `model_payload["content_preview"]`. When `error_code` is a string it is also passed through `redact_text()`. The payload always carries `redacted: True`. This closes the trust-boundary gap where protocol results previously escaped redaction on lower-sensitivity classifications.

## Internal Trace Debug Audit Objects (内部 trace/debug/audit 对象)

Internal-only execution data includes:

- `ToolExecutionRequest.batch_id`, run/session/task/phase ids, model request/response ids, `argument_digest`, and metadata;
- `ToolResult.metadata` entries such as `duration_seconds`, `backend`, `policy_decision_id`, `approval_grant_id`, and cache flags;
- `ToolResult.metadata.output_digest` as execution metadata, although the same value can become model-visible as `result_ref` when `ToolProtocolEngine` passes it through `ToolProtocolResultEnvelope.raw_result_ref`;
- `ToolProtocolResultEnvelope.policy_decision_id`, `approval_grant_id`, and `metadata`;
- `ToolProtocolStateStore` batch, record, phase, replay, and result binding rows;
- trace payloads emitted by `ToolExecutor` and `ToolProtocolEngine`;
- planner evidence entries created by `Planner.update_from_tool_result()`.

## State Transitions And Failure Paths

- Unknown tools return `ToolResult.failure(code="tool_not_found")`.
- Invalid JSON returns `bad_arguments_json`.
- Pydantic validation errors return `validation_error`.
- Reusing a tool call id with different arguments returns `conflicting_replay`.
- Non-replayable duplicates return `replay_not_allowed`.
- Invalid write/shell/delegated backend boundaries return `invalid_operation`.
- Dry-run can return `dry_run_blocked`.
- Policy denial, approval requirement, sandbox requirement, and planner denial return failure `ToolResult` values.
- Planner evidence update failure in `ToolExecutor._safe_update_planner()` emits `TOOL_DISPATCH_FAILED` with error severity and raises `RuntimeError("planner observation update failed")`; the runtime does not treat the tool call as successfully recorded.
- Handler exceptions return `internal_error` unless cancellation is raised.
- Protocol validation errors create synthetic `ToolProtocolResultEnvelope` values.
- `ToolProtocolTurnResult` can be processed, pending approval, rejected, invalid assistant, or no tool calls.

## Sandbox Fail-Closed Semantics

Sandboxed execution is governed by `SandboxManager.ensure_capabilities()`, which refuses to run rather than silently executing with unenforced isolation. The checks raise `SandboxCapabilityError` (caught by `SandboxManager.execute()`, which returns a `BACKEND_UNAVAILABLE` result and emits a `SANDBOX_CAPABILITY_FAILED` trace event) instead of appending a `SandboxViolation`:

- Network DENIED fail-closed: when `request.profile.network.mode == SandboxNetworkMode.DENIED` and the selected backend reports `capabilities.network_isolation` is False, `ensure_capabilities()` raises `SandboxCapabilityError("Backend ... cannot enforce denied network mode; network access is not isolated.")`. The previous behavior of appending a violation and continuing is no longer used; a denied network profile that cannot be enforced fails closed.
- READ_ONLY_WORKSPACE capability check: when `request.profile.filesystem.mode == SandboxFilesystemMode.READ_ONLY_WORKSPACE` and `capabilities.readonly_mount` is False, `ensure_capabilities()` raises `SandboxCapabilityError("Backend cannot enforce read-only workspace; write operations would not be blocked.")`.
- READ_ONLY_WORKSPACE chmod enforcement: `SandboxFilesystemManager._make_readonly()` raises `OSError("readonly sandbox capability failed...")` if any filesystem chmod operation needed to enforce read-only mode fails. The sandbox must fail closed rather than silently preparing a writable workspace.
- The same fail-closed raise applies to required hard network isolation (`require_hard_isolation` without `network_isolation`), memory limits (`max_memory_mb` without `memory_limit`), and process limits (`max_processes` without `process_limit`).

### Sandbox Artifact Redaction

`SandboxArtifactCollector.collect()` (constructed by sandbox backends with a `ContextRedactor`) redacts captured artifacts before persistence:

- Stdout/stderr are written as `stdout.log` / `stderr.log` text artifacts with `redacted=True`.
- Workspace file artifacts collected via `_write_file_artifact()` are read as bytes; when the bytes decode as UTF-8, the text is passed through `ContextRedactor.redact_text()` before being written and `redacted=True` is set on the `SandboxArtifact`. Non-UTF-8 (binary) files are stored as-is with `redacted=False`. Artifact collection is bounded by `SandboxResourceLimits.max_artifact_bytes` and constrained to paths inside `workspace_root`.

## Current Structure Assessment

The current structure is strong: model protocol concerns live in `tool_protocol`, execution governance lives in `tools/executor.py`, planner evidence is updated through `Planner.update_from_tool_result()`, and context receives a bounded projection.

The main complexity is that `ToolExecutor.execute_request()` is large because it owns validation, policy, approval, replay, caching, dispatch, output limiting, trace, and planner update. That makes docs and tests important for preventing silent boundary drift.

## Cache And Concurrency Behavior

`ToolExecutor` owns two in-memory stores that are touched on every tool call:

- `ToolResultCache` (OrderedDict-backed LRU keyed by `_cache_key()` output).
- `IdempotencyLedger` (dict keyed by `tool_call_id` for replay detection).

### Cache Key Construction

`_cache_key()` builds the cache key from:

- `tool_name`, `version`, model schema fingerprint (`_model_schema_fingerprint`);
- validated `arguments`;
- `workspace_root` string;
- `paths` snapshot from `_file_snapshots()`.

`_file_snapshots()` no longer scans the workspace tree. For each path returned by `_touched_paths()` (derived from `spec.resource_resolver`), it records lightweight `stat()` data:

- `size` (`st_size`);
- `mtime_ns` (`st_mtime_ns`);
- `is_dir`;
- `{"exists": False}` when `stat()` raises `OSError`.

This replaces the previous `rglob("*")` + `read_bytes()` + sha256 approach, which scanned the entire workspace on every cache key computation.

### Incremental Invalidation After Write Tools

When `spec.permission_level != READ_ONLY`, `ToolExecutor` calls `_invalidate_after_write(spec, validated_args, result)` instead of `self._cache.clear()`. The method collects affected paths from:

1. `result.content["changed_files"]` and `result.content["affected_files"]` (populated by `WorkspaceMutationManager` observation when the write tool records its mutation).
2. `spec.resource_resolver` output via `_touched_paths()`.

If at least one affected path is determined, `self._cache.invalidate_paths(affected)` evicts only cache entries whose `touched_paths` overlap the mutated paths (`_paths_overlap`). When no affected paths can be determined (for example free-form shell tools without a resource resolver), the method falls back to `self._cache.clear()` so stale read-only results are never served.

### Thread Safety

`ToolResultCache` and `IdempotencyLedger` each hold a `threading.RLock`. All access points are guarded:

- `ToolResultCache.get` (including TTL expiry and `move_to_end`);
- `ToolResultCache.set` (including LRU `popitem(last=False)`);
- `ToolResultCache.invalidate_paths` (iterates a snapshot via `list(self._entries.items())`);
- `ToolResultCache.clear` and `__len__`;
- `IdempotencyLedger.check` and `IdempotencyLedger.remember`.

`RLock` is used so compound operations (get+move_to_end, check+remember) stay safe under concurrent tool execution within the same thread.

## Production-Grade Target Structure

Current code has no separate object named `ToolRuntime`; the runtime object is `ToolExecutor`.

A production-grade target could split proposed components:

- proposed `ToolAdmissionRuntime` for policy/approval/planner admission;
- proposed `ToolDispatchRuntime` for backend and handler execution;
- proposed `ToolResultProjection` for model-visible result envelopes;
- proposed `ToolExecutionTrace` for trace-only metadata.

These are proposed names, not current code.

## Harness Usage Example

The model calls `read_file` with a JSON argument. `ToolProtocolEngine` validates the call, `ToolExecutor` validates the argument against the tool input model, policy allows read access, the handler returns content, `ToolResult` carries content plus `output_digest`, and `ToolProtocolResultBuilder` creates a bounded envelope. The next model turn sees only the tool message payload, not the Python handler, raw policy request, or full trace payload.

## Maintenance Rules

Update this document when changing:

- `ToolExecutionRequest`, `ToolResult`, `ToolProtocolResultEnvelope`, or `ToolObservationView`;
- `ToolExecutor.execute_request()` admission, cache, replay, handler, output limit, trace behavior, or planner update failure handling;
- `ToolProtocolEngine` validation, scheduling, replay, result binding, or context append behavior;
- `Planner.update_from_tool_result()`;
- `ContextManager.add_tool_protocol_result()` redaction behavior or sensitivity handling;
- `SandboxManager.ensure_capabilities()` fail-closed checks, `SandboxFilesystemManager._make_readonly()`, `SandboxCapabilities` fields, or `SandboxNetworkMode`/`SandboxFilesystemMode` enforcement;
- `SandboxArtifactCollector` artifact capture or redaction behavior.

## Verification

- `python scripts/verify_runtime_docs.py`
- `python -m pytest tests/test_tool_executor.py tests/test_tool_executor_policy_approval.py tests/test_tool_executor_redaction.py tests/test_tool_protocol_engine.py tests/test_tool_protocol_result.py tests/test_context.py --basetemp work/pytest-tmp`

## Last Verified Against

Last verified against commit `bd75275daccd357b25b5741734ac2740b3a2690f` on 2026-06-25 (Trust Boundary Contract: `ContextManager.add_tool_protocol_result()` now redacts unconditionally across all sensitivity levels matching `add_tool_result()`; sandbox fail-closed semantics in `SandboxManager.ensure_capabilities()` raise `SandboxCapabilityError` for unenforced `SandboxNetworkMode.DENIED` and `SandboxFilesystemMode.READ_ONLY_WORKSPACE` (`readonly_mount`) instead of appending violations; `SandboxArtifactCollector` redacts UTF-8 file artifacts via `ContextRedactor.redact_text()`).
