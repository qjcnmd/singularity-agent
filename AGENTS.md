# Singularity 仓库指令

Singularity 是以 Rust 实现的、面向可靠 coding task 的最小 headless harness；一次性 CLI 是当前唯一客户端，交互式终端与 Desktop 是路线图内共用同一核心能力的客户端。

## 每个任务都适用

### 事实、授权与范围

- 当前源码、可复现运行结果、实际协议/持久化数据和匹配版本的一手资料优先于记忆、摘要、历史实现和代理声明。关键结论区分已验证事实、推断、待验证假设和未知。
- 任务范围只包含用户明确要求和正确完成所必需的最小支撑。相邻缺陷、未来需求和顺便重构不自动纳入。
- 咨询、解释、审查和诊断默认只读；修改包含范围内的本地改动和必要的非破坏性验证。未经明确授权不得 push、发布、创建或关闭 Issue、merge/rebase/reset、删除未知文件或修改外部状态。
- 保护现有未提交、未跟踪和无关 worktree 改动；归属不明的文件默认保留。

### 项目不变量

- 核心保持 headless，CLI、TUI 和 Desktop 复用同一 Agent 能力；不要复制 Agent 状态或业务逻辑。
- 复用当前源码和 docs/singularity.md 中的对象边界、状态模型和数据流。比 Pi 基线更复杂的机制必须有当前消费者和明确必要性；删除优先，合并其次，新增最后。
- 不为未来的路由、多 Agent、任务图、插件平台、Sandbox、Approval 或分布式基础设施预建核心复杂度。安全、协议、持久化、执行、并发和恢复不变量不得因简化而削弱。
- 同一事实只保留一个权威来源；文档描述当前有效设计，不把计划、审查过程或失效迁移叙述写入长期事实源。

### 最小验证合同

- 纯文档、提示词、决策记录或注释改动：检查最终内容、链接和归属，并运行 git diff --check；默认不运行 Rust 测试、真实 Provider smoke、Evaluation 或等待 CI。
- 代码、协议、持久化、并发、安全、Provider、客户端、构建或发布入口改动：按风险增加定向测试、构建/静态检查和必要的真实链路验证。不要把局部绿灯表述为全量通过。
- 失败、超时、崩溃或错误产物只是症状。先固定输入、配置、版本和环境，沿目标、输入/状态、边界请求、中间转换、外部返回、Agent/工具/会话、checker/进程和产物重建因果链；未排除替代解释前不得归因于模型能力。

### 代码图导航

- 理解仓库结构、查找定义/调用关系/影响面时，优先使用已配置的 `codebase-memory-mcp`（如 `search_graph`、`search_code`、`trace_path`、`get_code_snippet`）缩小范围；关键事实仍必须以当前源码、`rg`、Git 和可复现运行验证。
- 检查图查询返回的 `total` 与 `has_more`，必要时分页或收窄查询；索引可能陈旧，代码图只是导航，不是事实源，也不要无目的输出完整仓库地图。
- 每次成功创建 Git 提交后，调用 `index_repository` 使用当前工作树根目录的绝对路径刷新索引；索引失败不得回滚已验证提交，必须报告失败原因和当前索引状态。不要启用 `auto_watch`。

### 评估基础设施

- 行为回归评估套件位于 `C:\Users\Lenovo\Desktop\Singularity-Evaluator`（独立 git 仓库，不进入本仓库）：黑盒调用 `sg run <instruction> --model <model> --json`，以任务目录内的 `checker.sh` 判分，按模型汇总通过率、token、工具调用与耗时；`eval-config.json` 已配置专用测试模型。
- 修改 AgentLoop、工具、提示词、输出截断、压缩或 Provider 链路等行为敏感层时，改动前后各跑一次对照，防止单元测试全绿但 Agent 实际变差；评估产生的模型调用花费不受限。
- runner 依赖 `--json` 输出的终态汇总结构，本仓库改动 CLI 输出格式时必须同步更新评估器解析。评估失败的归因顺序见 docs/agents/provider-evaluation.md。

## 按需读取的项目指令

- docs/agents/domain.md：领域与架构事实；涉及核心对象、进程边界、会话、工具或架构决策时必读。
- docs/agents/architecture.md：AgentLoop、Session、Tool、Context、Compaction、客户端边界或代码图。
- docs/agents/provider-evaluation.md：模型、协议、真实调用、Evaluation、checker 或归因。
- docs/agents/workflow.md：复杂任务、测试、Cargo、worktree、提交、远程操作或 Issue。
- docs/agents/skills.md：命中 Skill、需要委派或跨阶段恢复。
- docs/agents/issue-tracker.md 与 docs/agents/triage-labels.md：Issue 操作或 triage。

## 文档与代码注释

- docs/singularity.md 是当前核心架构唯一事实文档；架构、协议、会话、Provider、工具或评估事实变化时同步更新。
- 注释说明对象职责、原因和非显然不变量，不逐行复述代码；公共 Rust API 使用 Rustdoc 注释。
- 修改文字时保留 actor、条件、时序、强制性、失败、所有权、副作用和后果；删除重复、模糊、显而易见或仅对作者会话有意义的内容。

## Agent Skills

### 本机 Agent Skill 路由

不同 Agent 或运行模式不一定自动注入 Skill 目录。无论当前使用哪种 Agent，都必须根据实际任务、仓库状态、已产生的 diff 和当前阶段主动选择 Skill；任务措辞只是辅助信号，不是读取前提。在任务开始、出现新失败或设计决策、形成实质 diff、进入审查或交付门时重新匹配下表。命中一个或多个 Skill 后，先从对应绝对路径完整读取 `SKILL.md`，再继续该阶段。只读取命中的 Skill。

如果运行时已经注入并完整加载同一 Skill，不重复读取。Skill 缺失或不可读时明确报告实际路径和影响，不得声称已经使用。Skill 是工作流说明，不扩大修改、删除、Git 远程操作、发布、外部写入或子代理授权；上层指令、本项目约束和当前任务授权仍然生效。

| Skill | 主动触发条件 | 固定读取路径 |
| --- | --- | --- |
| `diagnosing-bugs` | 任务目标或执行过程出现影响结论或交付的错误、测试失败、卡顿、性能回退、偶发行为或不符合预期的结果；先建立能捕获原症状的反馈环。 | `C:\Users\Lenovo\.agents\skills\diagnosing-bugs\SKILL.md` |
| `research` | 标准、上游实现、API、版本或方案事实会影响决策，且外部调查构成需要沉淀为仓库 Markdown 的独立工作包。普通事实核对仍按本项目调查规则执行。 | `C:\Users\Lenovo\.agents\skills\research\SKILL.md` |
| `find-simplifications` | 工作涉及架构或重构，或实质 diff 新增/保留了较多概念、状态、文件、接口、配置、兼容路径、重复实现或自研基础设施；在完成前主动核实哪些复杂度没有当前消费者或必要约束。 | `C:\Users\Lenovo\.agents\skills\find-simplifications\SKILL.md` |
| `codebase-design` | 当前方案或 diff 新增、拆分或调整 module interface、seam、adapter、职责边界、依赖方向或测试接缝，或者复杂度归属不清。 | `C:\Users\Lenovo\.agents\skills\codebase-design\SKILL.md` |
| `tdd` | 新行为或缺陷修复存在稳定、可观察的公开 seam，测试先行能形成直接 red/green 反馈，或任务采用 TDD、red-green-refactor、集成测试驱动方式；写测试前先确认 seam。 | `C:\Users\Lenovo\.agents\skills\tdd\SKILL.md` |
| `code-review` | 已形成实质代码 diff、提交、分支或 PR 候选，尤其是跨 crate/模块、大范围重构，或涉及公共接口、协议、安全、持久化、并发和生命周期；每个重构阶段的语义审查门必须使用。纯审查阶段只报告，实施阶段可在当前授权范围内修复发现。 | `C:\Users\Lenovo\.agents\skills\code-review\SKILL.md` |
| `pre-push-checks` | 准备 push、force-push、标记 ready for review、发布候选，或声称本地门禁已通过；依据实时 diff 选择最小充分检查。 | `C:\Users\Lenovo\.agents\skills\pre-push-checks\SKILL.md` |
| `prose-standard` | 修改范围包含 Markdown、API/模块注释、提示词、工具描述、诊断、CLI/UI 文案、测试说明或 `AGENTS.md`；把文字视为接口和行为的一部分。 | `C:\Users\Lenovo\.agents\skills\prose-standard\SKILL.md` |
| `trim-reasoning-leakage` | 当前状态文档、注释、提示词或可见文本出现“本次修改、以前实现、这一 PR、某轮审查、计划阶段”等创作过程视角，或 diff 正在把过程叙述写入长期事实源。 | `C:\Users\Lenovo\.agents\skills\trim-reasoning-leakage\SKILL.md` |
| `doc-standards` | 文档改动涉及归属、拆分、移动、信息层级、教程与参考边界、重复内容、超长文档或失效状态文字，而不只是局部措辞。 | `C:\Users\Lenovo\.agents\skills\doc-standards\SKILL.md` |
| `archive-decision-records` | ADR、RFC、设计记录或 Agent notes 出现已实施、被取代、重复、失去决策价值、引用失效或活动语料膨胀；没有既有记录制度时只提出最小约定。 | `C:\Users\Lenovo\.agents\skills\archive-decision-records\SKILL.md` |
| `translate-docs` | 仓库存在语言配对约定，且文档新增、语义修改、重命名或删除会影响对应语言版本、术语或配对校验。 | `C:\Users\Lenovo\.agents\skills\translate-docs\SKILL.md` |
| `doc-site-sync` | 仓库存在文档站投影，且文档变动影响页面发布、manifest、route、navigation、locale、投影链接或站点构建。部署仍需单独授权。 | `C:\Users\Lenovo\.agents\skills\doc-site-sync\SKILL.md` |
| `record-browser-gif` | Web UI 流程、缺陷或修复需要以短 GIF 作为可核验的演示产物，且存在可运行的真实 UI；没有可录制 UI 时报告前置缺失。 | `C:\Users\Lenovo\.agents\skills\record-browser-gif\SKILL.md` |
| `merging-stacked-prs` | 当前 GitHub 工作包含 stacked/dependent PR、若干 PR 的有序落地，或目标 PR 的 base 是另一条开放 PR 分支。任何远程变更和合并仍需明确授权。 | `C:\Users\Lenovo\.agents\skills\merging-stacked-prs\SKILL.md` |

组合任务按实际阶段加载：调查/诊断/设计类 Skill 在方案或实现前使用；`prose-standard`、`trim-reasoning-leakage` 和文档类 Skill 在其内容进入修改范围时并用；`code-review` 在审查门使用；`pre-push-checks` 只在形成对外候选后使用。后续阶段出现新的触发语义时再读取对应 Skill，不提前加载无关文件。

### Issue tracker

任务和需求记录在 GitHub Issues。详见 `docs/agents/issue-tracker.md`。

### Triage labels

使用仓库当前定义的 triage 标签。详见 `docs/agents/triage-labels.md`。

### Domain docs

现有仓库指令和当前架构事实优先。详见 `docs/agents/domain.md`。
