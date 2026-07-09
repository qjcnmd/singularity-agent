# Singularity v0.1.0

Singularity 是本地优先的 CLI coding agent harness。当前 public runtime 是 Rust `sg` -> app-server JSON-RPC -> Rust AgentLoop；Python 代码只保留为 Python oracle/parity/dev-only 对照，不作为普通运行方式。历史设计、路线图和阶段报告由 git history 保留。

## 当前身份

- 产品名：`Singularity`
- Rust public runtime：Cargo/build artifact 中的 `sg`，通过 `crates/app-server` 的 JSON-RPC 协议运行
- Python 包：`singularity`，仅用于 internal oracle / parity fixture / dev-only 维护；`pyproject.toml` 不安装 Python console script
- 环境变量前缀：`SINGULARITY_`
- 项目本地状态目录：`.singularity/`

## 源码入口

- Rust CLI：`crates/cli/src/main.rs`
- Rust app-server：`crates/app-server/src/lib.rs`
- Rust protocol：`crates/protocol/src/lib.rs`
- Rust AgentLoop：`crates/agent/src/lib.rs`
- Rust tools / policy / sandbox / model / store：`crates/tools/`、`crates/policy/`、`crates/sandbox/`、`crates/model/`、`crates/store/`
- Python oracle/parity/dev-only 参考：`src/singularity/`，用于对照、fixture export 和迁移期验证，不作为 public runtime

核心模块数据流文档位于 `docs/architecture/modules/`。这些文档以源码为唯一事实来源，列出当前对象完整字段、调用链、生成者、消费者、落盘路径、trace/audit 行为、失败路径和维护规则。

## 基本安装

Python editable install 只安装内部库和测试依赖，不提供 public CLI。Rust public runtime 通过 Cargo 构建并运行 `sg`。

OpenAI-compatible provider 通过环境变量配置。API key 不接受 CLI 参数。

PowerShell：

```powershell
$env:SINGULARITY_BASE_URL = "https://api.openai.com/v1"
$env:SINGULARITY_API_KEY = "..."
$env:SINGULARITY_MODEL = "gpt-4.1-mini"
```

cmd.exe：

```bat
set SINGULARITY_BASE_URL=https://api.openai.com/v1
set SINGULARITY_API_KEY=...
set SINGULARITY_MODEL=gpt-4.1-mini
```

POSIX shell：

```bash
export SINGULARITY_BASE_URL=https://api.openai.com/v1
export SINGULARITY_API_KEY=...
export SINGULARITY_MODEL=gpt-4.1-mini
```

配置优先级：

```text
显式 CLI 参数 > SINGULARITY_* 环境变量 > .singularity/config.toml > 默认值
```

## Permission 与 Sandbox 边界

会话权限通过行业通用的 permission profile 表达：

- `read-only`、`workspace-write`、`danger-full-access`描述会话 filesystem 边界。
- `--add-dir`显式增加可写目录，不要求提升为`danger-full-access`。
- `on-request`允许高风险动作进入审批；`never`把需要审批的动作转为拒绝。
- protected paths由Policy、command、tool和workspace mutation边界执行，不能由模型提供的参数关闭。

当前 sandbox 只注册OS-native方向的`WindowsSandboxBackend`。Windows实现是account-backed OS sandbox：elevated setup创建并加固`SingularityOffline`与`SingularityOnline`两个专用本地账户、独立Credential Manager凭据、登录UI隐藏项、logon rights、受限Users组成员关系和state dir ACL；`network=denied`只使用受account-scoped outbound firewall约束的offline账户，`network=allowed`只使用不被该规则命中的online账户。runner随后用restricted low-integrity token、private desktop和Job Object运行命令。该结构只对齐OpenAI Codex公开的dedicated principal与firewall设计原则，不声称实现与Codex App相同。缺少任一doctor/setup/execution能力时仍报告`backend_unavailable`并fail closed，不回退到普通本地进程；workspace projection或文件复制也不会被表述为强隔离。非Windows平台在实现对应native backend前同样明确不可用。

## CLI

Rust `sg` 是 public runtime 入口。`sg run`、`sg chat`、`sg continue` 和 `sg eval run` 只通过 app-server JSON-RPC 进入 Rust AgentLoop；`turn/start` 没有公开后端选择字段。Rust capability 不满足时 fail closed，并返回结构化 blocker，而不是切到 Python。target-project Python commands 仍然可以作为项目验证命令经 Rust sandbox 执行，例如模型或 evaluation runner 可以运行 `python -m pytest` 来验证目标仓库。

```bash
cargo run -p singularity_cli --bin sg -- run "inspect the current project" --model gpt-4.1-mini
cargo run -p singularity_cli --bin sg -- continue <thread-id> "follow up"
cargo run -p singularity_cli --bin sg -- turn status <turn-id>
cargo run -p singularity_cli --bin sg -- trace <thread-or-run-id> --limit 20
cargo run -p singularity_cli --bin sg -- trace show <event-id>
cargo run -p singularity_cli --bin sg -- approvals
cargo run -p singularity_cli --bin sg -- approve <request-id> --decision allow --reason "operator approved"
cargo run -p singularity_cli --bin sg -- eval run docs/evaluation/public-representative-task.json --run-id <run-id> --json
```

Python oracle/parity/dev-only 路径仍用于迁移期对照、fixture export 和 schema parity，不作为普通运行方式，也不作为 Rust public runtime proof。

`eval` 是当前唯一的评估 CLI 命令组；`benchmark` 只表示基准任务、任务集或报告这一领域概念，不是 CLI alias。新增 evaluation 文档、manifest 和 CLI 示例必须使用 `eval` / `evaluation` / `benchmark` / `task` / `task set` / `result` / `report` / `runner` 等主流命名，不得恢复 `live` 命名。

## 运行链路

```text
Rust sg
-> AppServerClient
-> JSON-RPC over stdio
-> AppServer.handle_json()
-> SessionStore thread/turn/trace transaction
-> AgentLoopCapability gate
-> Rust AgentLoop.run()
-> OpenAiProvider
-> ToolBroker / PolicyEngine / WorkspaceTools / SandboxBackend
-> AgentLoopResult
-> Turn / Item / TraceEvent / Approval / ArtifactRef
```

## Evaluation

当前公开 manifest 位于 `docs/evaluation/`：

- `public-representative-task.json`

真实模型评估命令：

```bash
cargo run -p singularity_cli --bin sg -- eval run docs/evaluation/public-representative-task.json --run-id <run-id> --json
```

评估结果使用当前字段：

- `agent_completed`：AgentLoop / FinalReport 自认为完成
- `evaluation_passed`：独立 evaluator 判定通过
- `miscompletion_count`：`agent_completed and not evaluation_passed`
- `evaluation_metrics`：诊断 scorecard，包含 resolved、FAIL_TO_PASS/PASS_TO_PASS、verification、patch、trajectory、tools、context/compaction、efficiency、cost 和 safety；不改变硬通过语义

不要把 `evaluation_passed` 写回旧的 `completed` / `success` result alias。capability gate 仍只以现有 evaluator 结果和命令退出码作为硬判断；`evaluation_metrics`、cost 和 pricing unknown 只用于诊断/回归分析，不会让 gate 通过或失败。

Phase 8 后本地验证分为三层，CI Quality matrix 不降级，既有全量测试仍保留：

- `fast` gate：Codex 日常小改默认运行 `python scripts/verify_fast.py --git`。它执行 ruff、当前 mypy、changed-scope compileall 和 `scripts/test_impact.py` 推荐的受影响 pytest；低置信度或无明确测试时输出 `fallback_required=stage` 与 `skipped_reason`，不静默跳过，不跑真实 provider eval。
- `stage` gate：阶段收口运行 `python scripts/verify_stage.py`。它执行 deterministic mypy/ruff/compileall/runtime docs、过滤后的 pytest 和关键模块专项测试，不默认跑真实 provider eval。
- `capability` gate：只有 AgentLoop、ToolProtocol、sandbox、context、compaction、verification、CompletionGate、FinalReport 或 evaluation runner 变更时运行 `python scripts/verify_capability.py --force --run-id <run-id>`，默认使用单个公共任务 `docs/evaluation/public-representative-task.json`，并在 JSON 输出中附带 `evaluation_metrics` 摘要。

公共代表性任务来自 SWE-bench Lite dev split：`sqlfluff__sqlfluff-2419`，repo 为 `sqlfluff/sqlfluff`，base commit 为 `f1dba0e1dd764ae72d67c3d5e1471cf14d3db030`，FAIL_TO_PASS 目标为 `test/rules/std_L060_test.py::test__rules__std_L060_raised`。manifest 只把 issue 摘要、允许范围、模型可见 local smoke 和完成标准交给模型；evaluator `test_patch` 只在 baseline/verification workspace 中应用，gold patch 不存储也不进入 `ModelTurnRequest`。

## 运行时状态

默认 trace run 目录结构：

```text
<trace-run-dir>/
  events.jsonl
  spans.jsonl
  artifacts.jsonl
  artifacts/
  context.sqlite3
  tool_protocol.sqlite3
```

默认 context 数据库路径是 `<trace-run-dir>/context.sqlite3`，默认工具协议状态库路径是 `<trace-run-dir>/tool_protocol.sqlite3`。

这些文件是运行时状态，不是源码 fixture。生成物应位于 `work/`、`.singularity/` 或测试临时目录中，不应提交。

## 文档维护

- 核心模块文档只放在 `docs/architecture/modules/`。
- 修改核心运行对象、字段、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新对应模块数据流文档。
- 展示真实对象时必须列完整字段，不允许只列子集。
- 不允许为了文档或测试增加无运行价值字段。
- 不保留旧阶段报告、旧优先级报告、旧生产审查报告、旧路线图报告、旧 ADR、旧评估兼容清单、旧运行时文档或兼容说明。

校验命令：

```bash
python scripts/verify_runtime_docs.py
```

## 开发验证

```bash
python -m ruff check .
python -m mypy
python -m compileall src scripts
python scripts/verify_runtime_docs.py
python -m pytest tests --basetemp work/pytest-tmp
```

`python -m mypy` 是 `pyproject.toml` 中声明的聚焦类型检查。`python -m mypy src/singularity` 是全包类型债检查，若未通过需要单独报告。
