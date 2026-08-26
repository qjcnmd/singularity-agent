# Singularity 仓库指令

Singularity 是以 Rust 实现的、面向可靠 coding task 的 coding-agent harness。产品有三种形态：① 参照 pi 的无交互单次入口（`sg --print/--json <goal>`）；② 交互式 TUI，界面交互以 Grok Build 为主参照，功能以 pi、Codex CLI 和 Grok Build 为参照；③ 参照 Codex Desktop 的桌面端。app-server（stdio JSON-RPC）是形态③的后端接线口，不是独立用户入口；它只把 runtime 事实投影为协议，不复制执行语义。核心采用轻量协调器与多个职责清晰、接口窄且可替换的模块组合：Thread/Turn 生命周期是 Turn 执行唯一所有者（crates/runtime 的 TurnRunner/Conversation），Context/Compaction、Tool、Model/Provider、Session persistence、项目指令与提示词、Event sink 及客户端 adapter 各自保持独立。无交互模式、TUI 与 app-server 全部委托 runtime 执行；后续替换或重做任意明确模块时，修改应集中在该模块、adapter 和测试，不扩散到其他模块或客户端。

## 每个任务都适用

### 事实、授权与范围

- 当前源码、可复现运行结果、实际协议/持久化数据和匹配版本的一手资料优先于记忆、摘要、历史实现和代理声明。关键结论区分已验证事实、推断、待验证假设和未知。
- 任务范围只包含用户明确要求和正确完成所必需的最小支撑。相邻缺陷、未来需求和顺便重构不自动纳入。
- 咨询、解释、审查和诊断默认只读；修改包含范围内的本地改动和必要的非破坏性验证。未经明确授权不得 push、发布、创建或关闭 Issue、merge/rebase/reset、删除未知文件或修改外部状态。
- 保护现有未提交、未跟踪和无关 worktree 改动；归属不明的文件默认保留。

### 项目不变量

- 核心协调器保持 UI 解耦并支持无交互执行；交互式 TUI、无交互文本/JSONL 与桌面端通过稳定的共享接口复用同一能力，不复制 Agent 状态或业务逻辑。TUI 和无交互入口进程内调用 runtime，桌面端通过 app-server 调用同一 runtime。
- Context/Compaction、Tool、Model/Provider、Session persistence、项目指令/提示词、Event sink 和客户端 adapter 都是独立模块；模块内部可以更换实现，协调器只依赖其稳定接口和生命周期合同。
- 可替换性通过静态、窄、类型化的 seam 实现；模块替换不应要求修改其他模块的实现或客户端渲染。只为明确的定制热点建立 seam，不引入通用插件平台、动态脚本加载或依赖注入容器。
- 复用当前源码和 docs/singularity.md 中的对象边界、状态模型和数据流。任何超出当前最小合同的机制都必须有当前消费者和明确必要性；删除优先，合并其次，新增最后。
- 不为未来的路由、多 Agent、任务图、Sandbox、Approval 或分布式基础设施预建核心复杂度。安全、协议、持久化、执行、并发和恢复不变量不得因简化而削弱。
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

- 行为回归评估套件位于 `C:\Users\Lenovo\Desktop\Singularity-Evaluator`（独立 git 仓库，不进入本仓库）：黑盒调用 `sg --json <instruction> --model <model>`，以任务目录内的 `checker.sh` 判分，按模型汇总通过率、token、工具调用与耗时；`eval-config.json` 已配置专用测试模型。
- 修改 AgentLoop、工具、提示词、输出截断、压缩或 Provider 链路等行为敏感层时，改动前后各跑一次对照，防止单元测试全绿但 Agent 实际变差；评估产生的模型调用花费不受限。
- runner 依赖 `--json` 输出的终态汇总结构，本仓库改动 CLI 输出格式时必须同步更新评估器解析。评估失败的归因顺序见 docs/agents/provider-evaluation.md。

## 按需读取的项目指令

- docs/agents/domain.md：仓库读取顺序、单上下文约定与领域命名；探索仓库或引入新领域词汇时读取。
- docs/agents/architecture.md：模块接缝与替换边界；涉及模块替换、Sandbox/Approval 或候选简化调查时读取。
- docs/agents/provider-evaluation.md：模型、协议、真实调用、Evaluation、checker 或归因。
- docs/agents/workflow.md：复杂任务、测试、Cargo、worktree、提交、远程操作或 Issue。
- docs/agents/skills.md：命中 Skill、需要委派或跨阶段恢复。
- docs/agents/issue-tracker.md 与 docs/agents/triage-labels.md：Issue 操作或 triage。

## 文档与代码注释

- docs/singularity.md 是当前核心架构唯一事实文档；架构、协议、会话、Provider、工具或评估事实变化时同步更新。
- 注释说明对象职责、原因和非显然不变量，不逐行复述代码；公共 Rust API 使用 Rustdoc 注释。
- 修改文字时保留 actor、条件、时序、强制性、失败、所有权、副作用和后果；删除重复、模糊、显而易见或仅对作者会话有意义的内容。

## Agent Skills

### Issue tracker

任务和需求记录在 GitHub Issues。详见 `docs/agents/issue-tracker.md`。

### Triage labels

使用仓库当前定义的 triage 标签。详见 `docs/agents/triage-labels.md`。

### Domain docs

现有仓库指令和当前架构事实优先。详见 `docs/agents/domain.md`。
