# Rust Agent Host 架构

本文描述当前目标源码树的 Rust public runtime 边界，不描述未实现的 transport 或 UI。

## 长期方向

长期架构是 `Rust Core + App Server + CLI/TUI first`。CLI/TUI 是第一个 client；未来 desktop 复用同一 app-server protocol，不单独设计第二套 core。

当前 public runtime 是 Rust `sg` 进入 `crates/app-server`，再进入 Rust `AgentLoop`：

- Rust workspace 位于 `crates/`。
- 目录使用短名称：`core`、`protocol`、`store`、`policy`、`sandbox`、`tools`、`model`、`agent`、`app-server`、`cli`。
- Rust package / library 使用 `singularity_*`，例如 `singularity_core`、`singularity_protocol`。
- app-server 使用 JSON-RPC over stdio JSONL。
- Python oracle/parity/dev-only 内容只用于迁移期对照、fixture export 和 schema parity，不是普通用户运行入口。

## 已落地边界

`crates/protocol` 定义 JSON-RPC envelope、method params/result、`Thread`、`Turn`、`Item`、`TraceEvent`、`ArtifactRef` 和 app-server event。JSON-RPC params 使用 camelCase，例如 `clientInfo`、`threadId`、`turnId`、`runId`、`eventId`、`artifactId`、`eventTypes`；嵌入领域对象继续使用当前 parity schema 的 snake_case。public `turn/start` 只接受 thread 与输入内容，不公开后端选择。

`crates/store` 是 SQLite-backed persistence boundary。它持久化 thread、turn、item、trace event、artifact reference、pending approval、approval decision ledger 和 `schema_migrations`；`SessionStoreDescriptor` 负责可序列化 store schema 描述，真实 `SessionStore` 持有 SQLite connection。会一次创建多行 durable state 的 app-server 动作通过 store 事务提交，例如 thread + trace、turn + input item + trace、approval request + trace、approval decision + ledger + trace。approval decision trace 使用原 approval request 的 `session_id` / `task_id` 做关联，不用 `request_id` 冒充 session。

`crates/app-server` 实现 `initialize` / `initialized` handshake、server capabilities、thread list/read/start/resume/fork/archive/delete、turn start/interrupt/status、event subscription、approval list/center/request/decision、artifact fetch、trace list/show/tail、`eval/run` 和 `server/shutdown`。`turn/start` 会先校验 Rust store 中的 thread 存在，missing thread 直接返回 `Thread not found`，不会写 turn/native trace。thread 存在后，app-server 检查 `AgentLoopCapability::current()`，只有 `available=true/status=completed/blockers=[]` 才调用 `AppServer::run_native_agent_loop()`。Windows capability 会先执行 bounded restricted-token sandbox probe，probe 成功才进入 native path；probe 失败会以 `strict_command_sandbox_probe_failed:*` blocker fail closed。非 Windows capability 明确 blocked，并以 `strict_command_sandbox_unsupported_platform` fail closed，不会启动 provider 或 Python fallback。native input assembly 会由 `crates/core` 从最近 Git workspace root 到 thread 实际 `cwd` 按父级到子级读取 workspace 内 `AGENTS.md`；越界 link、读取错误、无效 UTF-8、非普通文件或字节上限超限 fail closed，缺失则正常。合并内容仅作为 `AgentLoopInput` 的内存字段进入首条 developer message，不写 user item、tool/history/trace。native 终态写回 turn 与 `component="agent_loop"` 的安全 trace；该路径不调用 Python，也不提供本地进程 fallback。

`eval/run` 由 Rust app-server 执行 native evaluation runner：校验 `evaluation.task_set/v1` manifest，准备 fixture/repo workspace，先加载 task workspace 的层级 `AGENTS.md`，再通过 Rust `AgentLoop`、`OpenAiProvider`、`ToolBroker`、`PolicyEngine` 和 `WorkspaceTools` 运行任务，再用 Rust-owned command sandbox 执行 smoke、准备/验证命令并写 `evaluation.result/v1` result/report artifact。provider 配置缺失、workspace 准备失败、sandbox unsupported、agent 未完成、smoke command 未通过 Rust command sandbox 或 verification 失败都会 fail closed。它不调用 Python AgentLoop，不用 fake/scripted provider，不伪造 verification pass。

Windows command backend 是 Rust-owned Codex-style restricted-token + low-integrity token + Job Object 实现，带受控 cwd/env、stdout/stderr capture、timeout、Job Object cleanup、stdio handle allowlist、敏感路径 deny、workspace path admission、read-only 写入 deny、workspace-write/danger-full-access 执行模式和 unsupported 状态；显式 danger-full-access 是 Codex CLI 对齐的 sandbox mode，不是 approval bypass，backend 不可用时仍返回 backend unavailable，不升级到 local_process、no_sandbox 或 relaxed；该路径不 shell out 到 `codex sandbox`，也不创建系统账户、改 firewall 或持久 ACL。

`crates/cli` 提供 `sg` CLI。`sg run` / `sg chat` / `sg continue` / `sg threads` / `sg trace` / `sg approvals` / `sg config doctor` 会启动或调用 stdio app-server，并只通过 JSON-RPC protocol 交换 initialize、thread、turn、trace 和 approval 请求；app-server 解析顺序是 `SINGULARITY_APP_SERVER_BIN` 显式覆盖、当前 `sg` binary 同目录的 `singularity_app_server` 构建/安装产物；两者都不存在时 fail closed，不再按 PATH 静默查找，避免开发和安装环境误连旧 app-server。请求读取以 matching response id 为完成条件，保留 matching response 之前到达的 notification，不依赖固定 notification 数量，也不在 response 后 drain 额外消息。stdout 关闭、app-server 提前退出或 response timeout 都返回明确错误，不无限等待；CLI 退出前会发送 `server/shutdown`，使 app-server 有机会按协议清理当前连接。`sg daemon` 启动 app-server stdio 进程。CLI 不直接依赖 `singularity_agent`、`singularity_model`、`singularity_tools` 或 `singularity_store`。

`crates/tools` 提供 Rust `ToolBroker`。所有工具在发送给模型前必须先注册到 `ToolRegistry`，工具名只能使用 `builtin.*`、`mcp.<server>.<tool>` 或 `python.<plugin>.<tool>`；`ToolBroker.tool_schema_payloads()` 只投影 name、redacted description 和 input schema；`ToolBroker.execute()` 对 unknown 或 denied tool 不调用 executor，并通过 `ToolResult.to_message_payload()` 输出安全摘要。`ToolResult` 是 agent loop 使用的工具调用结果，保留 preview、digest、artifact ref 和 result id 等字段；raw executor payload 是 `ToolOutput`。sandbox-required command boundary 已存在于 tools/sandbox crate；app-server native registry 包含 command schema。无 strict backend 时 command fail closed；Windows backend 支持 read-only、workspace-write 和 danger-full-access 三种 command mode。command result 的 internal audit metadata 会记录 sandbox mode、strict backend、approval policy/decision、command provenance 和 scope digest，但不会进入 model tool payload。target-project Python commands 仍然可以通过该 Rust command boundary 运行，例如 `python -m pytest` 是目标仓库验证命令，不是 Singularity Python runtime。

## Turn lifecycle 与 cancel 边界

Rust app-server 拥有 turn lifecycle 的 durable status machine：

```text
accepted -> running -> completed
accepted -> running -> failed
accepted -> running -> interrupted
accepted -> failed
running -> interrupted_requested -> interrupted
```

| Status | Rust owner | Python owner | SQLite fields | trace event | CLI rendering | retry/resume implication |
| --- | --- | --- | --- | --- | --- | --- |
| `accepted` | `AppServer::turn_start` | None | current protocol creates `turns.status="running"` and native `agent_loop_status="running"` after `SessionStore.create_turn_with_input_and_trace()` | app-server lifecycle event | running turn line | retry only before native run exists |
| `running` | `AppServer` + `SessionStore` | None | `turns.status="running"`, `agent_loop_status="running"` | app-server lifecycle event and native `agent_loop` trace event | running status line | no duplicate run |
| `completed` | `AppServer::update_turn_from_run_status` | None | `turns.status="completed"`, `agent_loop_status="completed"` | completion trace event and one typed agent message delta | completed plus summary | terminal |
| `failed` | app-server status mapping | None | `turns.status="failed"`, `agent_loop_status="failed"` | redacted failure trace event | failed non-zero output | new turn or recovery path |
| `interrupted_requested` | Rust app-server | None | `turns.status="interrupted"`, `agent_loop_status="cancel_requested"` | interrupt requested trace event | interrupted with cancel_requested | wait for terminal cleanup |
| `interrupted` | `SessionStore` durable status | None | `turns.status="interrupted"`, `agent_loop_status="cancelled"` | cancel result trace event | interrupted line | future turn starts from durable Rust thread state |

Cancel ownership map:

| Concern | Owner |
| --- | --- |
| turn/interrupt request owner | Rust app-server |
| native cancel transport | app-server interrupt handling |
| AgentLoop cancel semantics | Rust AgentLoop public runtime |
| durable status owner | SessionStore |
| trace owner | app-server |
| CLI owner | protocol renderer |

非目标：no OS kill primary cancel、no CLI-to-Python cancel、no Python backend selector、no local-process sandbox fallback。批准词表：user submitted unit = `turn`，Rust execution identity = `run`，recovery identity = `session`，lifecycle status = `status`，cancel action = `cancel`，interrupt protocol method = `interrupt`，state transition event = `lifecycle event`，internal oracle adapter = `oracle`。不要引入 verbose invented lifecycle names。

## Python 冻结范围

以下 Python 模块在迁移期作为只读 oracle / parity reference：

- `src/singularity/agent_loop.py` 和 AgentLoop 周边 turn/completion/failure recovery。
- planner / context / prompt assembly。
- evaluation runner 和 provider-backed benchmark。
- Windows sandbox backend。
- provider / model runner。
- verification / review / final report。
- tool protocol / tool executor / command executor / policy approval 主链路。

允许在 Python 侧新增的内容仅限 fixture export、schema/parity check、文档校验和迁移期测试。新增核心能力必须进入 Rust boundary，不能继续扩展 Python 主干。

## 当前验证边界

当前阶段必须通过：

- `python scripts/verify_rust_migration_boundaries.py`
- `cargo fmt --check`
- `cargo check --workspace --all-targets`
- `cargo test --workspace`
- `python scripts/export_rust_parity_fixtures.py`
- `python -m pytest tests/test_rust_parity_fixtures.py -q`
- `python scripts/verify_runtime_docs.py`

如果 provider 配置存在，还要通过 Rust `sg run` 和 Rust `sg eval run` 跑真实 provider 验证。Rust `sg eval run` 写 result/report artifact；provider 配置缺失、unsupported command mode、agent 未完成、smoke command 未通过工具执行或 verification 失败会返回明确 blocker/error，而不是 fake pass：

```text
sg eval run docs/evaluation/public-representative-task.json --run-id rust-native-cutover --json
```

该 evaluation 通过 Rust CLI -> app-server -> Rust `AgentLoop` -> Rust eval runner；Python oracle eval 只能作为显式对照，不能替代 Rust native proof。

验证报告必须记录 provider 配置的脱敏状态、是否进入 Rust AgentLoop、是否使用 Python oracle、turn/tool/approval 统计、local process fallback 计数、public/hidden verification、result/report/trace artifact path 和失败分类。Python oracle eval 只能作为对照，不能替代 Rust native proof。

## 下一阶段顺序

下一阶段应按风险和边界厚度迁移：

1. 更完整的 TUI 渲染和长期 app-server lifecycle 管理。
2. 更强的网络隔离如果需要系统级 firewall/ACL/setup，必须另行说明风险并请求确认。
3. 更完整的 policy audit writer 和 trace analytics 可以在 Rust 主运行时之上继续收敛，但不能重新引入 Python 默认运行时。

## 维护规则

Rust app-server protocol 是唯一富客户端边界。任何 desktop/TUI 新能力都必须先通过 `crates/protocol` 和 `crates/app-server` 暴露，再由 client 消费；不能绕过 app-server 直接调用 core 或 Python runtime。

M0 之后，`scripts/verify_rust_migration_boundaries.py` 是提交和 CI 的迁移漂移检查入口。它检查 `crates/cli` 不能直接依赖 agent/model/tools/store，crate 依赖必须留在显式 allowlist，Python runtime 改动只能落在 oracle/fixture/parity 允许路径，仓库不能提前出现 desktop/Web 启动文件，Windows native capability 必须经过 restricted-token sandbox probe 后才能返回 completed/no blockers，非 Windows native capability 必须保留 `strict_command_sandbox_unsupported_platform` blocker，CLI 和 app-server 必须在 native path 同时检查 native capability `available/status=completed/blockers=[]`；public CLI/protocol/docs 不得恢复后端选择或 Python route；`crates/sandbox` 不得公开 relaxed/no-sandbox contract 或 local-process executor，app-server stdio error 不得手拼 JSON，CLI 不得恢复固定 notification 等待或 post-response drain，store 不得恢复重复 approval decision public API，CLI/app-server 不得声明 unused tokio，并且 Rust `ToolResult.to_message_payload()` 不得输出 raw arguments、内部 approval/policy id、internal metadata 或明显 secret-like 文本。Python runtime allowlist 只覆盖 oracle/fixture/parity 路径；不允许在 Python 主干新增 agent runtime 能力。
