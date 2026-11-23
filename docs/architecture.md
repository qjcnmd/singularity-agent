# 架构说明

Singularity 是面向 Windows 本地的单用户桌面 AI 编程工作台，采用 Electron 前端 + Rust `stdio` JSON-RPC 后端进程架构。源码索引见 [directory.html](../directory.html)，完整执行与数据流见 [architecture.html](../architecture.html)。

## 进程与通信

- **Electron 主进程**（[main.ts](../apps/desktop/desktop/main.ts)、[backend.ts](../apps/desktop/desktop/backend.ts)、[preload.cts](../apps/desktop/desktop/preload.cts)）：维护单实例锁，通过 `SINGULARITY_HOME` 或 `~/.singularity` 派生 Chromium `userData` 目录并持久化启动主题颜色；托管 `singularity.exe` 子进程并在 `'close'` 事件上报异常退出与完整 stderr 诊断，通过 `singularity://app/` 加载静态资源，仅在受信任主框架转发 `rpc`、`connect` 与主题写入。
- **Rust 后端**（[main.rs](../crates/app/src/main.rs)、[logging.rs](../crates/app/src/logging.rs)、[transport.rs](../crates/app/src/desktop/transport.rs)、[rpc.rs](../crates/app/src/desktop/rpc.rs)）：持有数据目录文件锁并初始化 `tracing` 滚动诊断日志（写入 `~/.singularity/logs/singularity.log`）；stdin 逐行接收 JSON-RPC 请求并派发至独立异步任务；stdout 由独立有界写入协程（`mpsc::channel(512)`）按序输出响应与 `StreamEnvelope` 事件帧，后端事件通道在非聚焦会话下过滤高频流式进度、背压（`< 512`）仅丢弃可覆写的 `ToolOutput` 与已登记 `Running` 后台快照（保留聚焦 `TextDelta` 与首次 `Running`），真饱和（`capacity <= 1`）时预留末槽发送合并 `Ready` 重同步信号；停机时先取消查询、收齐已接受 RPC 并停机全部会话，再以非阻塞 `try_send` 排空剩余事件并保留真实 stdout 写入错误，退出前调用 `runtime.shutdown_background()` 避免 stdin 阻塞导致目录锁滞留。

## Rust Crates 分层与职责

| Crate | 核心职责与入口 |
| --- | --- |
| `app_core` | 产品身份与环境常量（`identity.rs`）、数据目录定位（`user_home.rs`）、规范工作区路径（`workspace.rs`）、分层 `AGENTS.md` 加载（`project_instructions.rs`）、`SKILL.md` 发现与规范路径去重（`skills.rs`）、原子写盘与中英文启发式 Token 估算（`lib.rs`） |
| `protocol` | 共享 RPC 方法枚举（含 `session.focus`）、请求/响应 DTO、历史条目标识（`params.rs`）、运行期流事件与 TypeScript 契约导出示例（`export_types.rs`） |
| `model` | 协议无关的模型客户端入口 `ModelClient`（`client.rs`）、`config.json` 读写与脱敏（`ModelConfigManager` + `context_window`）、`ProviderConfig` 与 `SelectedModel` 解析（`selection.rs`，拒绝 `max_output_tokens >= max_context_tokens`）、纯 LLM 语义消息模型 `Message`（不含前端展示字段）、全局 `reqwest` 连接池（`provider_client`）、SSE 切帧与终止帧校验（`transport/stream.rs`，收到 ≥1 条有效 SSE 事件后未封口或缺失终态提前 EOF 归类为 `Temporary` 可重试）、Messages（Anthropic 兼容 / DeepSeek：按 SHA-256 链式 `prefix_digest` 按需剥离失效签名思考块，并在 `prefix_digest` 算完后向末条消息最后一个可缓存块写入滚动 `cache_control` 断点；正常自适应无思考工具轮仍原样发送所选思考配置，仅当 `prefix_digest` 失配剥离了尾部 `tool_use` 助手消息的签名思考且后接 `tool_result` 时单次省略 `thinking`/`output_config`）与 OpenAI Responses 双协议编解码（含 `prompt_cache_key`、加密续接回传与多段 `summary_text` 自动分隔） |
| `mcp` | 轻量错误 `McpError` 与结果/发现类型（`result.rs`）、`mcp.json` 校验与按 URL/env/header/stdio `args`（`--flag=VAL`、`--flag VAL`、`KEY=VAL` 与内嵌 URL）凭据单遍脱敏（支持驼峰、复数、解码口令及 `auth`/`bearer`/`jwt`/`pass`/`pat` 关键字，排除 `pwd`，`config.rs`）、`McpManager` 连接池（`lib.rs`，按 `(directory, server)` 复用 `ConnectionCell`，持配置锁读文件并登记连接槽位，重连先锁新槽位再入表，`skip` 过滤禁用项，`tools()` 失败时关闭失效连接）、HTTP 客户端禁用重定向（`Policy::none()`）、工具调用不自动重放与原始描述/参数 Schema 保留（`client.rs`） |
| `agent` | `Agent` 主循环（`agent/mod.rs`，无待处理用户插话时连续 3 步完全相同工具调用均失败则以 `RepeatedToolFailure` 自动停止）、请求退避重试（`request_execution.rs`，最多 3 次、`Retry-After` 上限 60s、仅生成请求发 `MessageDiscarded`）、模块化上下文拼装与预览（`modules.rs`，所有 MCP 统一按工具粒度延迟加载，仅注摘要并经 `mcp_schema`/`mcp_call` 代理按需访问）、最多 8 并发工具调度及提问屏障（`dispatch.rs`）、上下文估算与压缩（`session/context.rs` 与 `agent/compaction.rs`：`latest_reply_usage` 跨越 `OperationStarted` 持续沿用直至遇到 `Compaction` 检查点，估算时排除异源提供方/模型的 `Thinking` 块；切到新模型且当前历史已达新模型 90% 时先用上一路由模型按新模型 15% 保留预算压到阈值以下，原模型不可用或预压失败则直接终止本轮；常规 90% 压力与手动压缩同口径保留最近 15% 工具配对平衡尾部，经 `plan_compaction_step` 在单次前缀回放可容纳时保留热前缀与完整指令/工具定义，超限时回退为头窗口分段压缩加单步请求内折叠——剥思考、图片占位、清工具参数并按比例截断文本——最多 8 轮且每次提交前复核 `can_commit()`/取消状态，未收敛显式报 `InvalidCompaction`；自动压缩非取消/非写入故障记录警告后继续当前回合；提供方超窗时至少保留最后一个完整平衡单元并压缩其之前的前缀后重发一次；生成与压缩请求均携带 `prompt_cache_key: Some(session_id)`）、JSONL v1 会话日志（`SessionManager::data()` 显式只读访问、`SessionEntry::Message` 侧边独立保存 `MessageDisplay` 展示元数据）及尾行修复（`session/`）、内置工具（`bash` 进程树、`OnceLock` shell 缓存与 Windows 默认 `PYTHONUTF8=1`、`50KB`/2000 行磁盘溢写与 `100KB` 内存尾部上限及 `will_write_disk` 按需卸载、`read` NUL 二进制与 `limit=0` 拦截、`edit`/`write` 共用路径锁与保留 BOM/CRLF 原子写盘、20 MiB/16384px 图像校验） |
| `app` | 工作台核心 `AppServer`（`desktop/app_server.rs`）、独立事件发布中心 `StreamHub`（`desktop/stream_hub.rs`，维护全局 `revision`、按请求序号 `FocusState` epoch 防止并发焦点回退的高频进度过滤、按会话追踪已登记运行中后台任务 ID 以确保首次 `Running` 与聚焦 `TextDelta` 绕过 `< 512` 背压丢弃，仅真饱和 `capacity <= 1` 时预留末槽发送合并 `Ready` 重同步信号）、右侧只读文件读取（`desktop/file_view.rs`）、`tracing` 按大小滚动诊断日志（`logging.rs`，先暂存再轮转防 Windows 锁文件清空归档）、单会话运行实体 `Conversation` / `ConversationView`（`runtime/conversation.rs`，`has_work` / `can_evict` 在 `auto_resume == false` 时忽略被动 `Background` 队列项）、历史回放与累计用量聚合（`runtime/history.rs`，`context_usage` 跨越无新回合的 `A -> B -> A` 切模型往返保留同模型读数）与会话文件目录 `ThreadCatalog`（`runtime/thread_catalog.rs`） |

## 关键状态、锁序与缓存

- **MCP 连接交接**：配置锁覆盖读取配置和登记槽位，连接表的锁只覆盖内存操作。连接关闭和网络等待在锁外。退出时先等待已接收的 RPC 和会话结束，再收齐待关闭的 MCP 连接。
- **切模型预压缩**：用旧模型上次的推理档位解析客户端。档位已移除时，配置错误随本轮失败返回。
- **单向锁序**：`AppServer` 严格遵守 `lifecycle -> publication -> sessions -> 会话状态`（`view -> state`）加锁顺序，不在锁内执行磁盘全量扫描、图片解码（`UserInput::from_uploads` 移至锁外）或长耗时网络 IO；`inspect_context` 在 `lifecycle` 锁内调用 `begin_context_preview` 返回 `ContextPreview::{Skip, Busy, Run(PreviewGuard)}` 串行化并发调用（`Busy` 时通过 `Conversation::changed` 的 `wait_context_preview` 挂起等待在途预览完成后重检，`PreviewGuard` 在 `Drop` 时自动清理并唤醒）。
- **会话内存上限**：`AppServer::sessions` 使用 `SessionCache` 限制最多缓存 `16` 个空闲已打开会话，插入新会话前先按 LRU 淘汰最久未访问且 `can_evict() == true` 的旧空闲会话（防止新会话自淘汰）；`has_work()` 与 `can_evict()` 在 `auto_resume == false` 时忽略仅剩被动 `Background` 通知的队列项；重新打开时 `ConversationView` 以当前全局 `revision` 初始化事件序号。
- **两段锁删除与停机**：`delete_session` 先在生命周期锁内标记 `closing` 并创建 `DeleteGuard`，锁外等待运行中操作与后台任务停止并清理磁盘目录；未打开的空闲会话直接在单次 `spawn_blocking` 中删盘。`shutdown` 通过 `JoinSet` 并发停止全部活跃会话后关闭 MCP 连接。
- **前端状态单一来源**（`apps/desktop/src/`）：`SessionStore` / `AppStore` 持有与后端 `revision` 对齐的会话、目录和流式增量状态，会话中途收到 `ready` 原地刷新而不卸载 `<Conversation>`，启动报错后收到 `ready` 或 `app_changed` 恢复同步并清理非活跃会话陈旧 `backgroundTasks`；`ContextPanel` 在 `context === null && phase === 'idle'` 时按需请求 `session.context`（回到 `idle` 或切模型 `selection` 时重拉）；`McpSettings` 仅在存在 `connecting` 服务器时按 1s 轮询 `mcp.list`；`DraftStore` 缓存已打开的 `IDBDatabase` 实例并按会话 key 250ms 防抖写入 IndexedDB（含未发送的助手回复文本批注，通过 `annotations.ts` 与 CSS Custom Highlight 还原高亮），在发送/删除/`pagehide` 时同步建事务刷盘；`viewPersistence.ts` 保存视图状态与全局强调色（`accentColor`，兼容读取旧 `assistantBubble`）。

## 运行边界

1. **单实例本地桌面应用**：面向 Windows 本地单用户，数据集中保存在单一 `~/.singularity`（或 `SINGULARITY_HOME`）下并加独占目录锁；不处理多实例并发写入同一数据目录，也不扩展跨平台服务端或网络接入。
2. **项目归类而非强沙箱**：工作区登记只决定 `AGENTS.md`、项目 Skills、MCP 目录键和工具默认工作目录，不对文件或命令施加额外内核级路径限制（由用户自行判断操作边界）。
3. **日志格式固定 v1**：不编写向后兼容的历史日志迁移逻辑；`0.1.0` 阶段变更日志格式时直接清理本地会话目录验证。
4. **本地直接交互模式**：不包含后台常驻守护进程、定时任务、外部 IM 网关或自动评测基础设施；扩展能力见 [已确认的取舍](decisions.md)。
