# Rust AgentLoop 主循环模块数据流

模块数据流文档 ID: agent-loop

源码证据路径:
- crates/agent/src/lib.rs
- crates/app-server/src/lib.rs

关键符号:
- AgentStatus
- AgentRunStatus
- AgentLoopCapability
- AgentLoopPlan
- AgentLoopStep
- AgentLoopInput
- ApprovalGrant
- AgentLoopResult
- PendingToolCall
- AgentLoop
- ToolRepair
- AppServer

字段清单:
- AgentLoopResult: status, completed, final_answer, model_turns, tool_calls, approval_count, approval_requests, pending_tool_calls, tool_results, tool_repairs, error
- AgentLoopInput: thread_id, turn_id, run_id, session_id, task_id, model_preferences, input, interrupted, max_turns, approval_grants
- AgentRunStatus: status, completed, final_answer, run_id, session_id, task_id, model_turns, tool_calls, approval_count, audit_events, trace_path, error
- AgentLoopCapability: available, status, reason, blockers
- ApprovalGrant: request_id, tool_name, resources, outcome
- PendingToolCall: request_id, tool_call_id, tool_name, raw_arguments, resources

## 这一层解决什么问题

Rust AgentLoop 层负责把一个已创建的 turn 转换成真实模型请求、工具调用、approval request、tool repair、最终答案和安全状态摘要。它是当前 public runtime 的主循环；Python AgentLoop 只保留为 oracle/parity/dev-only 参考，不是普通 CLI、app-server 或 evaluation 的执行后端。

## 当前源码位置

- crates/agent/src/lib.rs
- crates/app-server/src/lib.rs

## 关键类、函数、字段

关键符号和字段清单按 Rust public runtime 的源码对象列出。`AgentLoop` 是执行对象，`AgentLoopInput` 是 turn/run 输入边界，`AgentLoopResult` 是主循环输出边界，`AgentRunStatus` 是 app-server-facing 终态投影，`AgentLoopCapability` 是 turn/eval 入口的 fail-closed capability gate。

## 真实运行时调用链

普通 turn：`sg run` / `sg chat` / `sg continue` -> Rust CLI JSON-RPC `turn/start` -> `AppServer::turn_start()` -> `native_capability_ready()` -> `SessionStore.create_turn_with_input_and_trace()` -> `AppServer::run_native_agent_loop()` -> `OpenAiProvider::from_env()` -> `AgentLoop::new()` -> `AgentLoop::run()` -> `AgentLoopResult::to_run_status()` -> `AppServer::append_native_trace()` -> `SessionStore.update_turn_state()` / `SessionStore.append_item()`。

Evaluation：`sg eval run` -> Rust CLI JSON-RPC `eval/run` -> app-server native eval runner -> per task `AgentLoop::new()` -> `AgentLoop::run()` -> smoke command / public verification / hidden verification -> result/report artifact 写入 `work/evaluations/<run-id>/`。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例：`AppServer.turn_start()` -> `AgentLoopCapability.current()` -> `SessionStore.create_turn_with_input_and_trace()` -> `native_loop_input()` -> `AgentLoop.run()` -> `OpenAiProvider.complete()` -> `ToolBroker.execute()` -> `AgentLoopResult.to_run_status()` -> `AppServer.append_native_trace()`。`AppServer.turn_start()` 先读取 thread 并校验 Windows restricted-token command sandbox capability；capability 不满足时直接返回 JSON-RPC error，不生成 turn、trace 或 provider request。校验通过后，`SessionStore.create_turn_with_input_and_trace()` 生成对象并写入 sqlite store：turn、用户输入 item 和 app-server trace。随后 `native_loop_input()` 生成 `AgentLoopInput`，`AgentLoop.run()` 生成 `ModelTurnRequest`，通过 `OpenAiProvider` 调用真实 provider，解析 assistant message / tool calls，再由 `ToolBroker`、`PolicyEngine` 和 `WorkspaceTools` 执行被允许的 workspace tool 或 command tool。工具结果生成 `ToolResult` 并回送给后续模型 turn；需要人工或 policy approval 时生成 `ApprovalRequest` 与 `PendingToolCall`，由 app-server 持久化到 approval / pending_tool_calls 表。完成时 `AgentLoopResult.to_run_status()` 生成 `AgentRunStatus`，app-server 写入 `agent_loop` trace 摘要并更新 turn 状态；最终答案只在 `completed` 且非空时写为 agent message item，失败、blocked、cancelled 不伪造 assistant delta。

## 真实对象完整结构

### AgentLoopResult（主循环结果）

Rust AgentLoop 返回给 app-server 的完整结果。**边界**：不直接落盘；由 `AgentLoopResult::to_run_status()` 投影为 `AgentRunStatus`，再由 app-server 写入 turn status、trace summary、approval ledger 或 agent message item。

```rust
pub struct AgentLoopResult {
    pub status: AgentStatus,
    pub completed: bool,
    pub final_answer: Option<String>,
    pub model_turns: u32,
    pub tool_calls: u32,
    pub approval_count: u32,
    pub approval_requests: Vec<ApprovalRequest>,
    pub pending_tool_calls: Vec<PendingToolCall>,
    pub tool_results: Vec<ToolResult>,
    pub tool_repairs: Vec<ToolRepair>,
    pub error: Option<String>,
}
```

### AgentStatus（主循环状态）

`AgentStatus` 的 wire value 使用 snake_case；枚举值包括 `not_migrated`、`running`、`cancel_requested`、`completed`、`blocked`、`cancelled`、`failed`。public turn 状态由 app-server 映射到 `TurnStatus`，不会把 Python backend selector 暴露给 protocol。

```rust
pub enum AgentStatus {
    NotMigrated = "not_migrated",
    Running = "running",
    CancelRequested = "cancel_requested",
    Completed = "completed",
    Blocked = "blocked",
    Cancelled = "cancelled",
    Failed = "failed",
}
```

### AgentLoopInput（主循环输入）

`AgentLoopInput` 由 app-server 从 Rust thread/turn/user input 生成。**边界**：只在 native loop 内存中使用；不把原始 provider request、raw prompt 或 secret 写入 trace。

```rust
pub struct AgentLoopInput {
    pub thread_id: String,
    pub turn_id: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: String,
    pub model_preferences: ModelPreferences,
    pub input: Vec<AgentContextItem>,
    pub interrupted: bool,
    pub max_turns: u32,
    pub approval_grants: Vec<ApprovalGrant>,
}
```

### AgentRunStatus（app-server 终态投影）

`AgentRunStatus` 是 app-server 用来更新 SQLite turn 状态和 trace summary 的安全投影。

```rust
pub struct AgentRunStatus {
    pub status: AgentStatus,
    pub completed: bool,
    pub final_answer: Option<String>,
    pub run_id: Option<String>,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub model_turns: u32,
    pub tool_calls: u32,
    pub approval_count: u32,
    pub audit_events: Vec<Value>,
    pub trace_path: Option<String>,
    pub error: Option<String>,
}
```

### ApprovalGrant 与 PendingToolCall

approval resume 只消费与 pending request 匹配的 Rust-native pending tool call；`thread_id`、`turn_id` 和 `tool_call_id` 由 store 边界强校验。

```rust
pub struct ApprovalGrant {
    pub request_id: String,
    pub tool_name: String,
    pub resources: Vec<String>,
    pub outcome: ApprovalOutcome,
}

pub struct PendingToolCall {
    pub request_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub raw_arguments: String,
    pub resources: Vec<String>,
}
```

## 谁生成这些对象

- `native_loop_input()` 生成 `AgentLoopInput`；`AgentLoopInput.new()` 设置 thread/turn/run/session/task identity、模型偏好、最大 turn 数和初始用户输入。
- `AgentLoopCapability.current()` 生成 capability gate；Windows 先运行 restricted-token sandbox probe，非 Windows返回 strict command sandbox unsupported blocker。
- `AgentLoop.run()` 生成 `AgentLoopResult`、`ToolResult`、`ToolRepair`、`ApprovalRequest` 和 `PendingToolCall`。
- `AgentLoopResult.to_run_status()` 生成 `AgentRunStatus`，app-server 再把它映射到 durable turn status 和 trace summary。

## 谁消费这些对象

- `AppServer.turn_start()` 和 native eval runner 消费 `AgentLoopCapability`；capability 不满足时 fail closed。
- `AgentLoop.run()` 消费 `AgentLoopInput`、`OpenAiProvider`、`ToolBroker`、`PolicyEngine` 和 `WorkspaceTools`。
- `AppServer.run_native_agent_loop_with_provider()` 消费 `AgentLoopResult`，再写 approval request、pending tool call、trace、turn state 和 agent message item。
- `AppServer.resume_native_agent_loop_after_gate()` 消费已经持久化的 approval / pending tool call，再用 `ApprovalGrant` 继续同一 Rust AgentLoop 路径。

## 是否落盘

- `AgentLoopInput` 和 `AgentLoopResult` 本体不作为独立文件落盘。
- app-server 在 SQLite 中落盘 thread、turn、item、approval、approval decision、pending tool call、trace event 和 artifact ref。
- eval runner 把 native result/report artifact 写入 `work/evaluations/<run-id>/result.json` 和 `report.json`，其中保留 `agent_completed`、`tests_passed`、`evaluation_passed`、blocker 和 verification 结果的分离字段。

## 是否进入 trace / audit

- app-server 写 `component="agent_loop"` 的 trace summary，只包含 status、completed、run_id、session_id、task_id、model_turns、tool_calls、approval_count、audit_events 和已脱敏 error。
- `audit_events` 来自 tool result 的安全审计摘要，记录 sandbox mode、approval policy、command provenance、strict backend 名称和 scope digest；不包含 raw prompt、raw provider response、raw tool arguments、secret、token 或 `.env` 值。
- approval request/decision trace 使用 `thread_id` / `turn_id` 定位，tool-call approval 还绑定 `tool_call_id`。

## 失败路径

- `AgentLoopCapability` 不满足时，app-server 在 turn 创建前 fail closed。
- provider 配置缺失、认证失败、网络失败或模型配置错误由 `OpenAiProvider` 归类为 provider/model error，最终形成 failed/blocked status，不打印 secret。
- unknown tool、policy deny、strict sandbox backend unavailable、command nonzero、workspace patch conflict 或 max turns 都保持 fail closed；不会 fallback 到 Python、local process、no_sandbox 或 relaxed executor。
- approval resume 只有 pending request 的 `thread_id`、`turn_id`、`tool_call_id` 与当前 decision 匹配时才继续，否则写入安全失败状态。

## 当前结构问题

Rust AgentLoop 已经承担 public runtime 的普通 turn 和 eval runner 主路径，但仍是最小 native loop：它没有长期后台 daemon、PTY/TUI、provider registry 或完整 planner/context Rust 重写。Python oracle/parity/dev-only 仍可作为 fixture 和 schema 对照，但不得通过旧 `singularity.cli` 或 `agent_host` 路径恢复为 public runtime。

## 维护规则

修改 `AgentLoopInput`、`AgentLoopResult`、`AgentRunStatus`、approval/pending tool call、native eval runner、turn status 映射、tool result payload、provider error taxonomy 或 capability gate 时，必须同步更新本文件、`docs/singularity.md` 和 `docs/architecture/modules/rust-app-server-protocol.md` 的对应段落，并运行 `python scripts/verify_runtime_docs.py`、`python scripts/verify_rust_migration_boundaries.py` 和相关 Cargo 测试。
