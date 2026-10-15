# Singularity 架构说明

完整运行流程见根目录的 [architecture.html](../architecture.html)，支持拖动与缩放。图中展示执行顺序、条件分支、工具调用、模型请求、停止、压缩和历史恢复。

本文维护模块职责、状态归属与源码入口。产品运行边界见[宪章](constitution.md)，桌面交互细节见[工作台交互](desktop-ui.md)，本地调试命令见[开发指南](development.md)。

## 模块职责

| 模块 | 核心职责 | 源码入口 |
| --- | --- | --- |
| Electron 主进程与 preload | 窗口、托盘、原生对话框，隔离渲染进程与后端通信 | [main.ts](../apps/desktop/desktop/main.ts)、[preload.cts](../apps/desktop/desktop/preload.cts)、[backend.ts](../apps/desktop/desktop/backend.ts) |
| React 工作台 | 用户输入、草稿、任务显示与交互状态 | [app.tsx](../apps/desktop/src/app.tsx)、[sessionStore.ts](../apps/desktop/src/sessionStore.ts) |
| `app` | 后端装配、stdio RPC 适配、桌面状态投影与无交互入口 | [app_server.rs](../crates/app/src/desktop/app_server.rs)、[transport.rs](../crates/app/src/desktop/transport.rs)、[jsonl_mode.rs](../crates/app/src/jsonl_mode.rs) |
| `runtime` | 任务生命周期、操作互斥、引导输入与公开历史 | [conversation.rs](../crates/runtime/src/conversation.rs)、[runner.rs](../crates/runtime/src/runner.rs)、[thread_catalog.rs](../crates/runtime/src/thread_catalog.rs) |
| `agent` | 模型与工具循环、上下文装配、摘要压缩与会话日志追加 | [agent/mod.rs](../crates/agent/src/agent/mod.rs)、[session/mod.rs](../crates/agent/src/session/mod.rs) |
| `model` | 模型配置、Chat / Responses 编解码、HTTP / SSE 传输 | [config/manager.rs](../crates/model/src/config/manager.rs)、[openai/provider.rs](../crates/model/src/openai/provider.rs)、[transport/http.rs](../crates/model/src/transport/http.rs) |
| `mcp` | MCP 配置、按工作目录复用连接、工具发现与调用 | [lib.rs](../crates/mcp/src/lib.rs)、[client.rs](../crates/mcp/src/client.rs) |
| `core` | 数据目录解析、文件写入、项目指令与技能发现 | [user_home.rs](../crates/core/src/user_home.rs)、[project_instructions.rs](../crates/core/src/project_instructions.rs)、[skills.rs](../crates/core/src/skills.rs) |
| `protocol` | RPC 参数、结果、事件与 TypeScript 类型导出 | [rpc.rs](../crates/protocol/src/rpc.rs)、[params.rs](../crates/protocol/src/params.rs) |

工作台与 `--json` 共用 `runtime` 和 `agent`。桌面通信由 Electron 和 `app` 适配，执行规则由 `runtime` 与 `agent` 维护。协议对象、会话记录、模型消息和视图对象在各自边界转换。

模型工具定义和输出限制由 `model::types` 维护；`agent::session::request` 将它们转换为 JSONL 请求记录，读取轨迹时再转换为桌面协议对象。三种结构分别维护，JSONL 字段名不跟随桌面协议变化。

模型上下文由 `agent::session::context` 直接借用有效会话记录，消息转换与 Token 估算共用该模块的筛选与排序规则。普通请求读取全部有效对话；压缩从中选取较早历史，近期完整消息继续保留。借用在同次写者锁内结束，返回的消息与摘要材料拥有自己的数据；图片在释放锁后读取。

历史分页将请求使用的指令和工具放在页内定义表，请求条目只携带定义引用与各自的输出偏好。每页包含自己引用的全部定义，前端在历史转换入口还原请求详情；补读无需依赖已经加载的其他页。实时事件仍携带完整请求快照，不依赖历史页的定义表。

## 状态归属

| 数据或状态 | 维护模块 | 使用边界 |
| --- | --- | --- |
| 项目路径与分组 | `app::WorkspaceStore` | 维护项目登记与分组，不改变本机工具权限 |
| 任务运行状态与取消控制 | `runtime::Conversation` | 同一任务的普通执行与独立压缩共用操作占用 |
| 尚未保存的引导输入 | 会话内的 `SteeringInbox` | 仅在进程内保存，不能从会话文件恢复 |
| 消息、工具结果、固定请求定义、请求记录、摘要与图片快照 | `agent::SessionManager` 与会话图片存储 | 会话日志保存正式历史；摘要影响模型上下文，原历史仍保留 |
| 公开历史与任务索引 | `runtime::ThreadCatalog` 与历史转换模块 | 从会话记录生成，保留持久化模型与公开对象的边界 |
| 桌面活动事件与会话修订号 | `app::ConversationSlot` | 提供执行中的显示快照；正式历史来自会话日志 |
| 模型配置与密钥 | `model::ModelConfigManager` | 配置与密钥分文件保存；对外目录脱敏，执行读取配置快照 |
| MCP 配置与连接 | `mcp::McpManager` | 设置与执行共享同一实例，连接按工作目录复用 |
| 草稿、视图偏好与界面显示 | React 草稿存储、`AppStore` 与 `SessionStore` | 草稿与偏好在前端保存，执行显示来自后端历史和事件 |

未提交的问答按任务和问题保存在 `SessionStore` 内存中，切换任务时保留，提交成功或后端确认问题结束后清理。页面刷新后重新填写。

`SessionStore` 读取工作台失败时保留草稿、事件订阅和停止按钮，展示重试入口。正文缺失时，输入区读取 `liveSessions` 中当前任务的运行状态，继续响应停止和完成事件。等待重试时只更新任务运行状态，不积存正文；实际读取期间暂存事件，读取成功后回放并恢复提交。只有连接故障才停止监听。阅读位置记录历史条目及所属回合，临时输出借用邻近的已保存条目定位；读到所属回合仍找不到条目时清除失效位置。

会话首次请求前加载指令、Skills 目录和工具定义，并保存为固定前缀。后续用户输入、工具循环和重启恢复都使用保存的定义，不重新读取文件或扫描目录。普通请求按协议发送完整有效上下文，但旧前缀和旧消息不因重新组装而变化；新消息追加在尾部。

手动、阈值自动压缩和模型超限后的恢复共用同一个压缩入口。压缩沿用原定义，只把选定的较早历史送去总结，并在尾部追加总结要求；已有摘要随更新摘要的要求一起发送。近期对话不送去总结。自动阈值为上下文窗口的 90%；手动不检查阈值。保留近期完整消息后，若没有较早的对话可总结，直接结束本次压缩，不更换定义。摘要目标输出最多 8192 token，并受模型上限和窗口余量约束。普通生成与摘要共用最多 3 次请求尝试；摘要为空或截断、文件读取或持久化失败不自动重试。

摘要通过检查后读取最新指令、Skills 和工具。摘要和新定义写在同一条压缩记录中，写入成功后一起生效；失败不切换有效上下文。新上下文由新定义、摘要和近期保留对话组成。保留预算按压缩前有效对话（含旧摘要、不含固定指令和工具定义）的估算 Token 量取 10%，向上取整；向前扩展到完整消息和工具调用边界，旧摘要由新摘要替换。摘要替换较早历史，近期原文接在新摘要后；原始记录仍保留。压缩后的请求引用新定义；手动完成后等待输入，自动完成后继续原任务。模型明确拒绝上下文超限时，每次生成最多压缩并重发一次。

压缩期间后端拒绝用户停止，桌面不提供停止按钮；压缩使用独立于普通任务的请求令牌。正常退出和归档等待压缩完成或最终失败；退出时仅在压缩期间暂停原有的 10 秒强制结束计时，压缩结束后恢复。普通任务仍可停止，重试等待和工具执行响应其取消信号。

`McpManager` 在每轮准备执行连接和压缩后发现工具，复用健康连接并排序工具。本轮直接持有工具与连接，设置变更不替换正在使用的连接。模型可见的指令和工具定义只在首次加载与成功压缩后更新；恢复连接不会更新模型前缀。执行前同时检查工具在固定定义中且当前可用，不可用时返回错误，调用失败不自动重放。

MCP 设置页读取管理器保存的连接状态与最近一次工具发现结果，不启动服务或查询远端工具列表。分类打开时，每秒继续读取“连接中”的状态；查询不重叠，连接结束或离开分类后停止刷新。只有失败状态显示“重试连接”，用户点击后重建当前项目的连接并发现工具。应用退出时先停止并等待会话结束，再关闭 MCP 连接。

实际文件位置、备份与维护方式见[安装说明的数据目录章节](INSTALL.md#6-数据存储与目录维护)。

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
| 模型请求、失败分类与重试 | [request_execution.rs](../crates/agent/src/request_execution.rs)、[openai/provider.rs](../crates/model/src/openai/provider.rs) |
| 上下文与压缩 | [session/context.rs](../crates/agent/src/session/context.rs)、[agent/compaction.rs](../crates/agent/src/agent/compaction.rs)、[runner/compaction.rs](../crates/runtime/src/runner/compaction.rs) |
| 会话格式、追加写入与尾行修复 | [format.rs](../crates/agent/src/session/format.rs)、[manager.rs](../crates/agent/src/session/manager.rs)、[file.rs](../crates/agent/src/session/file.rs) |
| 历史、时间线、轨迹与用量显示 | [history.rs](../crates/runtime/src/history.rs)、[timeline.ts](../apps/desktop/src/timeline.ts)、[trajectory.ts](../apps/desktop/src/trajectory.ts)、[sessionUsage.ts](../apps/desktop/src/sessionUsage.ts) |

运行流程变化时更新 `architecture.html`；职责、状态归属或源码入口变化时更新本文。
