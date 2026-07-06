# Singularity v0.1.0

Singularity 是本地优先的 CLI coding agent harness。当前工作树只描述当前 Python 源码中真实存在的结构、命令、schema、trace 和评估入口；历史设计、路线图和阶段报告由 git history 保留。

## 当前身份

- 产品名：`Singularity`
- Python 包：`singularity`
- Rust CLI-first 入口：Cargo/build artifact 中的 `sg`，通过 `crates/app-server` 的 JSON-RPC 协议运行
- Python legacy/oracle 入口：`singularity-agent`、Python console script `sg`
- 环境变量前缀：`SINGULARITY_`
- 项目本地状态目录：`.singularity/`

## 源码入口

- CLI：`src/singularity/cli.py`
- KernelBootstrap（内核启动）：`src/singularity/kernel/bootstrap.py`
- AgentGraphBuilder（智能体图构建）：`src/singularity/kernel/graph.py`
- AgentKernel（智能体内核）：`src/singularity/kernel/agent_kernel.py`
- AgentLoop（智能体主循环）：`src/singularity/agent_loop.py`
- ModelRunner（模型运行器）：`src/singularity/model/runner.py`
- PromptAssemblyPipeline（提示词组装管线）：`src/singularity/instructions/prompt_assembly.py`
- ContextManager（上下文管理器）：`src/singularity/context/manager.py`
- ToolProtocolEngine（工具协议引擎）：`src/singularity/tool_protocol/engine.py`
- ToolExecutor（工具执行器）：`src/singularity/tools/executor.py`
- PolicyEngine（策略引擎）：`src/singularity/policy/engine.py`
- ApprovalGate（审批闸门）：`src/singularity/policy/approval.py`
- CommandExecutor（命令执行器）：`src/singularity/command/executor.py`
- SandboxManager（沙箱管理器）：`src/singularity/sandbox/manager.py`
- VerificationRunner（验证运行器）：`src/singularity/verification/runner.py`
- EvaluationRunner（评估运行器）：`src/singularity/evaluation/runner.py`

核心模块数据流文档位于 `docs/architecture/modules/`。这些文档以源码为唯一事实来源，列出当前对象完整字段、调用链、生成者、消费者、落盘路径、trace/audit 行为、失败路径和维护规则。

## 基本安装

```bash
pip install -e .
```

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

```bash
singularity-agent "inspect and verify this project" \
  --permission-profile workspace-write \
  --approval-policy on-request \
  --network-access denied \
  --add-dir ../shared-output \
  --windows-sandbox elevated
```

- `read-only`、`workspace-write`、`danger-full-access`描述会话 filesystem 边界。
- `--add-dir`显式增加可写目录，不要求提升为`danger-full-access`。
- `on-request`允许高风险动作进入审批；`never`把需要审批的动作转为拒绝。
- protected paths由Policy、command、tool和workspace mutation边界执行，不能由模型提供的参数关闭。

当前 sandbox 只注册OS-native方向的`WindowsSandboxBackend`。Windows实现是account-backed OS sandbox：elevated setup创建并加固`SingularityOffline`与`SingularityOnline`两个专用本地账户、独立Credential Manager凭据、登录UI隐藏项、logon rights、受限Users组成员关系和state dir ACL；`network=denied`只使用受account-scoped outbound firewall约束的offline账户，`network=allowed`只使用不被该规则命中的online账户。runner随后用restricted low-integrity token、private desktop和Job Object运行命令。该结构只对齐OpenAI Codex公开的dedicated principal与firewall设计原则，不声称实现与Codex App相同。缺少任一doctor/setup/execution能力时仍报告`backend_unavailable`并fail closed，不回退到普通本地进程；workspace projection或文件复制也不会被表述为强隔离。非Windows平台在实现对应native backend前同样明确不可用。

## CLI

Rust CLI-first 路径通过 app-server 协议运行。默认只创建 Rust thread/turn 并显示 `agent_loop_status=not_migrated`；需要进入当前 Python AgentLoop sidecar 时使用 `--agent-host python`，不要求手动设置 `SINGULARITY_PYTHON_SIDECAR=1`：

```bash
cargo run -p singularity_cli --bin sg -- run "inspect the current project" --model gpt-4.1-mini --agent-host python
cargo run -p singularity_cli --bin sg -- continue <thread-id> "follow up" --agent-host python
cargo run -p singularity_cli --bin sg -- turn status <turn-id>
cargo run -p singularity_cli --bin sg -- trace <thread-or-run-id> --limit 20
cargo run -p singularity_cli --bin sg -- trace show <event-id>
cargo run -p singularity_cli --bin sg -- approvals
cargo run -p singularity_cli --bin sg -- approve <request-id> --decision allow --reason "operator approved"
```

M1 的 Python sidecar 调用是同步 app-server request：`turn/start` 会先把 Rust turn、user item 和 turn trace 写入 SQLite，再调用 `agent/run`；同一 Rust thread 上已有 `python_sidecar` trace 的 `session_id` 时，后续 `sg continue --agent-host python` 调用 `agent/resume`。`--model` 写入 Rust thread，并作为 sidecar `model` 参数进入 Python `ProductionConfig`。默认 no-sidecar 路径只显示 `not_migrated` turn，不输出伪 assistant delta。

Python legacy/oracle 路径仍用于迁移期对照和真实 evaluation。运行一次 Python agent：

```bash
singularity-agent "inspect the current project" \
  --project-root . \
  --max-turns 12 \
  --approval-mode auto_safe \
  --trace-dir work/traces/runs \
  --context-db work/traces/runs/session/context.sqlite3 \
  --model gpt-4.1-mini \
  --base-url https://api.openai.com/v1 \
  --no-raw-artifacts \
  --dry-run \
  --strict
```

常用命令：

```bash
singularity-agent doctor --json
singularity-agent repair --dry-run --json
singularity-agent trace list --trace-dir work/traces/runs
singularity-agent trace show <run_id> --trace-dir work/traces/runs
singularity-agent index build --json
singularity-agent git status --json
singularity-agent memory list
singularity-agent approval remote export-request request.json decision.json --output approval-request.json --json
singularity-agent plugin list --json
singularity-agent eval run docs/evaluation/public-representative-task.json --json
singularity-agent eval provider-smoke --json
```

`eval` 是当前唯一的评估 CLI 命令组；`benchmark` 只表示基准任务、任务集或报告这一领域概念，不是 CLI alias。新增 evaluation 文档、manifest 和 CLI 示例必须使用 `eval` / `evaluation` / `benchmark` / `task` / `task set` / `result` / `report` / `runner` 等主流命名，不得恢复 `live` 命名。

## 运行链路

```text
CLI
-> KernelBootstrap.boot()
-> AgentGraphBuilder.build()
-> AgentKernel.run_task()
-> AgentLoop.run()
-> RunController.start()
-> Planner.step()
-> ModelRunner.build_request_from_context()
-> ModelTurnRequestBuilder.build_request()
-> PromptAssemblyPipeline.build_for_model_turn()
-> ContextManager.build_bundle()
-> ModelRunner.run_turn()
-> ToolProtocolEngine.process_model_turn()
-> ToolExecutor.execute_request()
-> PolicyEngine.enforce() / ApprovalGate
-> WorkspaceMutationManager / CommandExecutor / VerificationRunner
-> ContextManager.add_tool_protocol_result()
-> WorkspaceStateManager
-> TraceRecorder / AuditLog / FinalReport
```

## Evaluation

当前公开 manifest 位于 `docs/evaluation/`：

- `public-representative-task.json`

真实模型评估命令：

```bash
python -m singularity.cli eval run docs/evaluation/public-representative-task.json --run-id <run-id> --json
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
