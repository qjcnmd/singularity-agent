# 已确认的取舍

这里只记录**有意接受的行为限制和已知代价**，不记录常规架构设计。审查、修复或精简时，表里的条目不当作缺陷，也不再提议修改；除非用户要求改变，或表中写的前提不再成立。

| 行为限制 | 适用条件、代价与边界 | 源码入口 |
| --- | --- | --- |
| 压缩请求保留指令和工具定义，不单独拒绝工具调用 | 前缀压缩沿用本轮指令和工具定义以尽量命中前缀缓存，代价是工具定义计入压缩请求的输入和窗口预算。仅发送待压缩前缀，保留最近 15% 原文（溢出恢复时至少保留最后一个完整平衡单元）；回复未截断、未拒答、正文非空且框定后严格小于前缀估算时保存为 `<compacted-summary>` 检查点并忽略其中的工具调用，完整历史仍在日志中。仅当用户要求改变或实测出新的成本问题时再讨论，不以“参考项目做法”或“压缩不执行工具”为由移除工具定义。 | [compaction.rs](../crates/agent/src/agent/compaction.rs) |
| 流解码失败时不保留已收到的 Token 用量 | 解码失败的请求保留错误和计时，但不记录解码器已收到的用量，累计统计可能低于实际消耗。未上报的用量保持未知。 | [client.rs](../crates/model/src/client.rs)、[Messages 解码器](../crates/model/src/messages/stream.rs)、[Responses 解码器](../crates/model/src/responses/stream.rs) |
| 创建日志时被强制终止可能留下不完整文件 | 新会话独占新建日志并直接写入；创建过程中被强制终止时需要手动删除该日志，不增加创建事务或恢复流程。 | [manager.rs](../crates/agent/src/session/manager.rs) |
| 草稿存储错误不打断操作 | 首次读取失败时从空白继续，不提示、不禁用输入；新输入照常保存，允许覆盖当前任务的旧草稿。自动保存失败时保留当前输入，清理失败不影响已完成的任务删除或项目移除。 | [drafts.ts](../apps/desktop/src/drafts.ts) |
| 单份任务记录读取失败时，任务列表直接报错 | 末尾没有换行的不完整记录仍会忽略；完整记录解析或读取失败时，列表显示文件错误，不增加跳过任务和局部列表的处理流程。报错期间前端忽略流事件，修复文件后仅通过「重试」或 `Ctrl+R` 恢复，不做经其他操作触发的旁路隐式恢复。 | [thread_catalog.rs](../crates/app/src/runtime/thread_catalog.rs)、[file.rs](../crates/agent/src/session/file.rs)、[sessionStore.ts](../apps/desktop/src/sessionStore.ts) |
| 已显示部分输出后仍移除片段并自动重试 | 遇到已知暂时故障时，移除本次未完成的正文和思考，重新生成整次回复；最多尝试三次，等待期间可停止，不单独提示重试。 | [request_execution.rs](../crates/agent/src/request_execution.rs) |
| 排队输入只保存在内存中，崩溃时不恢复未落盘输入 | 运行中接受的新输入立即返回并显示“已接受”，待回合真正写入日志后才消费；回合或准备阶段失败、以及用户主动停止时清空未消费输入并暂停自动继续；未落盘前程序异常退出时后端不恢复队列，靠前端草稿兜底。 | [inbox.rs](../crates/agent/src/agent/inbox.rs)、[background.rs](../crates/app/src/runtime/conversation/background.rs) |
| 切模型不再回溯旧模型预压缩，超窗压缩统一单轮折叠裁剪 | 切模型后直接进入目标模型常规自动压缩（达目标窗口 90% 留尾 15%）；待压缩前缀超单次预算时由 `fold_oversized_messages` 单轮剥思考、图片占位并按 `#n` 折叠裁剪工具正文，不做多轮分段压缩。若极端工具数下折叠后仍超出空间，按正常报错或发请求自然失败返回，不发多轮分段请求，也不增加专用错误界面。 | [compaction.rs](../crates/agent/src/agent/compaction.rs)、[session/compaction.rs](../crates/agent/src/session/compaction.rs) |
| Bash 命令输出统一单轨纯文本捕获，不渲染终端 ANSI 配色 | 非 PTY 管道下常规命令默认本就无色；保留 `vte` 剥离强制彩色工具的转义符，并以单轨 `CaptureState` 缓冲同时服务模型与界面，不维护双轨缓冲，也不在前端做 ANSI 解析与着色渲染。 | [pump.rs](../crates/agent/src/tools/bash/pump.rs)、[output.rs](../crates/agent/src/tools/output.rs)、[CommandOutput.tsx](../apps/desktop/src/components/CommandOutput.tsx) |
| MCP 设置卡与新建任务页不展示未连接时的近似 Token 估算 | 避免未连接前基于不完整缓存给出失真估算；MCP 设置卡已连接项仅展示工具数，新建任务页胶囊对 MCP 项不估算 Token（固定为 0）。进入会话触发真实加载后，`ContextPanel` 照常按运行时实际注入内容统计 Token。 | [lib.rs](../crates/mcp/src/lib.rs)、[modules.rs](../crates/agent/src/agent/modules.rs)、[McpSettings.tsx](../apps/desktop/src/components/McpSettings.tsx) |
| 事件流通道满时仅记警告丢弃，不做背压分级与运行中自动重同步 | 本机 Electron 管道已有 `4096 + 512` 槽缓冲且流式进度与后台刷新已节流 ≤10 Hz，保留非聚焦高频过滤；极端通道真满时直接 `tracing::warn!` 丢弃该帧，不再维护背压分级状态机或合成运行中 `Ready` 重同步信号（需恢复时按 `Ctrl+R` 刷新）。 | [stream_hub.rs](../crates/app/src/desktop/stream_hub.rs)、[transport.rs](../crates/app/src/desktop/transport.rs) |
| MCP 报错对 URL 查询串统一整段打码，不做额外解码反匹配 | `mcp.json` 中的 Stdio 与 HTTP URL 报错时统一将整段 `?query` 替换为 `[redacted]`（含普通参数，不再单独保留可见）并辅以 `query_pairs()` 敏感键值脱敏，且不额外做百分号解码去匹配外部进程自行向 stderr 打印的解码后 userinfo 密码；敏感名继续覆盖长关键词与驼峰/分隔符词界短词。 | [config.rs](../crates/mcp/src/config.rs) |
| 编辑历史消息回退仅截断对话日志，不回滚磁盘文件也不加界面提示 | 编辑历史用户消息后只回退会话日志并从该轮重跑，不自动还原工作区中已改动或新建的文件，编辑框也不额外展示“文件不会回滚”提示文案。 | [file.rs](../crates/agent/src/session/file.rs)、[appStore.ts](../apps/desktop/src/appStore.ts)、[TimelineItem.tsx](../apps/desktop/src/components/TimelineItem.tsx) |
