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
- ApprovalRequest
- ApprovalDecision
- SessionStore
- SessionStoreDescriptor
- ToolSpec
- ToolRegistry
- ToolCallEnvelope
- ToolResult
- ToolObservation
- SandboxPolicy
- SandboxBackend
- SandboxBackendDescriptor
- CommandRequest
- CommandResult
- ModelTurnRequest
- ModelTurnResponse
- AgentLoopBridge
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
- ApprovalRequest: request_id, session_id, task_id, action, reason
- ApprovalDecision: request_id, decision_id, outcome, reason
- SessionStore: connection, descriptor
- SessionStoreDescriptor: backend, path, schema_version
- ToolSpec: name, version, description, input_schema, permission_level, risk_tags
- ToolRegistry: tools
- ToolCallEnvelope: protocol_version, run_id, session_id, task_id, tool_call_id, tool_name, raw_arguments
- ToolResult: ok, content, error_code, truncated, metadata
- ToolObservation: tool_call_id, tool_name, ok, status, visibility, content_preview, content_digest, result_ref, error_code, reference_ids, observation_id, truncated, redacted, policy_decision_id, approval_grant_id, internal_metadata
- SandboxPolicy: profile, filesystem, network, resources
- SandboxBackendDescriptor: backend, enforcement, capabilities
- CommandRequest: command_id, argv, cwd, purpose, timeout_seconds, network, filesystem
- CommandResult: command_id, execution_status, semantic_status, exit_code, duration_ms, timed_out, stdout_preview, stderr_preview, output_truncated, changed_files
- ModelTurnRequest: request_id, run_id, session_id, task_id, phase_id, action_id, purpose, messages, tools, tool_choice, model_preferences, budget, context_metadata, policy_metadata, trace_metadata
- ModelTurnResponse: request_id, response_id, status, assistant_message, tool_calls, usage, finish_reason, validation, error, provider_name, model_name, latency_ms, trace_event_ids, raw_response_ref, metadata
- AgentLoopBridge: status, completed
- AppServer: store, initialized, initialized_acknowledged

## 这一层解决什么问题

Rust App Server Protocol 层建立第一阶段迁移的硬边界：客户端只通过 JSON-RPC 请求和通知进入 app-server，app-server 只持久化 thread、turn、item、trace 和 pending approval，不直接复用 Python `AgentLoop` 内部对象。`sg` CLI 是第一个 client；未来 desktop 也接同一个 protocol，不再设计第二套 core。

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

`JsonRpcMessage` 是 wire envelope；`Thread`、`Turn`、`Item`、`TraceEvent` 和 `ArtifactRef` 是 app-server 的 durable protocol object；`ThreadIdParams`、`ThreadForkParams`、`TurnIdParams`、`TraceListParams`、`TraceShowParams`、`TraceTailParams` 和对应 result object 是 CLI agent protocol 的 request/response schema；`ToolSpec`、`ToolCallEnvelope`、`ToolResult`、`ToolObservation`、`PermissionProfile`、`ApprovalRequest`、`ApprovalDecision`、`SandboxPolicy`、`CommandRequest`、`CommandResult`、`ModelTurnRequest` 和 `ModelTurnResponse` 是第一阶段先迁移的 schema object。`SessionStore` 是 SQLite-backed persistence boundary，`SessionStoreDescriptor` 是可序列化的 store schema descriptor。`AppServer.handle_json()` 是 stdio JSONL transport 的入口。`AppServerClient` 是 `sg` 内部 stdio JSON-RPC client，不暴露 store 或 agent internals。

## 真实运行时调用链

`sg run` / `sg chat` / `sg continue` / `sg threads` / `sg trace` / `sg approvals` -> `AppServerClient::spawn()` 启动 `singularity_app_server` stdio process -> `initialize` request -> `initialized` notification -> 对应 thread/turn/trace/approval request -> `AppServer.handle_json()` -> `AppServer.handle()` -> `SessionStore.create_thread_with_trace()` / `create_turn_with_input_and_trace()` / `create_approval_with_trace()` / `record_approval_decision_with_trace()` 或 read/list/update helper -> `JsonRpcMessage::response()` 或 `AppEvent.to_notification()` 输出 JSONL -> `sg` 渲染 thread、turn、item、trace 或 approval 行。`turn/start` 只调用 `AgentLoopBridge::not_migrated()`，不会进入 Python `AgentLoop.run()`，也不会把 turn 伪装成已完成真实 agent loop。

## 真实任务中的对象流

以 `sg run "goal"` 为例：CLI 启动 app-server 子进程，发送 `JsonRpcMessage::request(Method::Initialize, ...)`；`AppServer.handle_json()` 反序列化为 `JsonRpcMessage`，`InitializeParams` 校验 `clientInfo` 后返回 `InitializeResult`。CLI 再发送 `initialized` notification，连接进入 ready 状态。随后 `thread/start` 生成对象 `Thread` 并由 `SessionStore.create_thread_with_trace()` 在同一个 SQLite transaction 中写入 `threads` 和 `trace_events`。`turn/start` 生成对象 `Turn` 和 user message `Item`，通过 `SessionStore.create_turn_with_input_and_trace()` 在一个 transaction 中写入 `turns` / `items` / `trace_events`，并输出 `turn/started -> item/started -> item/agentMessage/delta -> item/completed` 通知。`sg continue <thread-id> "instruction"` 先通过 `thread/read` 验证 thread 存在，再发送新的 `turn/start`。approval 流由 `approval/request` 写 pending row 和 trace，`approval/decision` 只能消费 pending approval 一次，并在同一个 transaction 中写 `approvals` decision fields、`approval_decisions` ledger row 和 trace；重复 decision 返回 `Pending approval not found`。

## 真实对象完整结构

### JSON-RPC 与 thread/turn/item

枚举值包括 `Method::Initialize = "initialize"`、`Method::ThreadList = "thread/list"`、`Method::ThreadRead = "thread/read"`、`Method::ThreadStart = "thread/start"`、`Method::ThreadResume = "thread/resume"`、`Method::ThreadFork = "thread/fork"`、`Method::ThreadArchive = "thread/archive"`、`Method::ThreadDelete = "thread/delete"`、`Method::TurnStart = "turn/start"`、`Method::TurnInterrupt = "turn/interrupt"`、`Method::TurnStatus = "turn/status"`、`Method::ApprovalList = "approval/list"`、`Method::TraceTail = "trace/tail"`、`Method::ServerShutdown = "server/shutdown"`、`ThreadStatus::Active = "active"`、`TurnStatus::Running = "running"`、`TurnStatus::Interrupted = "interrupted"`、`ItemKind::CommandExecution = "commandExecution"`、`ItemStatus::Completed = "completed"`。

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

`SessionStore` 持有真实 SQLite connection，不参与 JSON 序列化；`SessionStoreDescriptor` 是 schema object，用于记录 store backend、path 和 schema version。

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

pub struct CommandRequest {
    pub command_id: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub purpose: CommandPurpose,
    pub timeout_seconds: u64,
    pub network: SandboxNetworkPolicy,
    pub filesystem: SandboxFilesystemPolicy,
}

pub struct SandboxBackendDescriptor {
    pub backend: String,
    pub enforcement: SandboxBackendEnforcement,
    pub capabilities: SandboxCapabilities,
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
    pub tools: Vec<Value>,
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
    pub tool_calls: Vec<Value>,
    pub usage: ModelUsage,
    pub finish_reason: Option<String>,
    pub validation: Option<Value>,
    pub error: Option<Value>,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub latency_ms: Option<u64>,
    pub trace_event_ids: Vec<String>,
    pub raw_response_ref: Option<String>,
    pub metadata: Value,
}
```

## 谁生成这些对象

`JsonRpcMessage::request()` 和 `JsonRpcMessage::notification()` 生成 wire message；`SessionStore.create_thread()` / `create_thread_with_trace()` 生成 `Thread`；`SessionStore.create_turn()` / `create_turn_with_input_and_trace()` 生成 `Turn`；`SessionStore.append_item()` 生成 `Item`；`TraceEvent::new()` 生成 `TraceEvent`；`SessionStore.register_artifact_ref()` 生成 `ArtifactRef`；`ApprovalRequest::new()` 和 `ApprovalDecision::new()` 生成 approval object；`ToolSpec::new()`、`ToolCallEnvelope::new()`、`ToolResult::success()`、`ToolObservation::summary()`、`SandboxPolicy::isolated_verification()`、`CommandRequest::project_verification()`、`CommandResult::completed()`、`ModelTurnRequest::new()` 和 `ModelTurnResponse::completed()` 生成各自 schema object。

## 谁消费这些对象

`AppServer.handle()` 消费 `JsonRpcMessage` 并分派到 initialize、thread、turn、approval、trace handler；`SessionStore.create_thread_with_trace()`、`create_turn_with_input_and_trace()`、`create_approval_with_trace()`、`record_approval_decision_with_trace()`、`append_trace()` 和 `register_artifact_ref()` 消费 protocol object 写 SQLite；`ToolObservation.to_model_payload()` 消费 tool observation 并生成模型可见安全 payload；`AppEvent.to_notification()` 消费 event 并输出 JSON-RPC notification。`sg` 只消费 `singularity_protocol` 和 `singularity_core`，不消费 `singularity_agent`、`singularity_model`、`singularity_tools` 或 `singularity_store`。

## 是否落盘

`SessionStore.open()` 初始化 SQLite 文件并确保 `schema_migrations` 记录 `0001_initial_session_store` 和 `0002_durable_ledger`；`threads`、`turns`、`items`、`trace_events`、`artifact_refs`、`approvals` 和 `approval_decisions` 表是真实落盘点。thread read/list/resume/archive/delete 和 turn status/interrupt 读取或更新这些现有表；`approval/request` 写 pending approval，`approval/list` 读取未决 approval，`approval/decision` 更新同一 row 的 decision fields 并写 `approval_decisions` ledger；trace/list 支持 `limit` / `offset` 分页，trace/show 和 trace/tail 都从 SQLite `trace_events` 查询真实 events。`target/` 是 Rust build output，被 `.gitignore` 排除。

## 是否进入 trace / audit

`thread/start`、`turn/start`、`approval/request` 和 `approval/decision` 都写 `TraceEvent`，且由 store transaction 把对应业务 row 与 trace 一起提交或回滚。`ArtifactRef` 的 `summary` / `metadata` / `uri` 会对 secret-like marker 做本地 redaction 后落盘。Phase 1 Rust 层还没有 policy audit writer；完整 policy audit 仍由 Python `src/singularity/policy/audit.py` 保持 oracle。`ToolObservation.to_model_payload()` 明确不输出 `policy_decision_id`、`approval_grant_id`、raw arguments 或 internal metadata。

## 失败路径

连接未完成 `initialize` 或未收到 `initialized` notification 前，业务 request 返回 `Not initialized`。同一连接重复 `initialize` 返回 `Already initialized`。未知 thread / turn / trace run / trace event 分别返回 `Thread not found`、`Turn not found`、`Trace run not found`、`Trace event not found`。approval request 重复返回 `Approval already exists`；approval decision 找不到 pending request 或重复消费时返回 `Pending approval not found`。SQLite 和 JSON 解析错误作为 app-server internal error 返回。

## 当前结构问题

Phase 1 没有迁移模型 provider、Windows sandbox backend、evaluation runner 或 Python AgentLoop；`AgentLoopBridge::not_migrated()` 只是 host-facing status，不代表 agent 已完成。当前 `sg` 是最小 JSON-RPC client：可以启动 app-server 子进程并完成 run/continue/list/trace/approval 查询，但不管理长期后台 daemon 生命周期、PTY TUI 或交互式 approval prompt。Artifact ref 目前只持久化引用和 redacted metadata，不负责 artifact bytes 管理。WebSocket 和 Unix socket 只是后续 transport 方向，当前只实现 stdio JSONL。

## 维护规则

新增 client 必须只依赖 protocol/core 层，不得直接耦合 agent/model/tools/store。新增 app-server 方法必须先在 `crates/protocol` 定义 request/response/event schema，再由 `crates/app-server` routing 和 `crates/store` persistence 接入。任何改变 thread/turn/item/approval/trace wire format 的改动都必须更新 Rust tests、Python oracle fixture、本文档和 `docs/singularity.md` 的长期架构说明。

M0 迁移 guardrail 由 `scripts/verify_rust_migration_boundaries.py` 自动检查并进入 CI。该脚本阻断 CLI 直接依赖 agent/model/tools/store、未登记的新 crate 依赖、Python core runtime 非 allowlist 改动、Python RuntimeHost 类过渡实现、desktop/Web 抢跑文件、`turn/start` 伪装 Rust AgentLoop 已迁移，以及 `ToolObservation.to_model_payload()` 泄漏 raw arguments、approval/policy 内部 id、internal metadata 或 secret-like 文本。
