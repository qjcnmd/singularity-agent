# 架构说明

Singularity 是面向 Windows 本地的单用户桌面 AI 编程工作台，采用 Electron 前端 + Rust `stdio` JSON-RPC 后端进程架构。源码索引见 [directory.html](../directory.html)，完整执行与数据流见 [architecture.html](../architecture.html)。

## 进程与通信

- **Electron 主进程**（[main.ts](../apps/desktop/desktop/main.ts)、[backend.ts](../apps/desktop/desktop/backend.ts)、[preload.cts](../apps/desktop/desktop/preload.cts)）：维护单实例锁，通过 `SINGULARITY_HOME` 或 `~/.singularity` 派生 Chromium `userData` 目录并持久化启动主题颜色；托管 `singularity.exe` 子进程，通过 `singularity://app/` 加载静态资源，仅在受信任主框架转发 `rpc`、`connect` 与主题写入。
- **Rust 后端**（[main.rs](../crates/app/src/main.rs)、[logging.rs](../crates/app/src/logging.rs)、[transport.rs](../crates/app/src/desktop/transport.rs)、[rpc.rs](../crates/app/src/desktop/rpc.rs)）：持有数据目录文件锁并初始化 `tracing` 滚动诊断日志（写入 `~/.singularity/logs/singularity.log`）；stdin 逐行接收 JSON-RPC 请求并派发至独立异步任务；stdout 由独立有界写入协程（`mpsc::channel(512)`）按序输出响应与 `StreamEnvelope` 事件帧，后端事件通道保留控制帧槽位并在非聚焦会话下过滤高频流式进度、通道饱和时预留末槽发送合并 `Ready` 重同步信号；停机时先取消查询、收齐已接受 RPC 并停机全部会话，再以非阻塞 `try_send` 排空剩余事件并保留真实 stdout 写入错误，退出前调用 `runtime.shutdown_background()` 避免 stdin 阻塞导致目录锁滞留。

## Rust Crates 分层与职责

| Crate | 核心职责与入口 |
| --- | --- |
| `app_core` | 产品身份与环境常量（`identity.rs`）、数据目录定位（`user_home.rs`）、规范工作区路径（`workspace.rs`）、分层 `AGENTS.md` 加载（`project_instructions.rs`）、`SKILL.md` 发现与规范路径去重（`skills.rs`）、原子写盘与中英文启发式 Token 估算（`lib.rs`） |
| `protocol` | 共享 RPC 方法枚举（含 `session.focus`）、请求/响应 DTO、历史条目标识（`params.rs`）、运行期流事件与 TypeScript 契约导出示例（`export_types.rs`） |
| `model` | 协议无关的模型客户端入口 `ModelClient`（`client.rs`）、`config.json` 读写与脱敏（`ModelConfigManager` + `context_window`）、`ProviderConfig` 与 `SelectedModel` 解析（`selection.rs`）、纯 LLM 语义消息模型 `Message`（不含前端展示字段）、全局 `reqwest` 连接池（`provider_client`）、SSE 切帧与终止帧校验（`transport/stream.rs`）、Messages（Anthropic 兼容 / DeepSeek）与 OpenAI Responses 双协议编解码（含思考签名与加密续接回传） |
| `mcp` | 结构化错误 `McpError`（`error.rs`）、`mcp.json` 校验与按 URL/env/header 凭据单遍脱敏（支持驼峰、复数与解码口令，`config.rs`）、`McpManager` 连接池（`lib.rs`，按 `(directory, server)` 复用 `ConnectionCell`、`ConnectionPool` 代际号 `generation` 防并发配置覆盖、重连先锁槽位再入表、`skip` 过滤禁用项、`tools()` 失败清理并关闭失效连接、锁外异步关闭淘汰连接并在 `shutdown()` 排空）、工具调用不自动重放与原始描述/参数 Schema 保留（`client.rs`） |
| `agent` | `Agent` 主循环（`agent/mod.rs`）、请求退避重试（`request_execution.rs`，最多 3 次、`Retry-After` 上限 60s、仅生成请求发 `MessageDiscarded`）、模块化上下文拼装与预览（`modules.rs`，所有 MCP 统一按工具粒度延迟加载，仅注摘要并经 `mcp_schema`/`mcp_call` 代理按需访问）、最多 8 并发工具调度及提问屏障（`dispatch.rs`）、纯内存消息压缩与原子组分块（`compaction.rs`）、零模型调用旧工具输出微压缩（80% 软阈值锚定水位线 `ToolOutputsPruned`）与自动/手动上下文轮换（`context_rotation.rs`，优先沿用最近实际 `input_tokens`，超窗时按剥离图片/压缩工具→压缩助手→分块滚动阶梯降级自救并校验每步 Token 真正缩减）、JSONL v1 会话日志（`SessionManager::data()` 显式只读访问、`SessionEntry::Message` 侧边独立保存 `MessageDisplay` 展示元数据）及尾行修复（`session/`）、内置工具（`bash` 进程树与 `OnceLock` shell 缓存、100KB 溢写与 `will_write_disk` 按需卸载、`read` NUL 二进制与 `limit=0` 拦截、`edit` / `write` 共用路径锁与原子写盘、20 MiB/16384px 图像校验） |
| `app` | 工作台核心 `AppServer`（`desktop/app_server.rs`）、独立事件发布中心 `StreamHub`（`desktop/stream_hub.rs`，维护全局 `revision`、按请求序号 `FocusState` epoch 防止并发焦点回退的高频进度过滤、高水位保护与通道饱和末槽合并 `Ready` 重同步信号）、`tracing` 按大小滚动诊断日志（`logging.rs`，先暂存再轮转防 Windows 锁文件清空归档）、单会话运行实体 `Conversation` / `ConversationView`（`runtime/conversation.rs`）、历史回放与累计用量聚合（`history.rs`）、会话文件目录 `ThreadCatalog`（`thread_catalog.rs`）与右侧只读文件读取（`file_view.rs`） |

## 关键状态、锁序与缓存

- **单向锁序**：`AppServer` 严格遵守 `lifecycle -> publication -> sessions -> 会话状态`（`view -> state`）加锁顺序，不在锁内执行磁盘全量扫描、图片解码（`UserInput::from_uploads` 移至锁外）或长耗时网络 IO；`inspect_context` 在 `lifecycle` 锁内调用 `begin_context_preview` 占槽并持有 RAII `PreviewGuard`（`Drop` 自动清理并唤醒）。
- **会话内存上限**：`AppServer::sessions` 使用 `SessionCache` 限制最多缓存 `16` 个空闲已打开会话，插入新会话前先按 LRU 淘汰最久未访问且 `can_evict() == true` 的旧空闲会话（防止新会话自淘汰）；重新打开时 `ConversationView` 以当前全局 `revision` 初始化事件序号。
- **两段锁删除与停机**：`delete_session` 先在生命周期锁内标记 `closing` 并创建 `DeleteGuard`，锁外等待运行中操作与后台任务停止并清理磁盘目录；未打开的空闲会话直接在单次 `spawn_blocking` 中删盘。`shutdown` 通过 `JoinSet` 并发停止全部活跃会话后关闭 MCP 连接。
- **前端状态单一来源**（`apps/desktop/src/`）：`SessionStore` / `AppStore` 持有与后端 `revision` 对齐的会话、目录和流式增量状态，断流/错误态收到 `ready` 或 `app_changed` 时恢复同步；`DraftStore` 按会话 key 250ms 防抖写入 IndexedDB 并在发送/删除/`pagehide` 时刷盘；`viewPersistence.ts` 保存视图状态与全局强调色（`accentColor`，兼容读取旧 `assistantBubble`）。

## 运行边界

1. **单实例本地桌面应用**：面向 Windows 本地单用户，数据集中保存在单一 `~/.singularity`（或 `SINGULARITY_HOME`）下并加独占目录锁；不处理多实例并发写入同一数据目录，也不扩展跨平台服务端或网络接入。
2. **项目归类而非强沙箱**：工作区登记只决定 `AGENTS.md`、项目 Skills、MCP 目录键和工具默认工作目录，不对文件或命令施加额外内核级路径限制（由用户自行判断操作边界）。
3. **日志格式固定 v1**：不编写向后兼容的历史日志迁移逻辑；`0.1.0` 阶段变更日志格式时直接清理本地会话目录验证。
4. **本地直接交互模式**：不包含后台常驻守护进程、定时任务、外部 IM 网关或自动评测基础设施；扩展能力见 [已确认的取舍](decisions.md)。
