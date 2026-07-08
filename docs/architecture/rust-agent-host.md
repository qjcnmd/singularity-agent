# Rust Agent Host 架构

本文描述当前源码树已经落地的 Rust native AgentLoop cutover 边界，不描述未实现的 transport 或 UI。

## 长期方向

长期架构是 `Rust Core + App Server + CLI/TUI first`。CLI/TUI 是第一个 client；未来 desktop 复用同一 app-server protocol，不单独设计第二套 core。

当前源码已经把默认 `sg run` 切到 Rust app-server native AgentLoop：

- Rust workspace 位于 `crates/`。
- 目录使用短名称：`core`、`protocol`、`store`、`policy`、`sandbox`、`tools`、`model`、`agent`、`app-server`、`cli`。
- Rust package / library 使用 `singularity_*`，例如 `singularity_core`、`singularity_protocol`。这样避免 `core` 与 Rust 标准库 `core` 冲突，同时让文件结构保持短名称。
- app-server 使用 JSON-RPC over stdio JSONL。
- Python 当前实现保留为显式 oracle / parity fixture，不再是默认生产运行时。

当前方向是 Rust CLI-first：开发者优先使用 Cargo/build artifact 中的 Rust `sg` 进入 `crates/app-server`；Python `sg` console script 只保留为 legacy/oracle path。Rust CLI 默认选择 native；只有显式传入 `--agent-host python` 时才启动 Python sidecar，不要求用户直接设置 `SINGULARITY_PYTHON_SIDECAR=1`。

## 已落地边界

`crates/protocol` 定义 JSON-RPC envelope、method params/result、`Thread`、`Turn`、`Item`、`TraceEvent`、`ArtifactRef` 和 app-server event。JSON-RPC params 使用 camelCase，例如 `clientInfo`、`threadId`、`turnId`、`runId`、`eventId`、`artifactId`、`eventTypes`；嵌入领域对象继续使用当前 Python parity schema 的 snake_case。

`crates/store` 是 SQLite-backed persistence boundary。它持久化 thread、turn、item、trace event、artifact reference、pending approval、approval decision ledger、active sidecar run 和 `schema_migrations`；`SessionStoreDescriptor` 负责可序列化 store schema 描述，真实 `SessionStore` 持有 SQLite connection。会一次创建多行 durable state 的 app-server 动作通过 store 事务提交，例如 thread + trace、turn + input item + trace、approval request + trace、approval decision + ledger + trace。active sidecar run 只保存 `turn_id/thread_id/run_id/session_id/task_id/status/created_at/updated_at`，不保存 raw prompt、provider payload、tool args 或 env。approval decision trace 使用原 approval request 的 `session_id` / `task_id` 做关联，不用 `request_id` 冒充 session。

`crates/app-server` 实现 `initialize` / `initialized` handshake、server capabilities、thread list/read/start/resume/fork/archive/delete、turn start/interrupt/status、event subscription、approval list/center/request/decision、artifact fetch、trace list/show/tail、`eval/run` 和 `server/shutdown`。`turn/start` 会先校验 Rust store 中的 thread 存在，missing thread 直接返回 `Thread not found`，不会启动 Python sidecar 或写 sidecar/native trace。默认 `agentHost` 是 native；app-server 仍先检查 `AgentLoopCapability::current()`，只有 `available=true` 且 `blockers=[]` 才调用 `AppServer::run_native_agent_loop()`。当前 capability 为 `completed` 且无 blocker，因此 native path 会用 `OpenAiProvider::from_env()`、`ToolBroker`、`PolicyEngine`、`WorkspaceTools` 和 Rust `AgentLoop` 执行，并把终态写回 turn 与 `component="agent_loop"` 的安全 trace；该路径不调用 Python sidecar，也不提供本地进程 fallback。`eval/run` 由 Rust app-server 执行 native evaluation runner：校验 `evaluation.task_set/v1` manifest，准备 fixture/repo workspace，通过 Rust `AgentLoop`、`OpenAiProvider`、`ToolBroker`、`PolicyEngine` 和 `WorkspaceTools` 运行任务，再用 Rust-owned command sandbox 执行准备/验证命令并写 `evaluation.result/v1` result/report artifact；provider 配置缺失、workspace 准备失败、sandbox unsupported、agent 未完成或 verification 失败都会 fail closed。它不调用 Python AgentLoop，不用 fake/scripted provider，不伪造 verification pass。Windows command backend 是 Rust-owned Codex-style restricted-token + low-integrity token + Job Object 实现，带受控 cwd/env、stdout/stderr capture、timeout、Job Object cleanup、stdio handle allowlist、敏感路径 deny、workspace path admission、read-only 写入 deny、workspace-write/danger-full-access 执行模式和 unsupported 状态；它不 shell out 到 `codex sandbox`，也不创建系统账户、改 firewall 或持久 ACL。显式 `--agent-host python` 会让 CLI 设置 sidecar env；thread 校验通过后，app-server 先持久化 Rust turn/user item/turn trace，再通过 `PythonSidecarClient` 启动 `python -m singularity.agent_host.sidecar`。首个 sidecar turn 调 `agent/run`，后续 turn 通过上一条 `python_sidecar` trace 的 `session_id` 调 `agent/resume`，thread 的 `model` 会转发为 sidecar `model` 参数。sidecar 只作为 oracle 调用现有 `AgentHost -> KernelBootstrap -> AgentKernel -> AgentLoop`，Rust 只把安全状态摘要翻译成 turn/item/trace。sidecar 返回 `running` 时，app-server 写 active run row 并保留当前进程内 sidecar handle；持有该 handle 的进程内 `turn/status` 通过 `agent/status` 刷新 durable status，`turn/interrupt` 通过 `PythonSidecarClient::cancel(run_id)` 请求 cancel 并写 `agent_loop_status="cancel_requested"`。如果另一个短生命周期 app-server 进程只看到 active run row 但没有 sidecar handle，`turn/status` / `turn/interrupt` 只返回 durable turn status 和 active row status，不伪造 sidecar 查询或取消。`server/shutdown` 会清理当前进程持有的 active run，设置 shutdown_requested，并让 binary 主循环在写出响应后退出；CLI drop 会先发送 `server/shutdown`，再等待短窗口，只有 app-server 未退出时才 kill child。`thread/delete` 会先清理同一 thread 的 active run，再删除 thread/turn/item/trace/artifact row。stdio binary transport 的错误行统一通过 `JsonRpcMessage::error()` / `serde_json` 序列化为合法 JSON-RPC error envelope。item streaming 使用 `item/agentMessage/delta` 和 `item/commandExecution/outputDelta` 这类 typed delta，不再使用 generic `item/delta`；completed native 或 sidecar final answer 只作为 redacted agent message delta 投影一次；blocked/failed/cancelled native 或 sidecar failure 和 sidecar transport error 不输出伪 assistant delta。当前 `server/capabilities` 只声明 stdio transport 可用，WebSocket token transport 仍是未来 transport，显式返回 unavailable。

`crates/cli` 提供 `sg` CLI。`sg run` / `sg chat` / `sg continue` / `sg threads` / `sg trace` / `sg approvals` / `sg config doctor` 会启动或调用 stdio app-server，并只通过 JSON-RPC protocol 交换 initialize、thread、turn、trace 和 approval 请求；请求读取以 matching response id 为完成条件，保留 matching response 之前到达的 notification，不依赖固定 notification 数量，也不在 response 后 drain 额外消息。stdout 关闭、app-server 提前退出或 response timeout 都返回明确错误，不无限等待；CLI 退出前会发送 `server/shutdown`，使 app-server 有机会按协议清理 active sidecar run。`sg daemon` 启动 app-server stdio 进程。CLI 不直接依赖 `singularity_agent`、`singularity_model`、`singularity_tools` 或 `singularity_store`。

`crates/tools` 提供 Rust `ToolBroker`。所有工具在发送给模型前必须先注册到 `ToolRegistry`，工具名只能使用 `builtin.*`、`mcp.<server>.<tool>` 或 `python.<plugin>.<tool>`；`ToolBroker.tool_schema_payloads()` 只投影 name、redacted description 和 input schema；`ToolBroker.execute()` 对 unknown 或 denied tool 不调用 executor，并通过 `ToolResult.to_message_payload()` 输出安全摘要。`ToolResult` 是 agent loop 使用的工具调用结果，保留 digest、artifact ref 和 result id 等引用字段；raw executor payload 是 `ToolOutput`。sandbox-required command boundary 已存在于 tools/sandbox crate；app-server native registry 包含 command schema。无 strict backend 时 command fail closed；Windows backend 支持 read-only、workspace-write 和 danger-full-access 三种 command mode，read-only 通过 low-integrity token 阻断程序化 workspace 写入，workspace-write/danger-full-access 使用 restricted token + Job Object + path admission + controlled env/cwd/capture/timeout。由于当前 Codex-style backend 不做 firewall/network OS isolation，`network_access=denied` 的 command request 会返回 unsupported/fail closed。

`crates/policy` 提供 Rust `PolicyEngine` 纯决策边界。`PermissionRule` 按 `SettingsScope` 的 managed、user、project、local 顺序匹配，并按 hook tool_result -> deny rules -> caller-marked sensitive resource deny -> ask rules -> allow rules -> fallback ask 的顺序得到 `PermissionDecision`。`approval_policy = never` 会把 ask 转成 deny。当前 Rust policy 只消费调用方已经归一化好的 `PermissionRequest.resource` 和 `resource_sensitive` 安全分类，不解析 shell wrapper、argv、workspace path、network host、`.env` / SSH key marker 或命令等价形式；这些上下文归一化与 enforcement 属于 command/sandbox 或上层 tool boundary。当前 Rust policy 仍是 app-server/tool boundary 的纯决策对象，完整运行时 policy audit writer 和 Python AgentLoop 执行强制链尚未被 Rust 替换。

`crates/model` 提供 Rust model boundary schema、验证函数和 OpenAI-compatible provider adapter。它对齐现有 Python `singularity.model` 的 `ModelTurnRequest`、`ModelTurnResponse`、tool schema、tool call、capability metadata、provider config presence、redacted provider status、request/response validation、stream event、validation result、model error object 和 bounded retry decision；`provider_config_from_env()` 只读取环境变量是否存在，`validate_provider_config()`、`validate_model_request()`、`validate_model_turn_response()`、`validate_stream_events()`、`validate_model_response()`、`classify_model_error()` 和 `retry_decision()` 做边界验证、分类与 retry 决策。`OpenAiProvider::complete()` 是当前唯一 HTTP adapter：它按 base URL 生成 `/chat/completions` endpoint，发起 non-stream request，解析 `ModelTurnResponse`，并只返回脱敏 `ModelError`、`raw_response_ref` 或 `raw_error_ref`，不写 raw prompt、raw response、API key 或 Authorization header。

`crates/agent` 当前除 Python sidecar status adapter 外，持有 M9 的 schema/parity 切片 `PlannerState`、`ContextBundle`、`ContextSummaryEnvelope`、`ToolRepair`、`FinalReportMapping` 和 `AgentLoopCapability`，并提供小型 deterministic helper：`assemble_context_items()`、`planner_next_action()`、`repair_next_action()`、`completion_gate_allows_final()` 和 `final_mapping_from_status()`。`AgentLoop` 是当前 native 主循环：context item -> context assembly -> provider request -> provider response -> final answer 或 tool admission -> safe workspace tool result -> max-turn fail-closed。模型直接给非空 final answer 时会返回 completed；空 final answer、denied/unknown tool、workspace backend unavailable 和 max turns 都 fail closed。ask tool 不执行并返回 blocked；带 allow outcome 的 `ApprovalGrant` 可以在 agent crate 内放行一次普通 mutation tool，app-server approval decision 可恢复并消费 pending tool call 一次。`.env`、`.ssh`、`.git`、credential/secret/private-key path、cloud credential 目录和常见私钥文件名会先标记 sensitive 并 fail closed；multi-change patch 会按每个 change path 执行 policy 判定，任一 deny/ask 都会阻断整次写入。repairable workspace tool failure 会生成 typed `ToolRepair`，把安全 `ToolResult` payload 追加给下一轮模型请求；预算耗尽仍 fail closed。`AgentLoopCapability::current()` 返回 `available=true/status=completed/blockers=[]`，CLI 和 app-server 仍保留 capability gate 作为防回归检查。

## Turn lifecycle 与 cancel 边界

Rust app-server 拥有 turn lifecycle 的 durable status machine：

```text
accepted -> running -> completed
accepted -> running -> failed
accepted -> running -> interrupted
accepted -> failed
running -> interrupted_requested -> interrupted
```

| Status | Rust owner | Python owner | SQLite fields | trace event | CLI rendering | sidecar process status | retry/resume implication |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `accepted` | `AppServer::turn_start` | None | current protocol creates `turns.status="running"`; native starts with `agent_loop_status="running"`, explicit Python oracle starts with `agent_loop_status="not_migrated"` until sidecar status is observed | app-server lifecycle event | running turn line | not started or starting for explicit Python oracle only | retry only before active run exists |
| `running` | `AppServer` + `SessionStore` | Python AgentHost/AgentLoop | `turns.status="running"`, `agent_loop_status`, active run safe IDs | app-server lifecycle event and `python_sidecar` trace event | running status line | active | resume uses Python `session_id`; no duplicate run |
| `completed` | `AppServer::update_turn_from_run_status` | Python AgentLoop finalization | `turns.status="completed"`, `agent_loop_status="completed"`; async status completion also appends one `AgentMessage` item for the redacted final answer | completion trace event and one typed agent message delta | completed plus summary | exited | terminal |
| `failed` | app-server status mapping | Python sidecar when applicable | `turns.status="failed"`, `agent_loop_status="failed"` | redacted failure trace event | failed non-zero output | failed/not started | new turn or recovery path |
| `interrupted_requested` | Rust app-server | Python receives cancel only | `turns.status="interrupted"`, `agent_loop_status="cancel_requested"`, active run retained | interrupt requested trace event | interrupted with cancel_requested | cancel requested | wait for terminal cleanup |
| `interrupted` | `SessionStore` durable status | AgentLoop cancel semantics | `turns.status="interrupted"`, `agent_loop_status="cancelled"` | cancel result trace event | interrupted line | cancelled/exited | future turn may resume through Python session recovery |

Cancel ownership map:

| Concern | Owner |
| --- | --- |
| turn/interrupt request owner | Rust app-server |
| sidecar cancel transport | `PythonSidecarClient::cancel(run_id)` |
| AgentLoop cancel semantics | Python AgentHost/AgentLoop |
| durable status owner | SessionStore |
| trace owner | app-server |
| CLI owner | protocol renderer |

非目标：no OS kill primary cancel、no CLI-to-Python cancel、no native Rust AgentLoop cancel、no local-process sandbox fallback。批准词表：user submitted unit = `turn`，sidecar execution identity = `run`，Python recovery identity = `session`，lifecycle status = `status`，cancel action = `cancel`，interrupt protocol method = `interrupt`，stored lifecycle record = `active run`，state transition event = `lifecycle event`，sidecar process wrapper = `sidecar`。不要引入 verbose invented lifecycle names。

## Python 冻结范围

以下 Python 模块在迁移期作为只读 oracle / parity reference：

- `src/singularity/agent_loop.py` 和 AgentLoop 周边 turn/completion/failure recovery。
- planner / context / prompt assembly。
- evaluation runner 和 provider-backed benchmark。
- Windows sandbox backend。
- provider / model runner。
- verification / review / final report。
- tool protocol / tool executor / command executor / policy approval 主链路。

允许在 Python 侧新增的内容仅限 sidecar adapter、fixture export、schema/parity check、文档校验和迁移期测试。新增核心能力必须进入 Rust boundary，不能继续扩展 Python 主干。

## 当前验证边界

当前阶段必须通过：

- `python scripts/verify_rust_migration_boundaries.py`
- `cargo fmt --check`
- `cargo check --workspace --all-targets`
- `cargo check --workspace --all-targets`
- `cargo test --workspace`
- `python scripts/export_rust_parity_fixtures.py`
- `python -m pytest tests/test_rust_parity_fixtures.py -q`
- `python scripts/verify_runtime_docs.py`

如果 provider 配置存在，还要通过 Rust `sg run` 和 Rust `sg eval run` 跑真实 provider 验证。默认 `sg run` 是 native；`--agent-host python` 只用于显式 oracle。Rust `sg eval run` 写 result/report artifact；provider 配置缺失、network-denied sandbox unsupported、agent 未完成或 verification 失败会返回明确 blocker/error，而不是 fake pass：

```text
python -m singularity.cli eval run docs/evaluation/public-representative-task.json --run-id rust-host-phase1-parity --json
```

该 evaluation 仍通过 `KernelBootstrap -> AgentGraphBuilder -> AgentKernel -> AgentLoop.run`，用于证明 Python oracle 仍可运行；它不是 Rust AgentLoop 迁移完成证明。

验证报告必须记录 provider 配置的脱敏状态、是否进入 Rust AgentLoop、是否使用 Python sidecar、turn/tool/approval 统计、local process fallback 计数、public/hidden verification、result/report/trace artifact path 和失败分类。Python oracle eval 只能作为对照，不能替代 Rust native proof。

## 下一阶段顺序

下一阶段应按风险和边界厚度迁移：

1. 更完整的 TUI 渲染和长期 app-server lifecycle 管理。
2. 更强的网络隔离如果需要系统级 firewall/ACL/setup，必须另行说明风险并请求确认。
3. 更完整的 policy audit writer 和 trace analytics 可以在 Rust 主运行时之上继续收敛，但不能重新引入 Python 默认运行时。

## 维护规则

Rust app-server protocol 是唯一富客户端边界。任何 desktop/TUI 新能力都必须先通过 `crates/protocol` 和 `crates/app-server` 暴露，再由 client 消费；不能绕过 app-server 直接调用 core 或 Python runtime。

M0 之后，`scripts/verify_rust_migration_boundaries.py` 是提交和 CI 的迁移漂移检查入口。它检查 `crates/cli` 不能直接依赖 agent/model/tools/store，crate 依赖必须留在显式 allowlist，Python runtime 改动只能落在 sidecar/oracle/fixture/parity 允许路径，仓库不能提前出现 desktop/Web 启动文件，默认非 native `turn/start` 不能伪装 Rust AgentLoop 已迁移，native capability 必须保持 `available=true/status=completed/blockers=[]`，CLI 和 app-server 必须在 native path 同时检查 native capability `available` 和 `blockers` 为空；`crates/sandbox` 不得公开 relaxed/no-sandbox contract 或 local-process executor，app-server stdio error 不得手拼 JSON，CLI 不得恢复固定 notification 等待或 post-response drain，store 不得恢复重复 approval decision public API，CLI/app-server 不得声明 unused tokio，并且 Rust `ToolResult.to_message_payload()` 不得输出 raw arguments、内部 approval/policy id、internal metadata 或明显 secret-like 文本。Python runtime allowlist 只覆盖 sidecar/oracle/fixture/parity 路径；不允许在 Python 主干新增 agent runtime 能力。
