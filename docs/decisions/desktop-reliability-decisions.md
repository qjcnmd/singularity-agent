# Singularity 产品形态与可靠性决策记录

> 本文件保存当前仍然有效的决策依据与取舍。已实施决策的当前事实以 docs/singularity.md 与源码为准；实施记录另行保存并引用本文件中的决策编号。已被取代或机制已不存在的历史条目不保留于本文件，其演进过程由 Git 历史保存。

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

## 记录规则

后续每个决策追加新的 `D-xxx` 条目，并注明：问题、现状、选择、影响和验证。新证据推翻旧决策时，直接改写或移除失效条目，演进过程由 Git 历史保存。
