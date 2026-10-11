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
