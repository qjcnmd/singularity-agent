# Singularity v0.1.0

Singularity 是本地优先的 CLI coding agent harness。当前工作树只描述当前 Python 源码中真实存在的结构、命令、schema、trace 和评估入口；历史设计、路线图和阶段报告由 git history 保留。

## 当前身份

- 产品名：`Singularity`
- Python 包：`singularity`
- CLI 入口：`singularity-agent`、`sg`
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

## CLI

运行一次真实 agent：

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
singularity-agent eval run docs/evaluation/capability-regression-tasks.json --json
singularity-agent eval provider-smoke --json
```

`benchmark` 当前仍作为 `eval` 的同义命令组存在。新增 evaluation 文档、manifest 和 CLI 示例必须使用 `eval` / `evaluation` / `benchmark` / `task` / `task set` / `result` / `report` / `runner` 等主流命名，不得恢复 `live` 命名。

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

- `capability-regression-tasks.json`
- `capability-minimal-tasks.json`
- `capability-fix-math-test-only.json`
- `evaluation-baseline-example.json`

真实模型评估命令：

```bash
python -m singularity.cli eval run docs/evaluation/capability-regression-tasks.json --run-id <run-id> --json
```

评估结果使用当前字段：

- `agent_completed`：AgentLoop / FinalReport 自认为完成
- `evaluation_passed`：独立 evaluator 判定通过
- `miscompletion_count`：`agent_completed and not evaluation_passed`

不要把 `evaluation_passed` 写回旧的 `completed` / `success` result alias。

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
