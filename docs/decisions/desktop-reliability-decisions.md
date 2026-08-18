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

### D-011：持久事实与发布顺序

终态必须先写入 JSONL，再更新 SQLite 索引并发布 \`turn/completed\`、\`turn/failed\` 或 \`turn/interrupted\` 事件。客户端看到终态时，持久事实必须已经存在。

### D-012：重连

Desktop 重连采用 \`thread/resume\` 和 Session 状态重建，不实现事件 cursor/gap 重放。实时 delta 只保证在线连接期间传输。

### D-013：事件背压

保留有界队列、满时阻塞、不主动丢事件的策略。未来只有在真实性能证据出现时才合并 delta，不提前引入 Codex 式复杂事件分类。

### D-014：模型切换

Desktop 运行期间切换模型/Provider 不重启 app-server；当前 turn 保持原配置，切换从下一 turn 生效。thread 设置持久化，下一轮按新模型重新预算并必要时 compaction。

### D-015：Provider 私有续接

切换模型/Provider 后，不兼容的 provider-private reasoning replay 丢弃；可见消息、tool call、tool result 保留，从新 Provider 的公开历史重新开始。不得尝试跨协议转换 opaque replay。

### D-016：thread 设置存储

模型、Provider、reasoning effort 等非敏感选择追加为 JSONL \`thread_settings\` 记录，SQLite 只做索引；不重写 Session header，不只存内存。

### D-017：凭据边界

\`thread_settings\` 不保存 API key、Authorization header 或其他认证材料；凭据继续由全局配置/环境管理。

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

Desktop 接入前补齐 Codex 式 \`item/completed\` 生命周期：每个 \`item/started\` 必须对应 terminal 的 \`item/completed\` 或 \`item/failed\`，再发布 turn 终态。工具详细参数和结果继续通过已有 \`tool/execution/*\` 事件传递。

### D-024：Item 持久化

按 Codex 方式持久化最终 item、稳定消息、工具结果和 turn 状态；在线 delta 只用于实时显示，不逐字/逐 chunk 写入 JSONL。重连从最终 item 和 Session 状态重建，不实现 cursor/gap 事件重放。

### D-025：工具 Item

工具调用纳入统一 item 生命周期。每个 \`toolCallId\` 对应一个稳定 item，事件顺序为 \`item/started\` → \`tool/execution/start\` → \`tool/execution/update\`* → \`tool/execution/end\` → \`item/completed\` 或 \`item/failed\`。两类事件必须共享同一身份和终态，避免客户端维护矛盾状态。

### D-026：并行 Item 顺序

并行工具的实时事件按实际完成顺序到达；Session 中的最终 tool result 仍按模型 source order 持久化。UI 获得低延迟进度，模型上下文保持稳定顺序。

### D-027：能力声明

在现有 \`server/capabilities\` 中增加协议版本和固定 feature 列表，至少声明 item 生命周期、工具 item、thread 设置和 interrupted recovery。Desktop 按能力启用功能，不建立插件注册系统。

### D-028：协议兼容

采用能力先协商、协议只增不改、旧客户端继续基本运行、新客户端对旧 server 降级的兼容合同。新命令在能力缺失时返回 typed unsupported error，不依赖试错或内部崩溃。

### D-029：Thread 设置接口

增加独立 \`thread/settings\` 请求。设置更新与 thread 恢复分离；设置成功后从下一 turn 生效，并追加 JSONL \`thread_settings\` 记录。

### D-030：活动 Turn 中的设置变更

活动 turn 期间允许更新 thread 设置并立即持久化；当前 turn 及其 steer 继续使用启动时的旧 Provider/模型，设置只对下一 turn 生效。

### D-031：Usage 持久化

标准化 turn usage 持久化到 JSONL，SQLite 继续做聚合索引；\`thread/resume\` 恢复 usage 给 Desktop。Provider 未提供 usage 时显示 unknown，不伪造为 0。

### D-032：原始 SessionEntry 公共化（已撤回）

曾讨论让 Desktop/app-server 的历史读取返回原始 SessionEntry，包括 \`providerReasoningReplay\` 等 provider-private reasoning state；用户已撤回该选择，不得实施。最终公开历史边界待重新讨论。

### D-033：session/read entry_types（已撤回）

曾讨论是否扩展 \`session/read\` 的 entry_types；用户已撤回该选择。最终历史读取 API 和过滤范围待重新讨论。

### D-034：Thinking 公开投影

Desktop 公开显示文本 thinking block。DeepSeek 等 Provider 明确返回的完整 \`reasoning_content\` 可以完整保留并折叠展示；过滤 replay 元数据、Responses opaque output items 和 \`encrypted_content\`。最终历史读取采用公开结构化投影，不返回原始 SessionEntry。

### D-035：Thread 模型投影

thread_settings 追加后同步更新 SQLite 当前模型索引；JSONL 是历史事实，SQLite 是最新值投影，\`thread/list\`、\`thread/resume\` 和 \`session/read\` 保持一致。

### D-036：Metadata 上下文隔离

turn 生命周期、thread_settings、usage、item 状态等 metadata 只用于恢复和客户端展示，不进入模型请求上下文；模型仍只接收 user/assistant/toolResult 及必要 compaction 投影。

### D-037：SQLite 索引修复

JSONL 追加成功而 SQLite 更新失败时保留 JSONL；下次打开从 JSONL 修复 SQLite 索引，不回滚历史、不允许永久不一致。

### D-038：计划文件交付

完整实施计划保留在被 Git 忽略的 plan/ 目录，不修改 .gitignore；通过本机计划文件路径和执行提示词交给另一个模型实施。计划文件本身不代表代码已实施或已验证。

## 参考实现

- Pi：小核心、Agent Loop、Session JSONL、compaction、工具输出截断与临时 spill。
- Codex：app-server 的 thread/turn/item 分层、显式 \`TurnAborted\`/rollout 生命周期、模型切换按下一轮生效、Sandbox/Approval 与事件交付经验。

本文件中的外部参考只用于说明取舍；具体实施必须以 Singularity 当前源码、协议和验证结果为准。

## 记录规则

后续每次用户裁决追加新的 \`D-xxx\` 条目，并注明：问题、参考实现、当前代码事实、选择、影响和验收方式。若新证据推翻旧裁决，不删除旧条目，而是追加修正条目并标注取代关系。
