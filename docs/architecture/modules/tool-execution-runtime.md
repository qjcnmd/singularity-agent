# Tool Execution / ToolRuntime / ToolResult Runtime Flow

Runtime flow doc id: tool-execution-runtime
Source paths:
- src/singularity/tool_protocol/engine.py
- src/singularity/tool_protocol/result.py
- src/singularity/tool_protocol/models.py
- src/singularity/tools/executor.py
- src/singularity/tools/models.py
- src/singularity/planner/engine.py
- src/singularity/context/manager.py

Symbols:
- ToolProtocolEngine
- ToolProtocolEngine.process_model_turn
- ToolProtocolEngine.handle_model_turn_result
- ToolProtocolEngine.execute_plan
- ToolProtocolEngine.append_results_to_context
- ToolProtocolResultBuilder
- ToolProtocolResultBuilder.build
- ToolCallEnvelope
- ToolProtocolResultEnvelope
- ToolExecutor
- ToolExecutor.execute_tool_call
- ToolExecutor.execute_request
- ToolExecutionRequest
- ToolResult
- Planner
- Planner.update_from_tool_result
- ContextManager
- ContextManager.add_tool_protocol_result

## Module Boundary

This module owns model tool-call intake, protocol validation, scheduling, tool execution, result envelope construction, planner updates, and context binding.

It is responsible for converting provider tool calls into `ToolExecutionRequest`, calling `ToolExecutor`, converting `ToolResult` into `ToolProtocolResultEnvelope`, recording protocol state, updating planner evidence, and appending bounded tool messages back into context.

It is not responsible for exposing schemas to the model, building the model request, or rendering final prompt frames.

## Current Source Locations

- `src/singularity/tool_protocol/engine.py`: `ToolProtocolEngine` controls validation, schedule, execution, replay, and context append.
- `src/singularity/tool_protocol/result.py`: `ToolProtocolResultBuilder.build()` creates result envelopes.
- `src/singularity/tool_protocol/models.py`: `ToolCallEnvelope`, `ToolProtocolResultEnvelope`, `ToolObservationView`, `ToolProtocolTurnResult`.
- `src/singularity/tools/executor.py`: `ToolExecutor.execute_tool_call()` and `execute_request()`.
- `src/singularity/tools/models.py`: `ToolExecutionRequest`, `ToolResult`, `ToolError`, `ToolSpec`.
- `src/singularity/planner/engine.py`: `Planner.update_from_tool_result()`.
- `src/singularity/context/manager.py`: `ContextManager.add_tool_protocol_result()`.

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
13. `ToolExecutor._safe_update_planner()` or `_update_planner()` calls `Planner.update_from_tool_result()`.
14. `AgentLoop` reduces the `ToolProtocolTurnResult` and either continues, finalizes, blocks, or triggers failure analysis.

## Runtime Objects Passed

- `ToolCallEnvelope`: `tool_call_id`, `tool_name`, `raw_arguments`, `arguments`, `argument_digest`, `validation_errors`, model/run ids, and metadata.
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

Tests in `tests/test_context.py` confirm that policy decision ids, approval grant ids, raw arguments, internal debug metadata, and raw metadata do not enter the tool message content.

## Internal Trace Debug Audit Objects (内部 trace/debug/audit 对象)

Internal-only execution data includes:

- `ToolExecutionRequest.batch_id`, run/session/task/phase ids, model request/response ids, `argument_digest`, and metadata;
- `ToolResult.metadata` entries such as `duration_seconds`, `output_digest`, `backend`, `policy_decision_id`, `approval_grant_id`, and cache flags;
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
- Handler exceptions return `internal_error` unless cancellation is raised.
- Protocol validation errors create synthetic `ToolProtocolResultEnvelope` values.
- `ToolProtocolTurnResult` can be processed, pending approval, rejected, invalid assistant, or no tool calls.

## Current Structure Assessment

The current structure is strong: model protocol concerns live in `tool_protocol`, execution governance lives in `tools/executor.py`, planner evidence is updated through `Planner.update_from_tool_result()`, and context receives a bounded projection.

The main complexity is that `ToolExecutor.execute_request()` is large because it owns validation, policy, approval, replay, caching, dispatch, output limiting, trace, and planner update. That makes docs and tests important for preventing silent boundary drift.

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
- `ToolExecutor.execute_request()` admission, cache, replay, handler, output limit, or trace behavior;
- `ToolProtocolEngine` validation, scheduling, replay, result binding, or context append behavior;
- `Planner.update_from_tool_result()`;
- `ContextManager.add_tool_protocol_result()`.

## Verification

- `python scripts/verify_runtime_docs.py`
- `python -m pytest tests/test_tool_executor.py tests/test_tool_executor_policy_approval.py tests/test_tool_executor_redaction.py tests/test_tool_protocol_engine.py tests/test_tool_protocol_result.py tests/test_context.py --basetemp work/pytest-tmp`

## Last Verified Against

Last verified against commit `5f2202bd8cfcc2a4e4a66c025891550e52f3556e` on 2026-06-25.
