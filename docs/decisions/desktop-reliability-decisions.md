# Singularity 产品形态与可靠性决策记录

> 本文件保存当前仍然有效的决策依据与取舍。已实施决策的当前事实以 docs/singularity.md 与源码为准；实施记录另行保存并引用本文件中的决策编号。已被取代或机制已不存在的历史条目不保留于本文件，其演进过程由 Git 历史保存。
> D-073 之前条目里的命令名 `sg` 即现命令 `singularity`；条目原文按当时实际敲下的名字保留，不作为当前接口。

## 目标

Singularity 提供无交互单次入口与交互式 TUI 两种当前形态，桌面端为规划形态；各形态复用同一 headless Agent core、Session 和执行语义，执行全部委托 runtime。

## 决策

### D-001：Session durability

采用轻量持久化边界：普通追加保证完整写入、进程崩溃后可恢复以及内存状态不领先文件；不把每条追加都升级为断电级 fsync。已有 rewrite 的更强同步保留。

### D-002：Evaluation 分层

保留默认 3 tasks × 2 models 的快速 Evaluation；增加独立核心链路 smoke 集，覆盖 Responses restart、compaction、steer、会话恢复、并行工具等，不把所有链路塞入默认任务评估。

### D-004：交互式取消

必须支持 Bash 按进程组/作业整体取消，并为 Windows/Unix 提供回归测试；取消不能只停止 Agent loop 而留下后台子进程。

### D-006：崩溃中的 turn

进程重启时，活动 turn 标记为 interrupted，恢复已持久化历史并允许用户继续；不自动重放未完成 turn，避免重复工具副作用。

### D-008：孤立工具调用

工具调用已落盘但结果缺失时显示为"中断/状态未知/不可自动重试"，不伪装成普通失败，不自动重复执行；用户可通过新的明确请求决定是否重试。

### D-009：显式 turn 生命周期

采用最小显式 turn 生命周期记录，消息与工具保持 JSONL 追加。v4 实现为两条 operation ledger 记录承载一轮的生命周期：`operation_started`（接受意图，run 记录携带本轮冻结的模型配置快照）与 `operation_finished`（`outcome ∈ completed|failed|interrupted`，run 的终态同时是该 turn 唯一持久终态事实）。不构造完整 Event Sourcing。

### D-010：恢复修复

重开 Session 时把 durable 前缀归约为 operation 事实；发现未终结的 run operation 时，由修复一次性收敛出幂等的 `operation_finished(interrupted)`（未解决的工具调用补 synthetic failed ToolResult，`replay: never` 的已启动调用绝不重放），而不是每次读取都重新推断。归约只折叠事实、不审计写入者：跨进程排他由 OS 写者锁保证、记录由单一写者顺序追加，无法归约的记录无害跳过，开放 operation 各自独立收敛。

### D-013：事件背压

保留有界队列、满时阻塞、不主动丢事件的策略。未来只有在真实性能证据出现时才合并 delta，不提前引入复杂事件分类。

### D-014：模型切换

运行期间切换模型/Provider 不重启进程；当前 turn 保持原配置，切换从下一 turn 生效。thread 设置持久化，下一轮按新模型重新预算并必要时 compaction。

### D-015：Provider 私有续接

切换模型/Provider 后，不兼容的 provider-private reasoning replay 丢弃；可见消息、tool call、tool result 保留，从新 Provider 的公开历史重新开始。不得尝试跨协议转换 opaque replay。

### D-017：凭据边界

`thread_settings` 不保存 API key、Authorization header 或其他认证材料；凭据继续由全局配置/环境管理。

### D-018：配置不可用

恢复 thread 时原 Provider/模型不可用，历史仍可读，但继续执行前必须由用户选择可用配置；禁止静默 fallback 或偷偷改写 thread 设置。

### D-019：权限阶段

明确显示本机完整权限，核心不内置 Approval/Sandbox；保留独立扩展接缝，正式普及前再实现可选隔离。

### D-020：工具输出

保留 2000 行/50KB 有界工具输出；超限时 spill：显示有界预览、明确截断、提供完整输出文件引用。

### D-021：完整输出生命周期

完整 Bash 输出初版作为临时 artifact，UI 明确提示可能过期；Session 永久保存截断预览。只有明确要求跨天永久查看时，才新增 Session 专属 artifact store。

### D-023：Item 生命周期

补齐 `item/completed` 生命周期：每个 `item/started` 必须对应 terminal 的 `item/completed` 或 `item/failed`，再发布 turn 终态。工具详细参数和结果继续通过已有 `tool/execution/*` 事件传递。

### D-024：Item 持久化

持久化最终 item、稳定消息、工具结果和 turn 状态；在线 delta 只用于实时显示，不逐字/逐 chunk 写入 JSONL。重连从最终 item 和 Session 状态重建，不实现 cursor/gap 事件重放。

### D-025：工具 Item

工具调用纳入统一 item 生命周期。每个 `toolCallId` 对应一个稳定 item，事件顺序为 `item/started` → `tool/execution/start` → `tool/execution/update`* → `tool/execution/end` → `item/completed` 或 `item/failed`。两类事件必须共享同一身份和终态，避免客户端维护矛盾状态。

### D-026：并行 Item 顺序

并行工具的实时事件按实际完成顺序到达；Session 中的最终 tool result 仍按模型 source order 持久化。UI 获得低延迟进度，模型上下文保持稳定顺序。

### D-034：Thinking 公开投影

公开显示文本 thinking block。DeepSeek 等 Provider 明确返回的完整 `reasoning_content` 可以完整保留并折叠展示；过滤 replay 元数据、Responses opaque output items 和 `encrypted_content`。最终历史读取采用公开结构化投影，不返回原始 SessionEntry。

### D-036：Metadata 上下文隔离

turn 生命周期 ledger 记录（`record` 条目）、thread_settings/thread_name metadata 与 usage/compaction 的审计字段只用于恢复和客户端展示，不进入模型请求上下文；模型仍只接收 user/assistant/toolResult 及必要 compaction 投影。

### D-038：计划文件交付

实施计划在执行期间作为被 Git 忽略的本机交接文件使用，不修改 `.gitignore`；计划文件本身不代表代码已实施或已验证。执行完成后该临时计划可以删除，当前有效架构和决策以本文件、`docs/singularity.md`、源码和 Git 历史为准。

### D-039：开发阶段无兼容义务（硬切原则）

问题：项目未发布、无外部用户，重构中为旧字段、旧格式保留的读取兼容层只有维护成本，没有真实消费者。现状：wire 协议、会话 JSONL 与本地 config.json 在重构期间形状会多次变化。选择：开发阶段旧格式与旧字段一律硬切删除，不做读取兼容层；凡确需兼容必须写明理由与移除条件。本机 config.json 或旧会话文件残留旧结构时直接手动删除。影响：协议、会话格式与配置可以一次到位；本地已落地的旧结构文件需手动清理。验证：每个 Phase 门禁（fmt / clippy -D warnings / 全量测试）全绿，删除的旧形状无残留引用；确需兼容处有成文理由与移除条件。

### D-040：双 wire 协议骨架去重（P5-1）

问题：chat completions 与 responses 两套 wire 编解码各复制一套重试循环、流式解码与 attempt 遥测骨架，双协议维护成本翻倍且行为有漂移风险。本决策保留双协议（DeepSeek 等走 chat，OpenAI 原生走 responses）。现状（合并已实施）：重试、流式解码与 attempt 遥测为共享路径（`transport/mod.rs` 单文件 `ProtocolAdapter` 薄转发表 + `transport/stream.rs`），只有各协议编解码分别位于 `openai/chat.rs` 与 `openai/responses.rs`（形状再评估见 D-052）。影响：协议新增与重试/遥测行为修改只在单处落地。验证：双协议各自的 provider 测试全绿 + 至少一次真实 DeepSeek chat 冒烟（输出证据留存 outputs/）。

### D-042：凭据单文件原子替换（P5-3）

问题：早期世代机制使凭据目录出现多文件，导入也不清理旧世代。选择：只保留一个 `auth.json`；导入 = 写临时文件 + 同卷原子 rename；删除世代机制。影响：凭据目录恒为单文件；导入失败不留下半写文件。验证：auth 读写测试 + 目录单文件断言。

### D-043：AGENTS.md 预算截断（P5-4）

问题：项目指令超限（FileTooLarge/TotalTooLarge）现直接报错，使整个 turn 无法开始；超长 AGENTS.md 是常见现实。现状：`project_instructions.rs` 超限返回错误 → runner 使 turn/start 直接失败。选择：超限不报错：按预算截断并纳入前缀 + 发诊断告警"项目指令被截断"；真正 I/O 错误仍报错 fail closed。影响：超大 AGENTS.md 不阻断任务，截断对模型可见且客户端收到告警。验证：project_instructions 测试断言超限走截断 + 告警；I/O 错误路径断言报错。

### D-046：会话索引进程内化

问题：早期决策以 SQLite 会话索引为前提；该 store 层独立于 JSONL 唯一权威之外形成第二落盘事实，增加崩溃恢复与修复路径。现状：会话正文 JSONL 是唯一权威；进程内无常驻索引对象，列表、摘要与分页按需扫描顶层 JSONL 产生定位与展示元数据，退出不落盘。选择：不设第二持久化索引；持久事实与发布顺序遵循「先写 JSONL 再发布事件」（durable 先于发布）。影响：无索引修复路径、无 SQLite 依赖；会话删除把文件 rename 进 `archived/` 子目录归档保留（见架构文档归档条目）。验证：JSONL 唯一权威链路（终态事件发布前完成写盘）测试全绿。

### D-047：共享运行时硬切

问题：若各形态各自实现 turn 执行会造成多套状态机与事件投影漂移。现状：crates/runtime 是 Turn 执行的唯一所有者——TurnRunner 单轮管线（会话单写者贯穿、typed TurnEvent 事件源、fail-stop 终态化、明细终态原子收敛）与 Conversation 长驻协调器（`reserve_start` 原子预订链窗口、steer 注入当前轮、followUp FIFO 逐条自执行为独立新 turn、取消按轮独立）；CLI 无参数进入 TUI，--print/--json 单次执行。选择：客户端形态（TUI / headless）一律委托 runtime，客户端不复制执行状态；协议类型只存在于 crates/protocol，runtime 不依赖 UI。影响：任意客户端复用同一执行语义，替换客户端不触碰 runtime。验证：runtime 单元与集成测试全绿。

### D-049：产品形态与桌面端定位

问题：需要固定产品形态边界，避免把桌面端描述为泛化接入面。现状：无交互与 TUI 进程内调用 runtime。选择：产品为无交互单次入口、交互式 TUI 两种当前形态，桌面端为规划形态；桌面端接入时以 stdio JSON-RPC 适配层把 runtime 事实投影为协议，不构成独立用户入口、不复制执行语义。影响：产品文档、客户端合同和后续桌面端工作以此定位为准。

### D-051：会话单写者由 OS 文件锁强制执行

问题：单写者此前只由进程内约定（activate_turn 预订）保证，跨进程无法互斥；归档删除的「活动 turn 检查 → 打开校验 → rename」之间存在 TOCTOU 窗口，另一进程可在校验后开始 append。现状：SessionManager 是 JSONL 唯一可变持有者，`open_existing` 已含 repair 重写；toolchain 1.96 ≥ try_lock 稳定版 1.89，标准库直用零新依赖。选择：任何可能写 JSONL 的打开（create_with_file、open_existing 含 repair）都先获取会话写者锁，Guard 为 SessionManager 字段随实例释放；锁目录为 sessions 同级 `thread-writer-locks/`，目录创建走 `create_owner_only_dir`；`open_existing_read_only` 不加锁。归档先 try_lock 快速失败（冲突映射为「session is being written by an active writer」），校验与 rename 全程持锁，TOCTOU 随之消失。影响：跨进程双开同一会话的第二写者被快速拒绝；同进程测试中原「顺序双开」按新语义改为先释放再打开；只读投影路径不受影响。验证：writer_lock 单元测试（竞争拒绝/释放后复用/stale 清理/跨线程快速失败）、归档 vs 活动写者集成测试、resume 双开冲突测试。

### D-052：Wire 分派形状

问题：是否将 transport 的 ProtocolAdapter 重构为 trait 或分拆到各协议文件？
现状：ProtocolAdapter 薄转发表集中于 `crates/model/src/transport/mod.rs` 单文件（端点、请求载荷、reasoning 在场判定、响应解析、SSE 读取），各协议实现体分别位于 `openai/chat.rs` 与 `openai/responses.rs`。运行时协议选择下 trait 化不减少分支总数，仅拆分事实源位置。
选择：维持当前集中薄转发表，接入第三 wire 协议时重评该形状。
影响：双协议维持单点转发表，不引入额外 trait 间接层。
验证：双协议 provider 单元与集成测试全绿，wire golden 测试无漂移。

### D-053：ThreadCatalog 成为 Thread 目录操作与只读投影的唯一入口

问题：客户端和各调用点逐点传递 `(sessions_dir, coordinator)` 元组并直接调用 `store` 模块的底层函数，导致会话目录操作接缝发散。
现状：`ThreadCatalog` 封装 `sessions_dir` 与进程级写者锁协调器 `WriterLockCoordinator`，目录行为直接由其方法实现。
选择：`ThreadCatalog` 成为创建、列表、恢复、重命名、归档和只读分页历史（`paged_read`、`read_thread_summary`）的唯一公开入口；`Conversation` 不持有目录 CRUD。
影响：调用方只需持有 `ThreadCatalog` 单一实例，目录操作集中且易于测试与扩展。
验证：runtime 单元与集成测试、cli 目录操作测试全绿。

### D-054：Turn 终态与错误词表收敛为 protocol 单点定义

问题：runtime 与 protocol 之间曾存在平行的终态枚举与错误原因词表，增加了跨层映射与词形同步负担。
现状：`Thread`、`Turn`、`TurnStatus`、`TurnModelUsage`、`TurnFailureCause`、`TurnFailureStage` 统一在 `crates/protocol` 单点定义，runtime 经 `objects.rs`/`error.rs` 原样再导出。
选择：消除平行枚举与重复词表，protocol 成为 wire 形状、事件枚举与状态词表的单一权威事实源；runtime 负责执行语义并将具体 model 失败映射至 protocol 的 `TurnFailureCause`。
影响：跨层类型和错误词形零冗余，golden 测试单点守护线格式。
验证：protocol 与 runtime 测试全绿，错误词表一致性测试通过。

### D-056：设置落盘在 turn 边界记录

问题：设置变更提交点若直接写会话文件，需要获取会话写者锁；turn 执行期间锁被活动 turn 占用，「运行中改设置」因此成为报错场景——提交点写文件是把持久化放在了错误的层。
现状：`Conversation::update_settings` 在提交点只做校验与内存投影更新；`thread_settings` 的落盘由 `TurnRunner::run` 在 turn 开始时、于本轮已打开的同一会话写者上执行（与最后一条已记录值相同则跳过，位于 `operation_started` 之前），失败映射为 `Preparation { cause: Store }`。
选择：turn 边界记录编排——变更提交点只更新内存投影（运行中与空闲同路径，提交点不会因写者锁冲突失败）；持久化发生在下一 turn 开始时由 turn 记录；空 patch 返回 `NothingToApply`。
影响：无提交点持久化与回滚分支；会话列表与分页读取在下一 turn 运行前显示旧值（只读已落盘值的不变量由定义保持）；进程在下一次 turn 开始前崩溃会丢失未记录的变更（已知后果）。
验证：runtime 预订窗口测试断言提交点零写入；写者锁占用下的 mid-turn 提交测试（提交被接受、下一 turn 开始记录新 selector）；workspace 测试全绿。

### D-057：过度设计移除与单一所有者收敛

问题：多处机制的复杂度高于其承担的职责，需要按长期质量重构。原则：不保留旧结构，架构决策采用业界已验证的相同逻辑与语义；复杂度明显高于必要性的设计认定为过度设计并移除。
决策与现状（按层）：
- model：`complete_stream` 是 `Provider` trait 唯一必需方法、SSE 是唯一读取路径（流式能力声明与非流式 fallback 属过度设计，移除）；reasoning 累加按键序取首个非空键、每块只累加一次；流内与 200 内嵌错误按 wire 错误码类型化（`context_length_exceeded`/`rate_limit_exceeded`/`insufficient_quota`/`content_filter`），保留 provider 原文（有界）与 `provider_error_code` 诊断；Disabled 契约只约束需要 replay 的工具调用续接；不可恢复的响应校验在 provider 边界以类型化 `Err` 收敛。
- agent：`thread_settings` 落盘形状 provider+model 必填、reasoning 可选；compaction 条目持久化 summary/firstKeptEntryId/usage/details（估算规模经完成回调交付，不落盘）；reasoning replay 只从已存条目读取（发送侧重建分支移除）；会话层 `ThreadSummary`（`singularity_agent::session`）是 JSONL 派生摘要的唯一结构，外部只经 `ThreadCatalog` 摘要/分页投影的返回类型到达，runtime 根不再并列导出。
- runtime：turn 窗口释放按预订身份（世代序号）比对，迟到的旧 Drop 不得清空新窗口；失败终态事件携带本轮已记录的真实 usage。
- cli：edit 工具对文件原文逐字节精确匹配（换行/BOM 转换层属自设复杂度，移除）；会话换绑取消进行中的压缩并以会话世代号丢弃迟到回调；换绑门禁同时检查 TUI 相位与 runtime 活动窗口。
影响：删除一层能力声明、一条非流式路径与 edit 的全部换行/BOM 转换机制；压缩/换绑收敛为单一所有者语义。
验证：workspace fmt/clippy -D/test 全绿；新增回归测试覆盖窗口身份释放、错误码类型化映射、reasoning 双键单累加、Disabled 契约无工具调用合法性、失败终态 usage。

### D-058：会话列表降级为头部元数据

问题：`list_threads` 此前对每个会话文件完整解析并聚合（turns/tokens/title/model/status），列表打开成本 O(N×文件大小)；列表只需头部元数据 + 文件 mtime，无需逐会话聚合。
现状：agent 提供 `read_session_header`（严格 header 校验、坏文件 `Err` 由列表跳过）与 `file_modified_iso`；runtime `list_threads` 返回 `ThreadListing { thread_id, cwd, created_at, updated_at }`；TUI `/resume` 菜单显示 short id + 更新时间。
选择：列表只读 header + mtime；title/model/status/turn_count/total_tokens 全部移出列表类型。单会话聚合事实按需读取：`/session`、`/resume` 换绑初值、分页历史继续走单文件 `read_thread_summary`，不受影响。
影响：列表打开从 O(N×文件) 降为 O(N×首行)；`/resume` 菜单不显示标题/回合数/token 数。
验证：workspace fmt/clippy -D/test 全绿；`ThreadSummary` 消费点复核（仅单文件路径）。

### D-059：压缩期输入排队

问题：压缩持有会话一致性写窗口，此前压缩期间一切输入被拒绝（草稿保留）。压缩期间界面持续渲染，输入排队比拒绝更可用。
现状：TuiApp 维护 `compaction_queue: Vec<QueuedMessage{text, mode}>` 与 `QueueMode{Steer,FollowUp}`；submit_input/Alt+Enter 在压缩运行时入队（命令仍走 execute_command）；Alt+Up 优先整体出队回编辑器，队列空时维持 followUp 逐条撤回；换绑 `reset_session_state` 清空队列。
选择：flush 拆两段以避开与 turn 预订的竞态——`on_compact_finished` 把首条按普通提交启动回合（返回 `Action::Submit`，事件循环 spawn），其余在该回合 `TurnStarted`（注入窗口已开）时按通道注入，注入失败的退回队列不丢输入；队列与压缩结果无关地消费（取消/失败同样送达）。状态行显示 `queued:N` 计数与压缩期提示行。
影响：压缩期间输入不被拒；新增一个 TUI 暂存状态，接入既有 epoch/换绑/取消交互网；runtime/协议零改动（排队纯 UI 层，压缩窗口不变量不受影响）。
验证：workspace fmt/clippy -D/test 全绿；行为面为 TUI 交互（无既有测试脚手架，不新建测试基础设施），由评估器最终轮与手工走查覆盖。

### D-060：事件 wire 形状由 typed 枚举自身序列化

问题：`TurnEvent` 的 camelCase wire 形状由 protocol 内一份手写 `json!` 投影（约 148 行）决定，枚举与投影是同一事实的两个表示：新增或改字段必须同时改两处，且只有一处被 golden 覆盖。
现状：runtime 与全部客户端只使用 typed `TurnEvent`；envelope `{"method","params"}` 由 `turn_event_envelope` 单点生成。Pi 的对应结构是 `packages/agent/src/types.ts:429` 的 `AgentEvent` 判别联合——事件对象本身就是 wire 载荷，没有第二份投影层。
选择：把 wire 形状做成 `TurnEvent` 自身的 serde derive（`#[serde(untagged)]` + 各变体 `rename_all = "camelCase"`），嵌套载荷（`item`、`result`、`content`）由具名 payload 结构承载；删除手写投影。envelope 生成点与 golden 测试表保持不变。
影响：一个形状一个表示，字段增删只剩枚举一处；单条 `item/agentMessage/delta` 的投影多 4 次分配、多分配 14 字节内存（1474 B/11 次 → 1488 B/15 次），输出的 wire 字节不变，相对每 token 的模型往返成本不可测量。
验证：改动前后 `crates/protocol/tests/contract.rs` 的 14 个事件 golden 字符串逐字不变（`cargo test -p singularity_protocol` 通过），并以独立差分实验对比 derive 输出与原投影（14 事件，0 处不一致）。

### D-061：公开历史的单一事实归属与回放可见性

问题：同一 turn 的终态在读取面上有多个表示——`ThreadTurn.status`、轮内 `HistoryItem::Turn` 条目、以及 `Thread.last_turn_status`；同时 `HistoryItem::Compaction` 与 `HistoryItem::Settings` 有生产构造点却无任何读取者，`/resume` 回放因此不显示压缩点与设置变更。
现状：turn 终态的唯一落盘事实是 `operation_finished`；`ThreadTurn` 由 `project_turn_history` 按 run 边界分组产出；`sg --json` 的 summary 与 `/session` 各自走独立投影。Pi 在交互模式里为压缩摘要渲染专门组件（`packages/coding-agent/src/modes/interactive/interactive-mode.ts:3622`），即压缩点在历史可见面上是一等条目。
选择：turn 的状态与身份只归属 `ThreadTurn`，删除 `HistoryItem::Turn` 与 `HistoryItem::Usage`、删除 `Thread.last_turn_status`（及其 wire 键 `lastTurnStatus`）；崩溃收敛不变量（FR-016）改由 `ThreadSummary.status` 与 `ThreadTurn.status` 两个投影面钉住，原断言全部重定向而非删除；TUI 回放补渲染压缩点与设置变更两行 note，`HistoryItem` 的 match 因此穷尽、不再有静默吞条目的兜底分支。
影响：`Thread` 对象收缩为 `thread_id/model/cwd` 三个已解析事实；`HistoryItem` 由 8 型减为 6 型；恢复后的会话流不再对模型可见的压缩边界与换模型事实失明。
验证：workspace 全部门禁绿；`recovery_tests`、`conversation_tests`、`thread_catalog_tests` 与 protocol wire golden 在断言重定向后全部通过。

### D-062：写者锁只依赖句柄生命周期

问题：会话写者锁在 Guard Drop 时删除锁文件，而 Windows 上必须先关闭句柄才能删除，删除又与另一进程的清扫竞争，为此引入了一把跨进程的阻塞协调锁，把 acquire 与 Drop 都串起来。
现状：跨进程排他由 `File::try_lock`（Rust 1.96 稳定）在锁文件上强制，锁的持有等价于句柄存活；锁文件存在与否从不参与互斥判断。`remove_stale_thread_locks` 用 `try_lock` 自测存活，不依赖协调锁。
选择：Guard Drop 只释放句柄、不删文件；删除协调锁与其文件，目录创建移入 `acquire`；一次性 stale 清扫保留，负责回收无人持有的残留。锁文件数量与会话数量同阶，且残留文件对下一次获取无害。
影响：acquire 与 Drop 各少一次跨进程阻塞等待；Windows 的句柄/删除顺序约束消失；`WriterLockGuard` 不再需要保存路径。
验证：三个 writer_lock 单元测试（竞争拒绝、释放后复用、stale 清扫不伤活动锁）通过，其中释放后复用的断言改为「锁文件保留而 OS 锁已释放可被再获取」，直接钉住新语义。

### D-063：发布产物只含 sg 一个二进制

问题：Release 工作流与打包/签名脚本仍按两个包（`singularity_cli` 与已不存在的 `singularity_app_server`）构建、组包、签名并校验 SBOM，`cargo build --package singularity_app_server` 在解析阶段即失败，该工作流因此从未成功运行过。
现状：workspace 成员只有 core/protocol/model/agent/runtime/cli 六个 crate，交付物只有 `sg`；桌面端形态由既有条目约束，无独立二进制。
选择：发布链路只构建、签名、组包与校验 `sg` 一个二进制与其单份 CycloneDX SBOM，产物数校验从「恰好两份」改为「恰好一份」。
影响：Release 路径恢复可执行；每个二进制的发布成本（SBOM 暂存工作区、签名、attestation）减半。
验证：`cargo metadata` 无 `singularity_app_server` 包；打包脚本 `-DryRun` 早退路径实跑通过，两个脚本均通过 PowerShell 解析检查；`singularity_app_server` 在 `.github/` 归零。

### D-064：会话工作目录只有一个可用形状

问题：新建 Thread 的 cwd 经 `std::fs::canonicalize` 得到 Windows verbatim 路径（`\\?\C:\…`）并随系统提示词交给模型；同一事实在会话头里被换成 `//?/C:/…`，列表与投影各取一种写法。模型把提示词里的路径抄进 bash 命令即得 `cd: \\?\C:\…: No such file or directory`（真实评估轨迹中观测到）；TUI「在当前目录新建会话」又复制旧会话的字符串，使坏形状自我循环。
现状：`normalize_cwd_string` 与解析侧的 `normalize_cwd_text` 共同拥有该事实的唯一形状——正斜杠绝对路径、无 verbatim 前缀；`create_thread`、`resume_thread`、`ThreadListing`、`ThreadSummary` 与系统提示词都取这一个值。新 Thread 的目录直接来自 `std::env::current_dir()`。
选择：以纯词法的 `std::path::absolute` 取代 canonicalize 作为归一手段（不加 verbatim 前缀，也不要求目录已存在）。header 只在创建时写出、之后不重写，因此归一同时落在解析侧，使存量会话在读取时收敛。提示词把环境事实独立成行置于末尾、行尾不带句读，避免模型复制路径时连带标点。
影响：模型看到的目录与 shell 中可用的目录一致；并存写法归一；`canonical_thread_cwd` 连同其不可达的空值分支删除，runtime 少一个公开函数与一套并行归一机制。项目指令发现不受形状影响（verbatim 与归一两种形状下 `.git` 均在首层命中，实测）。
验证：`thread_cwd_projects_one_usable_shape_across_every_surface` 钉住创建、恢复、列表、会话头与提示词五处一致，覆盖冗余组件拼法、verbatim 输入与磁盘上的存量 `//?/` 头三种来源。两次故障注入分别令该测试失败：create 返回调用方原样字符串触发「resume rewrites the cwd」，去掉剥前缀触发 verbatim 断言。真实模型调用 `cd "<提示词路径>" && pwd` 返回 `/c/Users/…/<dir>` 且 `isError=false`。参考 Pi `packages/coding-agent/src/core/system-prompt.ts:39,166` 与 Codex `codex-rs/core/src/context/world_state/environment.rs:87`。

### D-065：一次 bash 调用不得无限期占住整个 turn

问题：`timeout_ms` 未提供时命令不超时（Pi 亦如此）。真实评估中模型写了 2×27×24×4×799 ≈ 414 万次迭代的穷举校验且未传 `timeout_ms`，sg 有约 500 秒没有任何 durable 活动，模型得不到反馈，整个 cell 直到评估器预算耗尽被杀。「一次工具调用可以无限期占住 turn」是 harness 属性，与具体模型和任务无关。
现状：`DEFAULT_TIMEOUT_MS = 300_000` 是命令的执行界；显式 `timeout_ms` 只放宽不收紧、无上限。界到点走既有的整树终止路径，把界前已捕获的输出连同终止原因与放宽办法一并返回并标记失败，等待环因此不再有「无 deadline」分支。
选择：给缺省路径加有限界，而不是依赖模型自觉传参——同一次评估 10 次 bash 调用零次携带 `timeout_ms`。界值取自两轮评估 284 次闭合调用的实测分布（p50 0.6s、p95 3.3s、p99 44.6s、最长合法调用 247.9s），既不截断任何实测合法调用又拦住不返回的计算。不采用 yield + 后台续跑：Pi 无此表面，Codex 的 `unified_exec` 位于实验开关之后，新增跨 turn 进程存储属加能力，超出以 Pi 为上限的基线。
影响：失控调用在 300 秒处变成一次可恢复的工具错误，模型随即能改用更大预算或收窄命令。单次调用界与整轮 cell 预算是两个层次：前者由本条目约束，后者见 D-066，且必须显著大于 300 秒，使一次调用不可能吃掉整轮预算。
验证：把界临时注入为 1500 ms 后真实模型调用 `sleep 20`（不带 `timeout_ms`）：工具实际占用 1.62 秒、`isError=true`、返回文本含终止与放宽说明，事后系统内无残留 `sleep` 进程。参考 Codex `codex-rs/core/src/exec.rs:61`。

### D-066：评估的 cell 预算按任务实测时长定，不按最坏模型表现压

问题：cell 预算过短时，评估测到的是墙而不是 agent 能力。实测 33 个通过的 `warehouse-audit` cell 最长用到 581 秒、38 个通过的 `cache-ttl` cell 有 2 个 ≥500 秒；预算低于这些值时，正在推进的工作会被判为 `timed_out`。
现状：`eval-config.json` 的 `timeout_secs` 取评估器自身的默认值 1800 秒（`DEFAULT_TIMEOUT_SECS`），六个任务六个 cell 并行，最坏墙钟约 30 分钟。判分仍以 checker 通过数为准，到点被杀的 cell 一律记为未通过，不按"差一点"折算。
选择：预算向上取值必须由**通过 cell 的实测时长分布**支持，而不是为了让某轮跑绿；同时它不得用来掩盖 harness 侧缺陷——单次工具调用的失控由 D-065 的执行界负责，先修界、再谈预算。
影响：长任务获得完成机会，`timed_out` 的含义变干净：它只表示整轮真的用尽了 1800 秒。每轮评估的等待与花费上限随之抬高，属已知代价。
验证：预算 1800 秒 ≫ 实测最长合法单次调用 247.9 秒 与 D-065 的 300 秒调用界；对照实测通过分布（581 秒 < 1800 秒）。该配置在评估器仓库中，是本地工具配置而非交付物。

### D-067：工具批次并发，同文件写互斥，且不向 provider 压制多工具调用

问题：一次模型响应里的多个工具调用被排成一条队，排队成本实测为一轮 123 次调用里 26 次落在同批前一个调用之后（21.1%）。同时请求装配在 `supports_tool_choice` 分支里附带把 `parallel_tool_calls` 置 false，向端点声明"不要一次发多个调用"——对声明支持 tool_choice 的 provider 这是一个会改变模型规划形状的行为性字段，而参考实现的 chat 路径从不发它、Responses 路径显式发 true。
现状：`execute_tool_batch` 按 source order 派发 `Started`，用 `thread::scope` 在至多 8 个 worker 的窗口内并发执行，主线程经 `mpsc` 统一发布 `Update`/`Ended`（`AgentEvents` 携带 `&mut dyn FnMut`，发布权不可跨线程），返回值仍与调用序列同长同序供落盘；`edit`/`write` 的目标路径经批内锁表取词法绝对键（Windows 折叠大小写与分隔符）互斥，同文件串行、不同文件并行。Chat 与 Responses 两条协议都不发 `parallel_tool_calls`，一次响应内的调用数由端点默认决定。
选择：回到参考实现的形状而不是自创契约——`packages/agent/src/agent.ts:237`（`toolExecution` 默认 `"parallel"`）、`agent-loop.ts:487-552`（并发执行 + 按 assistant source order 重组结果）、`packages/coding-agent/src/core/tools/file-mutation-queue.ts:28-61`（同文件写串行、异文件并行）、`packages/ai/src/api/openai-completions.ts`（该文件全程不出现 `parallel_tool_calls`）。批内并发用的锁是本仓库唯一新增机制：锁表活在一个批次内，因为批次之间本就串行，无需跨轮状态。
影响：并发把 21.1% 的排队等待换成实际并行；模型可以像在其他 harness 里一样一次发多个调用。代价是新增两处不变量必须由代码保证——每个 `Started` 恰有一个 `Ended`（worker 未回报时补一条模型可见失败并补发 `Ended`），以及同文件互斥。已知保守缺口：符号链接两侧可能取到不同键因而并行写同一物理文件；`bash` 可改写任意路径但无法从参数推出集合，因此与参考实现一样不加锁。
验证：真实调用把两次 `sleep 5` 放进同一条响应 → 事件顺序为 start、start、end、end，`attemptDurationMs` 求和 5.52 秒而整轮 11.1 秒，即两个调用的工具时间合计 5.58 秒（串行下界约 10 秒）；真实调用在同一条响应内发两个 `edit` 打同一文件 → 两处改动都在位、无丢更新、无 `isError`。以上覆盖 1 条主路径（并发确实发生）与 1 条关键失败路径（同文件并发写不丢更新）；锁的互斥性由构造保证（同键必然竞争同一把 `Mutex`），未做无锁反证——该批次工具时间仅 0.09 秒，竞态在对照下也未必触发。

### D-068：输出预算在装配处按剩余窗口收紧，HTTP 只有一个空闲读界

问题：两处 wire 事实没有归属。(1) 请求恒按模型配置声明的 `max_output_tokens` 出门（本机会话是 131 072），上下文涨起来之后「提示 + 声称的输出上限」可以越过窗口，兼容端点对这种请求直接 400——我们只能等它报错再走强制压缩兜底。(2) HTTP 客户端只有 `read_timeout`，旧值 120 秒；实测最长单次尝试 95.8 秒已用到该界的 80%，端点偶尔更慢一点就会把一次正常长生成判成网络失败。
现状：`Agent::output_budget_tokens`（`agent/src/request.rs`）在装配请求时取「配置声明值」与「`context_window − request_tokens − 4_096`」的较小者，压缩后重建请求时随之重算；压缩判定与它共用 `ContextView::request_tokens` 这一个取数口径，`TurnRequestSpec.max_output_tokens` 因失去读取方而删除（输出预算是派生值，冻结它只会造出第二个事实源）。`PROVIDER_TIMEOUT_SECONDS` 为 300 秒，且明确它作用在每次读操作上、读到即重置，因此不构成对总时长的限制。
选择：与参考实现同型——`packages/ai/src/api/simple-options.ts:12,15-17,34` 用 `contextWindow − 估算 − 4096` 钳 `maxTokens`，并经由所有 provider 共用的 `buildBaseOptions` 生效（chat 路径在 `openai-completions.ts:729` 展开它）；HTTP 空闲界参考 `packages/coding-agent/src/core/http-dispatcher.ts:4` 的 `DEFAULT_HTTP_IDLE_TIMEOUT_MS = 300_000`（同一值同时喂 SDK 的 timeout 与 undici 的 bodyTimeout/headersTimeout）。4_096 安全垫的存在理由写进常量文档：上下文计量只覆盖会话条目，不含装配出来的指令消息，也不含端点分词与我们估算的差值。
影响：长会话不再可能发出窗口放不下的请求，第二道闸门（`context_length_exceeded` → 强制压缩）从"预防性路径"退回成真正的兜底；短会话与配置窗口远大于声明输出的模型完全不受影响。300 秒把"连接死了要等多久"放大 2.5 倍，代价是一次真正卡死的连接要多等 300 秒才失败并被重试。
验证：零模型调用的本地假端点抓取真实出网请求体——窗口 8 192、声明输出 4 096、上下文计量 2 的配置下 `max_tokens` 出门为 4 094（= 8192 − 2 − 4096，钳制咬合且算术吻合）；窗口 256 000、声明输出 131 072 时仍为 131 072（不该咬合时不咬合）；两组都不含 `parallel_tool_calls`，`tool_choice` 只在 `supports_tool_choice: true` 时出现。仓库侧 fmt/clippy(-D warnings)/120 项测试/`build --bins` 全绿，`request_tests` 既有的「输出上限不超过快照声明」断言在收紧后仍成立。`read_timeout` 与 undici `bodyTimeout` 的语义等价性只核对到 reqwest 源码文档（`async_impl/client.rs:1446-1452`：每次读操作、成功读到即重置），未做慢流实测。

### D-069：工具越界读到题目种子，cell 工作区与评估树分家（评估器侧）

问题：`warehouse-audit` 的模型每格有 2–7 次工具调用越出 cell 工作区，四轮里三轮实际读取了 `tasks/warehouse-audit/workspace` 原始种子并与自己的工作副本 `diff`（另有多次 grep 整个 `evaluations/**` 历史目录）。根因不是模型好奇而是布局：cell 工作区原先在 `evaluations/<run>/<model>/<task>/workspace`，其祖先链上直接可达题目种子与历轮尝试产物。
现状：运行期工作区改到 `<LOCALAPPDATA>\singularity-eval\sg-eval-<task>-XXXX\workspace`（非 Windows 为系统临时目录），agent 跑完后才把最终形状复制回 `cell_dir/workspace` 供 checker 与取证；复制失败按 `crashed` 收尾，不允许 checker 对着残缺树给假 FAIL。子进程环境剥掉 cargo 注入的 `CARGO*`（`CARGO_MANIFEST_DIR` 直指仓库根）。
选择：cell 工作区不放 `%TEMP%`——本机 `%TEMP%` = `D:\Temp` 上方有一棵既存 `.git`，`find_workspace_root(cwd)` 会把那棵树认成本题的项目根，评估机上无关的 `AGENTS.md` 因此会被当成本题项目指令读入，每格实际提示词就不再只由题目决定。`<LOCALAPPDATA>` 之上既无仓库标记也无项目指令文件，cell 看到的项目指令面是空的。
影响：判分独立性恢复；`checker.sh` 的 `cd "$(dirname "$0")/workspace"` 约定与既有取证布局不变。代价是每次多一个目录、运行产物在异常中断时可能残留在 LOCALAPPDATA。
验证：真实跑一个探针格（一次调用，2 次工具调用）→ agent 自己 `pwd` 写回的文件为 `C:/Users/Lenovo/AppData/Local/singularity-eval/sg-eval-probe-H6GMjH/workspace`，cell `status=passed`、落回树可读、跑完一次性目录由 guard 清空。实现位于评估器仓库提交 `6700504`，非交付物。



### D-070：判分参考实现必须与题面规格同源（评估语料侧）

问题：`billing-calc` 的 checker 内嵌参考实现用浮点 `round(raw * factor, 2)` 生成期望值，而题面写的是「对单笔总额四舍五入到分」（`tasks/billing-calc/instruction.md:14`）。分钟费率与折扣系数 0.95 的乘积会恰好落在 `.xx5`（例：100.5 × 0.95 = 95.475），二进制浮点把它存成近似值，`round()` 于是给出 95.47，与题面要求的 95.48 相反。随机序列种子固定（`random.seed(20260815)`），20 组里撞平局的组号每轮相同：`random-2/8/11/12/19` 五组、每组三条 mismatch，跨轮字节一致。后果是判分方向反了——按题面写的解法（Decimal + `ROUND_HALF_UP`）被稳定判负，违反题面的浮点解法被稳定判胜。抽查六轮留存代码，样式与判分结果 100% 对应：四轮通过的 `calculator.py` 都是 `return round(raw * factor, 2)`，两轮失败的都用 Decimal 精确累加，中间一轮（`round4-pre`）只撞 3 个平局组因而 9 行 mismatch。这条本身就是一个把风格选择放大成 pass/fail 翻转的方差源。
现状：判分参考金额全程以 `Decimal` 累加、按 `ROUND_HALF_UP` 取到分，与题面同源；比较阈值未动（`abs(a - b) < 0.004`，只容浮点累加误差，差一分钱必须判不一致）。六个 checker 的硬断言与题面文本、种子代码三方核对过：只有这一处判分与题面矛盾。`warehouse-audit` 的起始库存 1000 与日期归一 `YYYY-MM-DD`、`multi-module-audit` 的 `window > len(data)` 行为与 `list[tuple[int, int]]` 返回、`verbose-suite-rootcause` 的负余额返回 `0.0`——这几项题面未写但由种子代码的签名或 docstring 声明，属可从工作区学得的契约；`repo-wide-rename` 的 `apply_volume_discount ≥ 80` 与干扰项阈值只以"达到预期调用频次"含糊声明，语料固定因而不误伤。
选择：修判分的参考实现，不放宽比较、不改题面——题面已经写明规则，需要被对齐的是实现。把 `close()` 放宽到 1 分会同时放过真实的金额错误，属用提高容差制造表面通过。
影响：该格的历史读数作废："模型在 billing-calc 上稳定失败"不成立，它是判分缺陷。后续轮次与历史轮次在 `billing-calc` 上不再可比，其余五格不受影响；同一缺陷也意味着任何用该语料得出的舍入相关结论需重新取证。
验证：同一份 workspace 代码、只换判分一侧的 2×2 对照（Git bash 实跑，零模型调用）：Decimal+四舍五入 ×旧判分 = exit 1（15 行 mismatch）；Decimal+四舍五入 ×新判分 = exit 0；浮点 `round()` ×旧判分 = exit 0；浮点 `round()` ×新判分 = exit 1（15 行 mismatch）。两种实现在两套判分下的自带 31 项测试均全绿，说明区分只来自判分参考实现，新判分保留判别力。实现位于评估器仓库提交 `0f0533d`，非交付物。

### D-071：评估结果自带被调用二进制的内容身份（评估器侧）

问题：`results.json` 与 `cell.json` 都不记录本轮实际执行的是哪个 `sg`，事后只能靠构建时间推断"这一轮对应哪个提交"。这不是理论风险：本机 PATH 上 `sg` 解析到的是 ast-grep 的别名（`where.exe sg` → `C:\Users\Lenovo\.cargo\bin\sg.exe`，其 `--help` 首行为 "Search and Rewrite code at large scale using AST pattern."），与交付二进制同名同字。一次误用裸 `sg` 的评估会静默测到别的程序，而产物里看不出差别。
现状：`run_eval` 在接受 `--sg-path` 并取绝对路径后立即读文件计算 SHA-256，连同路径与字节数以运行级字段 `sg_binary` 写入 `results.json`，并在开跑第一行打印同一身份，使中断的轮次也在日志里留下被测对象。一次运行的所有 cell 共用同一二进制，故只在运行级记录一份。
选择：身份取内容哈希而不是版本号或时间戳——评估器对被评估程序是黑盒，`sg` 当前也没有 `--version` 表面，而哈希对任意给定文件都成立，包括历史构建与别人的构建。为此引入 `sha2`：标准库无内容哈希，该版本（0.10.9）已在本地 registry 缓存且已存在于兄弟工作区的锁文件，离线门禁不受影响。不给 `sg` 加 `--version`：那要求被评估程序配合，对旧构建与外部构建无效，属把取证责任推给被测方；产品 CLI 是否需要版本表面是另一个决定。
影响：每轮评估的取证自洽——"这一轮跑的哪个构建"由产物自己回答，不再依赖构建时间与 Git 时间的相互印证。代价是每次运行多读一遍二进制（约 8 MB）与一个哈希依赖。
验证：扩展现有黑盒测试 `run_eval_black_box_isolates_cells_aggregates_results_and_returns_nonzero`（它已经用 fake sg 走完一整轮并解析 `results.json`），断言 `sg_binary` 的摘要等于对 `--sg-path` 那个文件实算的 SHA-256、字节数等于文件长度、路径指向同一文件名；评估器 12 项测试全绿，仓库门禁 fmt/clippy(`-D warnings`)/121 项测试/`build --bins`/`git diff --check` 全绿。实现位于评估器仓库，非交付物。

### D-072：用户级数据目录与启动目录解绑

问题：从 `$HOME` 启动 `sg` 必然失败，报 `SINGULARITY_HOME must not be inside the current repository`。默认数据目录就是 `$HOME/.singularity`，而边界函数 `find_workspace_root(cwd)` 在向上找不到 `.git` 时**以 cwd 本身为界**（`crates/core/src/project_instructions.rs:322`，该回退对"找项目根读 AGENTS.md"是正确的），于是启动目录是家目录时数据目录必然"在界内"，被 fail-closed 拒绝；错误文案还把它说成"当前仓库"，而此刻并不存在仓库。引入该检查的记录是 M6「`SINGULARITY_HOME` **显式设置时**先于目录创建校验不在当前仓库内」（提交 `7cc59613`），即本意只针对用户自己把数据目录指进仓库这一种情形；删除前模型配置侧正是这个窄语义（只在 `explicit` 分支检查），只有会话准备路径无条件检查，两处语义自此分叉。
现状：删除这条检查。`user_home.rs` 只保留 home 解析（`SINGULARITY_HOME` → `USERPROFILE` → `HOME`，非显式时追加 `.singularity`），不再有任何"数据目录 vs 启动目录"的比较；`find_workspace_root` 回到它唯一的事实职责——项目指令发现，并把失去消费者的 crate 内再导出移除、可见性收归本模块。数据目录与启动目录无关：从任何位置启动都解析到同一份用户级配置与会话。
选择：删而不是收窄，理由是基线对齐与既有事实。参考实现的会话目录固定在用户目录，没有"不许位于工作区内"这一概念；本仓库要防的那一类误用（把会话写进项目树被提交或随手删除）在参考实现里同样不存在，而它带来的代价已经落地为"主要产品入口在最常见的启动目录下不可用"。收窄成 `explicit` 分支虽然也能修好家目录，但会留下两处不同语义的边界检查，而这两处的差别正是本次故障的来源。
影响：`sg` 在任意目录可启动，包括家目录与磁盘根；显式把 `SINGULARITY_HOME` 指进项目仓库不再被程序阻止，只由文档提醒（`docs/INSTALL.md` 已把该变量定位为"测试与自动化隔离用户状态"的手段）。评估器一侧的 cell 隔离不依赖这条检查：cell 用独立 `SINGULARITY_HOME` 且工作区仍放在 `<LOCALAPPDATA>`，那里上方没有仓库，理由改记于 D-069。
验证：改动前后各跑一次同一对照（`sg --print --session <不存在的 id> x`，守卫在会话准备阶段触发、假 id 使其在解析会话处即退出，全程零模型调用）。改动前：家目录 → `SINGULARITY_HOME must not be inside the current repository`；仓库目录与 `D:\Temp` → `thread … was not found`。改动后：三个目录一律 `thread … was not found`，`--help` 首行仍为 `Singularity coding agent`。旧词形归零：`ensure_singularity_home_outside_workspace`、`ensure_home_not_repo_controlled`、`canonicalize_existing_prefix`、`path_starts_with` 与那句错误文案在源码中均 0 命中。仓库门禁 fmt/clippy(`-D warnings`)/119 项测试（随两条守卫测试一并移除）/`build --bins`/`git diff --check` 全绿。

### D-073：命令名收归单一事实源并改名

问题：命令行程序名在代码里没有归属者——除 `[[bin]] name` 之外，clap 的 `#[command(name = …)]`、7 条 CLI 文案里的 9 处名字、库内 2 条诊断前缀各写一遍字面量，改一次名要连注释一起动 16 处。名字本身与本机已装工具撞车：`C:\Users\Lenovo\.cargo\bin\sg.exe` 属于 ast-grep（`cargo install --list` 输出 `ast-grep v0.43.0: ast-grep.exe sg.exe`，其 `--help` 首行 "Search and Rewrite code at large scale using AST pattern."），`D:\python\Scripts` 下另有第三个同名文件，裸敲这个名字会静默跑到别的程序上；本仓库的 target 目录由 `.cargo/config.toml` 重定向，产物不在 PATH 上，按文档装好之后仍然叫不出产品。D-072 之后家目录成为正常启动位置，命令名歧义从不便升级为"人工验证与评估都可能测错对象"。
现状：程序名的唯一事实源是 `crates/cli/Cargo.toml` 的 `[[bin]] name = "singularity"`；`crates/cli/src/main.rs` 的 `pub(crate) const PROGRAM_NAME: &str = env!("CARGO_BIN_NAME")` 是唯一读取点，clap 属性、`Usage:` 行与全部 `PROGRAM_NAME: …` 形态的文案由它插值，改名只动 Cargo.toml 一处。`crates/agent/src/session/writer_lock.rs` 两条不阻断诊断去掉程序名前缀：库不拥有命令行名字，加前缀属于 CLI 输出层的职责。
选择：改名，而不是靠 PATH 顺序或 shell alias 绕过——alias 只修一个人的一次会话，修不了文档、发布产物与评估调用，而同名会让"刚才跑的是谁"不可判定。保留消息前缀而不删：它已在错误输出与评估日志中承担区分来源的作用，需要修的是名字没有 owner，不是名字出现在消息里；主流 harness 不加这种前缀（`D:\refs\codex\codex-rs\cli\src\state_db_recovery.rs:37` 写完整句子，codex-rs 全部 `.rs` 中 `"codex: ` 前缀 0 命中；Pi 的 206 个 `.ts` 中 `"pi: ` 前缀 0 命中，命令名由 commander 一次性设定），故本仓库的做法是收敛到常量而非跟平。新名取产品名，与 codex `[[bin]] name = "codex"`、Pi 的 `bin.pi` 同风格，`where.exe singularity` 本机无命中。
影响：产物文件名成为 `singularity.exe`；`README.md`、`AGENTS.md`、`docs/INSTALL.md`、`docs/singularity.md`、`docs/tui-manual-verification.md` 与发布链（`package-release.v1.ps1` 的 `ExpectedNames`、SBOM 组件键与 `sbom-singularity.cdx.json`、`sign-release-binaries.v1.ps1`、`release.yml` 的资产名与输出 `sbom_singularity`、Issue 模板示例）同批改。仓库 0 个版本 tag、远端 0 个 tag，无已发布产物因此失效。评估器的 `--sg-path`、`run_sg`、`sg_stdout.log` 与 `results.json` 的 `sg_binary` 键保持原名——它们指"被评估的那个二进制"，属该工具自身词表且已持久化在历史轮次产物中，本次只改其中断言被调用命令行的注释。共享 target 目录里改名前构建的 `sg.exe` 是失效产物，由 `cargo build` 重建，不作为入口。
验证：`cargo clippy --offline --locked -p singularity_cli --all-targets -- -D warnings` 干净，证明 `#[command(name = PROGRAM_NAME)]` 被 clap 派生接受。实跑新构建核对四条用户可见文案：`Usage: singularity.exe [OPTIONS] [GOAL]`；无 TTY 时 `singularity: interactive mode requires a terminal; use \`singularity --print <goal>\` or \`singularity --json <goal>\` for non-interactive execution`；缺 goal 时 `singularity: a goal is required: singularity --print <goal> | singularity --json <goal>`；两模式冲突时 `singularity: --print and --json are mutually exclusive`。旧词形归零：`crates`、活动文档与 `.github` 中 `sg`、`sg.exe`、`"sg"`、`sbom_sg` 命中 0；`docs/decisions`、`plan`、`specs`、`outputs` 的历史条目按当时实际命令名保留，对应关系记于本文件顶部。仓库确定性门禁六项（fmt、check、clippy `-D warnings`、test、build `--bins`、`git diff --check`）全部 exit 0，119 项测试通过、0 失败。

### D-074：判分脚本位置泄漏与越界判定内置（评估器侧）

问题：`ponytail-rename-singularity-20260902` 轮表面 6/6 通过，其中 `warehouse-audit` 是脏读数——92 次工具调用里有 7 次越出 cell：`cat tasks/warehouse-audit/checker.sh` 读走判分脚本、`diff -r` 比对种子树、把官方 checker 复制到 `/tmp` 自测，首次越界在第 21 次调用（全程 23% 处），之后还有 6 次 edit/write。泄漏源是题面语料里被 git 跟踪的 51 个 `.pyc`：Python 编译工件内嵌生成它的绝对源路径，`copy_dir_all` 原样复制进 cell，等于把题面目录位置交给被测 agent，拿到该路径后往上 `ls` 一层就是题面原文与判分脚本。上一轮同题 `escapes=0` 只是模型没去翻，环境一直可翻。
现状：题面语料不含 Python 编译工件（51 个 `.pyc` 出库，`.gitignore` 增补 `__pycache__/`、`*.pyc`），`copy_dir_all` 跳过 `__pycache__` 与 `.pyc`/`.pyo`，种子落位与判分取证两侧同时生效。每格在 agent 跑完、checker 启动之前扫描自身 rollout 的工具调用参数：命中题面源树、评估产物树或别的 cell 的临时工作区，即 status=`contaminated`，摘要写进 `cell.json.contamination`、计数进 `by_model.contaminated`，运行以非零退出；`timed_out`/`interrupted`/`failed`/`crashed` 这类结构性分类优先于该判定。
选择：把判定内置，而不是做 OS 级隔离。硬隔离（对 `tasks\`、`evaluations\` 加 `icacls` 拒绝 ACE，或容器）能防住主动搜索，但拒绝 ACE 有生命周期风险——评估进程被杀会残留，把用户和取证工具一起挡在题面目录外；容器则超出本机现有条件。agent 的 shell 能遍历磁盘是产品能力而非缺陷，为此在 cell 上收权会测到另一个产品。所以取"允许搜索、但越界必被看见"：判分前扫描是必需的，checker 运行会把 `checker.sh` 与解答树落进 cell 目录，事后无从区分运行期读到与判分后落下。
影响：FR-022 的第二道门从此要求"6/6 且无 contaminated"，一次无人发现的通过不再可能被记成证据。历史轮次的 cells 没有 `contamination` 字段，读数按原样保留、不回补。评估器新增一个 status 词形。
验证：判分中性——用本轮 6 格已通过的解答树做 A/B（Git bash 实跑，零模型调用），每题 A 组（含 5–16 个 `.pyc`）与 B 组（0 个）退出码全部一致为 0，删工件不改变任何一题判分。不误伤——对已录的 6 份 rollout 镜像同一套规则：5 个干净格命中 0（每格真读到 15–44 个参数叶子，非空转），越界格命中 7，37 次"提及自身临时目录"均未判越界。会咬——检测器经两次故障注入验证：归一化不再转换路径分隔符 → 命中数 1≠2 FAILED（Windows 反斜杠越界漏判）；把自身临时目录前缀认错 → 命中数 3≠2 FAILED（误判本 cell）。第一次注入不咬，追下去是测试夹具自己写成了非法 JSON、事件被解析器静默跳过，故补"夹具必须能解析"的自检。现场验证——复测 cell 树内含 `Singularity-Evaluator` 字样的文件 0 个，正对照为上一轮留存树里 8 个 `.pyc` 嵌有该字样。评估器 fmt / clippy `-D warnings` / 13 项测试 / `git diff --check` 全绿。实现位于评估器仓库提交 `8261c93`，非交付物。附带读数：泄漏面关闭后，`opencode-go/qwen3.8-flash#high` 上那次复测 `contamination=0` 但 1800 秒预算用尽判 `timed_out`（该模型同题前两格分别用掉 1309s 与 1481s）；换回 `opencode-go/deepseek-v4-flash#max` 后，同一二进制同一题 `passed`、118.3s、23 次工具调用、`contamination=0`（内置判定与独立镜像扫描各算一遍均为 0）。该题在泄漏面关闭后确认可干净通过。

## 记录规则

后续每个决策追加新的 `D-xxx` 条目，并注明：问题、现状、选择、影响和验证。新证据推翻旧决策时，直接改写或移除失效条目，演进过程由 Git 历史保存。
