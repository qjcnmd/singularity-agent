# 已确认的取舍

这里只记录**有意接受的行为限制和已知代价**，不记录常规架构设计。审查、修复或精简时，表里的条目不当作缺陷，也不再提议修改；除非用户要求改变，或表中写的前提不再成立。

| 行为限制 | 适用条件、代价与边界 | 源码入口 |
| --- | --- | --- |
| 压缩请求保留指令和工具定义，不单独拒绝工具调用 | 前缀压缩沿用本轮指令和工具定义以尽量命中前缀缓存，代价是工具定义计入压缩请求的输入和窗口预算。仅发送待压缩前缀，保留最近 15% 原文（溢出恢复时至少保留最后一个完整平衡单元）；回复未截断、未拒答、正文非空且框定后严格小于前缀估算时保存为 `<compacted-summary>` 检查点并忽略其中的工具调用，完整历史仍在日志中。仅当用户要求改变或实测出新的成本问题时再讨论，不以“参考项目做法”或“压缩不执行工具”为由移除工具定义。 | [compaction.rs](../crates/agent/src/agent/compaction.rs) |
| 流解码失败时不保留已收到的 Token 用量 | 解码失败的请求保留错误和计时，但不记录解码器已收到的用量，累计统计可能低于实际消耗。未上报的用量保持未知。 | [client.rs](../crates/model/src/client.rs)、[Messages 解码器](../crates/model/src/messages/stream.rs)、[Responses 解码器](../crates/model/src/responses/stream.rs) |
| 创建日志时被强制终止可能留下不完整文件 | 新会话独占新建日志并直接写入；创建过程中被强制终止时需要手动删除该日志，不增加创建事务或恢复流程。 | [manager.rs](../crates/agent/src/session/manager.rs) |
| 草稿存储错误不打断操作 | 首次读取失败时从空白继续，不提示、不禁用输入；新输入照常保存，允许覆盖当前任务的旧草稿。自动保存失败时保留当前输入，清理失败不影响已完成的任务删除或项目移除。 | [drafts.ts](../apps/desktop/src/drafts.ts) |
| 单份任务记录读取失败时，任务列表直接报错 | 末尾没有换行的不完整记录仍会忽略；完整记录解析或读取失败时，列表显示文件错误，不增加跳过任务和局部列表的处理流程。 | [thread_catalog.rs](../crates/app/src/runtime/thread_catalog.rs)、[file.rs](../crates/agent/src/session/file.rs) |
| 已显示部分输出后仍移除片段并自动重试 | 遇到已知暂时故障时，移除本次未完成的正文和思考，重新生成整次回复；最多尝试三次，等待期间可停止，不单独提示重试。 | [request_execution.rs](../crates/agent/src/request_execution.rs) |
| 排队输入只保存在内存中，崩溃时不恢复未落盘输入 | 运行中接受的新输入立即返回并显示“已接受”，待回合真正写入日志后才消费；回合或准备阶段失败、以及用户主动停止时清空未消费输入并暂停自动继续；未落盘前程序异常退出时后端不恢复队列，靠前端草稿兜底。 | [inbox.rs](../crates/agent/src/agent/inbox.rs)、[background.rs](../crates/app/src/runtime/conversation/background.rs) |
