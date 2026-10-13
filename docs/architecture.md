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

历史分页将请求使用的指令和工具放在页内定义表，请求条目只携带定义引用与各自的输出偏好。每页包含自己引用的全部定义，前端在历史转换入口还原请求详情；补读无需依赖已经加载的其他页。实时事件仍携带完整请求快照，不依赖历史页的定义表。

## 状态归属

| 数据或状态 | 维护模块 | 使用边界 |
| --- | --- | --- |
| 项目路径与分组 | `app::WorkspaceStore` | 维护项目登记与分组，不改变本机工具权限 |
| 任务运行状态与取消控制 | `runtime::Conversation` | 同一任务的普通执行与独立压缩共用操作占用 |
| 尚未保存的引导输入 | 会话内的 `SteeringInbox` | 仅在进程内保存，不能从会话文件恢复 |
| 消息、工具结果、请求记录、摘要与图片快照 | `agent::SessionManager` 与会话图片存储 | 会话日志保存正式历史；摘要影响模型上下文，原历史仍保留 |
| 公开历史与任务索引 | `runtime::ThreadCatalog` 与历史转换模块 | 从会话记录生成，保留持久化模型与公开对象的边界 |
| 桌面活动事件与会话修订号 | `app::ConversationSlot` | 提供执行中的显示快照；正式历史来自会话日志 |
| 模型配置与密钥 | `model::ModelConfigManager` | 配置与密钥分文件保存；对外目录脱敏，执行读取配置快照 |
| MCP 配置与连接 | `mcp::McpManager` | 设置与执行共享同一实例，连接按工作目录复用 |
| 草稿、视图偏好与界面显示 | React 草稿存储、`AppStore` 与 `SessionStore` | 草稿与偏好在前端保存，执行显示来自后端历史和事件 |

未提交的问答按任务和问题保存在 `SessionStore` 内存中，切换任务时保留，提交成功或后端确认问题结束后清理。页面刷新后重新填写。

`McpManager` 在回合首次模型请求前、自动压缩成功后发现工具；手动压缩缺少历史请求定义时，也通过该入口准备工具。准备时复用健康连接，为尚未建立或已确认关闭的连接重新握手，再查询工具列表。Agent 在两次发现之间持有同一份工具与连接快照。工具调用失败直接返回错误，不重连或重放；下一次准备再尝试恢复连接。

MCP 设置页读取管理器保存的连接状态与最近一次工具发现结果，不启动服务或查询远端工具列表。分类打开时，每秒继续读取“连接中”的状态；查询不重叠，连接结束或离开分类后停止刷新。只有失败状态显示“重试连接”，用户点击后重建当前项目的连接并发现工具。应用退出时先停止并等待会话结束，再关闭 MCP 连接。

实际文件位置、备份与维护方式见[安装说明的数据目录章节](INSTALL.md#6-数据存储与目录维护)。

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
