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
- ServerCapabilitiesResult
- TransportCapability
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
- ApprovalListResult
- ApprovalCenterResult
- EventSubscribeParams
- EventSubscribeResult
- ArtifactFetchParams
- ArtifactFetchResult
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
- ToolCallRequest
- ToolOutput
- ToolResult
- SandboxPolicy
- SandboxBackend
- SandboxBackendDescriptor
- CommandRequest
- CommandResult
- PatchChange
- PatchResult
- ModelToolSchema
- ModelToolCall
- ModelCapabilities
- ModelProviderConfig
- ModelProviderStatus
- ModelValidationResult
- ModelRetryDecision
- ModelError
- ProviderStreamEvent
- ModelTurnRequest
- ModelTurnResponse
- Provider
- OpenAiProviderConfig
- OpenAiProvider
- ProviderError
- AgentRunStatus
- AgentLoopCapability
- AgentLoopStep
- AgentLoopPlan
- AgentLoopInput
- AgentLoopResult
- AgentContextItem
- CompletionGateInput
- PlannerState
- ContextBundle
- ContextSummaryEnvelope
- ToolRepair
- FinalReportMapping
- PythonSidecarClient
- PythonSidecarConfig
- AppServer
- AppServer.handle_json
- AppServer.handle
- AppServer.event_notification
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
- ServerCapabilitiesResult: transports
- TransportCapability: transport, available, auth_token_required
- ThreadStartParams: model, cwd
- ThreadIdParams: thread_id
- ThreadForkParams: thread_id, model, cwd
- Thread: thread_id, model, cwd, status
- ThreadListResult: threads
- ThreadResult: thread
- ThreadForkResult: source_thread_id, thread
- ThreadDeleteResult: thread_id, deleted
- TurnStartParams: thread_id, input, agent_host
- TurnIdParams: turn_id
- Turn: turn_id, thread_id, status, agent_loop_status
- Item: item_id, turn_id, kind, payload, status
- TurnResult: turn
- TurnInterruptResult: turn_id, status, agent_loop_status
- ApprovalListResult: approvals
- ApprovalCenterResult: pending_approvals, decisions
- EventSubscribeParams: event_types
- EventSubscribeResult: subscription_id, event_types
- ArtifactFetchParams: artifact_id
- ArtifactFetchResult: artifact
- TraceListParams: run_id, limit, offset
- TraceShowParams: event_id
- TraceTailParams: run_id, limit, offset
- TraceListResult: events
- ArtifactRef: artifact_id, run_id, item_id, kind, uri, content_digest, summary, metadata, redacted
- TraceEvent: event_id, event_type, run_id, session_id, task_id, phase_id, action_id, parent_event_id, timestamp, monotonic_ms, component, severity, summary, payload, artifact_refs, policy_decision_id, approval_grant_id, sandbox_id, command_id, transaction_id, verification_id, span_id, redaction_applied, payload_hash
- AppEvent: method, params
- PermissionProfile: profile, workspace_roots, additional_writable_directories, network_access, approval_policy, protected_paths_enforced
- PermissionRequest: tool_name, operation, resource, resource_sensitive
- PermissionRule: rule_id, scope, outcome, operation, resource
- PermissionDecision: outcome, reason, rule_id, scope
- PreToolUseHook: hook_id, decision
- PolicyEngine: profile, rules, hooks
- ApprovalRequest: request_id, session_id, task_id, action, resources, reason
- ApprovalDecision: request_id, decision_id, outcome, reason
- SessionStore: connection, descriptor
- SessionStoreDescriptor: backend, path, schema_version
- ToolSpec: name, version, description, input_schema, permission_level, risk_tags
- ToolRegistry: tools
- ToolBroker: registry
- ToolCallRequest: protocol_version, run_id, session_id, task_id, tool_call_id, tool_name, raw_arguments
- ToolOutput: ok, content, error_code, truncated, metadata
- ToolResult: tool_call_id, tool_name, ok, status, view, preview, digest, artifact_ref, error_code, artifact_refs, result_id, approval_request_id, truncated, redacted, policy_decision_id, approval_grant_id, audit_metadata

`ToolResult` 是 agent loop 使用的工具调用结果；raw executor payload 是 `ToolOutput`。`ToolResult` 的 `digest`、`artifact_ref`、`artifact_refs` 和 `result_id` 承接 `ToolOutput.content` / `metadata` 中的引用字段，避免把大正文或 raw executor payload 直接发送给模型。`policy_decision_id`、`approval_grant_id` 和 `audit_metadata` 是 Rust 内部审计字段，使用 `#[serde(skip)]`，不得进入 wire payload、tool message 或 model request；command 工具的脱敏审计投影会汇总到 `AgentRunStatus.audit_events`，再进入 app-server native trace。
- SandboxPolicy: profile, filesystem, network, resources
- SandboxBackendDescriptor: backend, enforcement, capabilities
- CommandRequest: command_id, argv, cwd, purpose, timeout_seconds, network, filesystem
- CommandResult: command_id, execution_status, semantic_status, exit_code, duration_ms, timed_out, stdout_preview, stderr_preview, output_truncated, redacted, changed_files
- PatchChange: path, expected, replacement
- PatchResult: applied, changed_files, rolled_back, error
- ModelToolSchema: name, description, parameters_schema, capability_tags, risk_tags, metadata
- ModelToolCall: tool_call_id, tool_name, arguments, raw_arguments, parse_status, validation_errors, provider_metadata
- ModelCapabilities: supports_tools, supports_parallel_tool_calls, supports_streaming, supports_json_mode, supports_structured_outputs, supports_system_message, supports_developer_message, max_context_tokens, max_output_tokens, input_modalities, output_modalities
- ModelProviderConfig: provider_name, model_name, base_url_present, api_key_present
- ModelProviderStatus: ready, provider_name, model_name, api_key_status, base_url_status, blocker
- ModelValidationResult: valid, errors, warnings, repaired, repair_message
- ModelRetryDecision: retry, next_attempt, reason
- ModelError: kind, message, retryable, provider_name, model_name, raw_error_ref, metadata
- ProviderStreamEvent: event_type, text_delta, tool_call_id, tool_name, arguments_delta, usage_delta, error, metadata
- ModelTurnRequest: request_id, run_id, session_id, task_id, phase_id, action_id, purpose, messages, tools, tool_choice, model_preferences, budget, context_metadata, policy_metadata, trace_metadata
- ModelTurnResponse: request_id, response_id, status, assistant_message, tool_calls, usage, finish_reason, validation, error, provider_name, model_name, latency_ms, trace_event_ids, raw_response_ref, metadata
- OpenAiProviderConfig: provider_name, model_name, base_url, api_key
- OpenAiProvider: config, client
- ProviderError: message, error
- AgentRunStatus: status, completed, final_answer, run_id, session_id, task_id, model_turns, tool_calls, approval_count, events, audit_events, trace_path, error
- AgentLoopCapability: available, status, reason, blockers
- AgentLoopPlan: steps, blockers
- AgentLoopInput: thread_id, turn_id, run_id, session_id, task_id, model_preferences, input, interrupted, max_turns, approval_grants
- ApprovalGrant: request_id, tool_name, resources, outcome
- AgentLoopResult: status, completed, final_answer, model_turns, tool_calls, approval_count, approval_requests, tool_results, tool_repairs, error
- AgentContextItem: item_id, role, content, priority, token_count, public, evaluator_only, digest
- CompletionGateInput: verification_passed, unresolved_failures, interrupted
- PlannerState: task_id, current_phase, status, current_plan, completion_criteria, open_actions, blocked_actions, risk_escalations, evidence_refs
- ContextBundle: bundle_id, run_id, task_id, phase_id, model, provider, messages, included_item_ids, excluded_item_ids, budget, compression_snapshot_id, retrieval_query, render_policy, created_at, bundle_digest, metadata
- ContextSummaryEnvelope: version, summary_id, summary_payload, source_item_ids, cache_attribution, previous_summary_digest, summary_digest, rendered_summary, created_at, metadata
- ToolRepair: repair_id, run_id, session_id, task_id, phase_id, failed_tool_call_id, failure_kind, next_action, failed_result, recovery_report, repair_contract, created_at, metadata
- FinalReportMapping: mapping_id, run_id, session_id, task_id, phase_id, agent_loop_status, run_status, final_report_status, completion_status, final_answer, final_report, completion_assessment, contract_satisfaction, created_at, metadata
- PythonSidecarConfig: python_bin, module, project_root, python_path, env
- AppServer: store, initialized, initialized_acknowledged, python_sidecar, event_filter, sidecar_runs, shutdown_requested

## 这一层解决什么问题

Rust App Server Protocol 层建立第一阶段迁移的硬边界：客户端只通过 JSON-RPC 请求和通知进入 app-server，app-server 只持久化 thread、turn、item、trace 和 pending approval，不直接复用 Python `AgentLoop` 内部对象。`sg` CLI 是第一个 client；未来 desktop 也接同一个 protocol，不再设计第二套 core。显式 Python sidecar 只通过 JSON-RPC 子进程边界调用现有 Python AgentLoop，并把安全状态摘要翻译回 Rust protocol。

### Rust CLI-first ownership map

| Runtime concern | Current Python owner | Rust owner after this stage | Protocol object or method | Store/trace side effect | Parity expectation | Intentional divergence |
| --- | --- | --- | --- | --- | --- | --- |
| User command parsing | `singularity.cli:main` legacy/oracle | `crates/cli::Command` for default Rust path | CLI args -> `JsonRpcMessage::request` | None before app-server request | Rust CLI sends the same user goal/instruction text | Python CLI remains oracle entrypoint |
| Thread/session identity | `KernelBootstrap.prepare_launch` for oracle | `AppServer::thread_start` | `thread/start`, `ThreadStartParams`, `ThreadStartResult` | `SessionStore.create_thread_with_trace()` | Durable thread id is available before a turn | Python `run_id/session_id/task_id` remain sidecar internals |
| Turn creation | `AgentKernel.run_task` launch path for oracle | `AppServer::turn_start` | `turn/start`, `TurnStartParams`, `TurnStartResult` | `SessionStore.create_turn_with_input_and_trace()` | One Rust turn per submitted instruction | Explicit Python oracle can return `not_migrated` only when sidecar is disabled |
| AgentLoop execution | `AgentLoop.run` for oracle | `AppServer::run_native_agent_loop` is default production path | native `turn/start`; `AgentLoopInput`; `AgentLoopResult`; `AgentRunStatus` | Native terminal status updates turn and appends one `component="agent_loop"` trace event | Native final-answer, safe workspace-tool admission, typed tool repair retry, command resource approval, approval grant/resume and eval flow are tested in Rust | Python sidecar remains explicit oracle/fixture only |
| Tool protocol execution | `ToolProtocolEngine` for oracle | Rust `ToolBroker` for registered workspace tools and command tool | Native tool calls use `ToolCallRequest` and `ToolResult`; command uses `CommandRequest` / `CommandResult` | Native tool results stay in `AgentLoopResult`; native approval requests are persisted through the app-server approval ledger | Denied/unknown/ask tools do not execute | No local-process fallback or Codex CLI shellout |
| Provider/model call | Python `ModelRunner` for sidecar/oracle | `OpenAiProvider::complete()` builds non-stream OpenAI-compatible `/chat/completions` requests | `ModelTurnRequest` -> `ModelTurnResponse` | Only redacted `ModelError`, `raw_response_ref` or `raw_error_ref`; no raw provider body in Rust trace | Endpoint and tool-call argument validation match Python contract where covered | Rust provider is default native path; provider config errors fail closed |
| Approval request | Python approval gate for oracle | Rust AgentLoop approval request plus app-server ledger | `approval/request` | `create_approval_with_trace()` | Pending request is durable | Does not inject Python approval grant |
| Approval decision | Python approval gate consumes grants in oracle | `AppServer::approval_decision` | `approval/decision` | `record_approval_decision()` writes ledger + trace | Decision is single-use and auditable | Decision resumes one stored pending native tool call once |
| Sandbox/command execution | Python `CommandExecutor` / sandbox for oracle | Rust `SandboxBackend` owns Codex-style restricted-token command execution | `CommandRequest` / `CommandResult` behind `ToolBroker` and policy | Bounded command output summaries | Read-only, workspace-write, danger-full-access, timeout, capture, sensitive deny, path admission and unsupported states are Rust-tested | No system-account/firewall setup, local-process fallback, or `codex sandbox` shellout |
| Trace event write | Python `TraceRecorder` for oracle | `SessionStore.append_trace` | `trace/list`, `trace/show`, `trace/tail` | Thread/turn/approval/native/sidecar summaries persisted | CLI can locate event ids and summaries | Raw payload not shown by CLI renderer |
| Artifact reference | Python artifact store for oracle | `AppServer::artifact_fetch` | `artifact/fetch` | Reads `artifact_refs` | Return redacted reference metadata | Does not serve artifact bytes |
| Final answer/status | Python `AgentLoopResult` / `FinalReport` for oracle | `AgentRunStatus` and `AppServer::update_turn_from_run_status` | `turn/start`, `turn/status`, typed item delta | Turn status and `agent_loop_status` update; completed final answer is stored as an `AgentMessage` item and emitted once as redacted typed delta | Status maps are explicit and testable | Failed/blocked/cancelled paths have no fake assistant delta |
| Evaluation proof | Python `EvaluationRunner` for oracle | Rust native `eval/run` runner | Rust `sg eval run` -> `eval/run` | Rust native eval writes `evaluation.result/v1` result/report artifact | `agent_completed`, `tests_passed`, `evaluation_passed` and fallback diagnostics stay separate | Python eval remains oracle only |

### Rust sidecar status and error map

| Source condition | AgentRunStatus | TurnStatus | CLI output | Trace summary |
| --- | --- | --- | --- | --- |
| Native completed final answer | `completed` | `completed` | `turn <id> completed agent_loop_status=completed`; redacted `assistant <final_answer>` | `component/status/run_id/session_id/task_id/trace_path` safe keys |
| Native empty answer / denied or unknown tool / max turns | `failed` or `blocked` | `failed` or `blocked` | Non-zero CLI exit with safe status | `agent_loop` safe status/error summary |
| Native approval required | `blocked` | `blocked` | pending approval is listed through protocol | approval request trace only |
| Python sidecar `completed` | `completed` | `completed` | `turn <id> completed agent_loop_status=completed`; redacted `assistant <final_answer>` | `component/status/run_id/session_id/task_id/trace_path` safe keys |
| Python sidecar `blocked` | `blocked` | `blocked` | `turn <id> blocked agent_loop_status=blocked` | Same safe keys |
| Python sidecar `failed` / `max_turns_exceeded` | `failed` | `failed` | Non-zero CLI exit with failed status; no assistant delta | Same safe keys; no raw prompt/provider/tool payload |
| Python sidecar `cancel_requested` | `cancel_requested` | `interrupted` | `turn <id> interrupted agent_loop_status=cancel_requested` | app-server lifecycle event with safe IDs |
| Python sidecar `cancelled` / `canceled` | `cancelled` | `interrupted` | `turn <id> interrupted agent_loop_status=cancelled` | Same safe keys |
| Missing Rust thread | No sidecar/native call | No new turn | JSON-RPC `Thread not found` | No sidecar/native trace |
| Sidecar process/config/runtime error | `failed` | `failed` | Non-zero CLI exit and redacted summary | Safe `python_sidecar` failure summary |
| Capability gate drift | JSON-RPC error | no turn created | `Native AgentLoop is not production-ready` | No provider/tool/native trace |
| Approval required | Pending approval row | Existing turn status | `approval <request_id> <action>` then `approval <request_id> <outcome>` | Request/decision trace via store |

Intentional divergence table: Rust owns protocol routing, store writes, safe payloads, provider request creation, non-empty final-answer completion, safe workspace-tool tool_results, typed tool repair retry, and agent-crate approval grants. Python still owns full production AgentLoop semantics for default execution until approval resume, strict command sandbox, and Rust evaluation runner blockers are removed.

### Turn lifecycle and cancel boundary

The turn lifecycle is the app-server status machine for one submitted user turn:

```text
accepted -> running -> completed
accepted -> running -> failed
accepted -> running -> interrupted
accepted -> failed
running -> interrupted_requested -> interrupted
```

| Status | Rust owner | Python owner | SQLite fields | trace event | CLI rendering | sidecar process status | retry/resume implication |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `accepted` | `AppServer::turn_start` after protocol validation | None | current protocol creates `turns.status="running"`; native starts with `agent_loop_status="running"`, explicit Python oracle starts with `agent_loop_status="not_migrated"` until sidecar status is observed; accepted is a lifecycle edge, not a persisted TurnStatus | app-server lifecycle event | running turn line only | not started or starting for explicit Python oracle only | retry only if no active run exists |
| `running` | `AppServer`, `SessionStore`, active run record | Python `AgentHost` / `AgentLoop` after sidecar start | `turns.status="running"`, `agent_loop_status`, active run `turn_id/thread_id/run_id/session_id/task_id/status` | app-server lifecycle event and safe `python_sidecar` trace event | `turn <id> running` | active | do not start a duplicate run; resume uses Python `session_id` |
| `completed` | `AppServer::update_turn_from_run_status`, `SessionStore` | Python `AgentLoop` finalization | `turns.status="completed"`, `agent_loop_status="completed"`, active run cleared | safe completion trace event | completed line plus bounded assistant summary | exited | terminal; next work starts a new turn |
| `failed` | app-server status mapping and store | Python `AgentHost` / `AgentLoop` for sidecar failures | `turns.status="failed"`, `agent_loop_status="failed"` when reported, active run cleared | redacted failure trace event | non-zero failed line | failed or not started | retry creates a new turn or uses recovery |
| `interrupted_requested` | Rust app-server | Python receives cancel transport but does not own durable status | `turns.status="interrupted"`, `agent_loop_status="cancel_requested"`, active run retained | interrupt requested trace event | `turn <id> interrupted agent_loop_status=cancel_requested` | cancel requested | wait for terminal cleanup |
| `interrupted` | `AppServer`, `SessionStore` | Python AgentHost/AgentLoop cancel semantics | `turns.status="interrupted"`, `agent_loop_status="cancelled"` when reported, active run cleared | safe cancel result trace event | `turn <id> interrupted agent_loop_status=cancelled` | cancelled/exited | later work may resume from Python session recovery |

Cancel ownership map:

| Concern | Owner |
| --- | --- |
| turn/interrupt request owner | Rust app-server |
| sidecar cancel transport | `PythonSidecarClient::cancel(run_id)` |
| AgentLoop cancel semantics | Python AgentHost/AgentLoop |
| durable status owner | SessionStore |
| trace owner | app-server |
| CLI owner | protocol renderer |

Non-goals: no OS kill primary cancel, no direct CLI-to-Python cancel, no native Rust AgentLoop cancel, no local-process sandbox fallback.

Approved vocabulary:

| Concept | Required name |
| --- | --- |
| user submitted unit | turn |
| sidecar execution identity | run |
| Python recovery identity | session |
| lifecycle status | status |
| cancel action | cancel |
| interrupt protocol method | interrupt |
| stored lifecycle record | active run |
| state transition event | lifecycle event |
| sidecar process wrapper | sidecar |

Use short protocol names only: `cancel` for sidecar/AgentLoop cancel, `interrupt` for the JSON-RPC method and CLI command, `status` for public lifecycle output, `run` for an active sidecar run, and `session` only for Python resume identity.

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

`JsonRpcMessage` 是 wire envelope；`Thread`、`Turn`、`Item`、`TraceEvent` 和 `ArtifactRef` 是 app-server 的 durable protocol object；`ThreadIdParams`、`ThreadForkParams`、`TurnIdParams`、`TraceListParams`、`TraceShowParams`、`TraceTailParams`、`EventSubscribeParams`、`ArtifactFetchParams` 和对应 result object 是 CLI agent protocol 的 request/response schema；`ServerCapabilitiesResult` 当前只描述 stdio 可用和 WebSocket token transport 尚不可用，不启动 WebSocket server。`ToolSpec`、`ToolRegistry`、`ToolBroker`、`ToolCallRequest`、`ToolOutput`、`ToolResult`、`PermissionProfile`、`PermissionRequest`、`PermissionRule`、`PermissionDecision`、`PreToolUseHook`、`PolicyEngine`、`ApprovalRequest`、`ApprovalDecision`、`SandboxPolicy`、`SandboxBackendDescriptor`、`CommandRequest`、`CommandResult`、`PatchChange`、`PatchResult`、`ModelToolSchema`、`ModelToolCall`、`ModelCapabilities`、`ModelProviderConfig`、`ModelProviderStatus`、`ModelValidationResult`、`ModelRetryDecision`、`ModelError`、`ProviderStreamEvent`、`ModelTurnRequest` 和 `ModelTurnResponse` 是 Rust native runtime 的 schema object 与执行边界。`Provider`、`OpenAiProviderConfig`、`OpenAiProvider` 和 `ProviderError` 是 Rust model provider adapter 边界；当前实现 OpenAI-compatible non-stream chat completions，HTTP 错误映射到脱敏 `ModelError`。`SessionStore` 是 SQLite-backed persistence boundary，`SessionStoreDescriptor` 是可序列化的 store schema descriptor。`AgentRunStatus` 表示 Rust host 对 AgentLoop 状态的显式理解：app-server native 运行结果，或显式 Python sidecar oracle 返回 completed/blocked/cancelled/failed/not_migrated。`AgentLoopCapability` 表示当前 Rust native production capability：Windows 先执行 restricted-token sandbox probe，probe 成功为 `available=true/status=completed/blockers=[]`，probe 失败为 blocked 并返回 `strict_command_sandbox_probe_failed:*` blocker；非 Windows 为 `available=false/status=blocked/blockers=["strict_command_sandbox_unsupported_platform"]`。`AgentLoopInput`、`AgentLoopResult`、`AgentLoopPlan` 和 `AgentLoop` 是当前 native loop 的执行对象。`PlannerState`、`ContextBundle`、`ContextSummaryEnvelope`、`ToolRepair`、`FinalReportMapping`、`AgentContextItem` 和 `CompletionGateInput` 是 schema 与 helper contract，用于 Python oracle JSON roundtrip、context 安全过滤、planner/repair next action、completion gate 和 final status mapping。`PythonSidecarClient` 是 Rust host 到 Python explicit oracle sidecar 的 stdio JSON-RPC client。`AppServer.handle_json()` 是 stdio JSONL transport 的入口；binary stdio 错误由 `JsonRpcMessage::error()` 和 `serde_json` 输出合法 JSON-RPC error envelope。`AppServer.event_notification()` 只在当前连接 `event/subscribe` 设置了 event filter 后过滤后续 notification。`AppServerClient` 是 `sg` 内部 stdio JSON-RPC client，不暴露 store 或 agent internals。

## 真实运行时调用链

`sg run` / `sg chat` / `sg continue` / `sg threads` / `sg trace` / `sg approvals` / `sg eval run` -> `AppServerClient::spawn()` 启动 `singularity_app_server` stdio process -> `initialize` request -> `initialized` notification -> 对应 server/thread/turn/event/trace/approval/artifact/eval request -> `AppServer.handle_json()` -> `AppServer.handle()` -> `SessionStore.create_thread_with_trace()` / `create_turn_with_input_and_trace()` / `create_approval_with_trace()` / `record_approval_decision()` 或 read/list/update/eval helper -> `JsonRpcMessage::response()` 或 `AppEvent.to_notification()` 输出 JSONL -> `sg` 渲染 thread、turn、item、trace、approval 或 eval result 行。CLI 启动 app-server 时先使用 `SINGULARITY_APP_SERVER_BIN`，未设置时选择当前 `sg` binary 同目录的 `singularity_app_server` 构建/安装产物；同目录 binary 不存在时 fail closed，不按 PATH 静默查找。`eval/run` 读取并校验 `evaluation.task_set/v1` manifest，准备 workspace，经 Rust `AgentLoop` 执行任务，运行验证命令，写 `evaluation.result/v1` result/report artifact，并保持 `agent_completed`、`tests_passed`、`smoke_command_satisfied`、`evaluation_passed` 和 `local_process_fallback_count` 分离；manifest 的 `smoke_command` 会发给 AgentLoop prompt，模型未执行时 eval runner 在 agent 后通过同一个 Rust command sandbox 真实运行 smoke；`public_verification_command` / `hidden_verification_command` 只由 eval runner 在 agent 后执行。CLI request loop 以 matching response id 结束请求，只收集 matching response 之前到达的 notification，不在 response 后 drain 额外消息；JSON-RPC error、stdout EOF、child exit 或 read timeout 都不会无限等待。CLI drop 先发送 `server/shutdown`，让 app-server 清理当前进程持有的 active run；如果短等待后 app-server 仍未退出，CLI 才 kill child。默认 `turn/start` 进入 native path：`AgentLoopCapability::current()` 必须是 `available=true/status=completed/blockers=[]`；Windows restricted-token probe 通过后，`AppServer::run_native_agent_loop()` 用 `OpenAiProvider::from_env()` 读取 provider 配置、注册 `builtin.read/list/grep/edit/patch/command` schema，使用 thread `cwd` 作为 `WorkspaceTools` 根目录和 policy workspace root，再执行 Rust `AgentLoop::run()`；非 Windows gate 返回 `strict_command_sandbox_unsupported_platform` 并在 provider/tool/Python fallback 前停止。终态通过 `AgentLoopResult::to_run_status()` 更新 turn，并追加 `component="agent_loop"` 的安全 trace event；如果 native loop 返回 `approval_requests`，app-server 通过 `create_approval_with_trace()` 写入 pending approval。该路径不调用 Python sidecar，也不提供本地进程 fallback。Windows command backend 是 Rust-owned Codex-style restricted-token + Job Object 实现，不 shell out 到 `codex sandbox`，不创建系统账户、修改 firewall 或持久 ACL；显式 `network_access=denied` 或 `allowlist` 会 unsupported/fail closed，默认 command network mode 是 `allowed`，不伪造网络隔离。显式 `--agent-host python` 时，`singularity_app_server` 创建 `PythonSidecarConfig`，app-server 先持久化 Rust turn/user item/turn trace，再由 `AppServer.run_python_sidecar_if_enabled()` 通过 `PythonSidecarClient` 发送 `agent/run` 或 `agent/resume`；当同一 Rust thread 的历史 `python_sidecar` trace summary 中存在 `session_id` 时使用 `agent/resume`，否则使用 `agent/run`。CLI 仍只通过 app-server protocol，不直接调用 Python sidecar。

## 真实任务中的对象流

以 `sg run "goal"` 为例：CLI 启动 app-server 子进程，发送 `JsonRpcMessage::request(Method::Initialize, ...)`；`AppServer.handle_json()` 反序列化为 `JsonRpcMessage`，`InitializeParams` 校验 `clientInfo` 后返回 `InitializeResult`。CLI 再发送 `initialized` notification，连接进入 ready 状态。随后 `thread/start` 生成对象 `Thread` 并由 `SessionStore.create_thread_with_trace()` 在同一个 SQLite transaction 中写入 `threads` 和 `trace_events`。`turn/start` 先读取 thread；thread 缺失时直接返回 JSON-RPC error，thread 存在时生成对象 `Turn` 和 user message `Item`，通过 `SessionStore.create_turn_with_input_and_trace()` 在一个 transaction 中写入 `turns` / `items` / `trace_events`，然后进入 native AgentLoop。native path 更新 durable `agent_loop_status`，completed final answer 只输出 redacted `item/started -> item/agentMessage/delta -> item/completed` 一次；blocked/failed/cancelled 或 provider/tool/sandbox failure 不输出 assistant delta。显式 sidecar 路径把 completed/blocked/failed/cancelled/running/cancel_requested 映射成 `TurnStatus::Completed` / `Blocked` / `Failed` / `Interrupted` / `Running` / `Interrupted`，同时用 `SessionStore.update_turn_state()` 更新 durable `agent_loop_status`。running sidecar 先注册 active run；terminal sidecar 追加 `component="python_sidecar"` 的安全 trace event 并清 active run；cancel request 追加 app-server lifecycle event 且保留 active run 直到后续 status/cleanup；如果请求进入没有 sidecar handle 的 app-server 进程，则只返回 durable turn status 和 active row status，保持 durable turn 原状。`sg continue <thread-id> "instruction"` 先通过 `thread/read` 验证 thread 存在，再发送新的 `turn/start`。approval 流由 `approval/request` 写 pending row 和 trace，`approval/center` 读取 pending approval 与已记录 decision ledger，`approval/decision` 只能消费 pending approval 一次，并在同一个 transaction 中写 `approvals` decision fields、`approval_decisions` ledger row 和 trace；decision trace 的 `run_id` / `session_id` 使用原 request 的 `session_id`，`task_id` 使用原 request 的 `task_id`，payload 只包含 request/decision/outcome 安全摘要。重复 decision 返回 `Pending approval not found`。`artifact/fetch` 只从 `artifact_refs` 读取 redacted reference object，不返回文件内容。

## 真实对象完整结构

### JSON-RPC 与 thread/turn/item

枚举值包括 `Method::Initialize = "initialize"`、`Method::ServerCapabilities = "server/capabilities"`、`Method::ThreadList = "thread/list"`、`Method::ThreadRead = "thread/read"`、`Method::ThreadStart = "thread/start"`、`Method::ThreadResume = "thread/resume"`、`Method::ThreadFork = "thread/fork"`、`Method::ThreadArchive = "thread/archive"`、`Method::ThreadDelete = "thread/delete"`、`Method::TurnStart = "turn/start"`、`Method::TurnInterrupt = "turn/interrupt"`、`Method::TurnStatus = "turn/status"`、`Method::ApprovalList = "approval/list"`、`Method::ApprovalCenter = "approval/center"`、`Method::EventSubscribe = "event/subscribe"`、`Method::ArtifactFetch = "artifact/fetch"`、`Method::TraceTail = "trace/tail"`、`Method::ServerShutdown = "server/shutdown"`、`ThreadStatus::Active = "active"`、`TurnStatus::Running = "running"`、`TurnStatus::Blocked = "blocked"`、`TurnStatus::Interrupted = "interrupted"`、`ItemKind::CommandExecution = "commandExecution"`、`ItemStatus::Completed = "completed"`。

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
    pub digest: String,
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
    pub resource_sensitive: bool,
}

pub struct PermissionRule {
    pub rule_id: String,
    pub scope: SettingsScope,
    pub outcome: PermissionDecisionOutcome,
    pub operation: Option<PermissionOperation>,
    pub resource: Option<String>,
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

这些 schema object 是当前 Rust native cutover 边界；provider HTTP adapter、AgentLoop final answer、safe workspace tool result、repair retry、approval resume、Codex-style command sandbox、Rust evaluation runner 和 Windows 默认 native `sg run` 均已接入 Rust app-server path。capability gate 仍保留为防回归检查：只有 `available=true/status=completed/blockers=[]` 才能进入 native turn；Windows capability 成功状态必须先通过 restricted-token sandbox probe，非 Windows 当前以 `strict_command_sandbox_unsupported_platform` fail closed。

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
    Approved { approval_grant_id: String },
    Deny { reason: String },
    Ask { approval_request_id: String, reason: String },
}

pub struct ToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub ok: bool,
    pub status: String,
    pub view: ToolResultView,
    pub preview: String,
    pub digest: String,
    pub artifact_ref: Option<String>,
    pub error_code: Option<String>,
    pub artifact_refs: Vec<String>,
    pub result_id: Option<String>,
    pub approval_request_id: Option<String>,
    pub truncated: bool,
    pub redacted: bool,
    policy_decision_id: Option<String>,
    approval_grant_id: Option<String>,
    audit_metadata: Option<Value>,
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

pub struct ModelMessage {
    pub role: ModelRole,
    pub content: Vec<ContentBlock>,
    pub name: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_calls: Vec<ModelToolCall>,
    pub metadata: Value,
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

### agent sidecar status

`AgentRunStatus` 是 app-server-facing runtime status，承接 native Rust AgentLoop 和显式 Python oracle 的安全终态投影；`PythonSidecarClient` 只属于 oracle sidecar adapter。Python sidecar 只返回安全摘要字段；raw prompt、raw trace payload、raw tool arguments、provider response 和 secret-like 文本不得跨入 Rust trace/public payload。

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
    pub events: Vec<SidecarRunEvent>,
    pub audit_events: Vec<Value>,
    pub trace_path: Option<String>,
    pub error: Option<String>,
}

pub struct PythonSidecarStatus {
    pub run_id: String,
    pub status: String,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub final_answer: Option<String>,
    pub trace_path: Option<String>,
    pub events: Vec<SidecarRunEvent>,
}

pub struct AgentLoopCapability {
    pub available: bool,
    pub status: AgentStatus,
    pub reason: String,
    pub blockers: Vec<String>,
}

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

pub struct ApprovalGrant {
    pub request_id: String,
    pub tool_name: String,
    pub resources: Vec<String>,
    pub outcome: ApprovalOutcome,
}

pub struct AgentLoopResult {
    pub status: AgentStatus,
    pub completed: bool,
    pub final_answer: Option<String>,
    pub model_turns: u32,
    pub tool_calls: u32,
    pub approval_count: u32,
    pub approval_requests: Vec<ApprovalRequest>,
    pub tool_results: Vec<ToolResult>,
    pub tool_repairs: Vec<ToolRepair>,
    pub error: Option<String>,
}

pub struct PythonSidecarConfig {
    pub python_bin: String,
    pub module: String,
    pub project_root: PathBuf,
    pub python_path: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

pub struct ActiveSidecarRun {
    pub turn_id: String,
    pub thread_id: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}
```

### M9 planner/context/compaction/repair/finalization parity contract

这些对象属于 Rust `crates/agent` 的 schema/parity boundary 和 native runtime execution。它们镜像 Python oracle fixture 中已经存在的 planner state、context bundle、context summary envelope、tool-call repair contract 与 finalization mapping 安全字段，用于 JSON roundtrip 和 schema generation；A6 还增加 `assemble_context_items()`、`planner_next_action()`、`repair_next_action()`、`completion_gate_allows_final()` 和 `final_mapping_from_status()`，只做安全字段过滤、预算收敛、next action 和 status 映射。`AgentLoopCapability::current()` 在 Windows 先运行 restricted-token sandbox probe，成功时声明 `available=true/status=completed/blockers=[]`，失败时 blocked；在非 Windows 声明 `available=false/status=blocked/blockers=["strict_command_sandbox_unsupported_platform"]`。默认 `sg run` 在 Windows gate 通过时使用 native；显式 `--agent-host python` 才进入 Python oracle。默认非 native `turn/start` 仍使用 `AgentRunStatus::not_migrated()` 表示没有执行 native loop，不能伪装完成。

```rust
pub struct PlannerState {
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

pub struct ContextBundle {
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

pub struct ContextSummaryEnvelope {
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

pub struct ToolRepair {
    pub repair_id: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: String,
    pub phase_id: String,
    pub failed_tool_call_id: String,
    pub failure_kind: String,
    pub next_action: String,
    pub failed_result: Value,
    pub recovery_report: Value,
    pub repair_contract: Value,
    pub created_at: String,
    pub metadata: Value,
}

pub struct FinalReportMapping {
    pub mapping_id: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: String,
    pub phase_id: String,
    pub agent_loop_status: String,
    pub run_status: String,
    pub final_report_status: String,
    pub completion_status: String,
    pub final_answer: String,
    pub final_report: Value,
    pub completion_assessment: Value,
    pub contract_satisfaction: Value,
    pub created_at: String,
    pub metadata: Value,
}
```

## 谁生成这些对象

`JsonRpcMessage::request()` 和 `JsonRpcMessage::notification()` 生成 wire message；`SessionStore.create_thread()` / `create_thread_with_trace()` 生成 `Thread`；`SessionStore.create_turn()` / `create_turn_with_input_and_trace()` 生成 `Turn`，`SessionStore.update_turn_state()` 更新 sidecar/native result 对应的 durable `status` 和 `agent_loop_status`；`SessionStore.register_active_sidecar_run()` / `get_active_sidecar_run()` / `clear_active_sidecar_run()` 生成、读取、清理 active run safe metadata；`SessionStore.append_item()` 生成 `Item`；`TraceEvent::new()` 生成 `TraceEvent`；`SessionStore.register_artifact_ref()` 生成 `ArtifactRef`；`ApprovalRequest::new()` 和 `ApprovalDecision::new()` 生成 approval object；`PermissionRequest::new()` 只封装调用方传入的 resource，路径、命令和 host 归一化由 command/sandbox 或上层 tool boundary 负责；`PermissionRule::new()` 生成 declarative rule，`PermissionDecision::new()` 或 `PolicyEngine.evaluate()` 生成决策结果；`OpenAiProviderConfig::from_env()` 读取真实 provider 配置但不输出 secret，`OpenAiProvider::complete()` 生成 HTTP request 并把 provider response 解析为 `ModelTurnResponse`；`AgentLoopInput::new()` 生成显式 native loop 输入，`AgentLoop::run()` 生成 `AgentLoopResult` 和可持久化的 `approval_requests`，`AgentLoopResult::to_run_status()` 生成 Rust host-facing status；`PythonSidecarClient.run_agent()` 从 Python sidecar 的 `agent/run` response 生成 `PythonSidecarRunResult`，`PythonSidecarClient.resume_agent()` 从 sidecar `agent/resume` response 生成同一安全结果，`PythonSidecarClient.status()` / `cancel()` 从 `agent/status` / `agent/cancel` response 生成 `PythonSidecarStatus`，再由 app-server 生成 Rust host-facing status；`scripts/export_rust_parity_fixtures.py` 从 Python `PlannerState`、`ContextBundle`、`ContextSummaryEnvelope`、`ToolProtocolResultEnvelope`、`ToolProtocolRecoveryReport`、`RepairContract`、`AgentLoopResult` 和 `FinalReport` 导出 M9 oracle JSON，Rust `PlannerState` / `ContextBundle` / `ContextSummaryEnvelope` / `ToolRepair` / `FinalReportMapping` 只反序列化和 roundtrip 这些字段；`ToolSpec::new()`、`ToolBroker::register()`、`ToolCallRequest::new()`、`ToolOutput::success()` / `failure()`、`ToolResult::summary()` / `failed()`、`SandboxPolicy::isolated_verification()`、`CommandRequest::project_verification()`、`git_status_request()` / `git_diff_request()`、`CommandResult::completed()` / `policy_denied()` / `sandbox_backend_unavailable()`、`PatchChange::replace()` / `create()`、`ModelTurnRequest::new()` 和 `ModelTurnResponse::completed()` 生成各自 schema object 或最小执行边界对象。`provider_config_from_env()` 只读取 provider/model/base_url/api_key 是否存在并丢弃真实值；`ModelProviderStatus` 只输出 present(redacted)/missing 状态；`validate_provider_config()`、`validate_model_request()`、`validate_model_turn_response()`、`classify_model_error()`、`validate_stream_events()`、`validate_model_response()` 和 `retry_decision()` 验证 request/response/stream envelope、tool choice、tool-call parse status、tool-call arguments object、allowed tool names、provider capability metadata、error taxonomy 和 bounded retry 决策；真实 HTTP 只通过 `OpenAiProvider::complete()`，不会执行工具或写 trace。`ToolRegistry.register()` 只接受 `builtin.*`、`mcp.<server>.<tool>` 和 `python.<plugin>.<tool>` 命名空间，并拒绝重复 name。

## 谁消费这些对象

`AppServer.handle()` 消费 `JsonRpcMessage` 并分派到 initialize、server capabilities、thread、turn、event subscription、approval、artifact、trace handler；`AppServer::turn_start` 在 native path 先消费 `AgentLoopCapability::current()`，只有 `available=true/status=completed/blockers=[]` 才创建 native turn；`AppServer.run_native_agent_loop()` 消费 `TurnStartParams` 和 `turn_id`，并调用 `OpenAiProvider`、`ToolBroker`、`PolicyEngine`、`WorkspaceTools` 与 Rust `AgentLoop`；`AppServer.run_python_sidecar_if_enabled()` 只在显式 sidecar 配置存在且 `turn/start` 已通过 thread 校验和 durable turn 创建后消费 `TurnStartParams`、thread `model` 和上一条安全 sidecar `session_id`，并调用 `PythonSidecarClient`；`server/shutdown` 和 `thread/delete` 消费当前进程 active run metadata，先调用 cleanup/finalize 路径，再退出或删除 durable thread rows；`SessionStore.create_thread_with_trace()`、`create_turn_with_input_and_trace()`、`update_turn_state()`、`create_approval_with_trace()`、`record_approval_decision()`、`append_trace()` 和 `register_artifact_ref()` 消费 protocol object 写 SQLite；`approval/center` 消费 `SessionStore.list_pending_approvals()` 与 `list_approval_decisions()`；`artifact/fetch` 消费 `SessionStore.get_artifact_ref()` 并只返回 reference。`PolicyEngine.evaluate()` 消费调用方已归一化的 `PermissionRequest`，按 deny、caller-marked sensitive resource、hook ask、ask、allow、fallback ask 顺序返回 `PermissionDecision`；`approval_policy=untrusted` 和 `on-request` 保留 ask，`never` 把 ask 投影为 deny，deprecated `on-failure` 只被识别为历史模式并在 native approval request 上 fail closed，不映射到其他策略或 sandbox mode；policy 不解析 shell wrapper、argv、workspace path、network host、`.env` / SSH key marker 或 command 等价形式。`OpenAiProvider::complete()` 消费 `ModelTurnRequest` 并返回已验证的 `ModelTurnResponse` 或脱敏 `ProviderError`。`AgentLoop::run()` 消费 assembled context 和 provider response；没有 tool call 且 assistant content 非空时返回 completed，空 final answer fail closed；tool calls 先经 approval grant、`PolicyEngine` 和 `ToolBroker` admission，multi-change patch 会逐个 path 判定 policy，未知或 denied tool 不调用 executor 并立即 fail closed，ask tool 不调用 executor 并返回 blocked；registered read/list/grep/edit/patch/command 工具可经 `WorkspaceTools` 执行。`ToolBroker.tool_schema_payloads()` 消费 `ToolRegistry` 并只投影 name、redacted description 和 input schema 给模型；`WorkspaceTools.edit()` / `patch()` 只有 `Allow` 或 `Approved` decision 才能写普通路径，命中 `.env*`、`.git`、`.ssh`、cloud credential directory、credentials file、私钥后缀或名称、secret/secrets marker 这类 protected path 时无论是否有 approval grant 都 fail closed 为 `ProtectedPath`；`WorkspaceTools.command()` 只通过显式 strict `SandboxBackend` 执行 read-only、workspace-write 或 danger-full-access mode；backend 不可用、能力不足、approval denied/unavailable 或 policy ask/deny 都不会升级到 local_process、no_sandbox、relaxed 或 danger-full-access fallback。`CommandRequest.permission_resource()` 把 shell wrapper / argv 规范化为 policy 可消费的 command resource；git status/diff 只生成 sandbox-required `CommandRequest`，不绕过 command/sandbox 边界执行 git；`crates/sandbox` 不公开本机进程 executor 或 host filesystem mutation executor。`ToolResult.to_message_payload()` 消费 tool result 并生成 provider-safe tool payload；`AppEvent.to_notification()` 消费 event 并输出 JSON-RPC notification，`event/subscribe` 设置后由 `AppServer.event_notification()` 过滤当前连接后续通知。`sg` 只消费 `singularity_protocol` 和 `singularity_core`，不消费 `singularity_agent`、`singularity_model`、`singularity_tools` 或 `singularity_store`。

## 是否落盘

`SessionStore.open()` 初始化 SQLite 文件并确保 `schema_migrations` 记录 `0001_initial_session_store` 和 `0002_durable_ledger`；`threads`、`turns`、`items`、`trace_events`、`artifact_refs`、`approvals` 和 `approval_decisions` 表是真实落盘点。thread read/list/resume/archive/delete 和 turn status/interrupt 读取或更新这些现有表；`approval/request` 写 pending approval，`approval/list` 读取未决 approval，`approval/center` 读取未决 approval 与 decision ledger，`approval/decision` 更新同一 row 的 decision fields 并写 `approval_decisions` ledger；decision trace 按原 approval request 的 `session_id` / `task_id` 关联。trace/list 支持 `limit` / `offset` 分页，trace/tail 支持从尾部读取和 offset，trace/show 和 trace/tail 都从 SQLite `trace_events` 查询真实 events。`artifact/fetch` 读取 `artifact_refs` 表中已经 redacted 的 reference，不读取或管理 artifact bytes。app-server native branch 会更新同一 turn 的 durable status，并通过 `append_native_trace()` 写一个安全 `agent_loop` trace summary；ask decision 会写 pending approval row 和 approval trace；当前不会逐条持久化 native tool_result 或 raw provider response。Rust provider adapter 本身不直接写 SQLite、trace 或 artifact store；它只返回 bounded result object，由 app-server 或 AgentLoop 上层负责审计落盘。`target/` 是 Rust build output，被 `.gitignore` 排除。

## 是否进入 trace / audit

`thread/start`、`turn/start`、`approval/request` 和 `approval/decision` 都写 `TraceEvent`，且由 store transaction 把对应业务 row 与 trace 一起提交或回滚；missing thread 的 `turn/start` 不写 turn/item/sidecar/native trace。app-server native branch 会追加 `component="agent_loop"` 的安全 trace summary，payload 包含 status、completed、safe run/session/task id、event count、trace path handle、error 摘要和 command `audit_events`；这些 audit events 只记录 sandbox mode、approval policy、approval decision、command provenance、strict backend 名称和 scope digest，不包含 argv 原文、raw tool arguments、provider response、secret/env/token 或 evaluator-only metadata。显式 Python sidecar 路径会追加 `component="python_sidecar"` 的 trace summary 和 sidecar event 摘要；这些 payload 都不包含 raw prompt、raw trace payload、raw tool arguments、provider response、secret/env/token 或 evaluator-only metadata。approval decision trace payload 只包含 redacted `request_id`、`decision_id`、`outcome` 安全摘要。`ArtifactRef` 的 `summary` / `metadata` / `uri` 会对 secret-like marker 做本地 redaction 后落盘。Rust `PolicyEngine.evaluate()` 当前不写 trace/audit，只返回纯 `PermissionDecision`；后续 policy audit writer 必须只写脱敏资源 handle。Rust sandbox command result 只携带 bounded/redacted stdout/stderr preview、timeout/status 和 workspace-relative changed_files。Rust model boundary 只允许 `raw_response_ref` / `raw_error_ref` 这类 opaque handle，不保存 provider raw response body；`ModelError` 的 message 必须是已脱敏摘要。`ToolBroker` 的 public spec 不输出 permission/risk/audit metadata，并会 redaction 恶意 MCP 描述中的 prompt-injection/secret-like 文本；`ToolResult.to_message_payload()` 明确不输出 `policy_decision_id`、`approval_grant_id`、raw arguments、audit metadata 或 reference-only content。

## 失败路径

连接未完成 `initialize` 或未收到 `initialized` notification 前，业务 request 返回 `Not initialized`。同一连接重复 `initialize` 返回 `Already initialized`。未知 thread / turn / trace run / trace event / artifact 分别返回 `Thread not found`、`Turn not found`、`Trace run not found`、`Trace event not found`、`Artifact not found`；unknown thread 的 `turn/start` 在 sidecar/native 前失败。approval request 重复返回 `Approval already exists`；approval decision 找不到 pending request 或重复消费时返回 `Pending approval not found`。Python sidecar 启动失败、invalid JSON、AgentLoop blocked 或 sidecar returned error 不会 fallback 到本地 Rust fake completion；app-server 把 sidecar failure 写为 `agent_loop_status="failed"` 并记录 error summary。native gate 如果 unavailable、status 不是 completed 或 blockers 非空，app-server 返回 `Native AgentLoop is not production-ready`，并在创建 turn、写 trace、调用 provider 或调用 Python sidecar 前停止；Windows 当前 gate 通过后，缺少 provider env、provider authentication/network/model config 错误、provider response invalid、unknown tool、denied tool、ask decision、approval denied/unavailable、workspace backend unavailable、unsupported command mode 或 max turns 都 fail closed，不会伪造 assistant delta 或 completed turn。sandbox-required command 在没有 strict backend 时只能返回 backend unavailable，不启动本地进程。Rust model validation 会报告 missing provider/model/base_url/api_key、auth/network/sandbox permission/model config/provider unavailable 分类、stream event 顺序错误、tool choice 违规、unknown tool、invalid JSON、schema mismatch、provider capability 不支持等错误；`OpenAiProvider::complete()` 只做 HTTP adapter 和 response parse，不触发 tool execution 或 AgentLoop repair。SQLite 和 JSON 解析错误作为 app-server internal error 返回，stdio binary 仍输出可解析 JSON-RPC error line。

## 当前结构问题

当前已迁移 OpenAI-compatible provider HTTP adapter、Rust native AgentLoop 核心循环、typed tool repair retry、approval resume、Rust-owned Codex-style Windows restricted-token command backend、Rust evaluation runner 和 Windows 默认 CLI native cutover。`AgentLoopCapability::current()` 在 Windows 先运行 restricted-token sandbox probe，成功时返回 `available=true/status=completed/blockers=[]`，失败时 blocked；在非 Windows 返回 `available=false/status=blocked/blockers=["strict_command_sandbox_unsupported_platform"]`。显式 Python sidecar 仍可调用当前 Python AgentLoop 作为 oracle；默认 path 不调用 Python sidecar。native loop 的 provider final answer 可以在非空 assistant content 时 completed，空 final answer fail closed；安全 workspace tool admission/tool result、ToolRepair retry、command resource approval、approval grant/resume 和 command sandbox fail-closed 行为都由 Rust tests 覆盖。当前 `ToolBroker` 是最小 Rust tool boundary：它验证工具命名、投影 provider tool schema、阻断 denied/unknown tool 执行并生成安全 tool result；`PolicyEngine` 已提供 Rust 纯决策、hook 点、deny-first precedence、caller-marked sensitive resource deny 和 approval ask/deny 投影。`approval_policy` 和 `sandbox_mode` 正交：显式 danger-full-access 不是 bypass，也不会自动 approve；`never` 只禁止交互式 approval request，不能触发无 sandbox 执行；deprecated `on-failure` 不作为新 native 路径。`crates/sandbox` 当前提供 sandbox policy/backend descriptor、command request/result、git request helper、Windows restricted-token backend 和 `PatchChange` / `PatchResult` schema contract，不公开 relaxed local-process executor 或 host filesystem mutation executor；Windows backend 不 shell out 到 `codex sandbox`，不创建系统账户、修改 firewall 或持久 ACL；显式 `network_access=denied` 或 `allowlist` unsupported/fail closed，默认 command network mode 为 `allowed`。command resource normalization 位于 `CommandRequest.permission_resource()`，policy 不解析 shell wrapper；git helpers 只生成 command request，不建立第二套 git wrapper。Rust `crates/model` 当前拥有 model request/response schema、capability metadata、provider config presence check、redacted provider status、stream envelope validation、model request/response/tool-call validation、provider failure classification、bounded retry decision 和 `OpenAiProvider` HTTP client；它没有 provider registry、retry scheduler、context compaction、planner repair 或 AgentLoop 逻辑。当前 `sg` 是最小 JSON-RPC client：可以启动 app-server 子进程并完成 run/continue/list/trace/approval 查询，但不管理长期后台 daemon 生命周期、PTY TUI 或交互式 approval prompt；native path 必须同时检查 capability `available`、`status=completed` 且 `blockers` 为空。`event/subscribe` 是同一 stdio 连接上的通知过滤，不是后台 fan-out 服务；artifact ref 目前只持久化引用和 redacted metadata，不负责 artifact bytes 管理。`server/capabilities` 明确 WebSocket token transport 尚不可用；当前没有 WebSocket、Unix socket、Tauri、React、Electron 或第二套 core。

## 维护规则

新增 client 必须只依赖 protocol/core 层，不得直接耦合 agent/model/tools/store。新增 app-server 方法必须先在 `crates/protocol` 定义 request/response/event schema，再由 `crates/app-server` routing 和 `crates/store` persistence 接入。model boundary 可以维护与 Python `singularity.model` 对齐的 request/response/tool/stream/config schema、纯验证函数和 OpenAI-compatible provider adapter；不得在 model crate 中加入 tool execution、context manager、planner repair 或 AgentLoop。Agent boundary 只能按小切片推进 native loop；不得把 `Planner`、`ContextManager`、command runner 或 evaluation runner 一次性重写进 Rust。任何改变 thread/turn/item/approval/trace/model/agent-boundary wire format 的改动都必须更新 Rust tests、Python oracle fixture、本文档和 `docs/singularity.md` 的长期架构说明。

M0 迁移 guardrail 由 `scripts/verify_rust_migration_boundaries.py` 自动检查并进入 CI。该脚本阻断 CLI 直接依赖 agent/model/tools/store、未登记的新 crate 依赖、Python core runtime 非 allowlist 改动、Python RuntimeHost 类过渡实现、desktop/Web 抢跑文件、默认非 native `turn/start` 伪装 Rust AgentLoop 已迁移、Windows native capability 缺少 restricted-token sandbox probe、非 Windows native capability 没有 `strict_command_sandbox_unsupported_platform` blocker、CLI 或 app-server 绕过 native `available + status completed + blockers empty` gate、sandbox relaxed/no-sandbox contract 或 local-process executor、app-server 手写 JSON error、CLI 固定 notification 等待或 post-response drain、重复 approval decision public API、CLI/app-server unused tokio 依赖，以及 `ToolResult.to_message_payload()` 泄漏 raw arguments、approval/policy 内部 id、audit metadata 或 secret-like 文本。Python runtime allowlist 只覆盖 sidecar/oracle/fixture/parity 路径；不允许在 Python 主干新增 agent runtime 能力。
