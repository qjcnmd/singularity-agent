# Singularity 架构说明

完整架构见根目录的 [architecture.html](../architecture.html)，支持拖动与缩放。先看全貌，再按章节查看回合、请求、工具、压缩、后台、保存、工作台和配置。箭头标明顺序与分支，节点说明状态归属，悬浮和点击可定位源码。

本文维护模块职责、状态归属与源码入口。产品运行边界见[宪章](constitution.md)，桌面交互细节见[工作台交互](desktop-ui.md)，本地调试命令见[开发指南](development.md)。

## 模块职责

| 模块 | 核心职责 | 源码入口 |
| --- | --- | --- |
| Electron 主进程与 preload | 窗口、托盘、原生对话框，隔离渲染进程与后端通信 | [main.ts](../apps/desktop/desktop/main.ts)、[preload.cts](../apps/desktop/desktop/preload.cts)、[backend.ts](../apps/desktop/desktop/backend.ts) |
| React 工作台 | 用户输入、草稿、任务显示与交互状态 | [app.tsx](../apps/desktop/src/app.tsx)、[sessionStore.ts](../apps/desktop/src/sessionStore.ts) |
| `app` | 后端装配、stdio RPC 适配与桌面状态投影 | [app_server.rs](../crates/app/src/desktop/app_server.rs)、[transport.rs](../crates/app/src/desktop/transport.rs) |
| `runtime` | 任务生命周期、操作互斥、引导输入、事件转换与公开历史 | [conversation.rs](../crates/runtime/src/conversation.rs)、[runner.rs](../crates/runtime/src/runner.rs)、[event_projection.rs](../crates/runtime/src/event_projection.rs)、[thread_catalog.rs](../crates/runtime/src/thread_catalog.rs) |
| `agent` | 模型与工具循环、上下文装配、摘要压缩与会话日志追加 | [agent/mod.rs](../crates/agent/src/agent/mod.rs)、[session/mod.rs](../crates/agent/src/session/mod.rs) |
| `model` | 模型配置、Chat / Responses 编解码、HTTP / SSE 传输 | [config/manager.rs](../crates/model/src/config/manager.rs)、[openai/provider.rs](../crates/model/src/openai/provider.rs)、[transport/http.rs](../crates/model/src/transport/http.rs) |
| `mcp` | MCP 配置、按工作目录复用连接、工具发现与调用 | [lib.rs](../crates/mcp/src/lib.rs)、[client.rs](../crates/mcp/src/client.rs) |
| `core` | 数据目录解析、文件写入、项目指令与技能发现 | [user_home.rs](../crates/core/src/user_home.rs)、[project_instructions.rs](../crates/core/src/project_instructions.rs)、[skills.rs](../crates/core/src/skills.rs) |
| `protocol` | RPC 参数、结果、事件与 TypeScript 类型导出 | [rpc.rs](../crates/protocol/src/rpc.rs)、[params.rs](../crates/protocol/src/params.rs) |

工作台通过 `runtime` 调用 `agent`。桌面通信由 Electron 和 `app` 适配，执行规则由 `runtime` 与 `agent` 维护。协议对象、会话记录、模型消息和视图对象在各自边界转换。

模型配置的读写和脱敏目录由 `ModelConfigManager` 负责；保存、删除直接返回操作结果，`AppServer` 在同一发布锁内执行修改、重读并发布目录，刷新失败不改变已保存操作的结果；`config.json` 的读写入口在 [config/user/mod.rs](../crates/model/src/config/user/mod.rs)。模型、MCP 和项目登记共用 `core::write_json_atomic`，统一生成缩进 JSON、末尾 LF 并原子替换；各自仍负责目录创建、错误提示和写入成功后的内存提交。

`OpenAiProvider::complete_protocol_stream` 在同一个协议分支内完成请求编码、端点选择和响应解码，续接材料复用实际请求端点。共同 SSE 切帧器接受 CR、LF、CRLF，跨数据块的 CRLF 只算一个换行；只忽略流开头的 UTF-8 BOM。

活动球的动画生命周期由 [ActivityOrb.tsx](../apps/desktop/src/components/ActivityOrb.tsx) 管理；[pearlRenderer.ts](../apps/desktop/src/components/pearlRenderer.ts) 负责初始化画布并绘制球体。

工具定义和请求偏好使用 `protocol` 的共同类型；历史和实时工具结果共用 `ToolResult`。每轮首次请求前，把实际使用的 Harness 指令、文件指令、Skills 目录和工具定义保存为会话注入条目，直接展示正文。轮内复用这一份输入，下一轮重新读取并再次展示。请求记录只保存用量、耗时和完成状态。中断内容单独保存供显示，不进入后续模型上下文。

模型上下文由 `agent::session::context` 直接借用会话记录。工具结果按调用顺序保存，投影顺序读取，缺失结果补为未知；压缩切点保持完整工具组。普通请求与摘要共用消息转换和 Token 估算。发送预算包含有效对话、本轮指令和工具定义及实测校正；图片在释放写者锁后读取。

历史分页直接返回已合并的公开条目。注入记录含本轮正文；工具调用和结果组成一个条目；请求记录只用于统计。

每轮重新加载当前指令和工具定义，保存实际正文供会话展开查看；压缩记录只保存成功摘要与近期消息的保留位置。

历史投影将工具调用和结果组成一个条目。每次请求完成保存真实用量、耗时和完成状态；未完成请求仅实时显示。

## 状态归属

| 数据或状态 | 维护模块 | 使用边界 |
| --- | --- | --- |
| 项目路径与分组 | `app::WorkspaceStore` | 只保存项目身份与路径；界面从路径取文件夹名，不另存显示名称；项目分组不改变本机工具权限。移除前在生命周期锁和会话 slot 锁内检查普通执行、后台运行及待处理输入，后台终态与通知入箱之间不能穿插移除 |
| 任务运行状态与取消控制 | `runtime::Conversation`、`TurnControls` | 会话负责操作占用；本轮控制器负责输入、问答和停止，接受停止与冻结结果共用一把锁 |
| 尚未保存的用户输入 | 会话内的 `SteeringInbox` | 首条和追加消息共用内存队列，按入队顺序保存；运行快照按身份展示待处理消息，图片按需从输入箱读取；重启不恢复队列 |
| 消息、工具结果、每轮注入内容、请求记录、摘要与图片快照 | `agent::SessionManager` 与会话图片存储 | 会话日志保存正式历史；摘要影响模型上下文，原历史仍保留 |
| 公开历史与任务索引 | `runtime::ThreadCatalog` 与历史转换模块 | 从会话记录生成，保留持久化模型与公开对象的边界 |
| 当前操作条目与会话修订号 | `app::ConversationSlot` | 提供执行中的显示快照；历史快照来自会话写者的内存记录 |
| 会话累计用量 | `runtime::accumulate_request_usage` | 历史与实时请求共用累计规则；Slot 从已保存用量起算，每次请求观测更新合计，随事件与恢复快照发送；每次请求独立分配身份，前端只计算显示比率 |
| 模型配置与密钥 | `model::ModelConfigManager` | 同存于 `config.json`，一起原子提交；提供方统一设置协议，模型不单独配置；对外目录脱敏，执行读取配置快照 |
| MCP 配置与连接 | `mcp::McpManager` | 设置与执行共享同一实例，连接按工作目录复用；文件与设置接口共用 `protocol::McpTransportConfig`，省略的参数、环境变量和请求头按空值读取 |
| 草稿、视图偏好与界面显示 | React 草稿存储、`AppStore` 与 `SessionStore` | 未发送任务共用草稿，已发送任务按会话身份保存草稿；历史和事件更新同一份回合列表 |

模型响应、请求记录和公开观测共用 `protocol::ModelUsage`，请求的 `usage: Option<ModelUsage>` 从响应直接保存并传给历史与实时界面。输入和输出计数都有效时才有用量；总数优先用提供方上报值，缺失时取输入与输出之和。对象内输入、输出和总数均为必填，缓存计数独立可选；未上报不当成零，明确零值照常保存。响应随后因续接材料不合法被拒绝时，已上报用量仍由请求执行层保存。

模型发现只读取当前提供方的模型列表，填入空字段；已有值保留。配置只保存最终值，不记录自动或手动来源。切换模型 ID 保留已填参数。

未提交的问答按任务和问题保存在 `SessionStore` 内存中，切换任务时保留，提交成功或后端确认问题结束后清理。页面刷新后重新填写。

`SessionStore` 在首次发送成功后按返回的任务身份打开会话，与选择和恢复已有任务共用读取入口。读取期间暂存事件，按会话修订号丢弃旧响应。历史与实时事件更新同一份回合列表；模型设置保存后也发布同形状的条目。结束后保留结果，只刷新任务目录。读取失败保留草稿和停止入口，重试后恢复正文。阅读位置保存条目身份及所属回合，临时输出借用邻近已保存条目定位。移除项目时，按当前任务列表清理对应草稿。

Tokio 运行时由 `main` 持有；Electron 与 AppServer 只通过私有 stdio 通信。Conversation 长期持有一个写者，所有保存操作共用它。Slot 只保留当前操作的显示条目，读取时与持久历史合并，结算后清除活动内容。

每轮执行通过 `TurnRunner::prepare_agent` 解析模型并读取当前项目指令，Agent 扫描 Skills、发现 MCP 工具后保存本轮定义；轮内工具循环和自动压缩复用它。下一轮重新读取，包括重启后的第一次运行。手动压缩独立准备当前定义；没有早期对话可总结时直接完成，不扫描技能或调用模型。自动压缩阈值为窗口的 90%，保留近期有效对话的 10% 并扩展到完整工具组。摘要请求只包含早期对话、当前定义和总结要求，最多输出 8192 Token，另受模型上限与窗口余量约束。

`Agent::compact_now` 为本次压缩分配条目身份并发布运行状态；成功后使用同一身份保存摘要。失败、取消和无需压缩只发布当前反馈。成功摘要改变后续模型上下文。

普通回合与独立压缩共用事件转换。自动压缩错误关联当时显示的失败条目，重新打开后由保存的回合错误展示原因。未保存的错误由运行状态直接显示。

空白页共用一份待发送草稿，首次发送通过 `session.start` 创建真实会话并接受输入；后续消息使用 `session.submit`。点击发送时确定项目、模型、草稿和当前导航标记，再编码图片。首次发送只返回任务身份与目录；成功后清理未被继续编辑的草稿，失败保留草稿。只有期间未切换页面才打开新任务，正文读取的成败不改变发送结果。首次打开已有会话时解析日志，之后复用同一个写者。输入检查通过后进入队列，空闲时预订普通回合。Agent 按顺序保存并消费输入；准备失败保留队列，用户停止清空尚未消费的用户消息。手动压缩检查空闲后复用会话写者和独立停止令牌。

启动、运行和收尾期间都直接接受追加输入，停止期间拒绝接收。输入箱只负责保存队列，操作准入由 Conversation 决定。生成请求装配前交付已到达输入；压缩期间收到的消息进入下一次普通请求。当前操作结算后，宿主释放占用并检查队列；有待处理输入且允许续跑时，预订下一轮。新提交和结算共用同一会话发布锁。

摘要校验通过且尚未取消时，保存摘要与保留位置，随后切换有效上下文。失败或取消保留原上下文；成功保存后的摘要不因稍后的停止而撤回。压缩不刷新指令、技能或工具。原始历史仍保留，后续模型请求只取最后一次摘要及保留消息。自动压缩成功后继续原任务，手动压缩完成后处理已接收输入。模型明确返回上下文超限时，每次生成最多压缩并重发一次。

没有可压缩内容时直接显示结果；失败或取消保留原上下文。持久记录只保留成功摘要和真实模型请求用量。

手动与自动压缩都响应停止，网络请求和重试等待复用取消信号。取消记为中断，不显示执行失败。归档和退出先停止前台及压缩，再回收后台命令。Electron 完全退出统一等待最多十秒，不按压缩状态暂停退出计时。

`McpManager` 按工作目录复用健康连接和成功目录；首连、重连或发现失败时读取工具清单。每轮模型定义直接来自本轮实际工具目录，执行时在同一目录检查名称与参数，不再对照旧定义。设置变化在下一轮生效；服务器运行中移除工具时，调用失败直接返回，不重放。

MCP 设置页读取管理器保存的连接状态与最近一次工具发现结果，不启动服务或查询远端工具列表。分类打开时，每秒继续读取“连接中”的状态；查询不重叠，连接结束或离开分类后停止刷新。只有失败状态显示“重试连接”，用户点击后重建当前项目的连接并发现工具。应用退出时先停止并等待会话结束，再关闭 MCP 连接。本地服务器使用 Windows Job Object，并开启关闭句柄时终止整棵进程树；后端异常退出也会回收这些进程。

实际文件位置、备份与维护方式见[安装说明的数据目录章节](INSTALL.md#6-数据存储与目录维护)。

`SlotState` 保存会话修订号、当前操作条目、累计用量和未保存的错误。读取时从写者生成历史页，再合并当前操作内容；完成后清空活动条目。后台状态查询直接读取修订号，不为它构造整个运行快照。

归档先阻止目标任务接收新操作，等待执行结束，再移动日志；失败后解除限制。归档成功后独立清理草稿，草稿读取不作为归档前置条件。当前任务归档后回到同项目空白页。空白页没有后端会话，不需要从列表中隐藏或复用空会话。

工具参数在注册表统一检查。整批都支持并行时每组最多执行八个，否则整批顺序执行；提问和 MCP 属于独占工具。进度实时发布，每组全部结束后按模型调用顺序保存结果并发布完成事件，再启动下一组。慢工具会延迟同组其他结果保存。保存失败停止后续组，当前组已全部收尾。后台命令交付编号后本次工具调用结束，进程继续由会话持有。

Bash 通过 Tokio 异步读取 stdout、stderr，并同时等待主进程退出、超时或取消；不使用读管道线程和轮询。进度回调异步发送到有界队列，队列满时等待调度器消费，不阻塞运行时线程。Bash 与 MCP 共用 process-wrap：先挂起创建进程、加入 Job Object，再恢复运行。异常结束时终止整棵进程树，最多等待 5 秒回收主进程；随后最多用 2 秒读取剩余输出。后台进程仍占用管道时停止读取并提示截断，关闭作业句柄时终止后代进程。完整输出文件的同步写入放在 `block_in_place` 中，避免占用运行时工作线程。

### 后台命令

`Conversation` 持有后台进程管理器，普通回合完成不会关闭它。`bash` 显式传入 `run_in_background: true` 时，启动成功就返回任务编号；前台命令默认等待十秒，超过 `yield_time_ms` 后交付同一进程的编号，不重新执行。等待窗口与总执行时限分开：普通命令默认总时限为五分钟，自动转后台后沿用原时限；显式后台命令未指定 `timeout_ms` 时持续运行，直到退出或被停止。

模型通过 `task_list` 查看本进程的后台命令，`task_output` 读取输出并有限等待，`task_stop` 单独停止。工作台右侧只显示后台命令，按运行和结束分组；展开可看命令与输出，运行项提供停止入口。历史命令仅从保存记录读取供显示，不建立取消信号或完成信号，不接管旧 PID。

`BashTasks` 只管理本进程启动的命令及输出，复用会话写者保存开始和终态。进度只更新内存并发布；自然完成的通知先进入输入箱，再发布终态。AppServer 负责发布和启动待处理回合。保存失败报告实际原因并暂停自动续跑。历史中未结束的命令显示为中断，实时任务按相同编号覆盖保存记录。

回合结算与后台唤醒共用同一入口，检查空闲、允许续跑、未关闭及队列非空。内部通知只带命令编号和状态，不伪装成用户消息。用户停止前台会暂停自动续跑，后台命令继续；下一条用户消息恢复处理。单独停止后台命令不触发模型续跑。归档和退出先禁止续跑，再停止后台并等待保存、回收。

实现参考 [Codex 的命令工具](https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs)、[会话进程管理](https://github.com/openai/codex/blob/main/codex-rs/core/src/unified_exec/process_manager.rs)和 [Claude Code 的后台命令](https://code.claude.com/docs/en/interactive-mode.md#background-bash-commands)。本项目只采用并行、任务身份、有限等待和完成通知这些行为，不引入交互终端、远程执行或重启接管。

## 当前接受的行为限制

| 行为 | 适用条件与影响 | 源码入口 |
| --- | --- | --- |
| 压缩依靠模型遵守摘要指令，不单独拒绝工具调用 | 摘要请求保留工具定义。回复未截断且正文非空时，保存正文，不执行回复中的工具调用。正文用于替换此前的有效模型上下文，完整历史仍保存在日志中。 | [compaction.rs](../crates/agent/src/compaction.rs) |
| 流解码失败时不保留已收到的 Token 用量 | 解码失败的请求保留错误与计时，但不记录解码器已收到的用量，累计统计可能低于实际消耗。成功解码后才发生的续接校验失败仍保留用量；未上报用量保持未知。 | [provider.rs](../crates/model/src/openai/provider.rs)、[Chat 解码器](../crates/model/src/openai/chat/stream.rs)、[Responses 解码器](../crates/model/src/openai/responses/stream.rs) |

## 源码导航

| 修改场景 | 源码入口 |
| --- | --- |
| 桌面 RPC 与同步 | [后端 rpc.rs](../crates/app/src/desktop/rpc.rs)、[rpcClient.ts](../apps/desktop/src/rpcClient.ts)、[sync.ts](../apps/desktop/src/sync.ts) |
| 开始执行、停止与引导输入 | [conversation.rs](../crates/runtime/src/conversation.rs)、[runner.rs](../crates/runtime/src/runner.rs)、[inbox.rs](../crates/agent/src/agent/inbox.rs) |
| 工具名称与参数检查、并发调度 | [registry.rs](../crates/agent/src/tools/registry.rs)、[dispatch.rs](../crates/agent/src/agent/dispatch.rs) |
| 文件读取、命令执行与交互提问 | [read.rs](../crates/agent/src/tools/read.rs)、[bash/mod.rs](../crates/agent/src/tools/bash/mod.rs)、[questions.rs](../crates/agent/src/agent/questions.rs) |
| 后台进程、保存与自动续跑 | [bash/tasks.rs](../crates/agent/src/tools/bash/tasks.rs)、[conversation/background.rs](../crates/runtime/src/conversation/background.rs)、[BackgroundTasks.tsx](../apps/desktop/src/components/BackgroundTasks.tsx) |
| 模型请求、失败分类与重试 | [request_execution.rs](../crates/agent/src/request_execution.rs)、[openai/provider.rs](../crates/model/src/openai/provider.rs) |
| 上下文与压缩 | [session/context.rs](../crates/agent/src/session/context.rs)、[agent/compaction.rs](../crates/agent/src/agent/compaction.rs)、[runner/compaction.rs](../crates/runtime/src/runner/compaction.rs) |
| 会话格式、追加写入与尾行修复 | [format.rs](../crates/agent/src/session/format.rs)、[manager.rs](../crates/agent/src/session/manager.rs)、[file.rs](../crates/agent/src/session/file.rs) |
| 历史、时间线与用量显示 | [history.rs](../crates/runtime/src/history.rs)、[activity.rs](../crates/app/src/desktop/app_server/activity.rs)、[timeline.ts](../apps/desktop/src/timeline.ts)、[sessionUsage.ts](../apps/desktop/src/sessionUsage.ts) |

运行流程变化时更新 `architecture.html`；职责、状态归属或源码入口变化时更新本文。
