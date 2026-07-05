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

`crates/protocol` 定义 JSON-RPC envelope、method params/result、`Thread`、`Turn`、`Item`、`TraceEvent` 和 app-server event。JSON-RPC params 使用 camelCase，例如 `clientInfo`、`threadId`；嵌入领域对象继续使用当前 Python parity schema 的 snake_case。

`crates/store` 是 SQLite-backed persistence boundary。它持久化 thread、turn、item、trace event 和 approval pending/decision；`SessionStoreDescriptor` 负责可序列化 store schema 描述，真实 `SessionStore` 持有 SQLite connection。

`crates/app-server` 实现 `initialize` / `initialized` handshake、`thread/start`、`turn/start`、`approval/request`、`approval/decision`、`trace/list`、`trace/show`。`turn/start` 明确写入 `agent_loop_status = "not_migrated"`，不伪装 Python AgentLoop 已完成迁移。

`crates/cli` 只通过 app-server protocol 生成 JSON-RPC request。它不直接依赖 `singularity_agent`、`singularity_model`、`singularity_tools` 或 `singularity_store`。

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

1. trace event append/list/show 与 artifact reference。
2. approval pending store、decision ledger 和 policy projection。
3. tool registry / tool observation / safe model payload。
4. command request/result 与 sandbox capability contract。
5. model turn request/response adapter。
6. app-server client lifecycle 和 CLI/TUI 子进程管理。
7. 最后才迁移 AgentLoop orchestration。

## 维护规则

Rust app-server protocol 是唯一富客户端边界。任何 desktop/TUI 新能力都必须先通过 `crates/protocol` 和 `crates/app-server` 暴露，再由 client 消费；不能绕过 app-server 直接调用 core 或 Python runtime。

M0 之后，`scripts/verify_rust_migration_boundaries.py` 是提交和 CI 的迁移漂移检查入口。它检查 `crates/cli` 不能直接依赖 agent/model/tools/store，crate 依赖必须留在显式 allowlist，Python runtime 改动只能落在 sidecar/oracle/fixture/parity 允许路径，仓库不能提前出现 desktop/Web 启动文件，`turn/start` 在 Rust AgentLoop 迁移前必须保持 `agent_loop_status = "not_migrated"`，并且 Rust `ToolObservation.to_model_payload()` 不得输出 raw arguments、内部 approval/policy id、internal metadata 或明显 secret-like 文本。
