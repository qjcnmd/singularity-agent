# Rust App Server Protocol 模块数据流

模块数据流文档 ID: rust-app-server-protocol

源码证据路径:
- crates/core/src/lib.rs
- crates/protocol/src/lib.rs
- crates/store/src/lib.rs
- crates/policy/src/lib.rs
- crates/sandbox/src/lib.rs
- crates/tools/src/lib.rs
- crates/model/src/lib.rs
- crates/agent/src/lib.rs
- crates/app-server/src/lib.rs
- crates/cli/src/main.rs

关键符号:
- ClientInfo
- ErrorCode
- JsonRpcMessage
- JsonRpcError
- Method
- InitializeParams
- InitializeResult
- ThreadStartParams
- ThreadIdParams
- ThreadForkParams
- Thread
- ThreadListResult
- ThreadResult
- ThreadForkResult
- ThreadDeleteResult
- TurnStartParams
- TurnIdParams
- Turn
- TurnResult
- TurnInterruptResult
- Item
- TraceListParams
- TraceShowParams
- TraceTailParams
- TraceListResult
- ArtifactRef
- TraceEvent
- AppEvent
- PermissionProfile
- PermissionRequest
- PermissionRule
- PermissionDecision
- PreToolUseHook
- PolicyEngine
- ApprovalRequest
- ApprovalDecision
- SessionStore
- SessionStoreDescriptor
- ToolSpec
- ToolRegistry
- ToolBroker
- ToolCallEnvelope
- ToolResult
- ToolObservation
- SandboxPolicy
- SandboxBackend
- SandboxBackendDescriptor
- CommandRequest
- CommandResult
- CommandExecutor
- PatchChange
- PatchResult
- PatchExecutor
- ModelToolSchema
- ModelToolCall
- ModelCapabilities
- ModelProviderConfig
- ModelValidationResult
- ModelError
- ProviderStreamEvent
- ModelTurnRequest
- ModelTurnResponse
- AgentLoopStatusBridge
- PlannerStateBoundary
- ContextAssemblyBoundary
- ContextSummaryEnvelopeBoundary
- PythonSidecarClient
- PythonSidecarConfig
- AppServer
- AppServer.handle_json
- AppServer.handle
- Cli
- Command
- AppServerClient

字段清单:
- ClientInfo: name, title, version
- ErrorCode: code, message
- JsonRpcMessage: jsonrpc, method, id, params, result, error
- JsonRpcError: code, message
- InitializeParams: client_info, capabilities
- InitializeResult: user_agent, platform_family, platform_os
- ThreadStartParams: model, cwd
- ThreadIdParams: thread_id
- ThreadForkParams: thread_id, model, cwd
- Thread: thread_id, model, cwd, status
- ThreadListResult: threads
- ThreadResult: thread
- ThreadForkResult: source_thread_id, thread
- ThreadDeleteResult: thread_id, deleted
- TurnStartParams: thread_id, input
- TurnIdParams: turn_id
- Turn: turn_id, thread_id, status, agent_loop_status
- Item: item_id, turn_id, kind, payload, status
- TurnResult: turn
- TurnInterruptResult: turn_id, status
- TraceListParams: run_id, limit, offset
- TraceShowParams: event_id
- TraceTailParams: run_id, limit
- TraceListResult: events
- ArtifactRef: artifact_id, run_id, item_id, kind, uri, content_digest, summary, metadata, redacted
- TraceEvent: event_id, event_type, run_id, session_id, task_id, phase_id, action_id, parent_event_id, timestamp, monotonic_ms, component, severity, summary, payload, artifact_refs, policy_decision_id, approval_grant_id, sandbox_id, command_id, transaction_id, verification_id, span_id, redaction_applied, payload_hash
- AppEvent: method, params
- PermissionProfile: profile, workspace_roots, additional_writable_directories, network_access, approval_policy, protected_paths_enforced
- PermissionRequest: tool_name, operation, resource
- PermissionRule: rule_id, scope, outcome, operation, resource_pattern
- PermissionDecision: outcome, reason, rule_id, scope
- PreToolUseHook: hook_id, decision
- PolicyEngine: profile, rules, hooks
- ApprovalRequest: request_id, session_id, task_id, action, reason
- ApprovalDecision: request_id, decision_id, outcome, reason
- SessionStore: connection, descriptor
- SessionStoreDescriptor: backend, path, schema_version
- ToolSpec: name, version, description, input_schema, permission_level, risk_tags
- ToolRegistry: tools
- ToolBroker: registry
- ToolCallEnvelope: protocol_version, run_id, session_id, task_id, tool_call_id, tool_name, raw_arguments
- ToolResult: ok, content, error_code, truncated, metadata
- ToolObservation: tool_call_id, tool_name, ok, status, visibility, content_preview, content_digest, result_ref, error_code, reference_ids, observation_id, truncated, redacted, policy_decision_id, approval_grant_id, internal_metadata
- SandboxPolicy: profile, filesystem, network, resources
- SandboxBackendDescriptor: backend, enforcement, capabilities
- CommandRequest: command_id, argv, cwd, purpose, timeout_seconds, network, filesystem
- CommandResult: command_id, execution_status, semantic_status, exit_code, duration_ms, timed_out, stdout_preview, stderr_preview, output_truncated, redacted, changed_files
- CommandExecutor: process_manager
- PatchChange: path, expected, replacement
- PatchResult: applied, changed_files, rolled_back, error
- PatchExecutor: workspace_root
- ModelToolSchema: name, description, parameters_schema, capability_tags, risk_tags, metadata
- ModelToolCall: tool_call_id, tool_name, arguments, raw_arguments, parse_status, validation_errors, provider_metadata
- ModelCapabilities: supports_tools, supports_parallel_tool_calls, supports_streaming, supports_json_mode, supports_structured_outputs, supports_system_message, supports_developer_message, max_context_tokens, max_output_tokens, input_modalities, output_modalities
- ModelProviderConfig: provider_name, model_name, base_url_present, api_key_present
- ModelValidationResult: valid, errors, warnings, repaired, repair_message
- ModelError: kind, message, retryable, provider_name, model_name, raw_error_ref, metadata
- ProviderStreamEvent: event_type, text_delta, tool_call_id, tool_name, arguments_delta, usage_delta, error, metadata
- ModelTurnRequest: request_id, run_id, session_id, task_id, phase_id, action_id, purpose, messages, tools, tool_choice, model_preferences, budget, context_metadata, policy_metadata, trace_metadata
- ModelTurnResponse: request_id, response_id, status, assistant_message, tool_calls, usage, finish_reason, validation, error, provider_name, model_name, latency_ms, trace_event_ids, raw_response_ref, metadata
- AgentLoopStatusBridge: status, completed, final_answer, run_id, session_id, task_id, events, trace_path, error
- PlannerStateBoundary: task_id, current_phase, status, current_plan, completion_criteria, open_actions, blocked_actions, risk_escalations, evidence_refs
- ContextAssemblyBoundary: bundle_id, run_id, task_id, phase_id, model, provider, messages, included_item_ids, excluded_item_ids, budget, compression_snapshot_id, retrieval_query, render_policy, created_at, bundle_digest, metadata
- ContextSummaryEnvelopeBoundary: version, summary_id, summary_payload, source_item_ids, cache_attribution, previous_summary_digest, summary_digest, rendered_summary, created_at, metadata
- PythonSidecarConfig: python_bin, module, project_root, python_path, env
- AppServer: store, initialized, initialized_acknowledged, python_sidecar

## 这一层解决什么问题

Rust App Server Protocol 层建立第一阶段迁移的硬边界：客户端只通过 JSON-RPC 请求和通知进入 app-server，app-server 只持久化 thread、turn、item、trace 和 pending approval，不直接复用 Python `AgentLoop` 内部对象。`sg` CLI 是第一个 client；未来 desktop 也接同一个 protocol，不再设计第二套 core。显式 Python sidecar 是 migration bridge，只通过 JSON-RPC 子进程边界调用现有 Python AgentLoop，并把安全状态摘要翻译回 Rust protocol。

## 当前源码位置

- `crates/core/src/lib.rs`
- `crates/protocol/src/lib.rs`
- `crates/store/src/lib.rs`
- `crates/policy/src/lib.rs`
- `crates/sandbox/src/lib.rs`
- `crates/tools/src/lib.rs`
- `crates/model/src/lib.rs`
- `crates/agent/src/lib.rs`
- `crates/app-server/src/lib.rs`
- `crates/cli/src/main.rs`

## 关键类、函数、字段

`JsonRpcMessage` 是 wire envelope；`Thread`、`Turn`、`Item`、`TraceEvent` 和 `ArtifactRef` 是 app-server 的 durable protocol object；`ThreadIdParams`、`ThreadForkParams`、`TurnIdParams`、`TraceListParams`、`TraceShowParams`、`TraceTailParams` 和对应 result object 是 CLI agent protocol 的 request/response schema；`ToolSpec`、`ToolRegistry`、`ToolBroker`、`ToolCallEnvelope`、`ToolResult`、`ToolObservation`、`PermissionProfile`、`PermissionRequest`、`PermissionRule`、`PermissionDecision`、`PreToolUseHook`、`PolicyEngine`、`ApprovalRequest`、`ApprovalDecision`、`SandboxPolicy`、`CommandRequest`、`CommandResult`、`CommandExecutor`、`PatchChange`、`PatchResult`、`PatchExecutor`、`ModelToolSchema`、`ModelToolCall`、`ModelCapabilities`、`ModelProviderConfig`、`ModelValidationResult`、`ModelError`、`ProviderStreamEvent`、`ModelTurnRequest` 和 `ModelTurnResponse` 是第一阶段先迁移的 schema object 与最小执行边界。`SessionStore` 是 SQLite-backed persistence boundary，`SessionStoreDescriptor` 是可序列化的 store schema descriptor。`AgentLoopStatusBridge` 表示 Rust host 对 AgentLoop 状态的显式理解：默认 `not_migrated`，或由 Python sidecar 返回 completed/blocked/cancelled/failed。`PlannerStateBoundary`、`ContextAssemblyBoundary` 和 `ContextSummaryEnvelopeBoundary` 是 M9 schema/parity contract，只用于 Python oracle JSON roundtrip，不执行 provider、工具或 workspace mutation。compaction executor、repair 和 finalization mapping 仍是后续独立切片。`PythonSidecarClient` 是 Rust host 到 Python migration sidecar 的 stdio JSON-RPC client。`AppServer.handle_json()` 是 stdio JSONL transport 的入口。`AppServerClient` 是 `sg` 内部 stdio JSON-RPC client，不暴露 store 或 agent internals。

## 真实运行时调用链

`sg run` / `sg chat` / `sg continue` / `sg threads` / `sg trace` / `sg approvals` -> `AppServerClient::spawn()` 启动 `singularity_app_server` stdio process -> `initialize` request -> `initialized` notification -> 对应 thread/turn/trace/approval request -> `AppServer.handle_json()` -> `AppServer.handle()` -> `SessionStore.create_thread_with_trace()` / `create_turn_with_input_and_trace()` / `create_approval_with_trace()` / `record_approval_decision_with_trace()` 或 read/list/update helper -> `JsonRpcMessage::response()` 或 `AppEvent.to_notification()` 输出 JSONL -> `sg` 渲染 thread、turn、item、trace 或 approval 行。默认 `turn/start` 调用 `AgentLoopStatusBridge::not_migrated()`，不会进入 Python `AgentLoop.run()`，也不会把 turn 伪装成已完成真实 agent loop。显式设置 `SINGULARITY_PYTHON_SIDECAR=1` 后，`singularity_app_server` 创建 `PythonSidecarConfig`，`AppServer.run_python_sidecar_if_enabled()` 通过 `PythonSidecarClient` 发送 `agent/run`，Python `singularity.agent_host.sidecar` 调用 `AgentHost.start_run()`，再进入现有 `KernelBootstrap -> AgentKernel -> AgentLoop`；Rust 只消费 sidecar 返回的 status/final_answer/events/trace_path 安全子集。

## 真实任务中的对象流

以 `sg run "goal"` 为例：CLI 启动 app-server 子进程，发送 `JsonRpcMessage::request(Method::Initialize, ...)`；`AppServer.handle_json()` 反序列化为 `JsonRpcMessage`，`InitializeParams` 校验 `clientInfo` 后返回 `InitializeResult`。CLI 再发送 `initialized` notification，连接进入 ready 状态。随后 `thread/start` 生成对象 `Thread` 并由 `SessionStore.create_thread_with_trace()` 在同一个 SQLite transaction 中写入 `threads` 和 `trace_events`。`turn/start` 生成对象 `Turn` 和 user message `Item`，通过 `SessionStore.create_turn_with_input_and_trace()` 在一个 transaction 中写入 `turns` / `items` / `trace_events`，并输出 `turn/started -> item/started -> item/agentMessage/delta -> item/completed` 通知。默认路径写 `agent_loop_status="not_migrated"` 且 turn 仍为 running。显式 sidecar 路径会先得到 Python sidecar 的 `AgentLoopStatusBridge`，再把 completed/blocked/failed/cancelled 映射成 `TurnStatus::Completed` / `Blocked` / `Failed` / `Interrupted`，并追加 `component="python_sidecar"` 的安全 trace event。`sg continue <thread-id> "instruction"` 先通过 `thread/read` 验证 thread 存在，再发送新的 `turn/start`。approval 流由 `approval/request` 写 pending row 和 trace，`approval/decision` 只能消费 pending approval 一次，并在同一个 transaction 中写 `approvals` decision fields、`approval_decisions` ledger row 和 trace；重复 decision 返回 `Pending approval not found`。

## 真实对象完整结构

### JSON-RPC 与 thread/turn/item

枚举值包括 `Method::Initialize = "initialize"`、`Method::ThreadList = "thread/list"`、`Method::ThreadRead = "thread/read"`、`Method::ThreadStart = "thread/start"`、`Method::ThreadResume = "thread/resume"`、`Method::ThreadFork = "thread/fork"`、`Method::ThreadArchive = "thread/archive"`、`Method::ThreadDelete = "thread/delete"`、`Method::TurnStart = "turn/start"`、`Method::TurnInterrupt = "turn/interrupt"`、`Method::TurnStatus = "turn/status"`、`Method::ApprovalList = "approval/list"`、`Method::TraceTail = "trace/tail"`、`Method::ServerShutdown = "server/shutdown"`、`ThreadStatus::Active = "active"`、`TurnStatus::Running = "running"`、`TurnStatus::Blocked = "blocked"`、`TurnStatus::Interrupted = "interrupted"`、`ItemKind::CommandExecution = "commandExecution"`、`ItemStatus::Completed = "completed"`。

```rust
pub struct JsonRpcMessage {
    pub jsonrpc: Option<String>,
    pub method: Option<String>,
    pub id: Option<Value>,
    pub params: Value,
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
}

pub struct Thread {
    pub thread_id: String,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub status: ThreadStatus,
}

pub struct Turn {
    pub turn_id: String,
    pub thread_id: String,
    pub status: TurnStatus,
    pub agent_loop_status: String,
}

pub struct Item {
    pub item_id: String,
    pub turn_id: String,
    pub kind: ItemKind,
    pub payload: Value,
    pub status: ItemStatus,
}
```

### store / approval / trace

`SessionStore` 持有真实 SQLite connection，不参与 JSON 序列化；`SessionStoreDescriptor` 是 schema object，用于记录 store backend、path 和 schema version。Rust `PolicyEngine` 当前是纯决策对象，不直接写 SQLite；approval request / decision ledger 仍由 app-server 通过 `SessionStore` 落盘。

```rust
pub struct SessionStoreDescriptor {
    pub backend: String,
    pub path: String,
    pub schema_version: u32,
}

pub struct ArtifactRef {
    pub artifact_id: String,
    pub run_id: String,
    pub item_id: Option<String>,
    pub kind: String,
    pub uri: String,
    pub content_digest: String,
    pub summary: String,
    pub metadata: Value,
    pub redacted: bool,
}

pub struct ApprovalRequest {
    pub request_id: String,
    pub session_id: String,
    pub task_id: String,
    pub action: String,
    pub reason: String,
}

pub struct ApprovalDecision {
    pub request_id: String,
    pub decision_id: String,
    pub outcome: ApprovalOutcome,
    pub reason: String,
}

pub struct PermissionRequest {
    pub tool_name: String,
    pub operation: PermissionOperation,
    pub resource: String,
}

pub struct PermissionRule {
    pub rule_id: String,
    pub scope: SettingsScope,
    pub outcome: PermissionDecisionOutcome,
    pub operation: Option<PermissionOperation>,
    pub resource_pattern: Option<String>,
}

pub struct PermissionDecision {
    pub outcome: PermissionDecisionOutcome,
    pub reason: String,
    pub rule_id: Option<String>,
    pub scope: Option<SettingsScope>,
}

pub struct PreToolUseHook {
    pub hook_id: String,
    pub decision: PermissionDecision,
}

pub struct PolicyEngine {
    pub profile: PermissionProfile,
    pub rules: Vec<PermissionRule>,
    pub hooks: Vec<PreToolUseHook>,
}

pub struct TraceEvent {
    pub event_id: String,
    pub event_type: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: Option<String>,
    pub phase_id: Option<String>,
    pub action_id: Option<String>,
    pub parent_event_id: Option<String>,
    pub timestamp: Option<String>,
    pub monotonic_ms: Option<u64>,
    pub component: String,
    pub severity: String,
    pub summary: String,
    pub payload: Value,
    pub artifact_refs: Vec<String>,
    pub policy_decision_id: Option<String>,
    pub approval_grant_id: Option<String>,
    pub sandbox_id: Option<String>,
    pub command_id: Option<String>,
    pub transaction_id: Option<String>,
    pub verification_id: Option<String>,
    pub span_id: Option<String>,
    pub redaction_applied: bool,
    pub payload_hash: String,
}
```

### tool / sandbox / model

这些 schema object 是 Phase 1 的硬边界；执行、provider 调用和完整 AgentLoop 仍留在 Python oracle 中。

```rust
pub struct ToolSpec {
    pub name: String,
    pub version: String,
    pub description: String,
    pub input_schema: Value,
    pub permission_level: PermissionLevel,
    pub risk_tags: Vec<String>,
}

pub struct ToolBroker {
    registry: ToolRegistry,
}

pub enum ToolBrokerDecision {
    Allow,
    Deny { reason: String },
}

pub struct ToolObservation {
    pub tool_call_id: String,
    pub tool_name: String,
    pub ok: bool,
    pub status: String,
    pub visibility: ToolObservationVisibility,
    pub content_preview: String,
    pub content_digest: String,
    pub result_ref: Option<String>,
    pub error_code: Option<String>,
    pub reference_ids: Vec<String>,
    pub observation_id: Option<String>,
    pub truncated: bool,
    pub redacted: bool,
    policy_decision_id: Option<String>,
    approval_grant_id: Option<String>,
    internal_metadata: Option<Value>,
}

pub struct ModelToolSchema {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
    pub capability_tags: Vec<String>,
    pub risk_tags: Vec<String>,
    pub metadata: Value,
}

pub struct ModelToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub raw_arguments: String,
    pub parse_status: ModelToolParseStatus,
    pub validation_errors: Vec<String>,
    pub provider_metadata: Value,
}

pub struct ModelCapabilities {
    pub supports_tools: bool,
    pub supports_parallel_tool_calls: bool,
    pub supports_streaming: bool,
    pub supports_json_mode: bool,
    pub supports_structured_outputs: bool,
    pub supports_system_message: bool,
    pub supports_developer_message: bool,
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
}

pub struct ModelProviderConfig {
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub base_url_present: bool,
    pub api_key_present: bool,
}

pub struct ModelValidationResult {
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub repaired: bool,
    pub repair_message: Option<String>,
}

pub struct ModelError {
    pub kind: ModelErrorKind,
    pub message: String,
    pub retryable: bool,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub raw_error_ref: Option<String>,
    pub metadata: Value,
}

pub struct ProviderStreamEvent {
    pub event_type: ProviderStreamEventType,
    pub text_delta: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub arguments_delta: Option<String>,
    pub usage_delta: Option<ModelUsage>,
    pub error: Option<String>,
    pub metadata: Value,
}

pub struct CommandRequest {
    pub command_id: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub purpose: CommandPurpose,
    pub timeout_seconds: u64,
    pub network: SandboxNetworkPolicy,
    pub filesystem: SandboxFilesystemPolicy,
}

pub struct CommandResult {
    pub command_id: String,
    pub execution_status: CommandExecutionStatus,
    pub semantic_status: CommandSemanticStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub stdout_preview: String,
    pub stderr_preview: String,
    pub output_truncated: bool,
    pub redacted: bool,
    pub changed_files: Vec<String>,
}

pub struct CommandExecutor {
    process_manager: ProcessManager,
}

pub struct SandboxBackendDescriptor {
    pub backend: String,
    pub enforcement: SandboxBackendEnforcement,
    pub capabilities: SandboxCapabilities,
}

pub struct PatchChange {
    pub path: String,
    pub expected: Option<String>,
    pub replacement: String,
}

pub struct PatchResult {
    pub applied: bool,
    pub changed_files: Vec<String>,
    pub rolled_back: bool,
    pub error: Option<String>,
}

pub struct PatchExecutor {
    workspace_root: PathBuf,
}

pub struct ModelTurnRequest {
    pub request_id: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: String,
    pub phase_id: String,
    pub action_id: String,
    pub purpose: ModelPurpose,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ModelToolSchema>,
    pub tool_choice: ToolChoicePolicy,
    pub model_preferences: ModelPreferences,
    pub budget: ModelBudget,
    pub context_metadata: Value,
    pub policy_metadata: Value,
    pub trace_metadata: Value,
}

pub struct ModelTurnResponse {
    pub request_id: String,
    pub response_id: String,
    pub status: ModelTurnStatus,
    pub assistant_message: Option<ModelMessage>,
    pub tool_calls: Vec<ModelToolCall>,
    pub usage: ModelUsage,
    pub finish_reason: Option<String>,
    pub validation: Option<ModelValidationResult>,
    pub error: Option<ModelError>,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub latency_ms: Option<u64>,
    pub trace_event_ids: Vec<String>,
    pub raw_response_ref: Option<String>,
    pub metadata: Value,
}
```

### agent sidecar bridge

`AgentLoopStatusBridge` 和 `PythonSidecarClient` 属于迁移桥，不是最终 Rust AgentLoop。Python sidecar 只返回安全摘要字段；raw prompt、raw trace payload、raw tool arguments、provider response 和 secret-like 文本不得跨入 Rust trace/model-visible payload。

```rust
pub struct AgentLoopStatusBridge {
    pub status: AgentHostStatus,
    pub completed: bool,
    pub final_answer: Option<String>,
    pub run_id: Option<String>,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub events: Vec<SidecarRunEvent>,
    pub trace_path: Option<String>,
    pub error: Option<String>,
}

pub struct PythonSidecarConfig {
    pub python_bin: String,
    pub module: String,
    pub project_root: PathBuf,
    pub python_path: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}
```

### M9 planner/context/compaction parity contract

这些对象属于 Rust `crates/agent` 的 schema/parity boundary，不属于 app-server runtime execution。它们只镜像 Python oracle fixture 中已经存在的 planner state、context bundle 与 context summary envelope 安全字段，用于 JSON roundtrip 和 schema generation。默认 `turn/start` 仍使用 `AgentLoopStatusBridge::not_migrated()`。compaction executor、repair verification contract、execution outcome 和 finalization mapping 不在当前切片内。

```rust
pub struct PlannerStateBoundary {
    pub task_id: String,
    pub current_phase: String,
    pub status: String,
    pub current_plan: Vec<Value>,
    pub completion_criteria: Value,
    pub open_actions: Vec<Value>,
    pub blocked_actions: Vec<Value>,
    pub risk_escalations: Vec<Value>,
    pub evidence_refs: Vec<String>,
}

pub struct ContextAssemblyBoundary {
    pub bundle_id: String,
    pub run_id: String,
    pub task_id: String,
    pub phase_id: String,
    pub model: String,
    pub provider: String,
    pub messages: Vec<Value>,
    pub included_item_ids: Vec<String>,
    pub excluded_item_ids: Vec<String>,
    pub budget: Value,
    pub compression_snapshot_id: Option<String>,
    pub retrieval_query: Option<String>,
    pub render_policy: Value,
    pub created_at: String,
    pub bundle_digest: String,
    pub metadata: Value,
}

pub struct ContextSummaryEnvelopeBoundary {
    pub version: u32,
    pub summary_id: String,
    pub summary_payload: Value,
    pub source_item_ids: Vec<String>,
    pub cache_attribution: Value,
    pub previous_summary_digest: Option<String>,
    pub summary_digest: String,
    pub rendered_summary: String,
    pub created_at: String,
    pub metadata: Value,
}
```

## 谁生成这些对象

`JsonRpcMessage::request()` 和 `JsonRpcMessage::notification()` 生成 wire message；`SessionStore.create_thread()` / `create_thread_with_trace()` 生成 `Thread`；`SessionStore.create_turn()` / `create_turn_with_input_and_trace()` 生成 `Turn`；`SessionStore.append_item()` 生成 `Item`；`TraceEvent::new()` 生成 `TraceEvent`；`SessionStore.register_artifact_ref()` 生成 `ArtifactRef`；`ApprovalRequest::new()` 和 `ApprovalDecision::new()` 生成 approval object；`PermissionRequest::new()` 只封装调用方传入的 resource，路径、命令和 host 归一化由 command/sandbox 或上层 tool boundary 负责；`PermissionRule::new()` 生成 declarative rule，`PermissionDecision::new()` 或 `PolicyEngine.evaluate()` 生成决策结果；`PythonSidecarClient.run_agent()` 从 Python sidecar 的 `agent/run` response 生成 `PythonSidecarRunResult`，再由 `AgentLoopStatusBridge::from_sidecar()` 生成 Rust host-facing status；`scripts/export_rust_parity_fixtures.py` 从 Python `PlannerState`、`ContextBundle` 和 `ContextSummaryEnvelope` 导出 M9 oracle JSON，Rust `PlannerStateBoundary` / `ContextAssemblyBoundary` / `ContextSummaryEnvelopeBoundary` 只反序列化和 roundtrip 这些字段；`ToolSpec::new()`、`ToolBroker::register()`、`ToolCallEnvelope::new()`、`ToolResult::success()` / `failure()`、`ToolObservation::summary()` / `failed()`、`SandboxPolicy::isolated_verification()`、`CommandRequest::project_verification()` / `local_process()`、`git_status_request()` / `git_diff_request()`、`CommandResult::completed()` / `policy_denied()`、`CommandExecutor::new()`、`PatchChange::replace()` / `create()`、`PatchExecutor::new()`、`ModelTurnRequest::new()` 和 `ModelTurnResponse::completed()` 生成各自 schema object 或最小执行边界对象。`validate_provider_config()` 只检查 provider/model/base_url/api_key presence；`classify_model_error()` 只把已归一化的 provider failure 映射为 Rust 模型边界 category；`validate_stream_events()` 和 `validate_model_response()` 只验证 streaming envelope、tool choice、tool-call parse status、allowed tool names 和 provider capability metadata，不执行工具或 provider call。`ToolRegistry.register()` 只接受 `builtin.*`、`mcp.<server>.<tool>` 和 `python.<plugin>.<tool>` 命名空间，并拒绝重复 name。

## 谁消费这些对象

`AppServer.handle()` 消费 `JsonRpcMessage` 并分派到 initialize、thread、turn、approval、trace handler；`AppServer.run_python_sidecar_if_enabled()` 在显式 sidecar 配置存在时消费 `TurnStartParams` 并调用 `PythonSidecarClient`；`SessionStore.create_thread_with_trace()`、`create_turn_with_input_and_trace()`、`create_approval_with_trace()`、`record_approval_decision_with_trace()`、`append_trace()` 和 `register_artifact_ref()` 消费 protocol object 写 SQLite；`PolicyEngine.evaluate()` 消费 `PermissionRequest`，按 hook、deny、protected resource、defer、ask、allow、fallback ask 顺序返回 `PermissionDecision`，且 `approval_policy=never` 会把 ask 投影为 deny；policy 不解析 shell wrapper、argv、workspace path、network host 或 command 等价形式；Rust M9 boundary tests 消费 Python oracle fixture，校验 planner state、context bundle 和 context summary envelope 字段可被 Rust 解析；app-server runtime 当前不消费这些 M9 boundary object。`ToolBroker.model_visible_tools()` 消费 `ToolRegistry` 并只投影 name、redacted description 和 input schema 给模型；`ToolBroker.execute()` 消费 `ToolCallEnvelope` 与外部 policy decision，未知或 denied tool 不调用 executor；`CommandRequest.permission_resource()` 把 shell wrapper / argv 规范化为 policy 可消费的 command resource；`CommandExecutor.run_local()` 只消费显式 `HostWorkspace` request，遇到 read-only、copy-on-write、empty temp 或 hard-isolation request 时返回 backend error 而不是 local fallback；git status/diff 只生成 sandbox-required `CommandRequest`，不绕过 command/sandbox 边界执行 git；`PatchExecutor.apply()` 消费 `PatchChange` 并在后续 change 失败时回滚前序写入；Rust model validator 消费已经构造好的 model request/response/stream 对象并返回 `ModelValidationResult`，不消费 tool registry、policy engine、sandbox backend 或 HTTP client。`ToolObservation.to_model_payload()` 消费 tool observation 并生成模型可见安全 payload；`AppEvent.to_notification()` 消费 event 并输出 JSON-RPC notification。`sg` 只消费 `singularity_protocol` 和 `singularity_core`，不消费 `singularity_agent`、`singularity_model`、`singularity_tools` 或 `singularity_store`。

## 是否落盘

`SessionStore.open()` 初始化 SQLite 文件并确保 `schema_migrations` 记录 `0001_initial_session_store` 和 `0002_durable_ledger`；`threads`、`turns`、`items`、`trace_events`、`artifact_refs`、`approvals` 和 `approval_decisions` 表是真实落盘点。thread read/list/resume/archive/delete 和 turn status/interrupt 读取或更新这些现有表；`approval/request` 写 pending approval，`approval/list` 读取未决 approval，`approval/decision` 更新同一 row 的 decision fields 并写 `approval_decisions` ledger；trace/list 支持 `limit` / `offset` 分页，trace/show 和 trace/tail 都从 SQLite `trace_events` 查询真实 events。Rust `CommandExecutor` / `PatchExecutor` / model validator 当前不直接写 SQLite、trace 或 artifact store；它们只返回 bounded result object，后续接入 app-server 或 AgentLoop 时必须由上层负责审计落盘。`target/` 是 Rust build output，被 `.gitignore` 排除。

## 是否进入 trace / audit

`thread/start`、`turn/start`、`approval/request` 和 `approval/decision` 都写 `TraceEvent`，且由 store transaction 把对应业务 row 与 trace 一起提交或回滚。显式 Python sidecar 路径会追加 `component="python_sidecar"` 的 trace summary 和 sidecar event 摘要；这些 payload 只包含 sidecar status、safe IDs、trace path handle 和 event sequence/component，不包含 raw prompt、raw trace payload、raw tool arguments 或 provider response。`ArtifactRef` 的 `summary` / `metadata` / `uri` 会对 secret-like marker 做本地 redaction 后落盘。Rust `PolicyEngine.evaluate()` 当前不写 trace/audit，只返回纯 `PermissionDecision`；完整运行时 policy audit writer 仍由 Python `src/singularity/policy/audit.py` 保持 oracle，后续执行集成必须只写脱敏资源 handle。Rust `CommandExecutor` 的 stdout/stderr preview 有上限并会对 secret-like marker redaction，但当前没有生成 trace event；后续接入必须只写 bounded/redacted command evidence。Rust model boundary 只允许 `raw_response_ref` / `raw_error_ref` 这类 opaque handle，不保存 provider raw response body；`ModelError` 的 message 必须是已脱敏摘要。`ToolBroker` 的 model-visible spec 不输出 permission/risk/internal metadata，并会 redaction 恶意 MCP 描述中的 prompt-injection/secret-like 文本；`ToolObservation.to_model_payload()` 明确不输出 `policy_decision_id`、`approval_grant_id`、raw arguments、internal metadata 或 reference-only content。

## 失败路径

连接未完成 `initialize` 或未收到 `initialized` notification 前，业务 request 返回 `Not initialized`。同一连接重复 `initialize` 返回 `Already initialized`。未知 thread / turn / trace run / trace event 分别返回 `Thread not found`、`Turn not found`、`Trace run not found`、`Trace event not found`。approval request 重复返回 `Approval already exists`；approval decision 找不到 pending request 或重复消费时返回 `Pending approval not found`。Python sidecar 启动失败、invalid JSON、AgentLoop blocked 或 sidecar returned error 不会 fallback 到本地 Rust fake completion；app-server 把 sidecar failure 写为 `agent_loop_status="failed"` 并记录 error summary。Rust `CommandExecutor.run_local()` 遇到 sandbox-required request 返回 backend error，不启动本地进程；空 argv 或 spawn 失败返回 spawn failed；timeout 会 kill process tree 并返回 timed out；secret-like stdout/stderr 只输出 redacted preview。`PatchExecutor.apply()` 遇到路径逃逸、expected text 缺失、目标已存在或写入失败会回滚已经写入的文件。Rust model validation 会报告 missing provider/model/base_url/api_key、auth/network/sandbox permission/model config/provider unavailable 分类、stream event 顺序错误、tool choice 违规、unknown tool、invalid JSON、schema mismatch、provider capability 不支持等错误；它只返回 validation/error object，不触发 retry、HTTP、tool execution 或 AgentLoop repair。SQLite 和 JSON 解析错误作为 app-server internal error 返回。

## 当前结构问题

Phase 1 没有迁移模型 provider HTTP、Windows sandbox backend、evaluation runner 或 Rust native AgentLoop；`AgentLoopStatusBridge::not_migrated()` 只是 host-facing status，不代表 agent 已完成。显式 Python sidecar 可以调用当前 Python AgentLoop 作为 migration reference，但 Rust 只负责 app-server 边界、状态翻译和安全 trace summary。M9 当前只加入 planner state、context assembly 与 compaction summary deterministic boundary schema，尚未迁移 planner step、context assembler、compaction executor、failure analysis、repair planner、finalization mapping 或完整 `AgentLoop.run()`。当前 `ToolBroker` 是最小 Rust tool boundary：它验证工具命名、投影模型可见 schema、阻断 denied/unknown tool 执行并生成安全 observation；`PolicyEngine` 已提供 Rust 纯决策、hook 点、deny-first precedence、protected resource deny 和 approval ask/deny 投影；`PermissionProfile` 仍保留 profile schema 字段，但当前 Rust policy 不根据 workspace roots、network access 或 permission mode 自动推导 allow/deny；`CommandExecutor` / `PatchExecutor` 已提供最小 Rust side-effect boundary，但还没有接入 app-server、tool broker、Python AgentLoop 或完整 Windows sandbox backend。Rust `crates/model` 当前只拥有 model request/response schema、capability metadata、provider config presence check、stream envelope validation、model response/tool-call validation 和 provider failure classification；它没有 HTTP client、provider registry、retry loop、context compaction、planner repair 或 AgentLoop 逻辑。command resource normalization 位于 `CommandRequest.permission_resource()`，policy 不解析 shell wrapper；sandbox-required command 没有显式 backend 时 fail closed；git helpers 只生成 command request，不建立第二套 git wrapper。当前 `sg` 是最小 JSON-RPC client：可以启动 app-server 子进程并完成 run/continue/list/trace/approval 查询，但不管理长期后台 daemon 生命周期、PTY TUI 或交互式 approval prompt。Artifact ref 目前只持久化引用和 redacted metadata，不负责 artifact bytes 管理。WebSocket 和 Unix socket 只是后续 transport 方向，当前只实现 stdio JSONL。

## 维护规则

新增 client 必须只依赖 protocol/core 层，不得直接耦合 agent/model/tools/store。新增 app-server 方法必须先在 `crates/protocol` 定义 request/response/event schema，再由 `crates/app-server` routing 和 `crates/store` persistence 接入。model boundary 只能增加与 Python `singularity.model` 对齐的 request/response/tool/stream/config schema 或纯验证函数；不得在 M8 引入 provider HTTP、provider registry、retry scheduler、planner repair、context manager 或 AgentLoop。M9 boundary 只能一次迁移一个 deterministic slice，并必须先更新 Python oracle fixture 和 Rust parity tests；不得把 `Planner`、`ContextManager`、compaction executor 或 tool repair loop runtime 一次性重写进 Rust。任何改变 thread/turn/item/approval/trace/model/agent-boundary wire format 的改动都必须更新 Rust tests、Python oracle fixture、本文档和 `docs/singularity.md` 的长期架构说明。

M0 迁移 guardrail 由 `scripts/verify_rust_migration_boundaries.py` 自动检查并进入 CI。该脚本阻断 CLI 直接依赖 agent/model/tools/store、未登记的新 crate 依赖、Python core runtime 非 allowlist 改动、Python RuntimeHost 类过渡实现、desktop/Web 抢跑文件、`turn/start` 伪装 Rust AgentLoop 已迁移，以及 `ToolObservation.to_model_payload()` 泄漏 raw arguments、approval/policy 内部 id、internal metadata 或 secret-like 文本。
