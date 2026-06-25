# ModelTurnRequest / ModelToolSchema / Provider Tools Runtime Flow

Runtime flow doc id: model-turn-provider-tools
Source paths:
- src/singularity/agent_loop.py
- src/singularity/model/models.py
- src/singularity/model/runner.py
- src/singularity/model/request_builder.py
- src/singularity/model/tools.py
- src/singularity/model/messages.py
- src/singularity/model/providers.py
- src/singularity/instructions/prompt_assembly.py

Symbols:
- AgentLoop
- AgentLoop.run
- ModelTurnRequest
- ModelToolSchema
- ModelMessage
- ModelTurnResult
- ProviderRequest
- ModelRunner
- ModelRunner.build_request_from_context
- ModelRunner.run_turn
- ModelTurnRequestBuilder
- ModelTurnRequestBuilder.build_request
- ModelToolRenderer
- ModelToolRenderer.render
- ModelToolRenderer.to_provider_tools
- MessageConverter
- MessageConverter.to_provider_messages
- PromptAssemblyPipeline
- PromptAssemblyPipeline.build_for_model_turn

Field checks:
- ModelTurnRequest: request_id, run_id, session_id, task_id, phase_id, action_id, purpose, messages, tools, tool_choice, model_preferences, budget, context_metadata, policy_metadata, trace_metadata
- ModelMessage: role, content, name, tool_call_id, metadata
- ContentBlock: type, text, artifact_ref, metadata
- ModelToolSchema: name, description, parameters_schema, capability_tags, risk_tags, metadata
- ToolChoicePolicy: mode, tool_name, allowed_tool_names, max_tool_calls
- ModelTurnResult: request_id, response_id, status, assistant_message, tool_calls, usage, finish_reason, validation, error, provider_name, model_name, latency_ms, trace_event_ids, raw_response_ref, metadata
- ProviderRequest: request_id, purpose, messages, tools, tool_choice, preferences, policy_metadata, trace_metadata

## Module Boundary

This module owns the final model request object and provider conversion boundary.

It is responsible for assembling `ModelTurnRequest`, choosing messages and tools for the current turn, carrying internal context/policy/trace metadata, invoking `ModelRunner.run_turn()`, converting messages/tools into provider payloads, and normalizing provider responses into `ModelTurnResult`.

It is not responsible for executing tools returned by the model or deciding whether a tool should be allowed to run.

## Current Source Locations

- `src/singularity/agent_loop.py`: `AgentLoop.run()` calls `model_runner.build_request_from_context()` and `model_runner.run_turn()`.
- `src/singularity/model/models.py`: `ModelTurnRequest`, `ModelToolSchema`, `ModelMessage`, `ModelTurnResult`.
- `src/singularity/model/request_builder.py`: `ModelTurnRequestBuilder.build_request()`.
- `src/singularity/model/tools.py`: `ModelToolRenderer`.
- `src/singularity/model/messages.py`: `MessageConverter`.
- `src/singularity/model/runner.py`: request creation, capability adjustment, `ProviderRequest` creation, send, validation, trace.
- `src/singularity/model/providers.py`: `ProviderRequest`, provider request conversion, and OpenAI-compatible payload conversion.
- `src/singularity/instructions/prompt_assembly.py`: `PromptAssemblyPipeline.build_for_model_turn()`.

## Runtime Call Chain

1. `AgentLoop.run()` starts or resumes planner state.
2. Per turn, `planner.step()` selects the current action and allowed tools.
3. `planner.filtered_tools()` narrows available tool names.
4. `ModelRunner.build_request_from_context()` calls `ModelTurnRequestBuilder.build_request()`.
5. `ModelTurnRequestBuilder.build_request()` calls `ModelToolRenderer.render()`.
6. It calls `ModelToolRenderer.to_provider_tools()` to give the context assembler a tool-token budget shape.
7. If prompt assembly is configured, `PromptAssemblyPipeline.build_for_model_turn()` creates the stable prompt prefix.
8. `ContextManager.messages(tools=provider_tools, planner_context=..., persist=True)` provides dynamic tail messages.
9. `ModelTurnRequestBuilder` returns `ModelTurnRequest`.
10. `ModelRunner.run_turn()` validates and adjusts the request, then `_send_with_retry()` wraps it in `ProviderRequest`.
11. Provider code converts `ModelMessage` and `ModelToolSchema` into provider payload messages and tools.
12. Provider response is normalized into `ModelTurnResult`, including assistant message, parsed tool calls, usage, status, and provider metadata.

## Runtime Objects Passed

- `ModelTurnRequest`: `request_id`, `run_id`, `session_id`, `task_id`, `phase_id`, `action_id`, `purpose`, `messages`, `tools`, `tool_choice`, `model_preferences`, `budget`, `context_metadata`, `policy_metadata`, `trace_metadata`.
- `ModelMessage`: `role`, `content`, optional `name`, optional `tool_call_id`, `metadata`.
- `ContentBlock`: `type`, `text`, `artifact_ref`, `metadata`.
- `ModelToolSchema`: `name`, `description`, `parameters_schema`, `capability_tags`, `risk_tags`, `metadata`.
- `ToolChoicePolicy`: `mode`, `tool_name`, `allowed_tool_names`, `max_tool_calls`.
- `ProviderRequest`: `request_id`, `purpose`, `messages`, `tools`, `tool_choice`, `preferences`, `policy_metadata`, `trace_metadata`.
- `ModelTurnResult`: `request_id`, `response_id`, `status`, `assistant_message`, `tool_calls`, `usage`, `finish_reason`, `validation`, `error`, `provider_name`, `model_name`, `latency_ms`, `trace_event_ids`, `raw_response_ref`, `metadata`.

## Model-Visible Objects (模型实际可见对象)

The provider-visible request includes:

- messages converted from `ModelTurnRequest.messages`: role, content, optional name, optional `tool_call_id`, and provider-compatible assistant tool calls when present;
- tools converted from `ModelTurnRequest.tools`: function name, description, parameters, optional strict flag;
- tool choice converted from `ToolChoicePolicy`;
- provider request preferences such as model, temperature, top_p, max output tokens, json mode, and stream.

The model sees prompt assembly output, rendered context messages, planner context that was intentionally rendered into messages, and provider tool schemas.

For the OpenAI-compatible HTTP payload, preferences are projected as `model`, optional `temperature`, optional `top_p`, optional `max_tokens`, and optional JSON `response_format`. `stream` selects the provider call path; it is not message content or provider function schema.

`MessageConverter.to_provider_messages()` may produce an intermediate `metadata` key for local conversion state such as developer-message fallback. `_model_messages_to_openai()` removes that key before the OpenAI-compatible or legacy chat provider payload is sent, except that assistant `metadata["tool_calls"]` is safely converted into provider-compatible `tool_calls`.

## Internal Trace Debug Audit Objects (内部 trace/debug/audit 对象)

Internal-only request data includes:

- `ModelTurnRequest.context_metadata`: context budget, prompt ids/hashes, stable/dynamic hashes, tool schema hash, bundle id, bundle digest, compression snapshot id, context shape hash, ordering hash, and bundle metadata;
- `ModelTurnRequest.policy_metadata`;
- `ModelTurnRequest.trace_metadata`;
- `ModelMessage.metadata`, except the special assistant `tool_calls` projection described above;
- `ModelToolSchema.metadata`, except that `metadata["strict"]` can affect whether the provider function schema includes `strict: true`;
- request/response trace events and raw response references;
- model validation and budget diagnostics.

These fields may be carried to provider-adapter code as request metadata for observability. In the OpenAI-compatible provider, `ProviderRequest.policy_metadata` and `ProviderRequest.trace_metadata` are not included in the HTTP JSON payload, and the metadata dicts themselves are not provider message content or provider function schema.

## State Transitions And Failure Paths

- If provider capability does not support tools, `ModelRunner` can adjust or reject the request according to capability handling.
- Invalid messages or tool calls produce `ModelTurnStatus.INVALID_RESPONSE` or validation errors.
- Provider auth, network, timeout, or response-shape failures become `ModelError` on `ModelTurnResult`.
- `AgentLoop._outcome_from_model_failure()` maps non-success model turns into retryable, blocked, or fatal `ExecutionOutcome`.
- Failure analysis uses a separate `ModelPurpose.FAILURE_ANALYSIS` request without tools.

## Current Structure Assessment

The current structure is coherent because `ModelTurnRequest` is the stable model-boundary object and `ModelTurnRequestBuilder` owns the context-to-request projection. `ModelToolRenderer` keeps provider schemas narrow.

The main drift risk is that metadata fields on `ModelTurnRequest` and `ModelToolSchema` can be confused with model-visible content. Provider conversion must continue to exclude governance metadata from messages and tools unless it is intentionally rendered into prompt text.

## Production-Grade Target Structure

Current code has `ModelTurnRequestBuilder`, not a separate audited `ModelRequestProjection` object.

A production-grade target could add a proposed projection report with:

- proposed `model_visible_messages_hash`;
- proposed `model_visible_tools_hash`;
- proposed `excluded_internal_metadata_keys`;
- proposed `provider_payload_schema_version`;
- proposed `leak_check_status`.

Those fields are proposed. Current code uses hashes in `context_metadata` and trace metadata, not a dedicated leak report.

## Harness Usage Example

On a normal inspect-and-edit turn, planner allows read tools. `ModelTurnRequestBuilder` emits a stable system/developer prompt from `PromptAssemblyPipeline`, dynamic context messages from `ContextManager`, and tool schemas for the allowed read tools. The provider receives function schemas for those tools. If the model returns a `read_file` tool call, that response returns as `ModelTurnResult.tool_calls` and leaves this module for the tool protocol module.

## Maintenance Rules

Update this document when changing:

- `ModelTurnRequest`, `ModelMessage`, `ModelToolSchema`, `ModelToolCall`, or `ModelTurnResult`;
- `ModelTurnRequestBuilder.build_request()`;
- `ModelRunner.build_request_from_context()` or `run_turn()`;
- provider message/tool conversion;
- prompt assembly output shape;
- any metadata field that might be mistaken for model-visible content.

## Verification

- `python scripts/verify_runtime_docs.py`
- `python -m pytest tests/test_model_models.py tests/test_model_runner.py tests/test_model_tools.py tests/test_instruction_integration.py tests/test_prompt_assembly.py --basetemp work/pytest-tmp`
- `python -m pytest tests/test_production_baseline_alignment.py::test_runtime_call_chain_stays_on_kernel_graph_model_request_path --basetemp work/pytest-tmp`

## Last Verified Against

Last verified against commit `5f2202bd8cfcc2a4e4a66c025891550e52f3556e` on 2026-06-25.
