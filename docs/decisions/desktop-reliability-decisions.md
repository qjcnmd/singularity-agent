# Desktop 近期接入可靠性裁决记录

> 本文件是本轮架构讨论的单一裁决记录。它记录用户已确认的产品边界和取舍，不代表这些决策已经实施。实施计划、代码提交和验证结果必须另行记录，并引用本文件中的决策编号。

## 目标

Singularity 最终面向类似 Codex CLI/Desktop 的交互式 coding-agent 使用方式。CLI 和 Desktop 是两种客户端形态，通常由用户选择其中一种；两者复用同一 headless Agent core、Session 和协议语义。Desktop 是近期接入目标，不再按遥远未来消费者处理。

## 已确认裁决

### D-001：Session durability

采用 Pi 式轻量持久化边界：普通追加保证完整写入、进程崩溃后可恢复以及内存状态不领先文件；不把每条追加都升级为断电级 fsync。已有 rewrite 的更强同步保留。

### D-002：Evaluation 分层

保留默认 3 tasks × 2 models 的快速 Evaluation；增加独立核心链路 smoke 集，覆盖 Responses restart、compaction、steer、会话恢复、并行工具等，不把所有链路塞入默认任务评估。

### D-003：长驻 app-server 状态

Desktop 接入前增加最小运行时清理和长驻测试：结束 turn 后释放不再需要的临时索引；不提前构造完整生命周期框架。

### D-004：交互式取消

Desktop 初版必须支持 Bash 按进程组/作业整体取消，并为 Windows/Unix 提供回归测试；取消不能只停止 Agent loop 而留下后台子进程。

### D-005：客户端进程形态

CLI 保持一次性 app-server 使用方式；Desktop 在应用存活期间使用长驻连接。两者不是要求同时使用，也不为同时写同一 Session 的边缘场景增加协调系统。

### D-006：崩溃中的 turn

app-server/Desktop 重启时，活动 turn 标记为 interrupted，恢复已持久化历史并允许用户继续；不自动重放未完成 turn，避免重复工具副作用。

### D-007：继续语义

Desktop 的“继续”追加新 turn，不复用旧 turn、不新建 Session。中断历史保持不可变。

### D-008：孤立工具调用

工具调用已落盘但结果缺失时显示为“中断/状态未知/不可自动重试”，不伪装成普通失败，不自动重复执行；用户可通过新的明确请求决定是否重试。

### D-009：显式 turn 生命周期

采用 Codex 式最小显式 turn 生命周期记录，同时保留 Pi 式消息和工具 JSONL。记录至少覆盖 \`turn_started\`、\`turn_completed\`、\`turn_failed\`、\`turn_interrupted\`。不构造完整 Event Sourcing。

### D-010：恢复修复

重开 Session 发现没有终态的 \`turn_started\` 时，追加一次幂等的 synthetic \`turn_interrupted\`，而不是每次读取都重新推断。

### D-011：持久事实与发布顺序（已被 D-046 取代）

终态必须先写入 JSONL，再更新 SQLite 索引并发布 `turn/completed`、`turn/failed` 或 `turn/interrupted` 事件。客户端看到终态时，持久事实必须已经存在。

### D-012：重连

Desktop 重连采用 `thread/resume` 和 Session 状态重建，不实现事件 cursor/gap 重放。实时 delta 只保证在线连接期间传输。

### D-013：事件背压

保留有界队列、满时阻塞、不主动丢事件的策略。未来只有在真实性能证据出现时才合并 delta，不提前引入 Codex 式复杂事件分类。

### D-014：模型切换

Desktop 运行期间切换模型/Provider 不重启 app-server；当前 turn 保持原配置，切换从下一 turn 生效。thread 设置持久化，下一轮按新模型重新预算并必要时 compaction。

### D-015：Provider 私有续接

切换模型/Provider 后，不兼容的 provider-private reasoning replay 丢弃；可见消息、tool call、tool result 保留，从新 Provider 的公开历史重新开始。不得尝试跨协议转换 opaque replay。

### D-016：thread 设置存储（已被 D-046 取代）

模型、Provider、reasoning effort 等非敏感选择追加为 JSONL `thread_settings` 记录，SQLite 只做索引；不重写 Session header，不只存内存。

### D-017：凭据边界

`thread_settings` 不保存 API key、Authorization header 或其他认证材料；凭据继续由全局配置/环境管理。

### D-018：配置不可用

恢复 thread 时原 Provider/模型不可用，历史仍可读，但继续执行前必须由用户选择可用配置；禁止静默 fallback 或偷偷改写 thread 设置。

### D-019：权限阶段

Desktop 初版采用 Pi 路线：明确显示本机完整权限，核心不内置 Approval/Sandbox；保留独立扩展接缝，正式普及前再实现可选隔离。

### D-020：工具输出

保留 Pi 式 2000 行/50KB 有界工具输出；超限时采用 Codex 式 spill：显示有界预览、明确截断、提供完整输出文件引用。

### D-021：完整输出生命周期

完整 Bash 输出初版作为临时 artifact，UI 明确提示可能过期；Session 永久保存截断预览。只有明确要求跨天永久查看时，才新增 Session 专属 artifact store。

### D-022：Desktop 展示边界

Desktop 是独立客户端，本仓库只提供有界工具预览、工具状态、截断标记和可选 artifact 引用；客户端采用 Codex 式摘要优先、工具块折叠、按需展开，不把完整命令输出默认铺满界面。UI 展示阈值不硬编码进 Agent core。

### D-023：Item 生命周期

Desktop 接入前补齐 Codex 式 `item/completed` 生命周期：每个 `item/started` 必须对应 terminal 的 `item/completed` 或 `item/failed`，再发布 turn 终态。工具详细参数和结果继续通过已有 `tool/execution/*` 事件传递。

### D-024：Item 持久化

按 Codex 方式持久化最终 item、稳定消息、工具结果和 turn 状态；在线 delta 只用于实时显示，不逐字/逐 chunk 写入 JSONL。重连从最终 item 和 Session 状态重建，不实现 cursor/gap 事件重放。

### D-025：工具 Item

工具调用纳入统一 item 生命周期。每个 \`toolCallId\` 对应一个稳定 item，事件顺序为 \`item/started\` → \`tool/execution/start\` → \`tool/execution/update\`* → \`tool/execution/end\` → \`item/completed\` 或 \`item/failed\`。两类事件必须共享同一身份和终态，避免客户端维护矛盾状态。

### D-026：并行 Item 顺序

并行工具的实时事件按实际完成顺序到达；Session 中的最终 tool result 仍按模型 source order 持久化。UI 获得低延迟进度，模型上下文保持稳定顺序。

### D-027：能力声明（已被当前协议取代）

历史提案：曾建议在 \`server/capabilities\` 中增加协议版本和固定 feature 列表。该表面未进入当前协议，当前客户端直接遵循稳定的请求/事件合同；本节仅保留决策历史，不是当前事实。

### D-028：协议兼容（已被当前协议取代）

历史提案：曾采用能力先协商、协议只增不改的兼容合同。当前协议不再提供能力协商表面；新增请求按稳定 JSON-RPC/typed error 合同处理。本节仅保留决策历史，不是当前事实。

### D-029：Thread 设置接口

增加独立 \`thread/settings\` 请求。设置更新与 thread 恢复分离；设置成功后从下一 turn 生效，并追加 JSONL \`thread_settings\` 记录。

### D-030：活动 Turn 中的设置变更

活动 turn 期间允许更新 thread 设置并立即持久化；当前 turn 及其 steer 继续使用启动时的旧 Provider/模型，设置只对下一 turn 生效。

### D-031：Usage 持久化（已被 D-046 取代）

标准化 turn usage 持久化到 JSONL，SQLite 继续做聚合索引；\`thread/resume\` 恢复 usage 给 Desktop。Provider 未提供 usage 时显示 unknown，不伪造为 0。

### D-032：原始 SessionEntry 公共化（已撤回）

曾讨论让 Desktop/app-server 的历史读取返回原始 SessionEntry，包括 \`providerReasoningReplay\` 等 provider-private reasoning state；用户已撤回该选择，不得实施。最终边界由 D-034 确定：公开结构化 history projection，不返回原始 SessionEntry。

### D-033：session/read entry_types（已撤回）

曾讨论是否扩展 \`session/read\` 的 entry_types；用户已撤回该选择，不得实施。当前 \`session/read\` 的公开历史种类和过滤范围以 D-034 及当前 protocol 类型为准。

### D-034：Thinking 公开投影

Desktop 公开显示文本 thinking block。DeepSeek 等 Provider 明确返回的完整 \`reasoning_content\` 可以完整保留并折叠展示；过滤 replay 元数据、Responses opaque output items 和 \`encrypted_content\`。最终历史读取采用公开结构化投影，不返回原始 SessionEntry。

### D-035：Thread 模型投影（已被 D-046 取代）

thread_settings 追加后同步更新 SQLite 当前模型索引；JSONL 是历史事实，SQLite 是最新值投影，\`thread/list\`、\`thread/resume\` 和 \`session/read\` 保持一致。

### D-036：Metadata 上下文隔离

turn 生命周期、thread_settings、usage、item 状态等 metadata 只用于恢复和客户端展示，不进入模型请求上下文；模型仍只接收 user/assistant/toolResult 及必要 compaction 投影。

### D-037：SQLite 索引修复（已被 D-046 取代）

JSONL 追加成功而 SQLite 更新失败时保留 JSONL；下次打开从 JSONL 修复 SQLite 索引，不回滚历史、不允许永久不一致。

### D-038：计划文件交付

实施计划在执行期间作为被 Git 忽略的本机交接文件使用，不修改 `.gitignore`；计划文件本身不代表代码已实施或已验证。执行完成后该临时计划可以删除，当前有效架构和裁决以本文件、`docs/singularity.md`、源码和 Git 历史为准。

### D-039：开发阶段无兼容义务（硬切原则）

问题：项目未发布、无外部用户，重构中为旧字段、旧格式保留的读取兼容层只有维护成本，没有真实消费者。参考实现：不适用（项目内部原则）。当前代码事实：wire 协议、会话 JSONL 与本地 config.json 在重构期间形状会多次变化。选择：开发阶段旧格式与旧字段一律硬切删除，不做读取兼容层；凡确需兼容必须写明理由与移除条件。本机 config.json 或旧会话文件残留旧结构时直接手动删除。影响：协议、会话格式与配置可以一次到位；本地已落地的旧结构文件需手动清理。验收方式：每个 Phase 门禁（fmt / clippy -D warnings / 全量测试）全绿，删除的旧形状无残留引用；确需兼容处有成文理由与移除条件。

### D-040：双 wire 协议骨架去重（P5-1）

问题：chat completions 与 responses 两套 wire 编解码各复制一套重试循环、流式解码与 attempt 遥测骨架，双协议维护成本翻倍且行为有漂移风险。参考实现：Codex 单协议；本决策保留双协议（DeepSeek 等走 chat，OpenAI 原生走 responses）。当前代码事实：`model/transport` 中两类协议路径的重试/流式/遥测骨架近似复制。选择：编解码层各留一份（协议差异必要）；重试、流式与遥测骨架合并为一套共享路径。影响：协议新增与重试/遥测行为修改只在单处落地。验收方式：双协议各自的 provider 测试全绿 + 至少一次真实 DeepSeek chat 冒烟（输出证据留存 outputs/）。

### D-041：模型发现升级为运行时组件（P5-2）

问题：模型目录发现现为 fail-closed——任一坏条目拖垮整个目录；且用户配置缺 `context_window`/`max_output_tokens` 时无法自动补齐。参考实现：Codex models-manager（models_cache.json + TTL + RefreshStrategy）。当前代码事实：discovery 对坏条目 fail-closed；内置表作为兜底查询层；无 TTL、无缓存刷新。选择：discovery 子系统保留并升级为运行时组件：发现结果喂给运行时元数据（用户配置缺字段时自动填充）；坏条目 fail-soft（单条剔除，不拖垮目录）；内置表转为兜底合并层；引入 TTL 刷新策略。这是全场唯一"加能力"项，其余条目均为删除或收敛。影响：目录发现从构建期查询升级为运行时元数据源，引入缓存与刷新生命周期。验收方式：发现缓存单测 + 元数据填充集成测试 + 坏条目容错用例。

### D-042：凭据单文件原子替换（P5-3）

问题：`auth.v1-<uuid>.json` 世代机制使凭据目录出现多文件，导入也不清理旧世代。参考实现：Codex 单一 `$CODEX_HOME/auth.json`。当前代码事实：auth 写入生成 `auth.v1-<uuid>.json` 世代文件。选择：只保留一个 `auth.v1.json`；导入 = 写临时文件 + 同卷原子 rename；删除世代机制；现存历史世代文件提供一次性清理命令或在文档中指引用户手删。影响：凭据目录恒为单文件；导入失败不留下半写文件。验收方式：auth 读写测试 + 目录单文件断言。

### D-043：AGENTS.md 预算截断（P5-4）

问题：项目指令超限（FileTooLarge/TotalTooLarge）现直接报错，使整个 turn 无法开始；超长 AGENTS.md 是常见现实。参考实现：Codex 同 32KB 默认文件预算，剩余预算用尽即停止纳入。当前代码事实：`project_instructions.rs` 超限返回错误 → runner 使 turn/start 直接失败。选择：超限不再报错：按预算截断并纳入前缀 + 发诊断告警"项目指令被截断"；真正 I/O 错误仍报错 fail closed。影响：超大 AGENTS.md 不再阻断任务，截断对模型可见且客户端收到告警。验收方式：project_instructions 测试从断言报错改为断言截断 + 告警；I/O 错误路径仍断言报错。

### D-044：心跳保活不加（P5-5）

问题：是否增加协议级 heartbeat 保活。参考实现：Codex 无自定义心跳层，靠进程级存活检测与重连。当前代码事实：协议固定 14 个方法，无心跳；app-server 为 stdio 子进程；D-012 已确立 thread/resume 重连。选择：不加心跳，14 个方法保持不变。Desktop 端僵死检测用"请求超时 → 强杀 app-server 子进程 → D-012 thread/resume 重连"闭环。影响：协议表面保持最小；僵死恢复靠超时 + 重连，不留半开放协议面。验收方式：本决策记录本身；如 Desktop 相关代码已有超时兜底则确认即可，无代码改动。

### D-045：模型限额的目录来源与 fail-closed 边界（演进 D-041）

问题：D-041 裁决「发现结果喂给运行时元数据自动填充 context_window/max_output_tokens」，但发现缓存 schema 只含模型 id 清单，无数值限额来源；以编译期常量冒充填充会静默放宽校验合同。参考实现：Codex models-manager（策展目录 + 本地缓存 + TTL）；models.dev 公开模型目录 api.json。当前代码事实：未知模型缺限额一度收束为 fail closed；随后接入 models.dev 投影缓存作为限额第三级来源——捕获读路径只读 `metadata-cache.json`（TTL 24 小时）永不联网，刷新仅在模型目录发现成功后顺带拉取且网络失败 fail-soft；provider key 精确匹配 + base_url host 唯一归属回退，model id 精确 + 大小写回退。选择：限额解析优先级为用户顶层声明 > 内置表 > 目录投影缓存 > fail closed；不在目录覆盖内的 provider 保持显式声明。影响：公开目录覆盖内的未知模型可自动补齐限额；目录未覆盖或离线时维持拒绝而不是猜测。本条修订 D-041 中「发现结果自动填充数值限额」的实现路径，fail-soft、内置表兜底合并与 TTL 刷新语义不变。验收方式：E2E 断言无缓存 fail-closed、有缓存填充生效；真实拉取为 ignored 手动测试并留档 outputs/modelsdev-fill-check.log。

### D-046：会话索引进程内化（取代 D-011/D-016/D-031/D-035/D-037 的 SQLite 面）

问题：既有裁决（D-011 持久事实与发布顺序、D-016 thread 设置存储、D-031 usage 持久化、D-035 thread 模型投影、D-037 索引修复）以 SQLite 会话索引为前提；该 store 层独立于 JSONL 唯一权威之外形成第二落盘事实，增加崩溃恢复与修复路径。参考实现：无（项目内部收敛）。当前代码事实：会话正文 JSONL 是唯一权威，进程内 `SessionIndex` 启动时从 JSONL 重建；SQLite store 层已删除。选择：会话索引只存在于进程内（定位与展示元数据缓存，启动重建、退出不落盘），不再有第二持久化索引；D-011/D-016/D-031/D-035/D-037 中「先写 JSONL 再更新 SQLite 索引」的时序语义由「先写 JSONL 再更新进程内索引」承接，其余不变。影响：无索引修复路径、无 SQLite 依赖；`session/delete` 只删 JSONL 与备份。验收方式：删除 SQLite store 层与依赖；JSONL 唯一权威链路（终态事件通知前完成写盘 + 索引更新）测试全绿。

## 参考实现

- Pi：小核心、Agent Loop、Session JSONL、compaction、工具输出截断与临时 spill。
- Codex：app-server 的 thread/turn/item 分层、显式 \`TurnAborted\`/rollout 生命周期、模型切换按下一轮生效、Sandbox/Approval 与事件交付经验。

本文件中的外部参考只用于说明取舍；具体实施必须以 Singularity 当前源码、协议和验证结果为准。

## 记录规则

后续每次用户裁决追加新的 \`D-xxx\` 条目，并注明：问题、参考实现、当前代码事实、选择、影响和验收方式。若新证据推翻旧裁决，不删除旧条目，而是追加修正条目并标注取代关系。

### D-047：共享运行时硬切 + app-server 委托（演进 D-047：双入口硬切与进程内共享运行时）

问题：产品入口由 CLI、TUI 与后续 GUI 三种消费形态组成，若各自实现 turn 执行会造成多套状态机与事件投影漂移；此前 app-server 内维护并行 turn 管线，与 runtime 的 Conversation/TurnRunner 形成第二执行体。参考实现：Pi 的单次执行入口与 AgentSession 复用、Codex 的 thread/turn/item 分层与「设置下一轮生效」。当前代码事实：crates/runtime 是 Turn 执行的唯一所有者——TurnRunner 单轮管线（会话单写者贯穿、typed TurnEvent 事件源、fail-stop 终态化、明细终态原子收敛）与 Conversation 长驻协调器（`reserve_start` 原子预订链窗口、steer 注入当前轮、followUp FIFO 逐条自执行为独立新 turn、取消按轮独立、设置终态后自动应用）；CLI 无参数进入 TUI，--print/--json 单次执行；app-server 是 stdio JSON-RPC 适配器，turn/start 经 Conversation 预订同步裁定并发（先到先得、后到立即 invalid-state），worker 线程以预订执行整条链，事件由适配器 1:1 映射 typed TurnEvent 为协议通知，控制 lane（steer/followUp/interrupt）、设置与删除全部路由到共享 Conversation。选择：客户端形态（TUI / headless / app-server）一律委托 runtime，客户端不复制执行状态；协议类型只存在于 crates/protocol 与适配器，runtime 不依赖 protocol/UI；app-server 的并行管线与重复 helper 全部删除，以适配器测试证明协议事件名/字段/终态/取消/会话恢复无漂移。影响：执行、并发裁定与恢复语义只有 runtime 一个事实源；TUI/headless/app-server 三者共享同一 Conversation 链（含 followUp 队列与设置时序）；协议契约由 app-server 适配器测试维护。验收方式：runtime 行为测试（预订互斥、单活动 turn、失败收敛、中断、设置时序、followUp 恰一次）、app-server 适配器+协议合同测试（事件名/字段/终态/取消/恢复不漂移）、CLI 双入口合同、TUI 状态/布局/输入测试、进程级 stdio 并发 turn 冲突测试与真实 TTY e2e。
