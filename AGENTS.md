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
- 复用当前源码和 docs/singularity.md 中的对象边界、状态模型和数据流。任何超出当前最小合同的机制都必须有当前消费者和明确必要性；删除优先，合并其次，新增最后。改动涉及跨层同步结构（词形表、枚举映射、字段白名单、DTO 投影）时，先 grep 全仓库确认同步点，完成后核对全部调用点并跑全 workspace 测试。
- 不为未来的路由、多 Agent、任务图、Sandbox、Approval 或分布式基础设施预建核心复杂度。
- 同一事实只保留一个权威来源；文档描述当前有效设计，不把计划、审查过程或失效迁移叙述写入长期事实源。

### 参考对齐约束

- 参考项目源码本地克隆于 `D:\refs\pi`、`D:\refs\codex`、`D:\refs\grok-build`（浅克隆，更新由用户手动执行）。这是本仓库架构决策的强制参照源，不是可选项。
- **硬约束（写前）**：任何架构决策——模块边界、对象/状态模型、事件流、协议、命名与分层、持久化格式、并发与取消语义——必须引用参考项目源码的具体文件+行号作为对齐依据，并在交付汇报中给出；无法引用具体位置即视为未参考，不得落笔。功能机制与策略（压缩策略、工具策略、重试参数等）可自定义，但承载它们的架构结构仍须对齐。
- **产品文本隔离**：代码、注释和仓库内一切文档（含决策记录、README）只书写当前事实与理由，按正常工程写法组织；不得提及参考产品名、外部源码路径、行号，也不得留下「参考实现/对齐/参照/移植自某产品」之类的引用句式。对齐依据的引用只出现在交付汇报中，本文件是引用方法论的唯一落点。
- **轻量对照（写后）**：每完成一个模块或阶段，对照参考源码抽查架构对齐点（模块边界、事件流、命名/分层），输出对齐/偏离结论；偏离必须说明理由并经用户确认。不做逐行审查。
- 引用格式示例：`D:\refs\codex\codex-rs\thread-store\src\store.rs:120`（模块边界参照）。引用必须真实存在且与决策点语义相关，禁止伪造或装饰性引用。
- 参考源码只读，不得修改、不得提交、不得复制大段代码进本仓库（参照结构而非搬运实现）；许可证差异以本仓库 LICENSE 为准。

### 最小验证合同

- 纯文档、提示词、决策记录或注释改动：检查最终内容、链接和归属，并运行 git diff --check；默认不运行 Rust 测试、真实 Provider smoke、Evaluation 或等待 CI。
- 代码、协议、持久化、并发、安全、Provider、客户端、构建或发布入口改动：按风险增加定向测试、构建/静态检查和必要的真实链路验证。不要把局部绿灯表述为全量通过。
- 失败、超时、崩溃或错误产物只是症状。先固定输入、配置、版本和环境，沿目标、输入/状态、边界请求、中间转换、外部返回、Agent/工具/会话、checker/进程和产物重建因果链；未排除替代解释前不得归因于模型能力。

### 测试准入

- 调查优先复用现有测试或临时命令；临时测试在交付前删除。永久测试只保护独特的可观察回归、非平凡不变量或边界，并应能在对应故障下稳定失败。
- 新增前全仓搜索同一契约，由拥有该行为的模块测试；adapter 和客户端只保留自身映射或关键黑盒链路，不重复实现细节或仅追求覆盖率。
- 默认复用现有测试目标。仅当进程、环境、公开边界、fixture 生命周期或运行门禁确实独立时新建 `tests/*.rs`。
- 临时、ignored、真实 Provider 和迁移测试必须注明运行层与移除条件；失效功能、重复覆盖、实现镜像和无有效断言的测试直接删除。

### 代码导航

- **符号优先**：查定义、调用方、实现、影响面时，第一个动作是 Serena 符号工具；`grep` 与整文件 `read` 用于确认它定位到的目标。
  - 文件里有哪些对象 → `get_symbols_overview`；定义在哪 / 需要符号正文 → `find_symbol`（`include_body=True` 才拉实现）
  - 谁调用它 / 改动波及哪些调用点 → `find_referencing_symbols`（影响面结论以它的引用集合为准，不是关键词命中数）
  - trait 或接口有哪些实现 → `find_implementations`；还不知道符号名、或要查字符串字面量 → `search_for_pattern`
  - 重命名 / 删除符号 → `rename_symbol` / `safe_delete_symbol`（跨文件引用由工具更新或返回引用清单）
- 理解仓库结构、查找定义、引用、实现和影响面时，优先使用已配置的 Serena LSP 符号工具（如 `get_symbols_overview`、`find_symbol`、`find_referencing_symbols`）缩小范围；关键事实仍必须以当前源码、`rg`、Git 和可复现运行验证。
- Serena 的符号缓存只用于导航，不是事实源。首次使用、缓存缺失或大规模结构变更后运行 `serena project index` 刷新缓存；活动会话中的语言服务器直接跟踪当前文件变化，无需在每次提交后重复建立完整索引。

### 评估基础设施

- 行为回归评估套件位于 `C:\Users\Lenovo\Desktop\Singularity-Evaluator`（独立 git 仓库，不进入本仓库）：黑盒调用 `sg --json <instruction> --model <model>`，以任务目录内的 `checker.sh` 判分，按模型汇总通过率、token、工具调用与耗时；`eval-config.json` 已配置专用测试模型。
- 修改 AgentLoop、工具、提示词、输出截断、压缩或 Provider 链路等行为敏感层时，改动前后各跑一次对照，防止单元测试全绿但 Agent 实际变差；评估产生的模型调用花费不受限。一个完整的任务前后评估两次即可
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
