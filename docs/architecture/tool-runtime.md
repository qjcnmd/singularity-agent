# Tool Runtime Architecture

Miniharness v0.0.16 makes the tool layer a production boundary without rewriting the agent loop or context manager. The model still proposes OpenAI-style `tool_calls`; `agent.py` still forwards each call to `ToolRuntime.execute_tool_call`; `ContextManager` still stores the returned `ToolResult` as an observation.

## Contract

`ToolSpec` is the registered contract for a tool. It keeps the old constructor fields and adds production metadata:

- identity: `name`, `version`, `description`
- schemas: `input_model`, optional `output_model`
- execution: `handler`, `execution_backend`, `timeout_seconds`, `max_output_chars`
- policy shape: `permission_level`, `capabilities`, `operation`, `resource_resolver`, `risk_tags`
- safety: `side_effects`, `sensitivity`, `approval_profile`, `artifact_policy`
- runtime behavior: `cache_policy`, `idempotency_policy`, `retry_policy`, `streamable`, `enabled`, `delegates_policy_constraints`

The compatibility fields `cacheable` and `idempotent` still work, but runtime behavior reads the richer policy objects after `ToolSpec` normalizes defaults.

## Registry

`ToolRegistry` is the only source of exposed tools. Registration rejects duplicate names and invalid runtime contracts such as a write tool without a mutation backend or a shell tool without a command backend. `to_openai_tools(strict=True)` recursively sets `additionalProperties: false` and exports only safe metadata such as tool version and capabilities.

The old `dispatch()` convenience path now fails closed because it created a temporary runtime and could bypass the caller's policy, approval, trace, and planner configuration. Tests can use `dispatch_for_tests(..., runtime=...)` with an explicit runtime.

Local tool plugins enter the system at this boundary. `PluginRuntime` activates enabled plugins after built-in tools are registered and before `ToolRuntime` is constructed. A plugin can only call `PluginHost.register_tool()`, which converts the declaration into a `ToolSpec` named `<plugin_id>__<tool_name>` and registers it through `ToolRegistry`. The plugin does not receive `ToolRegistry`, `ToolRuntime`, policy, approval, command, sandbox, trace store, or planner objects.

## Runtime Pipeline

`ToolRuntime.execute_tool_call()` follows a fixed sequence:

```txt
resolve registered tool
parse arguments JSON
validate with Pydantic
check replay ledger
check runtime backend contract
check ToolPolicy admission
build PolicyRequest from ToolSpec shape and resolved resources
PolicyRuntime.enforce()
ApprovalGate.resolve() when review is required and a gate is configured
PlannerRuntime.authorize_tool_call()
read-only cache lookup
backend guard
handler execution with timeout
optional output_model validation
redaction and output truncation
cache store or invalidation
planner result update
trace/audit record
```

`ToolRuntime` does not create its own session policy runtime. CLI and tests must inject the active `PolicyRuntime`; construction fails if it is missing. The runtime also does not mutate files directly, run commands directly, choose verification commands, or implement a Git runtime. Those behaviors remain delegated to Workspace Mutation Runtime, Command Runtime, and Verification Runtime.

Plugin-provided tools are not a bypass. Once registered, they follow the same `ToolRuntime.execute_tool_call()` pipeline as built-in tools. High-risk plugin tools must still declare valid `ToolSpec` backend metadata, and registry admission rejects write or shell tools that do not delegate to the appropriate runtime.

## Policy, Approval, Planner

The runtime builds a `PolicyRequest` from each `ToolSpec` using declared `operation`, `capabilities`, and `resource_resolver`. `PolicyRuntime.enforce()` is the hard policy boundary. If policy returns `REQUIRE_REVIEW` and an `ApprovalGate` is configured, the gate produces an `ApprovalGrant`, the grant is registered on `PolicyRuntime`, and the same request is enforced again so the grant is consumed by the policy layer.

`SANDBOX_REQUIRED` fails closed in the tool layer unless the tool explicitly declares the verification delegated backend and `delegates_policy_constraints=true`. The current use case is verification: `run_verification` and `rerun_check` can pass sandbox constraints to `VerificationRuntime` / `CommandRuntime`, where sandbox enforcement belongs. Registry admission rejects this flag on non-verification backends.

Planner authorization remains a second, stricter gate. A planner denial prevents handler execution and records a compact policy observation when the planner supports it. Without a planner, standalone mode remains conservative: low-risk read-only tools may run, while write and shell tools still need their backend contracts and policy to allow them.

## Secret Safety

Read-only filesystem tools use `FileSensitivityClassifier` before exposing or reading paths. Sensitive paths include `.env`, `.env.*`, `.ssh`, private keys, `*.pem`, `*.key`, credentials, token, secret, and api-key-like names. `list_files` hides sensitive files and reports `sensitive_hidden_count`. `search_text` skips sensitive files and redacts secret-like lines before returning matches. `read_file` denies sensitive paths before reading file contents.

Trace payloads are also redacted. The legacy JSONL `TraceWriter` now applies the same redaction style as `TraceRuntime` so raw validated arguments, sensitive path names, secret-like exceptions, and secret-like output are not written to trace.

## Cache And Idempotency

`ToolResultCache` is a bounded LRU cache with optional TTL. Only cacheable, read-only, idempotent, non-sensitive tool results are stored. Cache keys include tool name, version, input schema fingerprint, normalized arguments, workspace root, and file or directory snapshots. Mutating tools clear the cache, and callers can invalidate path-specific entries with `invalidate_paths()`.

`IdempotencyLedger` tracks duplicate `tool_call_id` values before cache lookup for every tool, including cacheable read-only tools. Same id plus same args can replay the previous result when allowed; same id plus different args returns `conflicting_replay`; non-idempotent duplicates return `replay_not_allowed`. Cache hit tests therefore use a fresh `tool_call_id` with identical normalized arguments.

## Backends

The only direct execution backend implemented in the tool layer is `IN_PROCESS`. Delegated backend kinds are represented in the contract so command, mutation, verification, and future external backends can be wired without changing the public contract. If a delegated backend is declared but unavailable in standalone execution, the runtime fails closed with `delegated_backend_unavailable`.
