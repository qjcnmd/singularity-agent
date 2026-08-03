# Singularity 仓库指令

## 适用范围与任务收敛

本文件只保存 Singularity 仓库长期有效、可直接执行的项目规则；个人偏好、一次性任务要求和工具自身编排合同由更高层指令、当前用户消息或对应 Skill 管理，不在这里重复。子目录出现更具体的 `AGENTS.md` 时，只覆盖其目录树内的差异。

- 中大型、跨模块或高风险任务先明确可验证的目标、范围、禁止变化的合同、完成条件和风险；短小明确的修改直接执行，不为形式完整增加计划、文档或验证。
- 长任务只维护一个当前状态台账，复用已有台账优先。台账记录当前目标、候选 revision、仍有效与已失效证据、阻断和下一步，并用 `verified`、`inferred`、`unknown`、`invalidated` 区分事实状态。聊天摘要、计划勾选和代理报告不能替代当前 Git、源码、运行结果及外部状态。
- 台账、长日志、一次性提示词和诊断文件放在已忽略的 `outputs/` 或 `work/` 中，不加入 Git。稳定架构事实进入 `docs/singularity.md`，代码历史由 Git 保存。
- 长时间 Worker、Provider、构建或 Evaluation 只按真实进展、领域 timeout、明确阻断和取消信号判断；不得自行添加短墙钟上限来强制结束仍在推进的任务，也不为证明仍在运行而短间隔轮询或重复播报无变化状态。

## 事实来源与外部参考

- 当前源码、Git 对象、协议 payload、可复现运行和一手官方资料是事实来源；代码图、生成文档、历史日志、搜索摘要和模型输出只用于导航或提出待验证假设。
- 非平凡设计或根因修复前，先有界核对对应标准、平台原生机制或维护活跃的成熟实现。证据链应能回答：当前问题与约束、先例的具体机制和版本、适用差异、最终选择与明确省略项。
- 外部先例用于复用已验证的不变量和责任边界，不因知名而整套照搬。与当前产品消费者、安全合同或平台事实不符的表面不引入兼容层或占位接口。
- 调查预先写清待回答问题、所需证据和停止条件；检索不到直接先例时，明确标准空白，只实现最小、隔离且可删除的项目扩展。

## 代码图导航

理解仓库结构、查找符号、调用关系和影响范围时，优先使用 `codebase-memory-mcp` 缩小检索范围；查询不足或结果可能过期时，使用 `rg`、Git 和当前源文件验证。代码图不是代码事实的唯一来源，且只查询当前任务需要的局部信息，不无条件输出完整仓库地图。

每次成功创建 Git 提交后，必须调用 `codebase-memory-mcp` 的 `index_repository`，使用当前工作树根目录的绝对路径刷新代码图。索引失败不撤销已经创建的提交，但必须明确报告失败原因和当前索引状态。无法访问 `codebase-memory-mcp` 的环境（未配置该 MCP 的其他 agent）跳过本节的索引要求，直接使用 `rg` 和 Git 检索，并在最终回复中说明索引未刷新。

不要开启 `auto_watch`，直到已验证的上游版本修复 dirty worktree 下的重复索引问题（2026-07 记录；确认上游修复后删除本条）。

## 命令与磁盘

1. 本项目 Cargo 产物必须写入本地不提交的 `.cargo/config.toml` 或任务级 `CARGO_TARGET_DIR` 指定的专用 target 目录，不得静默回落到工作区 `target/`；运行 Cargo 前用 `cargo metadata --no-deps --format-version 1 --locked` 核对 `target_directory`。不提交机器专用绝对路径配置。
2. 默认保留本项目 Cargo 增量编译缓存，不在任务结束时执行 `cargo clean`；仅当缓存损坏、Rust 工具链切换、产物归属不明、专用 target 磁盘空间不足或用户明确要求时，才在核对 target 后清理。
3. 临时 Evaluation、日志、一次性 Worktree、测试临时目录和任务临时文件在每次任务结束时清理；用户明确要求保留时除外。
4. 删除或移动目录前先解析并校验绝对路径位于当前工作区或本次明确指定的临时目录。不得删除源码、用户数据、任务开始前已存在且归属不明的产物。
5. 最终回复说明产物实际写入位置、保留的 Cargo 缓存、已清理内容、清理失败项和保留原因。

## 项目实现与目标仓库语言边界

1. Singularity 的核心产品运行时、公共协议、安全边界和发布二进制使用 Rust。允许为构建、测试、审计或维护引入职责明确的主流辅助工具，但不得形成第二套产品运行时、绕过 Rust 主链路或安全协议，也不得恢复 Python agent runtime、sidecar 或兼容入口。
2. 目标仓库可以使用 Python、Rust、Node.js、Go 或其他语言；命令工具应在严格 sandbox 中使用宿主机 `PATH` 已安装的工具链，不得把实现语言边界误作目标仓库能力限制。
3. `sg` 只通过 stdio JSON-RPC 调用 `singularity_app_server`；CLI 不直接依赖 agent、model、tools 或 store crate。
4. 当前工作树只保留当前真实结构。历史命名、schema、CLI、环境变量和迁移说明由 Git 历史保存，不新增兼容垫片、弃用别名、迁移读取入口或旧路径 re-export。
5. Evaluation 使用 `evaluation`、`eval`、`task`、`task set`、`runner`、`result`、`report` 等主流命名，不恢复迁移期自造分类。

## Sandbox 上游复用边界

- `crates/windows-sandbox` 负责 Windows 账户、ACL、WFP、Job Object、setup、路径保护和进程生命周期；优先复用或轻量移植官方 `openai/codex` 中与当前威胁模型匹配的成熟 Win32 机制，但使用 Singularity 自有二进制、账户、GUID、配置和状态，不共享本机 Codex 实例或安全状态。
- `crates/sandbox` 只把通用 permission profile 投影到平台实现并返回 typed execution result，不维护第二套 ACL、setup、observer 或进程生命周期事实。
- 只保留当前消费者需要的 strict workspace-write/read-only、protected paths、network denied、Job Object、取消/超时、workspace change 和失败关闭。PTY、ConPTY、managed proxy、GUI/desktop、多会话服务、Codex 配置协议和 telemetry 等没有当前消费者的表面不建立兼容层。
- 上游采用点、必要本地差异和明确省略项记录在 `crates/windows-sandbox/UPSTREAM.md`。本地严格增强必须有直接威胁或运行证据；未知变更、monitor 丢失、路径身份不明和 enforcement 不可用继续失败关闭，不使用 local-process fallback 或环境特判。

## Agent、Provider 与 Evaluation 边界

- 模型可见产品工具由单一注册事实源产生；功能任务不通过 Evaluation task、required capability 或内部阶段维护第二套工具表面。工具选择自由与权限控制分离，副作用由 Policy、Approval 和 OS sandbox 约束。
- Provider 能力必须由显式配置、协商或实际 wire 证据确定，不从模型名称推断。一次 Evaluation trial 从开始到结束固定 `provider_id`、`model_id`、API protocol 和相关模型参数；不在运行途中自动路由、轮换或 fallback。
- Provider transport retry 是同一请求的网络恢复，不等于重新采样 Trial。错误分类、attempt 计数和最终失败必须保留在 typed trace 中，不通过吞错、换模型或重跑制造成功。
- Evaluation 是开发工具和普通产品调用方，不进入发布二进制或定义 Agent 语义。功能正确性主要由真实 patch、baseline/public/hidden tests 和最终 diff 判断；工具配对、参数、恢复、取消和 completion 等协议不变量由独立确定性 conformance 测试证明。
- `functional_task_success`、`agent_protocol_success` 与 `sandbox_security_success` 分别计算、发布和归因；外部门禁可以同时要求三者，但不得合并成无法定位责任层的单一失败。Evaluator 保护自身 patch、tests、`.git` 和依赖/系统路径，并审计异常改动，不用路径白名单向模型泄露答案位置或阻止合理跨文件修复。

## 工程原则

1. 测试、Evaluation、benchmark、监控指标和验收分数是观察产品行为的证据，不是反向定义产品语义的控制信号；不得为改善某次结果增加与真实领域合同无关的特判、放宽门禁或改变安全边界。
2. 对缺陷修复和非平凡功能，进入实现前先做有界的方向校验：从当前目标、威胁模型和真实调用链出发，判断现有架构、责任边界、状态模型与算法复杂度是否仍然合理，并在必要时对照行业通行设计、平台原生机制或权威资料。历史代码、现有测试和“当前就是这样做的”只能证明实现事实，不能证明设计正确。若基础方向错误，或继续沿用会产生级联补丁、重复事实源或不可接受的复杂度，应先提出并采用最小完整的结构性修正，不得在错误基础上叠加局部补丁；小型、明确、低风险修改只需核对直接边界，不借此扩大为无关重构或泛化调研。
3. 失败现象只能证明某个不变量被破坏，不能自动决定修改哪一层；修复前沿真实调用链区分各层责任，在拥有该不变量的最小正确抽象层修复。
4. 进入通用运行时的机制必须是稳定、可复用、对所有适用输入一致成立的领域规则；仅服务于某个实现、供应商、任务、数据集或当前分数的逻辑不是通用能力，必要的临时兼容必须隔离边界并记录原因、适用范围、生命周期和移除条件。
5. 外部系统或可替换实现的能力必须显式声明、验证和协商，不从名称、接口兼容、历史表现或乐观假设推断；能力不足时走明确、可审计的降级或拒绝路径。
6. 外部输入即使声称已校验，仍须在本地信任边界完整验证；复合输入或批量操作先整体验证再产生副作用，任一成员非法时不得静默挑选、改写或执行其余子集，除非领域合同明确规定部分成功语义。
7. 编排必须显式表达依赖、副作用和一致性边界；只有证明互不依赖且无冲突的操作才可并行；并发或批处理能力必须同时定义结果顺序、部分失败、取消、超时、授权、恢复、审计和持久化语义。
8. 错误模型保留输入拒绝、能力不支持、策略拒绝、权限边界、执行失败和基础设施故障的因果差异；可重试、可降级、需授权或必须终止由稳定错误语义决定，不由字符串匹配或当前验收需要决定。
9. 自动修复、规范化和兼容转换只在语义唯一、无权限扩大、无信息损失且属于明确领域合同时允许；存在歧义时拒绝或请求新的合法输入，不猜测意图或执行调用方没有合法提交的操作。
10. “最小改动”指在正确抽象层实现满足真实需求的最小完整机制；局部修复开始产生跨样本特判、重复分支或级联例外时，暂停实现，重新识别缺失的抽象、不变量或状态模型，并说明必要的架构范围。
11. 验证从领域不变量和确定性边界测试开始，逐级覆盖跨模块集成、真实运行和外部验收；保留失败证据和原始 gate 语义，让验收验证架构结果，不让实现迎合测试样本。

## 验收证据边界

- 产品能力与 Evaluation 工具分别维护验收矩阵。Evaluation 只提供开发期评估证据，不得进入发布二进制、产品运行依赖或反向定义 Agent 语义；产品结论不能只靠 Evaluation 分数证明。
- 安全、协议和恢复结论必须在拥有不变量的真实边界取证：Provider 历史检查实际 `ModelTurnRequest`，sandbox 检查操作系统实际 enforcement，workspace 身份检查 pinned handle 对应的对象身份，崩溃恢复检查真实 kill/restart 后的 Store 状态。marker、路径字符串、日志或局部 mock 不能替代这些事实。
- 跨 Provider、MCP 或 JSON-RPC 边界的字段、状态和错误语义，必须由适用版本的 wire schema 或规范具体条款定义，并通过实际出站或入站 payload 验证投影；内部 DTO 名称、trace 字段、测试 fixture 和 Evaluation 结果不构成外部协议事实。
- 每个自包含工作流形成稳定的本地候选提交；冻结 Evaluation 和其他昂贵验收必须绑定明确 candidate revision、输入与能力合同，最终通过后才 push。后续修改只使其实际影响范围内的旧证据失效，不机械重跑无关门禁。
- Issue 中的新发现分为当前 blocker、当前改动 regression、相邻缺陷和后续需求。只有前两类进入当前关闭条件；相邻缺陷和后续需求单独记录，不把一个 Issue 扩成持续吸收所有改进的长期分支。

## 复杂度与设计门禁

Singularity 当前是供单个用户安装在自己电脑上使用、可实际运行并持续演进的 Agent，不是面向未知产品形态或企业部署的通用框架。设计优先服务当前真实调用链和已确认的近期需求；不为未来插件、多租户、分布式服务、高可用、负载均衡或数据库替换预建基础设施，也不为这些没有当前消费者的假设增加部署、性能或验收复杂度。

- 采用“删除优先、合并其次、新增最后”。新增 crate、Trait、Manager、Service、Repository、Adapter、Schema、缓存、锁或消息机制，必须有当前消费者，或明确建立安全、事务、平台、协议、恢复或测试替身边界；否则使用私有函数、枚举或小型内部结构。
- 同一事实只能有一个权威来源。不得用并行数组、重复 DTO、动态 JSON、JSON/SQLite 双权威、长期双轨 checkpoint、无消费者兼容表或纯转发层维持状态。
- 保留安全和一致性所需的复杂度：受限令牌、ACL、Job Object、路径能力、TOCTOU、Approval/Policy、Checkpoint/迁移、Completion/Verification、Provider 能力协商、Tool 信任边界、JSON-RPC、Trace/Audit/Evaluation。不得以“简化”为名降低权限、恢复能力、错误区分或失败关闭。
- App Server 当前只需清晰表达 stdio transport、JSON-RPC、请求/Turn 生命周期、事件输出、取消、关闭和 Store/Provider 初始化；不得引入 Broker、CQRS、Event Sourcing、分布式队列、多租户连接或通用工作流框架。
- Provider、Store、Tool、Evaluation 的抽象必须对应真实消费者和不变量。指标只计算一次再投影；缓存失败是否可视为 miss 应按真实并发和安全合同决定，不为统一而制造反向依赖。

## 测试与验证

验证按最终 diff 的实际风险选择最小充分层级，低层证据通过后才升级；不得把“文件位于 Provider、Agent、sandbox 或 Evaluation crate”本身当作运行昂贵验收的理由。

| 改动类型 | 默认最小验证 |
| --- | --- |
| 文档、注释、项目指令、格式或不改变行为的元数据 | 静态阅读、引用/结构检查、`git diff --check`；不运行 Cargo、Provider、sandbox 或 Evaluation |
| 孤立常量、默认值、retry 数或同类参数，且不改变控制流结构、协议、错误分类、状态、安全或公共接口 | 静态确认最终值、直接引用、断言与文档一致 |
| 测试夹具、lint、CI 元数据或 hosted runner 兼容性 | 失败检查或精确受影响测试；必要时观察对应 CI，不运行 Provider Task 或 Evaluation |
| 局部产品行为 | 受影响 crate 的精确定向测试，按编译边界补最小 `check`/`clippy`；不默认全仓 |
| 模型调用链路、工具 schema/选择、Provider adapter、history/reasoning replay、tool recovery 或 completion | 确定性 ModelTurnRequest/状态转换回归通过后，至少一次真实普通产品调用 |
| Sandbox、Approval、workspace observation 或安全边界 | 拥有不变量的确定性测试和实际 OS enforcement 证据；未知状态继续失败关闭 |
| Evaluation runner、task success 归约或端到端能力 | 先做确定性归约回归，再运行实际受影响 task 的单 trial；该 trial 通过且候选稳定后，才运行一次完整冻结 Evaluation |

- 已通过且未受后续 diff 影响的证据保持有效，不因任务结束、push、CI 修复或文档提交机械重跑。
- 完整构建、全仓测试、跨平台验证和完整 Evaluation 只有在实际影响范围无法由更窄证据证明时运行。完整 Evaluation 不是模型调用链改动后的首次真实测试。
- 不新增或操纵 Trial 重采样、预算、timeout、门禁、task、工具权限或隐藏答案来赌分。真实 Provider Task 首次正确完成即为有效证据；失败保留首个错误、阶段和耗时，只修通用根因。
- 默认不运行 Codex Security 扫描；只有用户明确要求使用时才运行。
- 最终回复列出实际运行的检查、精确结果和未验证范围，不把局部证据描述为全量通过。

## 文档

1. `docs/singularity.md` 是唯一架构事实文档，只描述当前核心产品运行时中的 crate 边界、对象、调用链、持久化和失败路径。
2. 主链路、协议、状态映射、sandbox、approval、provider、evaluation、trace 或 store 变化时，同步更新 `docs/singularity.md` 的相关部分。
3. Skill、配置、提示词、架构文档和代码注释直接描述当前有效的行为、接口、约束和风险，不写对话过程、措辞迭代、临时路线或已结束方案。需要保留的失败证据和决策历史只进入对应状态台账、Issue 或 Git 历史。
4. 说明性文字按接收者最小化上下文；传给模型、工具或子代理的任务胶囊只包含当前动作需要的事实、授权、所有权、验收和升级边界，不转发无关对话或完整历史。

## 代码注释与可读性

1. 为模块、公共类型、结构体、枚举、trait 以及职责或语义无法从名称直接判断的关键函数、方法和更小独立单元补充简洁注释；通常使用一句话说明其含义、职责、契约或存在原因，并选择能够完整表达该语义的最小注释单元。
2. 注释应帮助读者理解代码“是什么、负责什么或为什么存在”，不得逐行复述实现过程、解释显而易见的语法、为简单 getter 或字段转发机械补注释，也不得以增加注释数量为目标制造阅读噪音。
3. 修改代码时必须检查同一语义单元及其直接相关注释；行为、职责、边界、错误语义或不变量发生变化时，同一改动中必须同步更新或删除过时注释。与代码事实不一致的注释视为缺陷。
4. Rust 公共 API 优先使用 `///` 或 `//!` 形成可生成文档的说明；仅与局部实现约束、非显然安全条件或特定算法原因有关的内容使用普通行内注释。

## 任务记录

优先复用与任务直接相关的现有 GitHub Issue。创建新 Issue 必须先取得用户对该 Issue 的一次性明确授权；没有授权时只在本地状态台账记录，不擅自创建。

对已授权或已有的复杂 Issue，最多追加两次简短评论：

1. 确认可靠根因后，记录现象、根因、关键证据和处理计划。
2. 任务完成后，记录实际修改、选择该方案的原因、验证结果、未验证范围和关联 commit。

记录应使用清晰中文解释真实文件、对象和调用链，帮助维护者理解和学习。不要记录原始推理、每条命令、文件读取过程、大段日志、敏感信息或重复内容。

简单、低风险且无需调查的任务不创建 Issue，只在最终回复中简要说明问题、原因、解决方式和验证结果。GitHub 写入失败时明确报告，不得静默跳过。

## Git、CI 与交付

- 开始前核对 `HEAD`、branch、tracked/untracked/ignored 状态并保护用户改动；不得用 reset、checkout、stash、rebase 或 clean 绕开 dirty tree，除非用户对该具体操作明确授权。
- 中大型任务按可独立验证、审查和回滚的阶段创建范围单一的本地提交。提交只是恢复点，不代表完成；未经明确授权不 push、发布、创建 PR、评论或关闭 Issue。
- 每次本地提交后按本文件“代码图导航”刷新索引。索引失败不回滚提交，但必须记录失败和当前索引状态。
- 纯文档、注释、项目指令或测试夹具的后续提交只失效其实际影响的证据；不为与产品行为无关的文件更换 Provider/Evaluation 候选或重跑全套验收。
- Git 只跟踪源码、测试、稳定配置和必要文档。`outputs/`、`work/`、session、prompt、probe、临时日志、一次性脚本和本机绝对路径配置保持 ignored；任务结束时只删除能够证明由本任务创建的临时产物。

## Agent skills

### Issue tracker

任务和需求记录在 GitHub Issues。详见 `docs/agents/issue-tracker.md`。

### Triage labels

使用默认的五类 triage 标签。详见 `docs/agents/triage-labels.md`。

### Domain docs

单上下文（single-context）布局；现有仓库指令和当前架构文档优先。详见 `docs/agents/domain.md`。
