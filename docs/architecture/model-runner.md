# Model / Inference Component

Singularity v0.0.14 adds `src/singularity/model/` as the model protocol boundary. The goal is to keep model calls auditable, validated, budget-aware, and policy-aware without moving tool execution into the model layer.

The compact boundary is:

```txt
AgentLoop
  -> Planner.step()
  -> ModelRunner.build_request_from_context()
  -> ModelTurnRequestBuilder.build_request()
  -> PromptAssemblyPipeline.build_for_model_turn()
  -> ContextManager.messages()
  -> ContextManager.build_bundle()
  -> ModelProviderRegistry
  -> ModelProvider.complete()
  -> ModelResponseValidator
  -> canonical ModelToolCall list
  -> ToolProtocolEngine.process_model_turn()
  -> ToolExecutor.execute_request()
```

`ModelRunner` does not execute tools, mutate files, run commands, stage commits, push branches, or implement a GitClient. It only prepares model requests and returns structured model turn results.

## Core Objects

`models.py` defines the stable objects:

```txt
ModelPurpose
ModelRole
ModelMessage
ContentBlock
ModelToolSchema
ToolChoicePolicy
ModelToolCall
ModelCapabilities
ModelPreferences
ModelBudget
ModelUsage
ModelError
ModelTurnRequest
ModelTurnResult
ModelValidationResult
```

All core objects expose `to_dict()` / `from_dict()` so request/result records can be serialized without depending on provider-specific payloads.

## Providers And Registry

`providers.py` defines `ModelProvider`, `ProviderRequest`, `ProviderResponse`, `MockModelProvider`, a legacy chat adapter, and an OpenAI-compatible provider implementation.

`registry.py` owns provider registration, default selection, capability checks, and safe provider capability summaries. Capability checks are explicit for tools, streaming, JSON mode, developer messages, parallel tool calls, and context/output limits.

Before a provider call, `ModelRunner` projects the internal request onto the selected provider's declared capabilities:

```txt
developer messages -> system or user role when developer role is unsupported
json_mode          -> disabled when unsupported
streaming          -> disabled when unsupported
parallel tools     -> max_tool_calls=1 when unsupported
tools required      -> structured unsupported_capability failure when tools are unsupported
```

Safe downgrades are recorded as `capability_adjustments` on the model result and trace metadata. Unsafe downgrades return a structured `ModelError(kind=unsupported_capability)` instead of sending a request the provider cannot satisfy.

The old `src/singularity/provider.py` remains the compatibility surface for callers and tests that still use `Provider.chat(messages, tools, tool_choice=...)`.

## Messages

`messages.py` converts between internal `ModelMessage` objects and provider chat messages. It preserves `tool_call_id`, tool message names, and metadata. When a provider does not support developer messages, developer content falls back to system or user role and records `developer_fallback` metadata.

`ContextManager` still owns ordering, trimming, compression snapshots, and OpenAI chat-shaped message history. `ModelRunner` consumes the rendered view; it does not reorder system/user/assistant/tool history.

## Tools

`tools.py` renders registered `ToolSpec` objects as `ModelToolSchema`, filters by allowed tool name, calculates a schema hash, and normalizes provider tool calls into canonical `ModelToolCall` objects.

Normalization checks:

```txt
tool_call_id
tool_name
allowed tool set
JSON arguments
Pydantic input schema
duplicate ids
```

The canonical call can be converted back to the OpenAI chat tool-call dict used by `ToolExecutor`. Tool execution still validates again inside `ToolExecutor`, including policy, planner, timeout, and component-boundary checks.

## Validation

`validation.py` rejects invalid model output before any tool is executed:

```txt
missing assistant message
empty response
tool_choice=none with tool calls
tool_choice=required without tool calls
too many tool calls
duplicate tool_call_id
unknown tool
invalid JSON
schema mismatch
provider lacks tool or parallel-tool capability
```

Invalid model output returns a structured `ModelTurnResult(status=invalid)` and emits `model.output.rejected`.

## Budget, Retry, And Streaming

`budget.py` estimates message and tool-schema tokens, checks input/total budgets, maps context-length and budget failures, and merges usage records.

`retry.py` retries retryable provider errors such as network, timeout, rate limit, and overload. Auth and invalid-request errors are not retried. Fallback model names can be applied during retry without bypassing request policy metadata.

`streaming.py` aggregates text deltas and tool-call argument deltas. It does not execute tools while streaming; full tool calls are parsed and validated only after aggregation.

## Trace And Redaction

Model events are structured:

```txt
model.request.created
model.response.received
model.request.failed
model.tool_call.proposed
model.output.rejected
```

Default trace payloads store counts, hashes, schema hash, usage, finish reason, tool-call metadata, and optional artifact refs. Raw prompt/response storage is disabled by default. When enabled, responses are written as redacted `TraceArtifactKind.MODEL_MESSAGE` text artifacts.

API keys, base URLs with credentials, raw secret-like content, `.env` content, and full prompts are not written to trace by the model runner. `ContextExportPolicy` blocks secret-like or env-like context from being sent to a remote provider by default.

Provider capability summaries contain booleans and limits only. They do not include raw provider payloads, API responses, prompts, or credentials.

## Final Reports

`TraceSummaryBuilder` aggregates model request, response, failure, proposed tool-call, and token usage counts into `model_usage_summary`. `FinalReport` exposes that as a top-level field, separate from planner evidence, so model accounting is not double-written into the task ledger.

## Reserved Extensions

The current implementation deliberately does not include:

```txt
multi-model routing
local model backend
semantic cache
speculative decoding
advanced tokenizer or exact provider pricing
cost dashboard
GitClient / PR / branch automation
```

Those are future layers on top of the model boundary, not hidden behavior in this release.
