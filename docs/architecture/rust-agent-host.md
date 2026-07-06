# Rust Agent Host 第一阶段架构

本文描述当前源码树已经落地的迁移边界，不描述未实现的 transport 或完整 AgentLoop 重写。

## 长期方向

长期架构是 `Rust Core + App Server + CLI/TUI first`。CLI/TUI 是第一个 client；未来 desktop 复用同一 app-server protocol，不单独设计第二套 core。

当前第一阶段只迁移硬边界和协议对象：

- Rust workspace 位于 `crates/`。
- 目录使用短名称：`core`、`protocol`、`store`、`policy`、`sandbox`、`tools`、`model`、`agent`、`app-server`、`cli`。
- Rust package / library 使用 `singularity_*`，例如 `singularity_core`、`singularity_protocol`。这样避免 `core` 与 Rust 标准库 `core` 冲突，同时让文件结构保持短名称。
- app-server 使用 JSON-RPC over stdio JSONL。
- Python 当前实现保留为 migration oracle / parity reference，不删除、不重写。

## 已落地边界

`crates/protocol` 定义 JSON-RPC envelope、method params/result、`Thread`、`Turn`、`Item`、`TraceEvent`、`ArtifactRef` 和 app-server event。JSON-RPC params 使用 camelCase，例如 `clientInfo`、`threadId`、`turnId`、`runId`、`eventId`、`artifactId`、`eventTypes`；嵌入领域对象继续使用当前 Python parity schema 的 snake_case。

`crates/store` 是 SQLite-backed persistence boundary。它持久化 thread、turn、item、trace event、artifact reference、pending approval、approval decision ledger 和 `schema_migrations`；`SessionStoreDescriptor` 负责可序列化 store schema 描述，真实 `SessionStore` 持有 SQLite connection。会一次创建多行 durable state 的 app-server 动作通过 store 事务提交，例如 thread + trace、turn + input item + trace、approval request + trace、approval decision + ledger + trace。approval decision trace 使用原 approval request 的 `session_id` / `task_id` 做关联，不用 `request_id` 冒充 session。

`crates/app-server` 实现 `initialize` / `initialized` handshake、server capabilities、thread list/read/start/resume/fork/archive/delete、turn start/interrupt/status、event subscription、approval list/center/request/decision、artifact fetch、trace list/show/tail 和 `server/shutdown`。默认 `turn/start` 明确写入 `agent_loop_status = "not_migrated"`，不伪装 Python AgentLoop 已完成迁移；`turn/start` 会先校验 Rust store 中的 thread 存在，missing thread 直接返回 `Thread not found`，不会启动 Python sidecar 或写 sidecar trace。显式设置 `SINGULARITY_PYTHON_SIDECAR=1` 且 thread 校验通过后，app-server 才通过 `PythonSidecarClient` 启动 `python -m singularity.agent_host.sidecar`，由 sidecar 调用现有 `AgentHost -> KernelBootstrap -> AgentKernel -> AgentLoop` 并把安全状态摘要翻译成 Rust turn/item/trace。stdio binary transport 的错误行统一通过 `JsonRpcMessage::error()` / `serde_json` 序列化为合法 JSON-RPC error envelope。item streaming 使用 `item/agentMessage/delta` 和 `item/commandExecution/outputDelta` 这类 typed delta，不再使用 generic `item/delta`。当前 `server/capabilities` 只声明 stdio transport 可用，WebSocket token transport 仍是未来 transport，显式返回 unavailable。

`crates/cli` 提供 `sg` CLI。`sg run` / `sg chat` / `sg continue` / `sg threads` / `sg trace` / `sg approvals` / `sg config doctor` 会启动或调用 stdio app-server，并只通过 JSON-RPC protocol 交换 initialize、thread、turn、trace 和 approval 请求；请求读取以 matching response id 为完成条件，保留 matching response 之前到达的 notification，不依赖固定 notification 数量，也不在 response 后 drain 额外消息。stdout 关闭、app-server 提前退出或 response timeout 都返回明确错误，不无限等待。`sg daemon` 启动 app-server stdio 进程。CLI 不直接依赖 `singularity_agent`、`singularity_model`、`singularity_tools` 或 `singularity_store`。

`crates/tools` 提供 Rust `ToolBroker`。所有工具在模型可见前必须先注册到 `ToolRegistry`，工具名只能使用 `builtin.*`、`mcp.<server>.<tool>` 或 `python.<plugin>.<tool>`；`ToolBroker.model_visible_tools()` 只投影 name、redacted description 和 input schema；`ToolBroker.execute()` 对 unknown 或 denied tool 不调用 executor，并通过 `ToolObservation.to_model_payload()` 输出安全摘要。

`crates/policy` 提供 Rust `PolicyEngine` 纯决策边界。`PermissionRule` 按 `SettingsScope` 的 managed、user、project、local 顺序匹配，并按 hook observation -> deny rules -> caller-marked sensitive resource deny -> ask rules -> allow rules -> fallback ask 的顺序得到 `PermissionDecision`。`approval_policy = never` 会把 ask 转成 deny。当前 Rust policy 只消费调用方已经归一化好的 `PermissionRequest.resource` 和 `resource_sensitive` 安全分类，不解析 shell wrapper、argv、workspace path、network host、`.env` / SSH key marker 或命令等价形式；这些上下文归一化与 enforcement 属于 command/sandbox 或上层 tool boundary。当前 Rust policy 仍是 app-server/tool boundary 的纯决策对象，完整运行时 policy audit writer 和 Python AgentLoop 执行强制链尚未被 Rust 替换。

`crates/model` 提供 Rust model boundary schema 和纯验证函数。它对齐现有 Python `singularity.model` 的 `ModelTurnRequest`、`ModelTurnResponse`、tool schema、tool call、capability metadata、provider config presence、stream event、validation result 和 model error object；`validate_provider_config()`、`validate_stream_events()`、`validate_model_response()` 和 `classify_model_error()` 只做边界验证与分类，不发起 HTTP provider call、不重试、不执行工具、不做 planner repair，也不迁移 AgentLoop。

`crates/agent` 当前除 Python sidecar bridge 外，只持有 M9 的 schema/parity 切片 `PlannerStateBoundary`、`ContextAssemblyBoundary`、`ContextSummaryEnvelopeBoundary`、`ToolCallRepairBoundary`、`FinalizationMappingBoundary` 和 `NativeAgentLoopCapability`。这些对象只从 Python oracle fixture 做 planner state、context bundle、compaction summary envelope、tool-call repair contract 与 finalization mapping JSON roundtrip，或显式声明 native Rust AgentLoop 尚不可用；它们不调用 provider、不执行工具、不写 workspace，也不让 Rust `turn/start` 声称 native AgentLoop completed。repair planner runtime、completion gate runtime、finalizer runtime 和 max-turns loop 仍是后续独立切片，不能和当前切片一起重写。

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

第一阶段必须通过：

- `python scripts/verify_rust_migration_boundaries.py`
- `cargo fmt --check`
- `cargo check --workspace --all-targets`
- `cargo test --workspace`
- `python scripts/export_rust_parity_fixtures.py`
- `python -m pytest tests/test_rust_parity_fixtures.py -q`
- `python scripts/verify_runtime_docs.py`

如果 provider 配置存在，还要通过当前 Python 路径运行真实 AgentLoop evaluation：

```text
python -m singularity.cli eval run docs/evaluation/public-representative-task.json --run-id rust-host-phase1-parity --json
```

该 evaluation 仍通过 `KernelBootstrap -> AgentGraphBuilder -> AgentKernel -> AgentLoop.run`，用于证明 Python oracle 仍可运行；它不是 Rust AgentLoop 迁移完成证明。

## 下一阶段顺序

下一阶段应按风险和边界厚度迁移：

1. tool registry / tool observation / safe model payload。
2. strict sandbox backend execution contract；当前 `crates/sandbox` 只保留 command request/result、backend descriptor、`PatchChange` / `PatchResult` schema contract 和 capability schema，不公开 relaxed local-process executor 或 host filesystem mutation executor。
3. model turn request/response adapter。
4. planner / context / repair / finalization parity contract 继续按一个行为切片一组 fixture 推进；repair 和 finalization 已有 schema parity，但 runtime 仍未迁移。
5. 更完整的 TUI 渲染和长期 app-server lifecycle 管理。
6. 最后才迁移 AgentLoop orchestration。

## 维护规则

Rust app-server protocol 是唯一富客户端边界。任何 desktop/TUI 新能力都必须先通过 `crates/protocol` 和 `crates/app-server` 暴露，再由 client 消费；不能绕过 app-server 直接调用 core 或 Python runtime。

M0 之后，`scripts/verify_rust_migration_boundaries.py` 是提交和 CI 的迁移漂移检查入口。它检查 `crates/cli` 不能直接依赖 agent/model/tools/store，crate 依赖必须留在显式 allowlist，Python runtime 改动只能落在 sidecar/oracle/fixture/parity 允许路径，仓库不能提前出现 desktop/Web 启动文件，`turn/start` 在 Rust AgentLoop 迁移前必须保持 `agent_loop_status = "not_migrated"`，`crates/sandbox` 不得公开 relaxed/no-sandbox contract 或 local-process executor，app-server stdio error 不得手拼 JSON，CLI 不得恢复固定 notification 等待或 post-response drain，store 不得恢复重复 approval decision public API，CLI/app-server 不得声明 unused tokio，并且 Rust `ToolObservation.to_model_payload()` 不得输出 raw arguments、内部 approval/policy id、internal metadata 或明显 secret-like 文本。
