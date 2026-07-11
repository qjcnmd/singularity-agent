# Singularity 仓库指令

## 事实入口与范围

1. 先读取 `.codex/repo-map.json` 定位最小相关 Rust crate、符号和测试；该文件不存在或明显过期时，使用本地 `repo-mapping` skill 刷新，但不要提交。
2. 先读实现、调用方、配置和相邻测试，再修改。不要把报告、旧文档或历史提交当作当前事实。
3. 默认不读取 `.git/`、`.singularity/`、`target/`、`work/` 或其他运行产物，除非任务明确涉及 Git、缓存、产物或环境诊断。
4. 不读取、输出或提交 `.env` 中的敏感值；环境检查只报告脱敏的 present/missing 状态。

## 命令与磁盘

1. Windows 上所有仓库命令必须通过 PowerShell 7（`pwsh.exe`）执行，不使用 Windows PowerShell 5.1。
2. Rust 构建和测试优先为单次命令设置 `CARGO_TARGET_DIR` 到空间充足的非系统盘，并复用同一目录；不要修改用户的全局 Cargo 配置。
3. 尽量不占用 C 盘。任务完成后，默认删除本次产生且可重建的 Cargo target、临时 evaluation、测试缓存、日志、临时工作树和一次性中间文件；用户明确要求保留时除外。
4. 删除或移动目录前先解析并校验绝对路径位于当前工作区或本次明确指定的临时目录。不得删除源码、用户数据、任务开始前已存在且归属不明的产物。
5. 最终回复说明已清理的产物，以及因交付或后续验证而保留的内容。

## Rust-only 边界

1. Singularity 的实现、构建、测试、持续集成（CI）和发布链路只使用 Rust。不要新增 Python runtime、包、脚本、测试、oracle、parity fixture、sidecar 或兼容入口。
2. `sg` 只通过 stdio JSON-RPC 调用 `singularity_app_server`；CLI 不直接依赖 agent、model、tools 或 store crate。
3. 当前工作树只保留当前真实结构。历史命名、schema、CLI、环境变量和迁移说明由 Git 历史保存，不新增兼容垫片、弃用别名、迁移读取入口或旧路径 re-export。
4. Evaluation 使用 `evaluation`、`eval`、`task`、`task set`、`runner`、`result`、`report` 等主流命名，不恢复迁移期自造分类。

## 运行时与安全

1. 主链路为 `sg -> AppServer -> AgentLoop -> ToolBroker -> WorkspaceTools -> SandboxBackend -> SessionStore`。
2. sandbox 保持 fail closed，并复用仓库内来自 Codex 的 Windows restricted-token、Job Object 和 elevated helper 实现。不得增加 local-process、no-sandbox 或 relaxed fallback。
3. `workspace-write` 下的命令必须在严格 sandbox 内执行；网络默认拒绝。权限、approval、protected path、cwd canonicalization 和越界写入检查不得弱化。
4. 取消必须传播到 provider 和在途 sandbox command；取消请求之后的晚到 completion、assistant item 或 terminal trace 不得覆盖 interrupted 状态。
5. provider 原始响应、prompt、tool raw arguments、环境变量、密钥和内部 audit metadata 不得进入公共 CLI、model tool payload 或未脱敏 trace。

## 文档

1. `docs/singularity.md` 是唯一架构事实文档，只描述当前 Rust 源码中的 crate 边界、对象、调用链、持久化和失败路径。
2. 主链路、协议、状态映射、sandbox、approval、provider、evaluation、trace 或 store 变化时，同步更新 `docs/singularity.md` 的相关部分。
3. 不恢复 `docs/architecture/modules/`、迁移报告、阶段报告、旧路线图或 Python 时代文档。

## 验证

根据影响范围运行以下检查；完整收口必须全部执行：

```text
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings
cargo test --workspace --all-targets --locked --no-fail-fast
git diff --check
```

影响 AgentLoop、provider、工具、sandbox、approval、evaluation、trace 或 completion 的改动还必须运行至少一次真实 provider 验证：

```text
cargo run -p singularity_cli --bin sg -- eval run docs/evaluation/public-representative-task.json --run-id <meaningful-run-id> --json
```

真实验证必须进入 Rust AgentLoop，不能用 fake、mock、scripted provider 或直接调用内部组件替代。无法运行时明确分类 `.env`、配置、认证、网络、模型、sandbox、runtime 或 verification blocker，不得声称完全验证。

## Git

1. 修改前确认仓库、分支、工作树和用户未提交改动；不覆盖无关内容。
2. 验证通过后创建范围单一、信息明确的本地提交。
3. 未经用户明确要求不得 push、merge、rebase、reset、删除 stash 或改写历史。

## Agent skills

### Issue tracker

任务和需求记录在 GitHub Issues。详见 `docs/agents/issue-tracker.md`。

### Triage labels

使用默认的五类 triage 标签。详见 `docs/agents/triage-labels.md`。

### Domain docs

单上下文（single-context）布局；现有仓库指令和当前架构文档优先。详见 `docs/agents/domain.md`。
