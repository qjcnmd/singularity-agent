# Singularity 仓库指令

## 项目目标与设计基线

本轮及后续架构讨论的用户裁决记录在 \`docs/decisions/desktop-reliability-decisions.md\`；涉及范围、状态、持久化、客户端或 Desktop 交互的实施必须引用对应决策编号，发现新证据时追加修正记录，不删除历史裁决。

Singularity 的首要产品目标是成为一个像 Pi 一样能让模型可靠完成 coding task 的最小 harness，最终通过类似 Codex CLI/Desktop 的交互式客户端使用；当前 CLI 是主要客户端，Desktop 是近期接入目标。核心能力必须同时满足一次性 CLI 与长驻交互客户端的可靠性要求。

当前核心架构与默认行为优先参考维护活跃、经过实际使用验证的主流 coding agent，主要以 Pi 的小核心、Agent Loop、Session、Context Compaction、Tool、Extension 和可嵌入能力作为基线；Codex CLI 等项目可作为辅助参考。

参考外部项目时复用其已经验证的对象边界、状态模型、数据流和默认策略，不机械复制语言、文件结构或内部命名。

当 Singularity 没有明确的当前需求要求不同设计时，优先采用参考实现中更简单、成熟的方案。

可自定义性只要求当前的 Context Compaction、Tool、Context 组装、Provider 和事件机制各自保留一个职责清晰的可替换接缝，以及一个可工作的默认实现。没有第二个当前实现或真实消费者时，不增加策略层、通用插件协议、多实现注册框架、兼容包装或额外状态；需求出现后沿现有接缝替换或深化，而不是提前并列多套机制。

任何比 Pi 基线明显更复杂的核心机制，都必须有当前真实消费者和明确必要性；“以后可自定义”本身不算当前消费者。未来可能需要的模型路由、多 Agent、任务图、自定义 Context 策略、额外工具、Sandbox、权限控制、插件或桌面能力，不作为提前增加核心复杂度的理由。

核心能力保持 headless，并与具体 CLI、TUI 或 Desktop UI 解耦。不同客户端应复用同一核心 Agent 能力，不复制 Agent 状态和业务逻辑。客户端目标包含长驻交互场景：取消必须及时终止正在执行的工具及其子进程树，结束 turn 后释放不再需要的运行时索引，持久历史不得依赖进程内存。当前实现采用 headless core 库 + 薄 app-server（stdio JSON-RPC）+ 客户端经同一协议连接，配置为共享全局文件、会话为统一 JSONL 格式；这是当前事实，不是永久架构合同，改变进程边界需以当前证据和用户裁决为依据。

现有源码、测试、Schema、历史实现和 Git 历史只能证明“当前这样实现”，不能证明设计本身正确。基础方向不合理时允许重构、替换或删除。

---

## 任务范围与复杂度控制

短小、明确、低风险的修改直接执行，不为形式完整额外创建计划、文档、状态机或验证流程。

中大型、跨模块或高风险任务先明确：

- 要解决的真实问题；
- 可验证的目标；
- 直接影响范围；
- 必须保持的外部合同；
- 完成条件；
- 已知风险或未知项。

调查和实现都应有明确停止条件。不要因为发现相邻问题而无限扩大当前任务。

复杂任务如果确实需要跨多个阶段或跨上下文保存状态，可以在 ignored 的 `outputs/` 或 `work/` 中维护简短状态记录；不为普通任务强制创建台账、计划或额外管理文件。

采用以下复杂度原则：

1. 删除优先，合并其次，新增最后。
2. 新增 crate、Trait、Manager、Service、Adapter、Schema、缓存、锁、队列或状态机必须有当前真实消费者或明确边界。
3. 同一事实只保留一个权威来源。
4. 不为了兼容已经废弃且尚未发布的本地设计保留双轨状态、别名、迁移层或兼容垫片。
5. 不为未来假设需求预建框架。
6. 当局部修复开始产生重复状态、级联例外或跨层补丁时，停止继续打补丁并重新检查抽象边界。
7. 更复杂不等于更 production-grade。优先选择边界清晰、行为可解释、状态更少、失败明确且易于维护的设计。


---

## 项目任务状态与阶段门

复杂任务使用一个任务状态源（优先复用 `outputs/exec/status.md`，或当前任务已经指定的等价记录），不建立相互竞争的计划。状态至少包含：

- 用户最终裁决与不可改变的参数（范围、Provider、模型、reasoning、并发、题目数量、是否替换而非增加）；
- 当前目标、阶段、候选 revision 和文件所有权；
- 依赖关系与不能并行的工作；
- 已验证事实、推断、未知、已失效证据和未决阻断；
- 本阶段完成条件、下一动作和恢复路径。

项目任务按以下门推进：

1. 调查：完成相关源码、配置、外部参考和风险面的事实核对；
2. 裁决：冻结范围、依赖、验收条件和用户参数；
3. 实施：只修改已冻结范围，阶段之间保持可构建或可验证；
4. 审查：独立复核 diff、调用链、错误路径和规格符合性；
5. 验证：运行与最终风险相称的定向、集成、真实链路和必要 Evaluation；
6. 交付：本地候选满足全部门禁后才允许 push，push 后再等待 CI。

未完成当前门时，不进入后续门；发现新证据推翻前置结论时，回到受影响的最小门重新开始。用户对参数的后续修正先更新状态并使受影响计划失效，不继续执行旧计划。

---

## 事实来源与架构调查

事实优先级：

1. 当前源码；
2. 当前 Git 状态和历史；
3. 实际协议 payload / trace / 持久化数据；
4. 可复现运行结果；
5. 对应版本的一手官方文档和上游源码。

代码图、搜索摘要、生成文档、历史日志和模型输出主要用于导航和形成假设，不能替代当前源码事实。

涉及非平凡架构设计、协议、Provider、Context、Session、Tool、持久化、并发或平台行为时，应有界核对维护活跃的一手实现或官方资料。

外部参考必须回答：

- 对方真正解决什么问题；
- 使用什么运行时对象和数据流；
- 哪些状态持久化，哪些只存在内存；
- 为什么采用这一边界；
- Singularity 与它有哪些真实需求差异。

如果 Singularity 比成熟参考设计更复杂，应明确说明额外复杂度解决的当前问题。没有充分理由时，默认向更简单的基线收敛。

---

## 代码图导航

理解仓库结构、查找符号、调用关系和影响范围时，优先使用 `codebase-memory-mcp`：

- `search_graph`
- `search_code`
- `trace_path`
- `get_code_snippet`
- 以及任务需要的其他代码图能力。

代码图用于缩小范围；关键结论仍需通过当前源码、`rg`、Git 或实际运行验证。

不要无目的输出完整仓库地图，只查询当前任务真正需要的部分。

成功创建包含产品代码或结构变化的 Git 提交后，使用当前工作树根目录刷新 `codebase-memory-mcp` 仓库索引。纯文档、提示词或不影响代码图结构的微小提交不要求机械重复索引。

如果索引失败：

- 不撤销已经完成的代码修改或提交；
- 明确报告失败原因和当前索引状态；
- 继续使用源码、`rg` 和 Git 完成事实验证。

不依赖 `auto_watch` 作为代码事实来源。

---

## Agent 核心与扩展边界

Agent 核心应尽量保持小而稳定。

核心优先负责：

- Agent Loop；
- Model / Provider 调用；
- Message 与 Tool Call / Tool Result 流转；
- Session；
- Context 构造与 Compaction；
- 基础 Tool 注册和执行；
- Interrupt / Steer / Queue 等必要交互语义；
- 必要的持久化与错误传播。

默认工具集、Session 语义、Context Compaction 和 Agent Loop 应优先参考 Pi 当前真实实现，再根据 Singularity 已确认需求做最小必要差异。

未来可能变化较大的能力应优先通过清晰扩展边界实现，而不是提前固化进 Agent 核心，例如：

- 自定义 Tool；
- Tool 替换或启停；
- 自定义 Context / Compaction；
- 模型路由；
- 多 Agent；
- 任务图；
- MCP；
- 外部搜索；
- 自定义 Verification；
- Sandbox 或外部隔离环境；
- 权限确认；
- Memory；
- 项目或用户工作流。

Sandbox 默认不启用：工具默认继承进程权限直接执行；Sandbox 作为可选执行模式，通过清晰扩展边界提供，不默认启用、不进入核心依赖路径（核心行为不因 sandbox 存在与否而改变），保持可维护性与可扩展性，避免强耦合。Approval/Permission 同理，不作为核心内置链。

扩展性不等于提前实现完整插件平台。只建立当前重构真正需要的稳定接缝。

可维护性与可扩展性是核心设计约束（对齐 Pi 的扩展模型，不强耦合）：

- 机制间低耦合：工具、Compaction、上下文组装、Provider、事件各自独立，通过明确接口通信，互不穿透；
- 可替换机制：工具注册表（可注册/替换/启停工具）、Compaction 策略、Context 组装、Provider 实现、事件回调均可独立替换或定制，替换成本保持低；
- 可配置：行为默认值通过配置覆盖（模型与能力声明、压缩阈值、工具集选择等）；
- 接缝真实存在但不预建框架：未来新增机制（自定义工具、自定义压缩/上下文策略、可选 Sandbox 模式、多 Agent 等）从既有接缝接入，不修改核心循环。

公共工具生命周期事件按 Pi 的默认观察模型实现：`tool/execution/start` 携带 `toolCallId`、`toolName`、完整 `args`，`tool/execution/update` 另携带 `partialResult`，`tool/execution/end` 携带结构化 `result` 与 `isError`。不得仅因安全偏好擅自把这些字段收缩为状态事件；如需改变协议，必须先更新架构事实文档、客户端合同、限长/脱敏策略和兼容验证。

不要因为现有测试依赖某个对象，就默认该对象必须继续存在。先判断对象本身是否仍属于目标架构。


---

## 子代理编排与接管

子代理只在任务范围、输入 revision、文件所有权和验收条件已经明确时启动。每个委派说明：

- 目标、范围、禁止触碰的文件和当前输入状态；
- 前置依赖、输出物、定向测试和完成条件；
- 是否允许并行，以及与其他子代理的冲突边界；
- 无产出阈值、取消方式、主代理接管路径和交付报告格式。

主代理对最终结果负责：不把子代理自报完成当作证据，合并前复核 diff、关键调用链、测试和失败路径。子代理达到任务台账设定的无产出阈值（默认 10 分钟）时，先检查实际进程和工作树，再选择继续、发送当前状态、接管或中断；不通过无限等待掩盖阻断。

---

## Rust 与产品边界

Singularity 核心产品代码和发布二进制使用 Rust。

允许使用职责明确的主流辅助工具进行构建、测试、审计和维护，但不要形成第二套 Agent 核心实现。

目标仓库可以使用 Python、Rust、Node.js、Go 或其他语言；不要把 Singularity 自身实现语言误作目标仓库语言限制。

CLI、TUI 和未来 Desktop 应消费同一核心 Agent 能力。


不要为了未来桌面端提前构造多进程服务、通用 daemon、多客户端调度、CQRS、Event Sourcing、分布式队列或其他没有当前消费者的基础设施。


---

## Provider 与外部协议

Provider 行为以当前配置、实际 API 合同和真实 wire 证据为准。

Provider 能力（Context Window、Tool Calling、Reasoning、API protocol、并发能力等）以模型静态声明（内置模型表 + 用户配置覆盖）为候选能力来源，不凭模型名称猜测。静态能力、用户配置、Agent effective 配置和实际 wire payload 是四个不同事实；遇到协议中断、截断、超时或并发异常时，先核对实际请求中的 model、protocol、reasoning、context/output 上限、timeout 和并发，再判断是否属于可重试的外部失败。配置构造错误按产品/配置缺陷诊断，不交给重试或 fallback 掩盖。

不要为了统一抽象而建立没有真实消费者的 capability system。

只有当不同 Provider 的真实协议差异确实需要显式建模时，才增加对应字段或类型。

Provider transport retry 只处理可明确重试的同一请求失败，不通过换模型、静默 fallback、重采样或吞错制造成功。

外部协议输入在本地边界进行必要校验，但不要为理论上所有非法状态构建庞大内部状态机。

错误应保留足够的真实因果差异，方便 Agent、用户和测试判断失败来源；不要依赖字符串匹配驱动关键控制流。

具体使用哪个 Provider、模型、reasoning level、API key 或 Evaluation 模型由当前配置、任务要求或测试配置决定，不写死在仓库长期指令中。

---

## 失败诊断、归因与第一性原理修复

* 所有 Agent、Provider、Tool、Session、Evaluation、CI 或客户端失败，必须遵循第一性原理：失败、超时、崩溃或错误产物只是观察结果，不得先归因于模型能力，更不能因为最后一次模型输出错误就停止调查。
* 固定输入、配置、版本和环境，从“目标与约束 → prompt/context/history/turn budget → 实际 wire → Provider/transport/解析 → Agent/tool/session 状态 → checker/进程 → 最终产物”逐段重建因果链；为候选根因建立最小可复现反馈环和对照实验，模型或子代理报告只作线索。
* 只有在证据链闭合、所有现实可行的产品、工具、协议、Provider、题目、checker、环境和资源解释均被排除，并且稳定对照实验确认失败属于模型能力边界时，才允许作出已确认的模型能力归因。否则必须保留未知、待验证或外部阻断状态，不得以“模型不行/不稳定”作为停止调查、停止修复或缩小范围的依据。
* 确认项目根因后，修复拥有该不变量的最小正确层，增加能捕获原症状的回归验证并重跑原始场景；提高 `max_turns`、timeout、重试次数或更换模型只能作为用户明确要求且已独立记录根因后的行为变化，不能替代根因修复。

---

## 测试与验证

验证目标是证明当前修改满足真实行为，而不是证明所有历史机制仍然存在。

按最终 diff 的实际风险选择最小充分验证。

默认顺序：

1. 静态检查和直接源码事实；
2. 精确受影响测试；
3. 受影响 crate 的 `check` / `clippy`；
4. 必要的跨模块集成测试；
5. 涉及 harness 真实链路行为的验证运行真实模型调用（见下方细粒度原则）；纯逻辑单元测试使用 mock；
6. 只有用户明确要求或修改确实涉及完整 Evaluation 行为时才运行昂贵 Evaluation。


### 长任务与后台验证

Evaluation、长时间构建、CI 观察和多子代理等待默认采用非阻塞运行。任务启动时记录开始时间、资源边界、预期进展和截止条件；检查只读取最新状态，不通过长时间阻塞等待制造进度。连续一个检查周期没有新证据时，转为现场诊断、缩小实验或中断并保留现场。进程树、管道、临时目录和后台任务必须有明确的清理与恢复路径。

不要因为修改发生在 Agent、Provider、Store、Tool 或 Evaluation crate 就机械运行全仓测试。

已通过且未受后续修改影响的证据继续有效，不机械重跑。

Mock、fake 和 test double 用于证明确定性边界；真实模型调用用于证明真实模型交互行为。两者根据测试目的选择


### Evaluation 运行合同

Evaluation 是验证工具，不是无目的的重复运行。每次运行前在任务状态中记录本次要改变的结论、使用的模型/Provider、题目集合、并发、超时和与上次不同的变量。

验证顺序优先采用：题目原始状态与参考解可解性 → 单 cell 冒烟 → 受控小批量 → 用户裁决的批次 → 完整套件。相同题目、相同模型、相同并发和相同假设连续失败后，先改变诊断方向或缩小实验，不原样重跑全量。

每个 cell 同时记录并报告：

- 产物/工作区是否达到 checker 目标；
- Agent turn 是否正常完成；
- Provider/transport 是否正常；
- checker 是否正常执行；
- 评估进程是否正常退出。

产物通过但 turn 或评估进程失败时，记录为多维结果，不静默升级为整体 passed；失败先分类为模型能力、题目规格、参考解/checker、Provider/协议、环境/资源或 Singularity 产品缺陷。任务规格必须能让正确执行的 Agent 通过，且应有参考实现证明 checker 可用。失败 cell 必须审查 transcript、原始输出和 checker 证据后再归因。

### Evaluation 失败归因与第一性原理修复门槛

* 遵循第一性原理：Evaluation 的 `failed`、`crashed`、`partial`、`timed_out` 或 checker 非零只是观察结果，不是“模型能力问题”的结论。不得因为模型最后一次输出错误、没有继续行动或 turn 失败就停止调查。
* 对每个失败 cell 固定题目、参考状态、配置、Provider/模型、并发、超时和环境，按“题目规格/参考解 → prompt/context/history/turn budget → 实际 wire payload → Provider/transport/解析 → Agent/tool/session 状态 → checker/评估进程 → 最终产物”逐段核对；使用最小可复现 cell 和对照实验排除 Harness、工具、协议、Provider、题目、checker、环境与资源等替代解释。
* 只有在上述证据链闭合、所有现实可行的非模型解释均被排除，并且相同约束下的稳定对照实验能证明失败属于模型能力边界时，才允许报告“已确认的模型能力问题”。证据不足时必须标为未知、待验证或外部阻断，不得以“模型不行/不稳定”作为停止修复的依据。
* 发现 Harness 或任务边界根因时，修复拥有该不变量的最小正确层，增加能捕获原症状的回归测试，重新运行原始 cell；不得用提高 `max_turns`、延长 timeout、增加重试、切换模型、吞错、放宽 checker 或重采样掩盖结构性问题。预算或模型变更只有在用户明确要求、且根因已独立记录时才可作为额外行为变化。

### 真实模型调用测试（细粒度原则，长期要求）

* 涉及 harness 真实链路行为的验证——Provider 调用与协议格式、上下文组装、Compaction 触发与摘要、工具执行与回放、Agent 循环行为、客户端链路——必须使用真实模型调用验证；fake/mock/test double 只允许用于与链路行为无关的纯逻辑单元测试（JSON 解析、时间戳、切点算法、统计聚合等）。
* 测试细粒度化：一次真实模型请求验证一个环节，不必每次跑完整任务。例如：
  - 验证 Compaction：手工构造接近窗口上限的会话上下文 → 真实调用生成摘要 → 检查摘要合理、落盘与重建正确、后续请求被 API 接受；
  - 验证工具调用格式：构造含 assistant tool_calls 的历史 → 真实调用确认重放被 API 接受（孤立 tool_call_id 会被真实 API 拒绝，曾实测 HTTP 400）；
  - 验证上下文组装：构造会话文件 → build_session_context → 真实调用确认消息序列合法。
* 阶段验收（重构期间）：每个阶段涉及的新机制各做一次细粒度真实请求测试；完整任务评估（固定题集 × 多模型、checker 判分、指标聚合）由独立轻量评估工具承担，不是阶段验收的默认方式。
* 真实调用测试必须隔离：使用临时目录、临时 SINGULARITY_HOME / DB 路径，不触碰真实用户配置与凭据（配置仅以副本形式读取）。
* 真实调用结果以证据形式留存（会话文件/请求摘要存档），报告注明实际命令与输出，不伪造成功。
* 重构阶段验收：每阶段完成后须做语义审查（逐文件复核 diff 与规格对照、抽查关键调用链与错误路径，不只看测试绿），并运行该阶段新机制的细粒度真实请求测试；本地提交可自行创建，push 必须用户明确授权。

测试已经存在不能证明对应架构必须保留。重构删除机制时同步删除只服务于该机制的测试、fixture 和 helper。

不为了通过 Evaluation 或 benchmark：

- 修改真实产品语义；
- 添加任务特判；
- 放宽正确性约束；
- 隐藏失败；
- 重采样直到成功；
- 调高 timeout 掩盖结构性问题。

最终报告必须明确：

- 实际运行了什么；
- 哪些通过；
- 哪些失败；
- 哪些没有验证；
- 哪些结论只是推断。

不要把局部证据表述为全量通过。

---

## Cargo、临时文件与磁盘

本项目 Cargo 构建产物使用专用 target 目录，不提交机器专用绝对路径配置。在本机运行 Cargo 前，先用 `cargo metadata --no-deps --format-version 1 --locked` 核对 target_directory；当前机器应使用 `D:\CargoTargets\singularity-agent`，不得静默回落到工作区或系统盘的 `target/`。

默认保留 Cargo 增量编译缓存，不在普通任务结束时执行 `cargo clean`。

只有以下情况才清理缓存：

- 缓存损坏；
- Rust 工具链发生不兼容切换；
- target 归属不明；
- 磁盘空间问题；
- 用户明确要求。

临时测试目录、一次性 worktree、诊断日志和 scratch 文件在确认属于当前任务后清理。

删除目录或文件前确认：

- 真实绝对路径；
- 所有权；
- 是否由当前任务创建；
- 是否包含用户数据或未知成果。

不得为了获得 clean working tree 删除来源不明的文件。

---

## Git 与远程操作

开始任务前检查：

- 当前 branch；
- HEAD；
- staged / unstaged；
- tracked / untracked；
- 必要时 ignored 状态。

保护用户已有修改。

未经用户明确授权，不：

- push；
- force push；
- 发布 release；
- 创建 PR；
- merge 远程分支；
- 创建或关闭 GitHub Issue；
- 修改其他远程状态。

不得使用 `reset`、`rebase`、`clean`、强制 checkout 或其他破坏性 Git 操作丢弃未知修改，除非用户明确授权该具体操作。

大型改动可以按独立、可审查、可回滚的阶段创建本地提交；简单修改不为了形式完整强制拆 commit。push 只对最终候选执行：先完成独立审查、定向/真实链路验证和必要 Evaluation，再在本地门禁全部通过后 push；CI 在 push 后验证，失败时回到本地修复闭环。一次 push 授权不代表可以提前 push 或跳过这些门禁。

Git 历史保存历史实现。当前代码和文档只描述当前有效设计，不为尚未发布的旧本地方案增加兼容层。

---

## Issue 与任务记录

优先复用与任务直接相关的现有 GitHub Issue。

创建新 Issue、修改或关闭 Issue 必须有用户明确授权。

复杂任务如果已有 Issue，记录应聚焦：

- 问题；
- 已确认根因；
- 最终方案；
- 关键验证；
- 未解决范围。

不要把原始思维过程、所有读取文件、每条命令或大段日志写入 Issue。

相邻缺陷和未来需求不自动进入当前任务完成条件。

---

## 文档

`docs/singularity.md` 维护当前核心架构概览，包括主要 crate、对象、调用链、状态、持久化和客户端边界。

如果某个领域未来需要独立详细文档，从 `docs/singularity.md` 链接到对应文档，不复制多个互相竞争的架构事实源。

架构发生实质变化时同步更新当前事实文档。

文档只描述当前有效设计，不保留已经结束的迁移过程、历史路线和失效接口。历史由 Git 和必要的 Issue 保存。

具体 Evaluation 操作、工具使用说明、测试运行手册和临时任务规则放在对应 `docs/`、Skill、配置或当前任务中，不塞入仓库全局指令。
当前架构重构的目标形态与用户裁决见 `docs/singularity.md`（流程图形态，随项目实时维护）；与定基线冲突的新设计决策需用户裁决。

---

## 代码注释与可读性

模块、公共类型、Trait 和职责无法直接从名称判断的重要函数应有简洁注释。

注释说明：

- 对象是什么；
- 负责什么；
- 为什么需要；
- 重要的不变量或非显然约束。

不要逐行解释代码，不为简单 getter、字段转发或明显 Rust 语法机械增加注释。

代码职责或行为变化时同步更新相关注释；过时注释视为缺陷。

Rust 公共 API 优先使用 `///` 和 `//!`。

命名优先采用 Rust 生态、Agent、LLM、Tool Calling、Session、Context、Provider、RPC 等领域已经广泛使用的术语。

不要仅根据中文语义创造新的架构名词。

---

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
