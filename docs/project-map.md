# Harness 项目地图

这份文件是给 agent 的第一入口。目标不是解释全部实现，而是用最少读取量把后续阅读收敛到正确的代码区。

## 先读顺序

1. `docs/project-map.md`
2. `README.md`
3. 对应的 `docs/architecture/*.md`
4. 对应的 `src/miniharness/...`
5. 最后只看相关 `tests/...`

## 一条主线

```text
pyproject.toml
└─ miniharness
   └─ src/miniharness/cli.py
      └─ src/miniharness/agent.py
         ├─ src/miniharness/context/
         ├─ src/miniharness/instructions/
         ├─ src/miniharness/model/
         ├─ src/miniharness/observability/
         ├─ src/miniharness/planner/
         ├─ src/miniharness/tools/
         ├─ src/miniharness/command/
         ├─ src/miniharness/verification/
         ├─ src/miniharness/workspace/
         ├─ src/miniharness/workspace_state/
         ├─ src/miniharness/policy/
         └─ src/miniharness/trace.py
```

## 目录速查

- `pyproject.toml`: 入口、依赖、脚本名。
- `README.md`: 当前版本的高层说明、运行方式、主流程。
- `docs/architecture/`: 各 runtime 的边界说明。
- `src/miniharness/`: 真实实现。
- `tests/`: 行为规格和回归判断。

### `src/miniharness/` 里各目录的职责

- `cli.py`: 命令入口、会话启动/恢复、runtime 组装。
- `agent.py`: model loop、context 注入、tool dispatch、最终收口。
- `provider.py`: 旧 OpenAI-compatible provider 兼容入口。
- `trace.py`: 旧 JSONL trace 兼容入口。
- `model/`: 模型调用运行时、provider registry、message/tool 转换、validation、budget、retry、streaming。
- `observability/`: 结构化 TraceRuntime、TraceStore、artifact、span、timeline、summary、redaction。
- `context/`: token budget、上下文组装、观察记录、压缩与恢复。
- `instructions/`: 指令来源、指令层级、信任等级、prompt injection 检测、PromptBundle 编译、PromptManifest。
- `planner/`: task state、phase、evidence、budget、completion。
- `tools/`: tool registry、tool runtime、read-only / mutation / command / verification tool wiring。
- `command/`: 进程执行、policy、输出、环境、后台过程。
- `verification/`: 项目探测、check 规划、失败解析、修复、完成判断。
- `workspace/`: 文件修改 runtime、pathing、snapshot、diff、rollback。
- `workspace_state/`: baseline、journal、artifact、ownership、健康状态、恢复。
- `policy/`: 风险分类、审批、审计、规则。

## 任务路由

| 任务 | 先看 | 再看 |
|---|---|---|
| 启动、入口、运行方式 | `README.md`, `src/miniharness/cli.py`, `src/miniharness/agent.py` | `tests/test_cli.py`, `tests/test_agent.py` |
| 模型调用、provider、tool call 校验 | `src/miniharness/model/`, `src/miniharness/provider.py`, `tests/test_model_*.py` | `docs/architecture/model-inference-runtime.md`, `tests/test_provider.py`, `tests/test_agent.py` |
| 指令来源、prompt 编译、prompt injection | `src/miniharness/instructions/`, `tests/test_instruction_*.py`, `tests/test_prompt_*.py` | `docs/architecture/instruction-prompt-runtime.md`, `src/miniharness/model/runtime.py`, `src/miniharness/agent.py` |
| 上下文账本、预算、压缩、恢复、引用、redaction | `src/miniharness/context/`, `tests/test_context*.py` | `README.md` 的 Context Manager 段 |
| 规划、阶段、完成条件 | `src/miniharness/planner/`, `tests/test_planner_runtime.py` | `docs/architecture/planner-task-execution-runtime.md` |
| 验证、检测、失败解析、修复 | `src/miniharness/verification/`, `tests/test_verification_runtime.py` | `docs/architecture/verification-runtime.md` |
| 命令执行、进程、输出、环境 | `src/miniharness/command/`, `tests/test_command_runtime.py` | `docs/architecture/command-runtime.md` |
| 工作区修改与回滚 | `src/miniharness/workspace/`, `tests/test_workspace_mutation.py` | `docs/architecture/workspace-mutation-runtime.md` |
| 工作区状态、基线、恢复、健康面板 | `src/miniharness/workspace_state/`, `tests/test_workspace_state_runtime.py` | `docs/architecture/local-workspace-state-runtime.md` |
| 策略、审批、风险 | `src/miniharness/policy/`, `tests/test_policy_*.py`, `tests/test_approval_gate.py` | `docs/architecture/policy-approval-runtime.md` |
| tool 注册与分发 | `src/miniharness/tools/`, `tests/test_tools.py`, `tests/test_tool_runtime.py` | `README.md` 的 Tool Runtime 段 |
| 可观测性、trace、artifact | `src/miniharness/observability/`, `tests/test_trace_*.py`, `tests/test_observability_*.py` | `docs/architecture/observability-trace-runtime.md` |

## 默认不读

除非正在排查运行结果、缓存、临时文件或会话残留，否则默认跳过：

- `.miniharness/`
- `.venv/`
- `.pytest_cache/`
- `outputs/`
- `work/`

这些目录更像运行产物，不是理解代码结构的第一信息源。

## 省 token 规则

- 先按上面的任务路由定位到单个子系统，不要先扫完整个 `src/`。
- 只在需要验证行为时再看对应 `tests/`，不要把所有测试文件都读一遍。
- 同一问题优先读 `README.md` + 1 个 architecture 文档 + 1 组源码文件，够了就停。
- 如果要更新这份地图，优先补路由和入口，不要把它写成另一份长 README。
