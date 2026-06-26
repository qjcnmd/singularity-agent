# Singularity Agent 指令

## 仓库事实入口

处理本仓库任务前，先读取 `.codex/repo-map.json`，用它定位最小相关文件集，不要默认全仓扫描。根据 repo map 判断入口、类、函数、导入导出和相邻测试；只有范围明确后再读源码。`.codex/repo-map.json` 是本地缓存，除非缺失、过期或明显不一致，否则不要刷新；刷新需要使用本地 `repo-mapping` skill，且不要提交该文件。

默认不要读取这些路径，除非任务明确涉及运行状态、缓存、产物或环境诊断：`.git/`、`.singularity/`、`.venv/`、`.pytest_cache/`、`.ruff_cache/`、`outputs/`、`work/`、`__pycache__/`。不要读取 `.env`，除非用户明确要求环境诊断并确认可以检查敏感值。

## 当前结构原则

当前工作树只表达当前真实结构，历史信息由 git history 保留。不要保留旧阶段报告、旧优先级报告、旧生产审查报告、旧 roadmap 报告、旧 manifest、旧命名文档或兼容说明。仓库文档只描述当前源码树中真实存在的结构、字段、调用链、schema、CLI 入口和数据流。

除非用户明确要求兼容，不允许实现或恢复兼容垫片、旧 schema、弃用别名、迁移读取入口、旧 CLI 入口、旧类名 re-export、旧 schema alias 或旧命名读取逻辑。不要为了文档或测试制造无运行价值字段，也不要把解释性概念硬塞进 runtime、result schema 或 trace payload。

## 命名规则

生产运行时对象、文件、schema 和 CLI 命令使用主流领域命名。Evaluation 相关命名必须使用 `evaluation`、`eval`、`evaluator`、`evaluation harness`、`benchmark`、`task`、`task set`、`result`、`report`、`runner`、`experiment` 等主流概念；不要恢复 `live` 命名。

不要引入提示词污染式名称、解释性静态字段、重复 alias 字段或非主流自造名。用户明确要求移除兼容层时，修复现有路径，不要建立并行结构。

## 范围纪律

代码任务从映射出的子系统和对应测试开始，保持改动在任务边界内，保留当前运行时分层，不做无关清理。涉及大改、重构、删除、迁移或高风险操作前，先简短说明影响。

## 模块数据流文档

核心模块文档位于：

```text
docs/architecture/modules/
```

每个核心模块必须有一份中文“模块数据流”文档。文档以当前源码为唯一事实来源，按真实模块边界组织，不按历史阶段、历史任务或旧报告组织。文档展示真实对象时必须列出当前源码完整字段，不允许只列子集。

每份模块文档必须包含：

- 这一层解决什么问题
- 当前源码位置
- 关键类、函数、字段
- 真实运行时调用链
- 真实对象完整结构
- 谁生成这些对象
- 谁消费这些对象
- 是否落盘
- 是否进入 trace / audit
- 失败路径
- 当前结构问题
- 维护规则

代码结构、模块边界、类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema、evaluation result 变化时，必须同步更新对应模块文档。模块文档必须用中文客观描述；英文术语可以保留，但首次出现要给中文说明。

完成运行时敏感变更前运行：

```text
python scripts/verify_runtime_docs.py
```

最终回复需要说明修改了哪些源码文件、更新了哪些 `docs/architecture/modules/*.md` 文件；如果没有更新模块数据流文档，必须说明该变更为什么不影响已记录的数据流。

## 真实模型验证

Singularity 是生产级本地 CLI coding agent harness。任何影响 agent 能力、执行行为、模型交互、prompt assembly、context management、tool exposure、tool execution、planner、repair flow、verification flow、evaluation harness、CLI task execution、tracing、reporting、policy/approval 或 benchmark 行为的改动，必须至少运行一次真实模型验证。

Fake provider、mock provider、unit tests 和 synthetic harness tests 只能作为辅助验证，不能替代最终 agent 能力验证。

真实验证要求：

1. 先运行相关 unit/static checks，再运行至少一个真实模型 Singularity agent 验证。
2. 真实验证必须进入真实 AgentLoop 路径，例如 `KernelBootstrap -> AgentGraphBuilder -> AgentKernel -> AgentLoop.run`。
3. 不要绕过 AgentLoop 直接调用 Planner、ToolExecutor、VerificationRunner、FailureAnalyzer、RepairPlanner 或 EvaluationHarness internals 来声称真实验证。
4. 使用项目现有 `.env` / 配置加载路径读取 provider 配置。不要打印、复制、提交或暴露 API keys、secrets、原始敏感 trace、截图或 markdown。
5. 环境就绪检查只报告脱敏状态，例如 `SINGULARITY_API_KEY=present(redacted)`、`SINGULARITY_BASE_URL=present`、`SINGULARITY_MODEL=present`。
6. Evaluation/benchmark 工作优先运行真实 evaluation benchmark：

```text
python -m singularity.cli eval run docs/evaluation/capability-regression-tasks.json --run-id <meaningful-run-id> --json
```

7. 非 evaluation 的 agent 变更运行能覆盖变更路径的最小真实任务，但仍必须使用真实模型 provider 和真实 AgentLoop。
8. 最终输出必须包括真实模型命令、脱敏 provider/model/config 状态、是否进入 AgentLoop、是否不是 fake/scripted/mock/fallback、result/report/trace artifact 路径、状态、turn/tool 统计、verification result 和失败摘要。
9. 如果真实模型验证无法运行，必须明确分类 blocker：`.env` not found or not loaded、required env var missing、authentication/provider error、base_url/network error、model name/config error、sandbox/permission error、AgentLoop/runtime error、verification failure、user explicitly prohibited real model calls。
10. 不要静默用 fake-provider tests 替代真实验证。真实模型调用成功或 blocker 被修复前，任务不能算完全验证。
