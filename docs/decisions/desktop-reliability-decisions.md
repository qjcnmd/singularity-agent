# Singularity 三种产品形态与可靠性裁决记录

> 本文件是产品形态与可靠性裁决的单一记录，保存用户已确认的决策依据、取舍与取代关系。已实施裁决的当前事实以 docs/singularity.md 与源码为准；实施记录另行保存并引用本文件中的决策编号。

## 目标

Singularity 提供三种产品形态：参照 pi 的无交互单次入口、界面交互以 Grok Build 为主且功能参照 pi/Codex CLI/Grok Build 的交互式 TUI，以及参照 Codex Desktop 的桌面端。三者复用同一 headless Agent core、Session 和执行语义；app-server 是桌面端的后端接线口。

## 已确认裁决

### D-001：Session durability

采用 Pi 式轻量持久化边界：普通追加保证完整写入、进程崩溃后可恢复以及内存状态不领先文件；不把每条追加都升级为断电级 fsync。已有 rewrite 的更强同步保留。

### D-002：Evaluation 分层

保留默认 3 tasks × 2 models 的快速 Evaluation；增加独立核心链路 smoke 集，覆盖 Responses restart、compaction、steer、会话恢复、并行工具等，不把所有链路塞入默认任务评估。

### D-003：长驻 app-server 状态

Desktop 接入前增加最小运行时清理和长驻测试：结束 turn 后释放不再需要的临时索引；不提前构造完整生命周期框架。

### D-004：交互式取消

Desktop 初版必须支持 Bash 按进程组/作业整体取消，并为 Windows/Unix 提供回归测试；取消不能只停止 Agent loop 而留下后台子进程。

### D-005：客户端进程形态（已被 D-049 取代）

历史方案曾让 CLI 使用一次性 app-server、桌面端使用长驻连接。现行进程形态由 D-049 统一规定。

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

### D-012：重连（已被 D-049 取代）

历史方案指定桌面端通过尚未实现的 `thread/resume` 重连。桌面端协议属于形态③，具体恢复合同在出现桌面端消费者时确定。

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

### D-030：活动 Turn 中的设置变更（已被 D-048 取代）

活动 turn 的设置时序以 D-048 为准。

### D-031：Usage 持久化（已被 D-046 取代）

标准化 turn usage 持久化到 JSONL，SQLite 继续做聚合索引；\`thread/resume\` 恢复 usage 给 Desktop。Provider 未提供 usage 时显示 unknown，不伪造为 0。

### D-032：原始 SessionEntry 公共化（已撤回）

曾讨论让 Desktop/app-server 的历史读取返回原始 SessionEntry，包括 \`providerReasoningReplay\` 等 provider-private reasoning state；用户已撤回该选择，不得实施。最终边界由 D-034 确定：公开结构化 history projection，不返回原始 SessionEntry。

### D-033：thread/read entry_types（已撤回）

曾讨论是否扩展 \`thread/read\` 的 entry_types；用户已撤回该选择，不得实施。当前 \`thread/read\` 的公开历史种类和过滤范围以 D-034 及当前 protocol 类型为准。

### D-034：Thinking 公开投影

Desktop 公开显示文本 thinking block。DeepSeek 等 Provider 明确返回的完整 \`reasoning_content\` 可以完整保留并折叠展示；过滤 replay 元数据、Responses opaque output items 和 \`encrypted_content\`。最终历史读取采用公开结构化投影，不返回原始 SessionEntry。

### D-035：Thread 模型投影（已被 D-046 取代）

thread_settings 追加后同步更新 SQLite 当前模型索引；JSONL 是历史事实，SQLite 是最新值投影，\`thread/list\`、\`thread/resume\` 和 \`thread/read\` 保持一致。

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

问题：早期世代机制使凭据目录出现多文件，导入也不清理旧世代。参考实现：Codex 单一 `$CODEX_HOME/auth.json`。选择：只保留一个 `auth.json`；导入 = 写临时文件 + 同卷原子 rename；删除世代机制。影响：凭据目录恒为单文件；导入失败不留下半写文件。验收方式：auth 读写测试 + 目录单文件断言。

### D-043：AGENTS.md 预算截断（P5-4）

问题：项目指令超限（FileTooLarge/TotalTooLarge）现直接报错，使整个 turn 无法开始；超长 AGENTS.md 是常见现实。参考实现：Codex 同 32KB 默认文件预算，剩余预算用尽即停止纳入。当前代码事实：`project_instructions.rs` 超限返回错误 → runner 使 turn/start 直接失败。选择：超限不再报错：按预算截断并纳入前缀 + 发诊断告警"项目指令被截断"；真正 I/O 错误仍报错 fail closed。影响：超大 AGENTS.md 不再阻断任务，截断对模型可见且客户端收到告警。验收方式：project_instructions 测试从断言报错改为断言截断 + 告警；I/O 错误路径仍断言报错。

### D-044：心跳保活不加（P5-5，连接恢复部分已被 D-049 取代；"9 个业务方法"计数已失效，注册表现为 13 方法）

问题：是否增加协议级 heartbeat 保活。参考实现：Codex 无自定义心跳层，靠进程级存活检测与重连。当前代码事实：协议注册表有 9 个业务方法，无心跳；app-server 为 stdio 子进程；`thread/resume` 从未实现。选择：不增加心跳，保持协议表面最小。桌面端连接恢复合同在出现形态③消费者时确定。验收方式：协议注册表测试与本记录一致。

### D-045：模型限额的目录来源与 fail-closed 边界（演进 D-041；models.dev 投影缓存第三级已移除，现行链路为用户声明 > 内置表 > 默认常量）

问题：D-041 裁决「发现结果喂给运行时元数据自动填充 context_window/max_output_tokens」，但发现缓存 schema 只含模型 id 清单，无数值限额来源；以编译期常量冒充填充会静默放宽校验合同。参考实现：Codex models-manager（策展目录 + 本地缓存 + TTL）；models.dev 公开模型目录 api.json。当前代码事实：未知模型缺限额一度收束为 fail closed；随后接入 models.dev 投影缓存作为限额第三级来源——捕获读路径只读 `metadata-cache.json`（TTL 24 小时）永不联网，刷新仅在模型目录发现成功后顺带拉取且网络失败 fail-soft；provider key 精确匹配 + base_url host 唯一归属回退，model id 精确 + 大小写回退。选择：限额解析优先级为用户顶层声明 > 内置表 > 目录投影缓存 > fail closed；不在目录覆盖内的 provider 保持显式声明。影响：公开目录覆盖内的未知模型可自动补齐限额；目录未覆盖或离线时维持拒绝而不是猜测。本条修订 D-041 中「发现结果自动填充数值限额」的实现路径，fail-soft、内置表兜底合并与 TTL 刷新语义不变。验收方式：E2E 断言无缓存 fail-closed、有缓存填充生效；真实拉取为 ignored 手动测试并留档 outputs/modelsdev-fill-check.log。

### D-046：会话索引进程内化（取代 D-011/D-016/D-031/D-035/D-037 的 SQLite 面）

问题：既有裁决（D-011 持久事实与发布顺序、D-016 thread 设置存储、D-031 usage 持久化、D-035 thread 模型投影、D-037 索引修复）以 SQLite 会话索引为前提；该 store 层独立于 JSONL 唯一权威之外形成第二落盘事实，增加崩溃恢复与修复路径。参考实现：无（项目内部收敛）。当前代码事实：会话正文 JSONL 是唯一权威；进程内无常驻索引对象，列表、摘要与分页按需扫描顶层 JSONL 产生定位与展示元数据，退出不落盘。选择：不再有第二持久化索引；D-011/D-016/D-031/D-035/D-037 中「先写 JSONL 再更新 SQLite 索引」的时序语义由「先写 JSONL 再发布事件」（durable 先于发布）承接，其余不变。影响：无索引修复路径、无 SQLite 依赖；`session/delete` 把会话文件 rename 进 `archived/` 子目录归档保留（见架构文档归档条目）。验收方式：JSONL 唯一权威链路（终态事件发布前完成写盘）测试全绿。

### D-048：活动轮设置时序与索引一致性（D-030/D-035 的落地形态，已被 D-055/D-056 取代）

问题：活动轮期间 `thread/settings` 若立即改写索引，线程列表会读到尚未落盘的投影；若只排队，终态后索引无人同步，模型展示永远滞后。参考实现：无（项目内部收敛）。当前代码事实：`queue_settings` 返回 `SettingsApplyTiming`（`AppliedNow`/`QueuedForNextTurn`/`NothingToApply`）；活动轮只合并单份意图，可信终态后自动持久化并以 `thread/settingsApplied` 事件发布更新投影；持久化失败保留意图并返回 Settings 错误。选择：生效时点由协调器唯一裁定（客户端不再用自身相位猜测），索引任何时刻只读已落盘值——排队时 `thread/settings` 返回 `queued=true` 且索引保持旧值，终态后随事件同步。影响：`thread/list` 与 `thread/read` 的模型字段永不领先 JSONL；协议结果新增 `queued` 字段（additive）。验收方式：运行时时序与持久化失败契约测试、app-server 进程级集成测试（活动轮排队→索引不变→终态后收敛）。

## 参考实现

- Pi：小核心、Agent Loop、Session JSONL、compaction、工具输出截断与临时 spill。
- Codex：app-server 的 thread/turn/item 分层、显式 \`TurnAborted\`/rollout 生命周期、模型切换按下一轮生效、Sandbox/Approval 与事件交付经验。

本文件中的外部参考只用于说明取舍；具体实施必须以 Singularity 当前源码、协议和验证结果为准。

## 记录规则

后续每次用户裁决追加新的 \`D-xxx\` 条目，并注明：问题、参考实现、当前代码事实、选择、影响和验收方式。若新证据推翻旧裁决，不删除旧条目，而是追加修正条目并标注取代关系。

### D-047：共享运行时硬切 + app-server 委托（产品形态由 D-049 修正）

问题：若各形态各自实现 turn 执行会造成多套状态机与事件投影漂移；此前 app-server 内维护并行 turn 管线，与 runtime 的 Conversation/TurnRunner 形成第二执行体。参考实现：Pi 的单次执行入口与 AgentSession 复用、Codex 的 thread/turn/item 分层与「设置下一轮生效」。当前代码事实：crates/runtime 是 Turn 执行的唯一所有者——TurnRunner 单轮管线（会话单写者贯穿、typed TurnEvent 事件源、fail-stop 终态化、明细终态原子收敛）与 Conversation 长驻协调器（`reserve_start` 原子预订链窗口、steer 注入当前轮、followUp FIFO 逐条自执行为独立新 turn、取消按轮独立、设置终态后自动应用）；CLI 无参数进入 TUI，--print/--json 单次执行；app-server 是 stdio JSON-RPC 适配器。选择：客户端形态（TUI / headless / app-server）一律委托 runtime，客户端不复制执行状态；协议类型只存在于 crates/protocol 与适配器，runtime 不依赖 protocol/UI。产品形态定位由 D-049 修正。影响与验收方式保持不变。

### D-049：三种产品形态与桌面端后端

问题：双入口框架无法表达已确认的桌面端产品形态，并把 app-server 的角色描述为泛化接入面。参考实现：pi 的单次执行与交互终端、Grok Build 的终端交互、Codex Desktop 及其 app-server。当前代码事实：无交互与 TUI 进程内调用 runtime；app-server 把同一 runtime 投影为 stdio JSON-RPC。选择：产品固定为无交互单次入口、交互式 TUI、桌面端三种形态；app-server 只作为桌面端后端接线口，不构成独立用户入口。影响：产品文档、客户端合同和后续桌面端工作均以三种形态为准。

### D-050：协议与认证文件硬切命名

问题：桌面端历史读取方法需要与 Thread 领域统一，认证文件名不需要版本后缀。参考实现：Codex app-server 的 `thread/read` 与 `$CODEX_HOME/auth.json`。当前代码事实：协议面尚无外部消费者，认证文件只有单文件读路径。选择：按 D-039 硬切为 `thread/read` 与 `auth.json`，不保留 wire 或文件读取兼容层；app-server crate 与其余协议方法保留。影响：protocol、app-server、runtime 配置读取、测试和文档同步使用新名称。

### D-051：会话单写者由 OS 文件锁强制执行

问题：单写者此前只由进程内约定（activate_turn 预订）保证，跨进程无法互斥；`session/delete` 的「活动 turn 检查 → 打开校验 → unlink」之间存在 TOCTOU 窗口，另一进程可在校验后开始 append，删除后写入落入 unlinked inode。参考实现：codex-rs `thread-store/src/local/writer_lock.rs`——每会话一把锁文件 + `std::fs::File::try_lock()` 快速失败 + 协调锁串行化 stale 锁清理 + Guard Drop 先关句柄再删锁文件（Windows 必须先关句柄才能删文件）。当前代码事实：SessionManager 是 JSONL 唯一可变持有者，`open_existing` 已含 repair 重写；toolchain 1.96 ≥ try_lock 稳定版 1.89，标准库直用零新依赖。选择：任何可能写 JSONL 的打开（create_with_file、open_existing 含 repair）都先获取会话写者锁，Guard 为 SessionManager 字段随实例释放；锁目录为 sessions 同级 `thread-writer-locks/`，目录创建走 `create_owner_only_dir`；`open_existing_read_only` 不加锁。`session/delete` 先 try_lock 快速失败，冲突映射为 `APP_ERROR_INVALID_STATE` + 「session is being written by an active writer」（该自定义码已由 D-057 收敛为标准 invalid-request -32600），校验与 unlink 全程持锁，TOCTOU 随之消失。影响：跨进程双开同一会话的第二写者被快速拒绝；同进程测试中原「顺序双开」按新语义改为先释放再打开；只读投影路径不受影响。验收方式：writer_lock 单元测试（竞争拒绝/释放后复用/stale 清理/跨线程快速失败）、delete vs 活动写者集成测试、resume 双开冲突测试。

### D-052：Wire 分派形状（Declared 协议降级层已随变体删除而消亡）

问题：是否将 transport 的 ProtocolAdapter 重构为 trait 或分拆到各协议文件？未声明协议变体 Declared 如何降级？
参考实现：Codex 单一协议适配器、Pi 多 provider 适配器。
当前代码事实：ProtocolAdapter 薄转发表集中于 `crates/model/src/transport/mod.rs` 单文件（端点、请求载荷、reasoning 在场判定、响应解析、SSE 读取），各协议实现体分别位于 `openai/chat.rs` 与 `openai/responses.rs`。运行时协议选择下 trait 化不减少分支总数，仅拆分事实源位置。
选择：维持当前集中薄转发表，接入第三 wire 协议时重评该形状；未声明协议变体 `Declared` 的降级按用途分层且各自单点（transport 折叠为 Chat wire，attempt 观测标记 `Unsupported`）。
影响：双协议维持单点转发表，不引入额外 trait 间接层；协议与观测降级语义清晰独立。
验收方式：双协议 provider 单元与集成测试全绿，wire golden 测试无漂移。

### D-053：ThreadCatalog 成为 Thread 目录操作与只读投影的唯一入口

问题：客户端和各调用点逐点传递 `(sessions_dir, coordinator)` 元组并直接调用 `store` 模块的底层函数，导致会话目录操作接缝发散。
参考实现：codex-rs `thread-store/src/store.rs` 统一 Thread 目录管理。
当前代码事实：`ThreadCatalog` 封装 `sessions_dir` 与进程级写者锁协调器 `WriterLockCoordinator`；`store` 底层函数不再对外直接暴露。
选择：`ThreadCatalog` 成为创建、列表、恢复、重命名、归档和只读分页历史（`paged_read`、`read_thread_summary`）的唯一公开入口；`Conversation` 不持有目录 CRUD。
影响：调用方只需持有 `ThreadCatalog` 单一实例，目录操作集中且易于测试与扩展。
验收方式：runtime 单元与集成测试、cli 及 app-server 目录操作测试全绿。

### D-054：Turn 终态与错误词表收敛为 protocol 单点定义

问题：runtime 与 protocol 之间曾存在平行的终态枚举与错误原因词表，增加了跨层映射与词形同步负担。
参考实现：项目内部收敛。
当前代码事实：`Thread`、`Turn`、`TurnStatus`、`TurnModelUsage`、`TurnFailureCause`、`TurnFailureStage` 统一在 `crates/protocol` 单点定义，runtime 经 `objects.rs`/`error.rs` 原样再导出。
选择：消除平行枚举与重复词表，protocol 成为 wire 形状、事件枚举与状态词表的单一权威事实源；runtime 负责执行语义并将具体 model 失败映射至 protocol 的 `TurnFailureCause`。
影响：跨层类型和错误词形零冗余，golden 测试单点守护线格式。
验收方式：protocol 与 runtime 测试全绿，错误词表一致性测试通过。

### D-055：Thread 设置立即生效（取代 D-048）

问题：D-048 的排队机制（待生效意图合并、终态自动应用、`thread/settingsApplied` 事件、协议 `queued` 字段）是为「活动轮期间可改设置且当前轮不变」这一自设需求引入的全链机制；对照 pi 与 codex 均无排队设计，属过度实现。
参考实现：codex 设置更新立即生效（`core/session/mod.rs` 会话配置就地更新）；pi `setModel` 立即应用（`agent-session.ts`）。
当前代码事实：`Conversation::queue_settings` 在提交点一次完成校验、内存投影更新与 `thread_settings` metadata 持久化；`SettingsApplyTiming` 只剩 `AppliedNow`/`NothingToApply`；`thread/settingsApplied` 事件与协议 `queued` 字段已删除。
选择：设置修改立即校验、立即持久化、立即生效（当前 turn 保持启动时 selector，下一 turn 读取生效）；turn 执行期间写者锁被占用，持久化失败回滚内存投影并报错，设置修改在 turn 间隙提交。空 patch 返回 `NothingToApply`。
影响：删除排队意图、字段合并、终态应用与事件发布全链；任何时刻 thread/list 与 thread/read 只读已落盘值的不变量由「提交点持久化」直接保证。
验收方式：runtime 预订窗口测试断言提交点立即持久化；protocol golden 事件表无 `thread/settingsApplied`；workspace 测试全绿。

### D-056：设置落盘移到 turn 边界记录（修订 D-055）

问题：D-055 的「提交点持久化」要求变更提交点获取会话写者锁；turn 执行期间锁被活动 turn 占用，「运行中改设置」因此成为报错场景，而参考实现在该场景下正常接受——提交点写文件是把持久化放在了错误的层。
参考实现：codex `core/src/session/mod.rs` 的 `update_settings` 只做内存就地更新、不落盘（thread_settings 更新仅发不物化的通知事件），持久化由 turn 自身开始时写入的 TurnContext 记录承载；协议语义为「for subsequent turns」（`app-server-protocol/src/protocol/v2/thread.rs`）。
当前代码事实：`Conversation::update_settings` 在提交点只做校验与内存投影更新；`thread_settings` 的落盘由 `TurnRunner::run` 在 turn 开始时、于本轮已打开的同一会话写者上执行（`record_thread_settings_metadata`，与最后一条已记录值相同则跳过，位于 `turn_started` 之前），失败映射为 `Preparation { cause: Store }`。
选择：对齐 codex 的 turn 边界记录编排——变更提交点只更新内存投影（运行中与空闲同路径，提交点不会因写者锁冲突失败）；持久化发生在下一 turn 开始时由 turn 记录；空 patch 返回 `NothingToApply`。
影响：删除提交点持久化与回滚分支；thread/list、thread/read 在下一 turn 运行前显示旧值（只读已落盘值的不变量由定义保持）；进程在下一次 turn 开始前崩溃会丢失未记录的变更，与 codex 一致。
验收方式：runtime 预订窗口测试断言提交点零写入；新增写者锁占用下的 mid-turn 提交测试（提交被接受、下一 turn 开始记录新 selector）；workspace 测试全绿。

### D-057：深度审查 18 项裁决实施——pi/Codex 完全对齐与过度设计移除

问题：对 HEAD 352a0677 的深度审查产出 18 项裁决清单（讨论记录 `discussion/2026-08-30-11-34-discussion.md`）。用户授权：允许完全重构、不保留旧结构、以长期质量为主，架构决策直接采用 pi/Codex 的相同逻辑与语义；项目复杂度明显高于两者的设计认定为过度设计并移除。
参考实现：Codex `core/src/tasks/mod.rs:844-855`（按预订身份释放窗口）、`codex-api/src/sse/responses.rs:413-477`（按 wire 错误码的流内错误分类）、`app-server` 测试 -32600/-32602 标准码策略（`analytics_client_tests.rs:533-577`）、`message_processor.rs:887-888`（单初始化旗标）；Pi `agent.ts:486-508`（单一所有者）、`openai-completions.ts:592-623`（reasoning 键按序取首个非空）、`agent-session.ts:882-891/:1177-1179`（换绑 abort 压缩、压缩期拒绝新输入——后者经用户裁决由 D-059 改为压缩期排队）、`footer.ts:87-111`（累计 usage）、`tools/edit.ts`（原文精确匹配，无换行/BOM 转换层）。
选择与当前代码事实（按层）：
- model：`complete_stream` 是 `Provider` trait 唯一必需方法、SSE 是唯一读取路径（流式能力声明与非流式 fallback 属过度设计，移除）；reasoning 累加按键序取首个非空键、每块只累加一次；流内与 200 内嵌错误按 wire 错误码类型化（`context_length_exceeded`/`rate_limit_exceeded`/`insufficient_quota`/`content_filter`），保留 provider 原文（有界）与 `provider_error_code` 诊断；Disabled 契约只约束需要 replay 的工具调用续接；响应状态镜像枚举删除，不可恢复的响应校验在 provider 边界以类型化 `Err` 收敛。
- agent：`thread_settings` 落盘形状 provider+model 必填、reasoning 可选；compaction 条目持久化 summary/firstKeptEntryId/usage/details（估算规模经完成回调交付，不落盘）；reasoning replay 只从已存条目读取（发送侧重建分支移除）；会话层 `ThreadSummary` 是 JSONL 派生摘要的唯一结构，runtime 转发导出。
- runtime：turn 窗口释放按预订身份（世代序号）比对，迟到的旧 Drop 不得清空新窗口；失败终态事件携带本轮已记录的真实 usage。
- protocol/app-server：JSON-RPC 错误投影收口在 transport 单函数——参数错误 -32602、状态冲突与握手违规 -32600（标准 invalid-request，自定义 -32002/-32003/-32005 退役）、资源不存在 -32004、其余 -32603；错误对象只有一种类型；初始化门禁单旗标，claim lane 与 control lane 共用；claim 失败一律以类型化 `Err` 收敛（并行「直接响应」路径退役）；wire 词形由 serde snake_case 投影单源提供，Display 投影同一词形，手写词表移除。
- cli：edit 工具对文件原文逐字节精确匹配（换行/BOM 转换层属自设复杂度，移除，对齐 pi）；TUI 状态行 token 数逐轮累计（对齐 pi footer）；会话换绑取消进行中的压缩并以会话世代号丢弃迟到回调（对齐 pi dispose-aborts-compaction）；换绑门禁同时检查 TUI 相位与 runtime 活动窗口。
影响：删除一层能力声明、一条非流式路径、一份状态镜像枚举、两份同形结构（ErrorCode/JsonRpcError、SessionProjection/ThreadSummary）、三枚自定义错误码、四张手写词表与 edit 的全部换行/BOM 转换机制；错误语义与握手行为与 Codex app-server 对齐，压缩/换绑/累计用量与 pi 对齐。
验收方式：workspace fmt/clippy -D/test 全绿；退役词形（`provider_streaming_unsupported`、`-32002/-32003/-32005`、`tokensBefore`、`ModelTurnStatus`、`SessionProjection`、`wire_str`、`initialized_acknowledged`、`TurnClaim`、edit 归一化 helper）全仓 grep 归零；新增回归测试覆盖窗口身份释放、错误码类型化映射、reasoning 双键单累加、Disabled 契约无工具调用合法性、失败终态 usage。

### D-058：会话列表降级为头部元数据（用户裁决，取代 D-053 的列表全投影面）

问题：`list_threads` 此前对每个会话文件完整解析并聚合（turns/tokens/title/model/status），列表打开成本 O(N×文件大小)；两家参考均无此形态——pi 列表只读文件首行 header + mtime（`jsonl/repo.ts:65-87`），Codex 列表同样不做逐会话聚合。Agent 曾推荐维持现状（真实规模小），用户裁决砍显示、完全对齐 pi。
参考实现：pi `packages/agent/src/harness/session/jsonl/repo.ts:65-87`（`readTextLines(maxLines: 1)` + `file.mtimeMs` + `modifiedAt` 降序）；pi 的会话 name 是条目 fact、不在 header，故其列表同样不显示标题。
当前代码事实：agent 新增 `read_session_header`（严格 header 校验、坏文件 `Err` 由列表跳过）与 `file_modified_iso`；runtime `list_threads` 返回 `ThreadListing { thread_id, cwd, created_at, updated_at }`；协议 `thread/list` 响应改为 `ThreadListItem` 列表（不再复用单会话 `Thread` 形状）；TUI `/resume` 菜单显示 short id + 更新时间。
选择：列表只读 header + mtime；title/model/status/turn_count/total_tokens 全部移出列表类型。单会话聚合事实按需读取：`/session`、`/resume` 换绑初值（⑧ 累计口径）、`thread/read`、`session/delete`、`thread/settings` 继续走单文件 `read_thread_summary`，不受影响。
影响：列表打开从 O(N×文件) 降为 O(N×首行)；`/resume` 菜单不再显示标题/回合数/token 数（与 pi 同形）；`thread/list` 协议字段变化（无外部消费者，开发阶段硬切，D-039）。
验收方式：workspace fmt/clippy -D/test 全绿；`ThreadSummary` 消费点复核（仅单文件路径）。

### D-059：压缩期输入排队，完整对齐 pi interactive-mode（用户裁决，取代 D-057 的压缩期拒绝面）

问题：压缩持有会话一致性写窗口，D-057 落地后压缩期间一切输入被拒绝（草稿保留）。Agent 推荐维持（现状无损、无观察到的痛点），用户裁决完整对齐 pi 的压缩期排队交互。
参考实现：pi `interactive-mode.ts:3137-3147`（压缩中 Enter→steer 队列）、`:4115-4125`（Alt+Enter→followUp 队列）、`:4393-4399`（入队记历史+清编辑器+状态提示）、`:4353-4370`（pending 列表显示）、`:4372-4391`（dequeue 整体倒回编辑器，队列文本与草稿空行拼接）、`:4411-4478`（压缩结束 flush：首条为 prompt、其余按通道注入、失败回队）。pi 的扩展命令压缩中立即执行映射为本仓斜杠命令立即执行；pi 的 willRetry 分支无对应物（本仓压缩无自动重试），不设。
当前代码事实：TuiApp 新增 `compaction_queue: Vec<QueuedMessage{text, mode}>` 与 `QueueMode{Steer,FollowUp}`；submit_input/Alt+Enter 在 `compaction.is_running()` 时入队（命令仍走 execute_command）；Alt+Up 优先整体出队回编辑器，队列空时维持 followUp 逐条撤回；换绑 `reset_session_state` 清空队列（pi :2057 同向）。
选择：flush 拆两段以避开与 turn 预订的竞态——`on_compact_finished` 把首条按普通提交启动回合（返回 `Action::Submit`，事件循环 spawn），其余在该回合 `TurnStarted`（注入窗口已开）时按通道注入，注入失败的退回队列不丢输入；队列与压缩结果无关地消费（取消/失败同样送达）。状态行显示 `queued:N` 计数与压缩期提示行，替代 pi 的 pending 列表逐条显示（本仓状态行形状）。
影响：压缩期间输入不再被拒；新增一个 TUI 暂存状态，接入既有 epoch/换绑/取消交互网；runtime/协议零改动（排队纯 UI 层，压缩窗口不变量不受影响）。
验收方式：workspace fmt/clippy -D/test 全绿；行为面为 TUI 交互（无既有测试脚手架，不新建测试基础设施），由评估器最终轮与手工走查覆盖。
