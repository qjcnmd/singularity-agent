# Tool Execution Architecture

Singularity makes the tool layer a production boundary without moving tool execution into the model layer. The model still proposes OpenAI-style `tool_calls`; `AgentLoop` passes the model turn to `ToolProtocolEngine.process_model_turn()`, the protocol engine converts calls into `ToolExecutionRequest` objects, and `ToolExecutor.execute_request()` performs the actual tool execution. `ContextManager` stores the bound `ToolProtocolResultEnvelope` as a `ToolObservation`.

## Contract

`ToolSpec` is the registered contract for a tool. It keeps the old constructor fields and adds production metadata:

- identity: `name`, `version`, `description`
- schemas: `input_model`, optional `output_model`
- execution: `handler`, `execution_backend`, `timeout_seconds`, `max_output_chars`
- policy shape: `permission_level`, `capabilities`, `operation`, `resource_resolver`, `risk_tags`
- safety: `side_effects`, `sensitivity`, `approval_profile`, `artifact_policy`
- agent behavior: `cache_policy`, `idempotency_policy`, `retry_policy`, `streamable`, `enabled`, `delegates_policy_constraints`

The compatibility fields `cacheable` and `idempotent` still work, but agent behavior reads the richer policy objects after `ToolSpec` normalizes defaults.

## Registry

`ToolRegistry` is the only source of exposed tools. Registration rejects duplicate names and invalid execution contracts such as a write tool without a mutation backend or a shell tool without a command backend. `to_openai_tools(strict=True)` recursively sets `additionalProperties: false` and exports only safe metadata such as tool version and capabilities.

The old `dispatch()` convenience path now fails closed because it created a temporary component and could bypass the caller's policy, approval, trace, and planner configuration. Tests can use `dispatch_for_tests(..., component=...)` with an explicit executor.

Local tool plugins enter the system at this boundary. `PluginManager` activates enabled plugins after built-in tools are registered and before `ToolExecutor` is constructed. A plugin can only call `PluginHost.register_tool()`, which converts the declaration into a `ToolSpec` named `<plugin_id>__<tool_name>` and registers it through `ToolRegistry`. The plugin does not receive `ToolRegistry`, `ToolExecutor`, policy, approval, command, sandbox, trace store, or planner objects.

## Component Pipeline

`ToolExecutor.execute_request()` follows a fixed sequence. `execute_tool_call()` remains a thin wrapper for direct provider-style tool call dictionaries.

```txt
resolve registered tool
parse arguments JSON
validate with Pydantic
check replay ledger
check component backend contract
check ToolPolicy admission
build PolicyRequest from ToolSpec shape and resolved resources
PolicyEngine.enforce()
ApprovalGate.consume_matching_grant() or ApprovalGate.resolve() when review is required and a gate is configured
Planner.authorize_tool_call()
read-only cache lookup
backend guard
handler execution with timeout
optional output_model validation
redaction and output truncation
cache store or invalidation
planner result update
trace/audit record
```

`ToolExecutor` does not create its own session policy component. CLI and tests must inject the active `PolicyEngine`; construction fails if it is missing. The component also does not mutate files directly, run commands directly, choose verification commands, or implement a GitClient. Those behaviors remain delegated to WorkspaceMutationManager, Command Execution, and VerificationRunner.

Plugin-provided tools are not a bypass. Once registered, they follow the same `ToolExecutor.execute_request()` pipeline as built-in tools. High-risk plugin tools must still declare valid `ToolSpec` backend metadata, and registry admission rejects write or shell tools that do not delegate to the appropriate component.

## Policy, Approval, Planner

The component builds a `PolicyRequest` from each `ToolSpec` using declared `operation`, `capabilities`, and `resource_resolver`. `PolicyEngine.enforce()` is the hard risk and policy decision boundary. If policy returns `REQUIRE_REVIEW` and an `ApprovalGate` is configured, `ApprovalGate` consumes a matching existing grant or resolves a new one, registers it in the grant store, and returns the consumed grant to `ToolExecutor`; `PolicyEngine` does not store or consume approval grants.

`SANDBOX_REQUIRED` fails closed in the tool layer unless the tool explicitly declares the verification delegated backend and `delegates_policy_constraints=true`. The current use case is verification: `run_verification` and `rerun_check` can pass sandbox constraints to `VerificationRunner` / `CommandExecutor`, where sandbox enforcement belongs. Registry admission rejects this flag on non-verification backends.

Planner authorization remains a second, stricter gate. A planner denial prevents handler execution and records a compact policy observation when the planner supports it. Without a planner, standalone mode remains conservative: low-risk read-only tools may run, while write and shell tools still need their backend contracts and policy to allow them.

## Secret Safety

Read-only filesystem tools use `FileSensitivityClassifier` before exposing or reading paths. Sensitive paths include `.env`, `.env.*`, `.ssh`, private keys, `*.pem`, `*.key`, credentials, token, secret, and api-key-like names. `list_files` hides sensitive files and reports `sensitive_hidden_count`. `search_text` skips sensitive files and redacts secret-like lines before returning matches. `read_file` denies sensitive paths before reading file contents.

Trace payloads are also redacted. The legacy JSONL `JsonlTraceRecorder` now applies the same redaction style as `TraceRecorder` so raw validated arguments, sensitive path names, secret-like exceptions, and secret-like output are not written to trace.

## Cache And Idempotency

`ToolResultCache` is a bounded LRU cache with optional TTL. Only cacheable, read-only, idempotent, non-sensitive tool results are stored. Cache keys include tool name, version, input schema fingerprint, normalized arguments, workspace root, and file or directory snapshots. Mutating tools clear the cache, and callers can invalidate path-specific entries with `invalidate_paths()`.

`IdempotencyLedger` tracks duplicate `tool_call_id` values before cache lookup for every tool, including cacheable read-only tools. Same id plus same args can replay the previous result when allowed; same id plus different args returns `conflicting_replay`; non-idempotent duplicates return `replay_not_allowed`. Cache hit tests therefore use a fresh `tool_call_id` with identical normalized arguments.

## Backends

The only direct execution backend implemented in the tool layer is `IN_PROCESS`. Delegated backend kinds are represented in the contract so command, mutation, verification, and future external backends can be wired without changing the public contract. If a delegated backend is declared but unavailable in standalone execution, the component fails closed with `delegated_backend_unavailable`.
