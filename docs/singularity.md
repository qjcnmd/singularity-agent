# Singularity 当前架构

本文只描述当前 Rust 源码。历史结构和已经移除的接口由 Git 历史保存。

## 1. 系统边界

Singularity 是本地命令行编码代理；核心合同保持平台无关，当前 Windows 发行包由四个 release binary 组成：

| Binary | 所属 crate | 职责 |
| --- | --- | --- |
| `sg` | `crates/cli` | 解析用户命令，启动 app-server，发送和渲染 stdio JSON-RPC |
| `singularity_app_server` | `crates/app-server` | 拥有 thread/turn 生命周期、AgentLoop 装配、持久化和 evaluation runner |
| `singularity-command-runner` | `crates/windows-sandbox` | elevated sandbox 中的受限命令 runner |
| `singularity-windows-sandbox-setup` | `crates/windows-sandbox` | UAC 提权后配置受限账户、ACL 和网络隔离 |

四个文件在 release 中同目录部署。`sg` 只发现同目录的 app-server；sandbox helper 也从当前 executable 的同目录或资源目录解析。缺失 helper 时关闭失败，不搜索或调用另一个 agent runtime。

生产 AgentLoop 只在当前绑定的 backend 声明 strict command sandbox 能力时可用。Windows 构建绑定 restricted-token/Job Object adapter；Linux 构建绑定 user/mount/network/PID namespace、seccomp 与 Landlock adapter。当前没有 macOS strict adapter，无法满足同一合同的平台明确返回 unavailable，不会退回本地进程执行。

## 2. Crate 边界

| Crate | 直接职责 | 关键对象 |
| --- | --- | --- |
| `core` | 公共错误码、脱敏检测、取消令牌、项目指令加载 | `CancellationToken`、`ProjectInstructions`、`ErrorCode` |
| `protocol` | stdio JSON-RPC 方法和公共传输对象 | `JsonRpcMessage`、`Thread`、`Turn`、`Item`、`TraceEvent` |
| `policy` | 权限 profile、规则优先级和 approval 决策 | `PermissionProfile`、`PolicyEngine`、`ApprovalRequest` |
| `windows-sandbox` | Codex 来源的 Windows restricted-token、Job Object、ACL、WFP 和 elevated helper 实现 | `PermissionProfile`、`ElevatedSandboxProfileCaptureRequest` |
| `sandbox` | 产品命令请求/结果模型及 Windows/Linux strict adapter | `CommandRequest`、`CommandResult`、`WindowsSandboxBackend`、`LinuxSandboxBackend` |
| `tools` | 模型可见工具注册、准入、工作区文件操作和 command adapter | `ToolBroker`、`ToolResult`、`WorkspaceTools` |
| `model` | provider 配置快照、模型对象、OpenAI-compatible HTTP adapter | `ProviderConfigSnapshot`、`ModelTurnRequest`、`OpenAiProvider` |
| `agent` | 上下文组装、模型/工具循环、approval checkpoint resume 和 completion gate | `AgentLoop`、`AgentLoopInput`、`AgentLoopResult` |
| `store` | SQLite thread/turn/item/trace/approval/artifact history and recovery state | `SessionStore`、`StartedTurn`、`CommittedTurnOutcome` |
| `evaluation` | `evaluation.task_set/v5` manifest、计划、`evaluation.result/v8` task/trial result 与 `evaluation.evidence/v3` 安全证据模型 | `EvaluationManifest`、`WorkspacePlan`、`EvaluationResult`、`EvaluationEvidence` |
| `app-server` | 协议调度、runtime 装配、跨 thread 并发、持久化和 evaluation 执行 | `AppServer` |
| `cli` | 最终用户命令和 app-server 子进程客户端 | `Command`、`AppServerClient` |

依赖方向：

```mermaid
flowchart LR
    CLI["cli"] --> Protocol["protocol"]
    CLI --> Core["core"]
    App["app-server"] --> Agent["agent"]
    App --> Eval["evaluation"]
    App --> Store["store"]
    Agent --> Model["model"]
    Agent --> Policy["policy"]
    Agent --> Tools["tools"]
    Tools --> Sandbox["sandbox"]
    Sandbox --> Win["windows-sandbox"]
    Store --> Protocol
    Protocol --> Policy
    Model --> Core
    Tools --> Core
```

CLI 不直接依赖 agent、model、tools 或 store。公共协议对象只在 `protocol` 定义，安全执行细节不进入 protocol。

### JSON-RPC 2.0 传输合同

stdio 的每一行是一个完整 JSON 值；JSONL 只负责 framing，不改变 JSON-RPC 2.0 语义。所有 envelope 都必须带 `jsonrpc: "2.0"`，并由互斥的 request、notification、success 或 error 类型表示。request id 只接受字符串或可精确表示的 JSON 整数；`null` 仅用于服务端无法关联合法请求时的 response/error id，带小数的 number 与 request `id: null` 都按 Invalid Request 处理。响应按解析后的合法 id 原样关联，error envelope 不允许省略 `id`。`METHOD_REGISTRY` 是 method 名、调用类型、params schema 和 result schema 的唯一事实源；dispatcher 与 CLI 都从该 registry 查找并校验合同。

单请求、空 batch 和 mixed batch 都受支持。batch 按输入顺序串行分发，不并行执行有副作用项；notification 项即使 method 未知或 params 非法也不产生响应，全 notification batch 不输出任何行。batch response 保持请求项的输入顺序。解析失败、无效请求、未知方法、无效参数和内部错误分别使用 `-32700`、`-32600`、`-32601`、`-32602` 和 `-32603`；合法调用触发的 runtime 状态冲突使用项目错误 `-32005`，不占用 `-32600`。标准错误不回显原始输入或内部诊断，`data` 仅允许显式脱敏内容。

## 3. 主调用链

```text
sg run <goal>
  -> AppServerClient::spawn
  -> initialize / initialized
  -> agent/capability
  -> thread/start
  -> turn/start
     -> SessionStore::create_allocated_turn_with_input_trace_and_history
     -> AppEvent::turn_started
     -> AppServer::run_agent_loop
        -> load_project_instructions(thread.cwd, thread.cwd)
        -> AgentLoop::run_with_events_and_checkpoints
           -> assemble context
           -> OpenAiProvider completion / Responses streaming
           -> ToolBroker admission
           -> WorkspaceTools execution
           -> append tool result or next model turn
           -> completion gate
     -> SessionStore::commit_turn_outcome
     -> terminal item events
     -> AppEvent::turn_completed
     -> turn/start response

turn/resume <turn-id>
  -> SessionStore::claim_suspended_turn (claim a paused or suspended turn)
  -> decode and advance the typed TurnCheckpoint resume-attempt epoch
  -> AgentLoop::resume_turn_with_events_and_checkpoints
     -> continue the same turn from the last safe boundary without rerunning an unknown tool call
  -> the same checkpoint/event and terminal-outcome path as turn/start
```

当 AgentLoop 返回 blocked approval 时，`AgentLoopResult.pending_approvals` 中每个 `PendingApprovalOccurrence` 同时拥有 request 与 opaque typed checkpoint；AppServer 只在写入 Store 前通过 `ApprovalCheckpoint::encode` 投影 serialized payload 和显式 tool-call binding。`approval/decision` 从 Store 取回 opaque payload 后，在 claim/resume 前通过同一 occurrence codec 解码；恢复调用使用 `AgentLoop::resume_pending_approval_with_events_and_checkpoints`，在批准工具的副作用前后复用同一 `ToolCallsReady`/`ToolResultsCommitted` durable checkpoint callback，Store 不读取 checkpoint 字段。该 `ApprovalCheckpoint` 是 approval continuation 的独立 fail-closed 合同，不与普通 turn 的 `TurnCheckpoint` 或 `turn_checkpoints` 表合并。

`singularity_app_server` 的 Tokio stdin owner 继续处理 protocol 请求；单请求中的 `turn/start` 和可能继续运行 AgentLoop 的 `approval/decision` 由独立 blocking request worker 使用新的 SQLite connection 执行，因此同一进程可以在 turn 或 approval continuation 运行时接收 `turn/interrupt` 和 `server/shutdown`。batch 始终在 stdin owner 按项串行处理。不同 workspace 可以并发，同一 workspace 由 Store execution guard 串行。进程最多同时接纳 16 个 request worker；stdout 使用容量 64 的控制响应队列和容量 256 的事件通知队列，由单独异步 writer 按全局 reserve order 合并两条队列写出。只有 typed `Progress` + `BestEffort` 事件在 event queue 满时可以丢弃，并以 `event/gap` 显式声明丢弃的 cursor 范围；其非阻塞 `try_send`、order reservation 与 gap commit 在同一短临界区完成，SQLite 写入不在该锁内执行。`State` 和 `Gap` 事件走可靠背压，不静默丢失，并在可能阻塞的发送前释放全局排序锁。Turn 创建与 approval checkpoint 提供显式 `TransportTraceBinding`，transport 不解析公共 JSON 推断 thread/turn；Full 成功登记 drop 后写 `EventQueueDrop`，gap 成功进入可靠队列后写 `EventGap`，frame 的 JSON、换行和 flush 全部成功后由 `spawn_blocking` 写 `WriterVisible`。这些 sample 使用同一 trusted-reopen SQLite Trace；Store/projector/writer 失败会锁存全局 execution stop 并终止 transport。worker 超限和控制队列过载遵守各自的错误或背压合同；真实 transport 断开或 write/flush 失败同样 fail closed。worker 复用同一个 active-turn cancellation registry；stdout 仍由唯一 writer 串行输出 JSONL。

每次 app-server stdio transport 启动时生成独立的 trace-session UUID；drop、gap 和 writer-visible event ID 同时绑定该 UUID 与进程内 output order。output order 只负责本进程排序，不再被误作跨进程全局身份，因此同一 Turn 在新的 CLI/app-server 进程中继续或恢复时可以从本地序号重新开始而不会覆盖或碰撞既有 SQLite Trace。

## 4. Thread、Turn 与 Continue

### Thread

`Thread` 字段为 `thread_id`、`model`、`cwd`、`status`、`sandboxMode` 和 `approvalPolicy`。后两个字段是 thread 创建时持久化的不可变安全快照：sandbox 只允许 `read-only` 或 `workspace-write`，approval 只允许 `on-request` 或 `never`，网络固定为 denied；缺省值为 `workspace-write`/`on-request`。`thread/fork` 默认继承 source 快照，显式参数才覆盖；`thread/resume`、普通 turn、continue 和 approval resume 都从 Store 读取快照。`thread/start` 把当前 CLI 工作目录规范化为绝对目录并持久化；后续 turn 始终使用该 workspace。`status` 只有 `active` 和 `archived`。

### Turn

`Turn` 字段为 `turn_id`、`thread_id`、`status`、`agent_loop_status`。公共状态映射如下：

| Agent 状态 | Turn 状态 | 含义 |
| --- | --- | --- |
| `running` | `running` | worker 正在执行 |
| `paused` | `paused` | 用户请求已在安全边界生效，保留 `TurnCheckpoint` 并释放执行 owner；可显式 `turn/resume`，不是终态 |
| `suspended` | `suspended` | owner 已退出但存在安全 `TurnCheckpoint`；可显式 `turn/resume`，不是终态 |
| `completed` | `completed` | completion gate 已接受 final answer |
| `blocked` | `blocked` | 等待 approval 或外部条件 |
| `failed` | `failed` | provider、context、tool 或 runtime 终止失败 |
| `cancel_requested` | 保持原 `running` / `blocked` | 已记录取消请求，worker 正在收敛；该行是中间状态，不是 Turn 终态 |
| `cancelled` | `interrupted` | 取消已经传播并完成 |

`event/subscribe` 的 event type 列表是 app-server 进程内共享的并发快照，不写入 Store；主请求线程更新后，已创建的 request worker 在下一次事件通知读取时可见最新列表。每次通知只在短暂读取锁内复制快照，锁不跨越事件构造或其他 I/O；已经构造或发送的事件不追溯重过滤。`turn/started` 在 AgentLoop 调用前发送；终态 item、`turn/completed` 和 matching response 在事务提交后发送。失败、blocked 或 cancelled 不伪造成功 assistant item。

Turn 一旦持久化为 `running`，事件通知、AgentLoop、approval checkpoint、cancellation monitor 或 terminal outcome 的失败都会在同一个请求编排路径立即尝试 typed `failed` terminalization；失败原因只投影稳定的 stage/cause 分类，不把 Store、SQLite 或 workspace 原文带到公共错误。终态提交前，编排路径停止并在有界窗口内收敛 cancellation monitor，再以原子冻结的 typed outcome 区分 `InfrastructureFailure` 与 `UserCancellation`；monitor 无法在窗口内收敛时按基础设施失败处理。若并发路径已经安全提交 `blocked`、`completed`、`failed`、`interrupted` 或 `cancelled`，补偿会保留该状态；补偿本身失败时同时返回原始 stage/cause 与脱敏的 `store`、`state_changed` 或 `event_notification` cleanup 分类。

`turn/start` 在创建 Turn 前取得基于 Store 文件和 canonical workspace 的跨进程 `WorkspaceExecutionGuard`，并持有到 AgentLoop outcome 提交完成；SQLite 的同一 `BEGIN IMMEDIATE` 事务还会拒绝该 workspace 已存在任何非终态 Turn。OS 文件锁区分仍存活的 owner，SQLite 约束保留持久状态，因此同一 Store 中共享一个 workspace 的不同 logical thread 也不会并发修改同一文件树；不同 workspace 不被全局串行化。`thread/archive` 和 `thread/delete` 仍只拒绝目标 thread 自身存在非终态 Turn。

### Continue 与交互式 Turn

`sg continue` 先调用 `thread/resume`，再创建一个新的 `turn/start`。app-server 从 SQLite 读取最多 64 个已完成历史 turn，只投影成按 turn/item sequence 排序的 user/assistant conversation message。当前 turn 不会重复进入 history。

`turn/input` 接受调用方提供的 `inputId`、`delivery` 和非空 `input`，只允许向非终态 Turn 写入新消息。`inputId` 是幂等键：同一 Turn、delivery 和内容的重复请求即使在 Turn 随后终态化后也返回既有结果，换绑 Turn、delivery 或内容则 fail closed；终态 Turn 的新 `inputId` 仍被拒绝。每批内容先作为真实 `ItemKind::UserMessage` 按该 Turn 的 item sequence 持久化；`turn_inputs` 只保存 `inputId`、所引用 item、`steer`/`follow_up` 和 pending/consumed 消费关系，不复制消息正文，也不是第二消息事实源。

`steer` 在下一个完整 checkpoint 边界可见，不改变已经发出的 `ModelTurnRequest`；`follow_up` 只在当前响应已经完整提交或即将进入 finalization-only 时可见。多个 eligible input 按 item sequence 稳定消费，并在同一 SQLite 事务中把消费关系改为 consumed、写入已经包含这些 user message 的新 `TurnCheckpoint`；输入使旧 plan、verification、repair 和 final-review 决策失效，下一模型请求必须先重新规划。若 `steer` 在线性化工具执行前到达，`begin_tool_executions_at_checkpoint` 不登记执行 owner，AgentLoop 为每个未执行的结构化调用追加 typed `ToolResult`，其 `error_code` 为 `not_executed_due_to_user_input`，再处理新 user message；若执行 owner 已先登记，输入等待完整 `ToolResult` 的安全边界，不能取消、猜测或重试已经开始的副作用。

`turn/pause` 与取消及 owner 丢失不同：终态 Turn 拒绝该请求，已经 `paused` 的 Turn 幂等保持；`suspended` Turn 因没有活动 owner，可立即转为 `paused`；其他非终态 Turn 只持久化 pause request。存在活动 AgentLoop 时，它到下一个安全 checkpoint 边界后才在同一事务中保存 checkpoint、清除请求并转为 `paused`。若同一边界也有 eligible input，Store 原子提交其消费、新 checkpoint 和 `paused` 状态。`Paused` 保留用户意图且不生成 `Interrupted`；`Suspended` 表示进程 owner 丢失后存在安全恢复边界；`Interrupted` 是取消已经收敛或没有安全恢复边界的终态。

`turn/resume` 只接受 `paused` 或 `suspended` Turn。取得同 workspace execution guard 后，Store 在一个 `BEGIN IMMEDIATE` 事务中完成单 owner claim并把同一 Turn 改回 `running`；AppServer 随后解码并校验持久化的 typed `TurnCheckpoint`，递增、保存 `resume_attempt` epoch，再从 checkpoint 的完整 message/tool-result 状态继续，不创建新 Turn，也不合成“continue”用户消息。claim 后任一失败都会释放该 claim：尚无在途副作用时回到 `suspended`，已登记为 `running` 的工具则先归约为 `unknown` 再悬挂，避免留下无 owner 的 `running` Turn。若该 Turn 存在 `unknown` tool execution，claim 直接拒绝，既不调用 provider，也不消费排队输入；系统不能猜测外部副作用结果或自动重新执行该工具。

AgentLoop 在完整 model response、工具执行前后和 model request 前发布 typed checkpoint 边界；Store 的终态提交在同一个写事务中重新检查 `pause_requested` 和所有 pending `turn_inputs`。只要任一项存在，`Completed`、`Failed` 或 `Interrupted` 提交就返回 `TurnBoundaryPending` 且不产生终态副作用；AppServer 随即从最后一个 durable checkpoint 原子消费该边界并继续同一 Turn。因而晚到 input/pause 与 finalization 的先后由 SQLite 写事务决定，不会把已接受但未消费的用户输入静默越过，也不会把该竞争误归约成失败终态。

Approval decision 使用同一 boundary gate。Allow 只有在同一事务确认没有更早的 steer/pause 时才可把待执行工具标记为 executing；边界已经存在或在事务前竞争到达时，AppServer 重新读取并把 ApprovalCheckpoint 原子移交为普通 TurnCheckpoint。Deny 在没有 interactive boundary 时保持既有 failed 终态；若已有 steer、follow_up 或 pause，则记录 deny decision 但不 terminalize，追加 model-visible `approval_denied`/cancelled ToolResult，删除旧 pending execution，按 item sequence 消费真实 UserMessage，并把同一 Turn 置为 `running` 或 `paused`。旧工具在两种 handoff 中都不会执行。

## 5. 项目指令与上下文

普通 Thread runtime 不向上寻找 `.git`：Thread create/fork 先从输入路径逐组件 `nofollow` 绑定 `WorkspaceTools` 根 capability，再持久化与该对象身份一致的 canonical display path；不会先跟随 symlink/junction 后把外部目标当成 workspace。每次 turn 和允许执行的 approval continuation 都在写入 Running turn 或记录 allow decision 前，从持久化路径完成一次根 capability 绑定，并把同一个 `WorkspaceTools` 实例贯穿项目指令、AgentLoop、文件工具和 command cwd；绑定失败不产生新的非终态 turn 或已消费的 continuation。AppServer 使用该绑定的 `Thread.cwd` 同时作为 workspace root 与 cwd，调用显式边界的 `core::load_project_instructions(thread.cwd, thread.cwd)`。core 的 `load_project_instructions_from_cwd` 仅保留给有界的独立调用；显式 root→cwd API 按 root 到 cwd 的顺序读取每层的一个项目指令文件：若存在 `AGENTS.override.md` 则选择它，否则选择 `AGENTS.md`：

- 单文件最大 32 KiB，总计最大 64 KiB。
- canonical workspace 通过文件系统根 capability 逐分量 `nofollow` 打开，root→cwd 的每层目录都由父目录句柄相对打开；路径解析后插入的 symlink/junction/reparse point 不会成为新的 ambient 逃逸。
- 每个候选文件以目录 capability 相对 `nofollow` 打开，文件类型、hard-link count、长度和正文都从同一个已打开句柄取得；symlink/junction、非普通文件、多 hard-link 对象、I/O 失败、非法 UTF-8 或超限都关闭失败。
- 文件路径在打开后即使被替换，本轮仍只读取已经验证的原对象；来源摘要和 aggregate digest 均基于该次有界读取的实际字节。
- 合并结果作为 developer message 注入，不修改 user goal；只把合并后的正文发送给模型。
- 内部 `ProjectInstructions` 同时保存 workspace-relative source path、每个文件的 SHA-256 和按合并正文及来源顺序计算的 aggregate SHA-256。`AgentLoopInput` 在本轮固定该 aggregate digest；若产生 pending approval，内部 checkpoint 保存同一 digest，resume 只接受重新加载的同一 bounded snapshot，文件、来源顺序或 override 选择变化会在执行批准工具前以稳定错误 fail closed；绝对路径、原始 source metadata 与原始正文不进入普通 trace、CLI 或模型工具 payload。

`AgentLoopInput` 包含 thread/turn 标识、user input、model preference、turn 上限、项目指令、历史、interrupt 标志和 approval grants；默认最大模型回合数为 16，调用方仍可逐 turn 配置。模型请求只保留本次 provider 调用所需的 `request_id`，工具请求只保留 tool call 标识、名称和原始参数；运行状态不再复制 run/session/task/phase/action 占位字段。AgentLoop 在每次 run 和 approval resume 前调用 `Provider::negotiate_tool_capabilities`，按 effective model 返回的 contract 建立 request，并使用 strict tool schema、并行工具调用能力、每请求最大工具定义数和 context/output 上限。对同一 `ProviderConfigSnapshot` 与 effective model，成功 negotiation 同时可命中进程内快照和 app-server 状态目录相邻的版本化持久 cache；cache key 绑定 provider、规范化 endpoint 的 SHA-256、effective model、最终协议、adapter/probe contract version 以及 context/output limits，endpoint 原文和凭证永不写入 key、record、lock 名或 fingerprint。持久 record 只保留已完整 probe 且本地验证过的 protocol/profile 与 `ProviderProtocolContract`；schema、adapter/probe version、协议、模型、endpoint、limits 不匹配、TTL 过期、损坏、未知字段或非法 contract 都是 miss。锁采用由完整安全 key 摘要命名的 per-key 文件锁，网络 probe 期间保持该 key lock；global cache-file lock 只覆盖短时 read/replace，旧的未占用 key-lock 文件在 global lock 内有界清理。等待检查 cancellation/deadline，owner 取得锁后重新读取，写入使用同目录唯一临时文件、flush/sync 和平台原子 replace。probe 总 deadline、取消、写入失败和 stable capability rejection 都 fail closed；stable rejection 同时失效进程内 entry、持久 record，并以 tombstone 防止旧值回流。cache hit 不产生 probe/attempt/usage/latency 计量。`ProviderRuntimeFingerprint` 对外分离 `provider_fingerprint`、`model_fingerprint` 与可选 `negotiation_fingerprint`，只来自这些真实 runtime 边界。OpenAI-compatible probe 依次验证产品上限内的 Direct tool definitions、Agent 实际发送的 developer/user 角色、strict schema、parallel/single 调用，以及 assistant tool calls、逐调用 tool result 与下一模型回合原生调用组成的完整历史。若 probe 响应出现 reasoning content，adapter 还必须通过固定控制证明工具回合已关闭该模式；控制不支持、未生效或后续真实工具响应再次出现 reasoning content 都 typed fail closed，不保存或回放私有 reasoning。Direct 是唯一工具定义模式；容量不足直接 typed fail closed，Agent 不隐式改写、隐藏或替换工具。真实业务参数始终在本地信任边界接受完整 `ToolSpec` validation，未验证的 system message 与 JSON mode 保持关闭。当前保守估算按每 4 个 ASCII 字符约 1 token、每个非 ASCII 字符 1 token，并另计消息 framing、当回合实际可见工具 schema、developer 指令、固定开销和输出预算；工具视图只按当前 Direct 工具集合计入预算。它用于关闭失败的容量门禁，不声称等同 provider tokenizer。当前输入不能容纳时直接返回 context overflow，而不是截断任务含义；历史只按完整的 user/assistant 对保留，并保持原始对话顺序。`ContextBundle` 只保留消息、包含/排除项和真实预算；最终 AgentLoop trace 记录脱敏后的包含/排除 item ID、预算明细和普通工作回合上限，不记录消息内容。`model_turn_limit` 是该 trace-only 诊断字段：它只表示普通工作回合预算，不包含仅在端点额外允许的一次 terminal finalization；若 `finalization_ready()` 在最后一个允许的工作响应处理后才成立，AgentLoop 才允许恰好一次超出该预算的 finalization-only provider 调用，较早进入 finalization-only 时的格式纠正重试则消费尚未使用的预算，`model_turns`、usage 和 provider attempts 按实际 provider 调用累计，否则仍按 max turns fail closed。

`ProviderProtocolContract` 将 native structured tools、strict schema、`supports_parallel_tool_calls`、每请求工具定义容量、required tool choice 和 reasoning/history 约束作为独立能力。Provider 只声明自身明确验证过的能力；Agent 的本地 `ToolChoicePolicy.max_tool_calls` 仍按并行能力选择 8 或 1。每次真实 provider attempt 保存开始/结束墙钟、send-to-headers、TTFT、retry/backoff、usage、状态与 model-turn parent；capability-cache Hit/Miss 在实际查找边界记录独立 `observed_at_unix_ms`，因此取消发生在 PromptAssembly 之前时仍可作为 parentless negotiation evidence 挂到 Turn，而不会伪造 Prompt parent 或当前时间。产品只接受 Direct structured tools，定义数超过协商容量时 typed fail closed，不自动切换或隐藏工具。普通工作回合使用 `Auto` 并投影真实工具；当 verification 已通过且 plan 已完成时，AgentLoop 从同一状态派生 finalization-only 请求，使用 `None` 且不投影工具。该请求必须返回绑定当前 workspace revision、真实 change digest 和已满足 command scope digests 的 strict typed Final Review JSON；provider stream 在本地完整缓冲并通过一致性与 verdict 校验后，只把 `Accept.final_answer` 投影给客户端。无效 envelope、`Reject`、`Repair`、取消或流失败都不泄露原始 JSON 或中间文本；strict review 内容解析失败时，原响应同样不向客户端投影，只能在剩余 model-turn budget 内进入无工具的有界格式纠正。completion、plan、verification 与 review gate 仍由 Agent 本地决定，不把状态机责任转嫁给 provider。

## 6. AgentLoop

生产 turn 与 approval resume 通过 `AgentLoop::run_with_events` / `AgentLoop::resume_pending_approval_with_events` 执行；两者共享以下真实步骤：

1. 组装 developer、history 和当前 user message。
2. 协商 provider tool capabilities，按返回 contract 构造 `ModelTurnRequest` 和真实 Direct `ToolSpec`。定义数超过已协商容量时直接拒绝，不隐式切换或隐藏工具。模型输入 schema 使用可移植 JSON Schema 子集；strict 只在全部当前 schema 满足本地 strict 检查且 provider 已声明支持时发送。每个 response 的工具调用数量由 contract 和本地 `max_tool_calls` 共同约束；只有彼此独立的只读调用才允许并行，mutation、command、plan、approval-sensitive 或依赖前序结果的调用必须按序提交。
3. 调用 provider，并在协商前、等待期间和返回后检查 `CancellationToken`；typed cancellation 与 token cancellation 都归约为 `Cancelled`。
4. adapter 先按协商上限和本次 `request.tools` 的实际名称验证完整 response；超出请求上限、隐藏工具、名称/ID/JSON envelope 非法时不选择、不规范化也不执行任何调用。AgentLoop 不针对 provider 错误码重试；typed provider failure 保留原始因果并结束当前 run。
5. AgentLoop 对 response 中全部 Direct tool calls 执行整批 preflight：验证工具可见性、`ToolSpec` model/executable input contract、profile binding、workspace/protected-path 边界和 `PolicyEngine` 的 allow/deny/ask。approval grant 先在临时集合中匹配，只有整批获准执行后才提交消费；任一成员非法时不执行合法子集。模型不可见的名称或外层 envelope 不会被解包、规范化或转成另一种工具调用。
6. 多调用批次只有在全部工具的 execution mode 都是 `parallel_read` 且全部 allow/approved 时才并发执行；结果按原调用顺序回传。任何 preflight rejection、exclusive 或 ask 成员使整批零执行，不执行合法子集。每个拒绝结果都保留顶层自身稳定错误和既有 `content` validation detail，并在 `content` 中明确 `batch_executed=false`、`call_executed=false`、当前 execution mode 或 preflight 类别、首个触发工具/错误/类别及下一步；模型必须先修正 preflight 失败，再把 mutation、command、plan 或 approval-sensitive 调用单独提交并等待结果。该恢复合同不增加 Provider schema 字段，不包含 raw arguments、路径或内部 audit metadata。只读批次允许部分执行失败，但全部结果仍按序返回；取消发生后丢弃晚到批次结果。
7. 单个 ask 生成绑定 request/thread/turn/tool call 及项目指令 aggregate digest 的内部 checkpoint；checkpoint 与 pending approval 在 store 的同一事务中写入，包含继续运行所需的 messages、既有 tool results、按真实追加顺序绑定的 tool-result occurrence、安全数值 accounting、已消费 grants、approval count、completion tracker 和 model-turn offset。
8. 执行允许的 Direct 工具，把 `ToolResult::to_message_payload()` 按原顺序作为 tool message 送回下一模型回合；provider-facing assistant/tool history 保持 canonical tool name、call id 和本地验证后的参数，payload、内部 `ToolResult`、Policy、Approval、pending call 和执行状态使用同一真实工具名称。pending Approval checkpoint 会从保存的 provider-facing call 重新经过同一真实 `ToolSpec` 和 profile binding，再与 canonical pending call 比较，拒绝名称或参数篡改。typed repairable failure 由 completion tracker 和下一回合反馈处理；结构完整但名称不在当前模型工具视图中的 native 调用在 Policy 和 executor 前归为 `Visibility/tool_not_visible`，以原 call id 返回下一模型回合。这类 fail-closed 拒绝在 provider-facing assistant/tool history 中固定使用 `tool_rejected` 和 `{}`，保留原 `call_id`/`tool_call_id` 配对；内部 `ToolResult`、原始 tool name、原始错误和 typed audit 诊断保持不变。`tool_rejected` 只是历史占位，不注册为 `ToolSpec`，不进入 `ToolBroker`、Policy、Approval、sandbox 或 executor。该 `tool_selection` 失败只有后续一个合法可见工具成功时才解除，模型不能直接用文本跳过。宿主 PATH 中不存在或绝对路径不可用的可执行文件归为 `command_executable_unavailable` capability，当前 adapter 无法安全表达的 batch 调用归为 typed unsupported，这些路径都在零执行后把安全原因返回下一模型回合。真正的 sandbox backend/infrastructure/timeout/cancelled 仍终止当前 run，不伪装成普通输入修复。
9. 没有 tool call 时应用 completion gate，接受或拒绝终态候选；repairable failure 或 completion/plan/verification 状态仍不可 final 时，下一 `ModelTurnRequest` 携带 typed tool result 或固定 developer feedback并继续使用 `Auto`。required verification 已通过，且存在的显式 plan 也已完成后，run 与 approval resume 共用的下一请求进入 finalization-only：`tools=[]`、`tool_choice=None`、本地 tool-call 上限为 0，并提供当前 changed paths、diff digest、plan 风险证据和已通过 verification digests。若该 readiness 恰好在最后一个普通工作回合响应后才成立，则允许一次超出普通回合预算的同一终态请求。provider 返回结构化 tool call、非 JSON、错绑 revision/digest/evidence 或缺少 verdict 必需字段时都不进入 Policy 或执行器；无效 envelope、tool call、取消与 stream/provider failure 立即 fail closed，strict Final Review 内容或 evidence 解析失败则在剩余 model-turn budget 内保存真实 Assistant 响应、追加有界格式纠正并再次发出 finalization-only 请求，预算端点仍 fail closed。只有 typed `Accept` 的非空 `final_answer` 才能完成，`Reject`/`Repair` 必须重新进入同一 bounded repair/verification 循环；格式纠正不形成 repair decision、不消费 Repair Attempt，也不执行工具。反馈同时列出全部未满足的 plan 与 verification 不变量，不以其中一个遮蔽另一个；普通文本不能绕过本地 completion gate。

普通 turn 的 durable checkpoint 事件按真实副作用边界提交：`Initial` 在开始运行前保存安全状态；`BeforeModelRequest` 固化即将发出新请求前的完整状态；`ModelResponseCommitted` 只在完整响应已经进入 AgentLoop 状态后发布；`ToolCallsReady` 在一个事务中先保存 checkpoint，再在没有已接受 steer/pause 时登记 `tool_executions` 的 `running` owner；`ToolResultsCommitted` 携带同一执行批次的完整 tool-call ID 集合。Store 在同一个 `BEGIN IMMEDIATE` 事务中验证显式集合非空且唯一，并要求它与该 thread/turn 的全部 `running` execution 完全相等；任一 ID 缺失、重复、陌生、跨 Turn 或不再 `running` 都在写入前整体拒绝。验证通过后事务写入包含完整批次结果的新 checkpoint，并删除、核对全部显式 execution；单工具沿用同一机制的单元素批次。批准后的 approval continuation 也在实际工具副作用边界调用同一 callback，避免恢复后仍停留在 approval 前 checkpoint。进程或 owner 丢失时，仍在途的 execution 只转换为 `unknown`，永不自动重新执行；有安全 checkpoint 且没有 unknown execution 的 Turn 归约为 `suspended`，没有可恢复边界的 Turn 则按既有中断/失败合同终止。approval continuation 仍使用独立的 opaque typed `ApprovalCheckpoint` 与 `pending_tool_calls`，不把两种 checkpoint schema 混为 Store 的动态语义。

checkpoint、pending tool call、原始 prompt、provider payload 和内部 audit metadata 不序列化到 `AgentLoopResult`、CLI response 或普通 trace payload。checkpoint 保存 model usage、provider attempts、completion/plan、revision-bound verification plan、bounded repair state、最后的 typed review verdict、context compaction、recovery metrics 和 tool-call fingerprints，但不作为 provider capability 真值。allow-resume 只接受当前 active blocked turn 的一次性 decision，校验 checkpoint 的完整绑定及项目指令 aggregate digest 后恢复原 messages、tool results、已消费 grants、approval count 和 model-turn offset；digest 不匹配时在重新协商或执行 pending tool 前失败。通过校验后再重新协商当前 effective model 的 capabilities，执行 pending tool 并继续模型循环；恢复时再次验证 plan/revision、repair budget 与 completion evidence 的一致性；取消、失败和 max-turn 返回都保留恢复前的回合计数。

`AgentRunStatus.audit_events` 在 Agent 到 trace 的 seam 通过 typed closed allowlist 生成：只保留受限的验证、approval decision、command scope digest、sandbox enforcement/fallback、policy 与 bounded label 字段；绝对 cwd、raw arguments、原始 reason、grant/request/decision ID 以及未知嵌套 JSON 都被丢弃。普通 `TraceEvent` 与 Evaluation 的安全投影使用同一 allowlist；pending approval 的 command audit 只读取 `PendingToolCall.resources` 中已绑定的 `PermissionResource::CommandScope` digest，缺少或非法 binding 时显式记录 `command_scope_digest=unavailable` 与 `policy_scope_binding=unavailable`，不从 raw arguments 以默认权限重算。

当下一次 model request 超出 context budget 时，AgentLoop 使用确定性的 `compact_model_messages`：保留全部 system 消息、初始 developer 指令、最新 user 消息和最近完整的 assistant/tool 配对，把较早消息和原始 tool output 换成包含 `agent_context_compaction`、压缩数量、失败数量、plan 当前摘要、verification 摘要、recovery 计数，以及当前仍有效的 verification、plan 和 repeated-repair 控制指令的 developer 摘要；保留的 tool 消息只带 `ok`、错误码、截断标记和重新读取提示。只有压缩后 token 数严格下降且仍在窗口内才应用，否则返回 context overflow。`AgentContextTrace` 记录 `compaction_count`、`compacted_message_count`、压缩前后 token 数，并进入普通 agent trace；approval checkpoint 同时保存该 trace、plan、completion tracker、model usage、provider attempts、repair 和 tool-call fingerprint 状态。resume 会校验 checkpoint 的绑定、plan/completion、provider attempt 和 compaction 单调性，再从同一状态继续，损坏或不一致时 fail closed。

请求大小同时计入尚未压缩的安全 tool result 的数值 accounting；该 accounting 只影响本地预算判断，真正发送给 provider 的仍是有界安全 payload，压缩后以短消息重新计数。AgentLoop 按 `tool_results` 的真实追加顺序把 occurrence 与 assistant/tool message 一一配对：真正等待用户决定的 pending approval 使用隐藏结果且没有 tool message；整批 preflight 零执行产生的 `approval_required` sibling rejection 是普通 `Visible` 安全失败，继续进入 provider history 并可跨后续 checkpoint 恢复；被省略结果同样没有 tool message。`Visible` 只接受普通安全 payload，`Compacted` 只接受 AgentLoop 生成的固定有界占位，不按跨 turn 重复的 call id 反查结果。approval checkpoint 只保存安全的数值 accounting 与 occurrence binding，不保存 raw 内容；resume 对完整安全 content 可精确重建并校验数值，对大/摘要结果至少校验安全下界，缺失或不一致则 fail closed。只有当前 checkpoint version 可读取，旧版、未来版、缺少 occurrence binding 或缺少可信 accounting 的 payload 均拒绝。因而 compaction 计数、verification 和 workspace revision 仍由同一安全状态继续推进。

completion gate 保持以下不变量：

- final answer 不能为空。
- 没有 manifest 指定的 typed verification 时，edit/patch 之后必须以一个具有合法精确 digest 的成功 command 作为最新 command 观察；后续合法成功 command 会成为新的终态观察。
- typed verification requirement 使用 canonical command scope 的 SHA-256 digest 和大于零的成功次数；要求的 digest/count 必须精确组成最后的成功 command 后缀多重集合。mutation 会清空后缀；其他成功 command 会占据后缀位置，直到其后重新运行完整 required commands 才能恢复终态。
- Evaluation smoke 与 completion gate 共用 Agent 的 terminal command observation：只纳入最后一次 workspace mutation 之后、`ok=true` 且 digest 格式精确的 command，并只保留要求数量的最后后缀；失败 command 不占后缀，成功但缺失或非法 digest 的 command 会使此前后缀失效。结果按原顺序保留重复项，不能用旧 smoke 形成 Passed evidence。
- `WorkspaceTools` 为 edit/patch 和 backend 明确报告的 workspace-write command 变化分配单调的内部 workspace revision；edit/patch 的发布者同时从实际 prepared mutation 的相对路径、原正文 bytes 和发布后正文 bytes 计算 producer-owned `WorkspaceChangeSummary`。Windows/Linux sandbox 在启动 workspace-write child 前通过已打开的 workspace directory capability 建立有界、拒绝跟随链接的 metadata/content-digest 快照：普通文件、目录和链接进入可验证摘要。Linux 对 protected path 只保留不透明 metadata：受保护普通文件仍比较全部已采集 metadata，受保护目录及其普通祖先只比较存在、类型、权限与对象身份，目录内部由 trusted runtime 产生的 length、timestamp、link-count 和内容波动不属于 Agent 可见 workspace revision；对象替换、权限变化或普通 workspace 漂移仍 fail closed。Windows 不把 sandbox setup 必须物化的 protected sentinel 纳入 diff digest，而由受限目录通知与 ACL enforcement 检测其写入。两端都不把 protected 名称或正文放入 changed paths 或模型。child 退出后以同一规则复验；最终 before/after 差异生成规范化相对路径和 SHA-256 diff digest，无法证明的 protected identity/permission 漂移、root identity 变化或 Windows 目录通知与最终快照冲突都归为 `Unknown`。执行前无法建立快照时返回 `capability_not_supported:workspace_change_summary`，不启动 child；执行后无法复验时 fail closed。缺失、非法 digest 或 edit/patch 请求路径与实际路径不一致会在动态规划前拒绝，不能用 command cwd、模型参数或 call fingerprint 代替真实 diff。summary 继续保存物理 changed paths 和 digest；只有 producer 证明全部差异都是本轮新生成的闭集工具链产物，且已有祖先目录只发生由这些子项引起的非安全 metadata 波动时，`verification_relevant=false`。缺失字段、非法 digest、非规范/重复/未知路径、preexisting artifact、source/artifact 混合变化或目录身份与权限变化都默认 relevant。workspace-write 下可信的 artifact-only physical `Changed` 仍进入 sandbox event 和 checkpoint，但 `WorkspaceTools` 投影为当前 revision 的 semantic `Unchanged`，避免验证命令自身生成的缓存使刚完成的 evidence 失效；read-only 下任何 physical `Changed` 仍 hard fail。Evaluation 复用同一个闭集分类器，并通过 pristine source snapshot 保留 tracked artifact 路径。成功 verification 同时保存 `Unchanged` observation 和该 revision。任何 semantic `Changed` observation 会清空旧 evidence，多条 exact requirement 只有在所有 observation 仍绑定同一 revision 时才能共同满足。
- workspace-write command 必须由 `SandboxBackend` 显式返回 `Unchanged`、`Changed` 或 `Unknown`；未知变化不按命令名称、语言或任务猜测，直接 fail closed，Evaluation 对 success/failure 两种 expectation 都把它归为 sandbox blocker，不能把观测失败计作预期的语义失败。revision、observation 和 audit metadata 只进入内部 completion/checkpoint 校验，不进入模型 tool payload；approval resume 先恢复并核对同一 revision/evidence 关系，损坏或不一致的 checkpoint 拒绝执行。
- 存在未解决的可修复 tool failure 时不能完成。
- 若已建立 plan，所有 plan step 必须为 `completed`；否则 final answer 被拒绝并要求再次调用 `update_plan`。
- Verification Planning 由 `AgentVerificationPlan` 单一持有：它把 `GeneralMutation`、空集合、Optional/Null、零值、索引边界和除零等领域风险绑定到 exact command requirement。同一个 exact action 可以同时证明多个风险；动态 plan 按 command scope 合并 requirement 并保留该 scope 明确声明的最大 repeat count，不把风险标签机械累加成重复命令。真实 tool result 首先由 `CompletionTracker` 验证 workspace observation，再把当前 `WorkspaceRevision` 绑定到 plan；任何 Changed mutation 会清空旧 command evidence 并从新 revision 重新规划，缺失、冲突或未知 observation 保持 fail closed。`VerificationPlan`/`RepairPlanning` 事件只投影风险/计数/revision 和有界预算，不携带 prompt、raw arguments、路径或 audit metadata。
- Final Review 是同一状态机的 typed verdict：`Accept` 只在 response 明确绑定当前 revision、change digest、全部 terminal verification digests，且 completion evidence、plan 和 repair state 同时满足时产生；`Reject`/`Repair` 会阻止 `Completed`，把有界安全原因作为下一回合的固定 developer feedback，并由同一个 `AgentRepairPlan` 驱动重新 mutation、重新规划与重验。此时 Repair Context 以真实 review reason 作为失败 requirement/evidence 和上次结果，保留当前 changed path/symbol、revision 与 mutation 摘要，并明确要求先改变失败策略；最后一次成功 command 或已经满足的旧 `current_gap` 不能覆盖该因果。repair attempt ledger 在一次 run 内单调递增，只有新的 repair decision 已绑定 mutation revision，并完成该 revision 的 terminal verification cycle 时才提交一次；transport retry、checkpoint resume、read、no-op、无效 replan、未执行调用和无 mutation response 都不消费 attempt。模型可见 repair context 从同一 revision-bound verification plan 投影失败 requirement/evidence、changed path/symbol、当前 revision、上一次动作/结果，以及下一批必须精确复用的 command/cwd/timeout/sandbox/network actions；该投影整体有界，不能用截断后的命令制造不同 scope。checkpoint version 3 持久化 ledger，并要求它与统一 repair metric、可恢复的失败 occurrences 以及 active plan 一致。workspace 已变更时，checkpoint 必须保留 revision-bound plan 和 producer change summary；该摘要还要与产生它的 tool-result occurrence 中的 revision、changed paths 和 digest 完全一致。绝对路径、父目录分量、缺失/替换摘要和 repair ledger reset 都在执行 pending tool 前 fail closed。最多允许 3 次 repair，第四次请求记录 `Exhausted` 并 fail closed。证据冲突、预算耗尽或取消都在 AgentLoop 内终止为 `Failed`/`Cancelled`，不会被普通 final text 覆盖。
- 普通工作回合达到 turn 上限时，只有在最后一个允许的工作响应处理后已满足 `finalization_ready()` 才允许一次 terminal finalization；该请求的 provider error、取消、空文本或结构化 tool call 仍 fail closed。没有 readiness 时，达到上限仍返回 failed，不改写为 completed。

`update_plan` 是 AgentLoop 唯一的规划控制工具，不执行 workspace 操作。`steps` 至少 1、最多 64 个；每个 step 的 `step` 非空且最多 512 个字符，`status` 只能是 `pending`、`in_progress` 或 `completed`，step 文本去空白后必须唯一且最多一个 `in_progress`。真实 mutation 后同一输入还必须携带有界 `verification` entries；每项分别声明 risk、证据、精确 `affected_path`、领域 `affected_symbol`、当前 gap、required count 和一个完整 Command action。同一 exact action 可以同时证明多个风险，Runtime 按 scope digest 取这些 entry 的最大 required count，而不是把风险标签累加为重复执行次数；不同 action 则必须逐项精确执行，语义近似但 scope 不同的 command 不能替代已安装 requirement。Runtime 按字符串精确匹配 producer 摘要中的相对路径，不裁剪合法文件名，也不从 symbol 或自由文本 evidence/current gap 反推 typed failure 是否已处理；失败因果由 repair state 与 exact requirement 持有，自由文本只作有界说明。changed paths 作为 JSON 数据嵌入 developer 指令，控制字符只以转义形式出现。Runtime 同时校验当前 permission profile、拒绝网络的有界 action，并把 action 规范化为 exact command scope digest 后交给同一 `CompletionTracker`。无 mutation 时拒绝 verification entries，mutation 后缺失 entries 或先执行 command 也拒绝。输入和嵌套对象都拒绝 unknown fields；成功调用递增 `plan_update_count`，对模型只返回脱敏 plan summary、risk/path/symbol 和 action digest，不回显命令正文、diff 或内部 audit metadata。执行 plan 的最后完成状态和 verification evidence 都由 completion gate 强制检查。Repair Context 把尚未执行的第一个 exact scope 作为 required action，并将其余 distinct scope 按 plan 顺序投影为有界 additional actions；每个 scope 只出现一次并携带剩余成功次数。Command 是 exclusive tool，因此本轮只能提交 required action，得到 ToolResult 后才按序进入后续 action；additional actions 不是批量执行授权。只要当前 revision 仍有未失败的 exact action，下一次普通模型请求就在 Completion Gate 前把可见工具收窄为该 action 的唯一 `command` schema，并将 `max_tool_calls` 固定为 1；Provider 未声明 required tool choice 时仍保持协商得到的 `Auto`，不伪造能力。该 Developer 投影只存在于对应请求，ToolResult 后从同一 plan 重新派生下一 action，不持久化旧命令；exact action 真正执行失败或遇到 typed capability/policy/profile/protected-path pre-execution boundary 后立即解除此约束以允许改变策略。超过 action 数量或 command 字符预算时显式标记 truncated，留待后续安全边界继续。缺失验证或可纠正的 pre-execution command 错误触发 repair 时，Agent 同时把 required input 投影为本轮 command 的 exact model schema，并在执行边界重新校验；改变 command、cwd 或 timeout 的响应在策略与 sandbox 前拒绝，且不消耗 mutation-bound Repair Attempt。稳定 capability/policy/profile/protected-path 拒绝则明确释放 exact pin，要求先进行有界的新 mutation/replan；输入、approval 和暂态基础设施错误仍保留 exact pin。exact action 在当前 revision 真正执行失败后，其脱敏失败结果保持为该 repair episode 的因果证据，后续不同 scope 的诊断结果不能覆盖它；Repair Context 不再把该失败 action 投影为 required，而是明确要求改变修复策略，直至新 mutation 和 revision-bound replan 建立下一轮 exact action。每个 action 都分离可提交的 `command_tool_input` 与只读 policy context；该投影不改变 CompletionTracker 的 exact digest/count、permission、network denied 或 transactional workspace 合同。

产品只接受 Direct structured tools。Provider 不支持 Direct structured tools 或当前定义容量不足时，AgentLoop 返回 typed unsupported；不通过隐式路由、文本伪调用或静默别名恢复。

`AGENT_DEVELOPER_INSTRUCTIONS` 要求多步骤工作使用 `update_plan` 保持简洁计划，在证据或失败改变路径时更新，并在 final answer 前完成；简单只读或单步工作跳过计划。

## 7. Model 与 provider

`ProviderConfigSnapshot` 在 app-server 启动时只捕获一次配置。进程环境层优先；如果该层完全没有 provider 变量，则从当前目录向上查找最近 `.env`。`SINGULARITY_MODEL`、`SINGULARITY_BASE_URL`、`SINGULARITY_API_KEY` 和 token limits 必须来自同一层，`SINGULARITY_MODEL_PROVIDER` 缺失时使用 `openai_compatible`。当前唯一注册的生产 adapter 是 `openai_compatible`；其他 provider 名称在 client initialization 阶段以 `provider_adapter_unsupported` fail closed，不会借用 OpenAI transport 伪装为已支持。出现第二个真实生产 Provider 前不建立单实现 Registry 或 Factory 框架。context window 默认 128000，output limit 默认 4096；用户不配置 tool-call、tool-definition、required、strict 或 wire protocol 能力。

Provider 配置值在本地 client initialization 信任边界完整校验，不会静默 trim 或纠正。进程环境和 `.env` 的 provider、model、base URL、API key 及 token limit 值如果含 `CR`、`LF`、`NUL` 或首尾空白，以 `provider_configuration_invalid` typed fail closed，错误不携带原始值且不会产生 provider attempt；`.env` 标准 `CRLF` 行尾由解析器正常处理。

每次 run/resume 都调用 capability negotiation；未固定 endpoint 的 OpenAI-compatible base URL 依次尝试 Responses 与 Chat Completions；显式以 `/responses` 或 `/chat/completions` 结尾的 URL 只使用对应协议。只有带有 typed capability-specific validation evidence 的稳定 capability rejection 才能失效已绑定的 negotiation；普通 HTTP 400/422、认证、网络、限流、5xx、取消、JSON decode、body/envelope validation 和无法安全重放的输出都保留原始因果并终止，不会被宽泛归类为能力拒绝。最终协议作为 `ProviderCapabilityMetadata.api_protocol` 进入安全 trace，但不进入 Agent execution model，也不替代独立的工具能力 contract。没有当前 tools 且没有历史 tool call/result 的独立 Provider 调用，在未显式固定 Responses 时保持 Chat Completions；只要请求仍携带工具历史，包括 `tools=[]` 的 finalization-only 回合，就继续使用同一 effective model 已协商并缓存的协议与能力合同。

OpenAI-compatible adapter 使用固定 developer/user 消息验证 Direct tool definitions、strict schema、parallel/single 调用和多轮原生 tool history；只有完整多轮 profile 成立才写入上述 snapshot/persistent cache。reasoning/history 不满足 adapter 合同、工具调用缺失或使用文本伪调用时 typed fail closed。Direct profile 不成立时直接返回 typed unsupported，Agent 也不从单纯的容量数推断替代能力。生产 app-server 只接受 canonical file-backed SQLite DB；所有 SQLite URI（包括 `:memory:` 与 `file:` 形式）、DB symlink/reparse/hard-link 以及与 cache/lock/temp 名称或 Windows alias 碰撞的 basename 都在 Store 打开前拒绝。DB/state directory 的 canonical path 是 cache 注入和后续 Store 连接的唯一事实源；crate 内部 `SessionStore::open(":memory:")` 测试用途不改变。

Provider 失败通过 `ProviderDiagnostic` 投影稳定的 `code`、`stage`、transport category、命中 timeout 时的配置 deadline 秒数、HTTP status 和 response validation codes。该对象不包含 API key、Authorization、endpoint、prompt、原始响应、provider/model 名称或底层 error source；AgentLoop、app-server trace 与 Evaluation result/report/evidence 只持久化这一安全投影。原始错误 message 仍经过公共边界脱敏，诊断字段不会因 message 被整体替换为 `[redacted]` 而丢失。timeout deadline 通过本地 hanging HTTP transport 回归测试验证，不用字段序列化代替真实 reqwest 超时路径。

`OpenAiProvider` 使用 reqwest rustls 客户端，并把同一个 `ModelTurnRequest` 分别投影为 Responses typed items 或 Chat Completions messages。Responses 请求把开头连续的 system/developer 基础指令合并到顶层 `instructions`，其余对话保持在 typed `input`；它使用 flat function definitions、`store=false`、`function_call`/`function_call_output` 的 `call_id` 关联和 `reasoning.effort=none`。adapter 只重放当前合同理解且本地验证过的 message/function items，出现非空 reasoning、未知 output item 或未知 message content part 时 fail closed。Chat Completions 使用标准 nested function definitions 与 assistant `tool_calls`/tool messages；若多轮工具调用或 history-only finalization 暴露 reasoning，只有固定 `thinking.type=disabled` 控制被真实 probe 证明后才采用。history-only finalization 仍保留原生 assistant tool call 与 tool result history，并在没有当前工具定义时发送该 reasoning control；Provider 违反合同时两条路径都 typed fail closed。两条 wire 路径共用同一个请求 validation、bounded body、retry、response normalization 和本地结构校验；对合法已注册工具，`ToolSpec`、模型输入和 executable input 仍在 Agent/Tool 本地信任边界完整验证并使用 canonical tool name；只有 fail-closed 拒绝的 provider-facing history 使用 `tool_rejected` 占位，不形成第二套 Agent 或工具执行路径。

每次 complete 在 current-thread Tokio runtime 中执行可取消 HTTP future；配置、请求校验、HTTP status、body、JSON decode 和 response validation 都使用稳定诊断。请求上限为 1 时发送 `parallel_tool_calls=false`，大于 1 时才发送 `true`；strict 仍由 contract 与本地 schema 检查共同决定。普通请求使用 `tool_choice="auto"`，显式 `Required` 只有 contract 声明支持时才可发送。缺失/重复 call id、非对象参数、reasoning/history 违约或 assistant 文本中的完整 `<tool_call>...</tool_call>` envelope 都在执行前 typed fail closed；文本永不解析执行。合法 Direct tool history 使用 canonical tool name，拒绝历史只保留 `tool_rejected` 占位和 call id 配对，不形成第二套执行路径。

一次 provider complete 最多执行 3 次 attempt，重试只覆盖可重试的网络/timeout 或 response body read 错误，以及 HTTP 429 和 5xx；请求本地校验、JSON decode 和 response validation 不通过时不重试。重试 backoff 以 50 ms 为基数并逐次翻倍（在最多 3 次 attempt 下实际等待 50 ms、100 ms），且每次等待都检查 cancellation。每次响应或错误携带 `ProviderAttemptMetadata` 的 `attempt_count`、`retry_count` 和总 `latency_ms`；AgentLoop 按真实 provider operation 累加这些字段，能力协商在产生 model turn 前失败时也保留 `ProviderError.provider_attempt_metadata`，因此 Evaluation 不会把已发生的 probe attempts 归约为 0。`ModelUsage` 同时累计 input/output/total、cached input、reasoning token 和可选 cost；这些是诊断和 evaluation 投影，不改变 completion 或 blocker 语义。

`ProviderAttemptEvent` 位于真实 reqwest attempt 边界：每次 capability probe 或 completion 在发送 HTTP request 前同步产生 Start，同一个 request 在成功、错误、取消或安排 retry 时产生唯一 End；observer 拒绝 Start 会在网络连接前 fail closed，拒绝 End 会停止后续 retry。AgentLoop 只为这些 typed transport event 绑定 Turn/Prompt parent 与 occurrence identity，`TraceProjector` 按 callback 顺序直接写入 SQLite；最终聚合的 `ProviderAttemptMetadata` 只用于状态和 Evaluation 汇总，失败协商也只消费同一份 `ProviderError.provider_attempt_metadata` 一次，不再反向补造第二组 span。Start/End 共享 provider、model、protocol、operation phase、attempt/retry identity，End 追加 send-to-headers、TTFT、backoff、usage 和 typed error。当前 provider 与 tool 执行都没有独立 admission queue，因此对应 queue timing 明确保持 unavailable，而不是把未测量值伪造成 `0 ms`；引入真实有界队列时必须从入队与开始执行的同一 occurrence 生命周期派生该指标。

公共 `providerConfiguration` 只表示配置状态，包含来源、snapshot id、`configured`、`configurationBlocker` 和三个字段的 present/missing；它不声称网络或模型请求已经成功。Provider error 只投影稳定 code、阶段、可靠 transport 类别、HTTP status 和 response validation codes。API key、base URL 原值、Authorization header、原始 response 和原始 prompt 不进入 CLI、Evaluation 或 trace。普通 agent trace 与 Evaluation `agent-trace.json` 可以记录安全的 `provider_protocol`，仅包含可选 contract 和 capability metadata；不会记录 model 名、prompt、raw response、key 或 base_url。HTTP 200 但返回文本伪工具调用时 fail closed，本地完整 `ToolSpec` validation 不关闭。

## 8. Tool、Policy 与 Approval

产品注册的工具为：

```text
read
list
grep
edit
patch
command
update_plan
read
list
grep
edit
patch
command
update_plan
```

产品运行时向 `ToolBroker` 注册七个具有真实 executor 的 Direct 工具。`ToolRegistry` 的单一 `ToolEntry` 同时拥有稳定 `ToolId`、版本、模型 exposure、能力、authorization、typed executor 与 `ToolSpec`；Agent 不再按工具名称另建 Policy 或 execution 分派表。每个 `ToolSpec` 拥有模型 schema、execution mode、model-input validator 和 execution-input validator；模型 schema 只表达调用方需要提交的输入，sandbox/network 等执行策略由 Agent、Policy 和 backend 绑定，不进入模型 payload。`command` 的模型合同是 `{command:string,cwd?,timeout_seconds?}`，可信内部 `argv` 仅用于平台 adapter 和 evaluation executor。AgentLoop 对一个响应的全部调用先完成 model validation、profile binding、workspace/preflight，任一失败时不对合法子集执行 Policy、Approval 或 executor；ToolBroker 在 Allow/Approved 闸门再次验证 execution contract。所有输入拒绝 unknown fields；非法参数返回可修复的 `invalid_tool_arguments`，不猜测、不改写 shell string，也不解析文本伪调用。

Evaluation 中存在两套独立的 exact verification 合同：AgentLoop 内部的 typed verification completion gate 只依据 canonical command-scope digest/count 判断 final answer 是否可接受；Agent stage 完成后，app-server 再从 `AgentLoopResult.tool_results` 独立检查 manifest 的 post-agent smoke，要求 smoke 的 digest/count 精确组成最后一次 edit/patch 之后最后的成功 `command` 后缀多重集合，按同一 canonical cwd、timeout、sandbox/network scope 计算 digest，并为重复 smoke 要求不同的成功 result。前者阻止过早 final，后者决定 `agent` stage 是否 passed；两者都不能用相似命令、旧 mutation 前结果、smoke 后的其他 command 或 timeout/network 设置差异冒充 exact 证据。

默认 workspace-write profile 是 network denied、approval on-request、protected paths enforced；read-only profile 对 Write 做 hard deny，workspace-write 只允许 workspace capability 内的 direct write，命令仍经过严格 OS sandbox。read 和 sandbox command 有显式 allow rule；写入仍经过路径敏感性和 protected path 检查。Policy 只比较 `PermissionResource`：workspace path、command scope digest 或 tool id；`ToolId`、`WorkspaceRelativePath` 和 `CommandScopeDigest` 在构造与反序列化时都重验格式，路径拒绝 absolute/drive/ADS、反斜杠、空/`.`/`..` 分量。`PermissionProfile` 不再复制 workspace roots 或 writable directories，workspace capability 由 `WorkspaceTools` 持有。`PermissionDecisionCause` 区分 rule、filesystem profile、network profile、protected resource、no matching rule 和 approval policy；AgentLoop 再投影为 input/visibility/capability/policy/profile/workspace/protected/approval/sandbox/backend/infrastructure/execution/timeout/cancelled 的 `ToolFailureKind`，恢复性由类型和少量稳定 execution code 决定，不解析 human-readable reason。deny/protected/network 先于 approval，approval 不能扩大这些边界。

`WorkspaceTools::new` 从平台根 anchor 逐组件 no-follow 打开并保留 workspace directory capability；构造失败直接返回 typed error，AppServer 和 Evaluation 不会退化为 ambient path I/O。read/list/grep/edit/patch 后续只从该 capability 解析 slash-separated relative components，拒绝 symlink/reparse、非普通文件、无法验证的对象身份及 link count 大于一的文件；Windows 另外从对象 handle 恢复真实相对名称后执行 protected-path 检查，并拒绝 ADS、DOS device、尾随点/空格和短名表达。Windows protected-path glob 生成会把 workspace root 的 glob 元字符按字面量转义，resolver 在选择有界扫描根时还原这些转义，合法目录名不能改变 deny pattern 语义。工具准入完成输入校验和 typed resource 投影，执行及 resume 重新经过同一 capability 边界；command scope 绑定 canonical workspace-relative cwd、timeout 与实际 sandbox/network policy。command 在调用 sandbox 前把 capability directory identity 与 ambient cwd identity 对齐并持有两侧 guard；Windows namespace guard 在执行期间禁止 delete sharing，非 Windows 产品 backend 当前明确 unavailable，不能把一次 path 检查冒充平台 sandbox。

read/list/grep 共享同一 `CancellationToken`，在开始 I/O、目录递归和 entry、固定大小文件块及阻塞读取返回边界检查，取消返回 typed `Cancelled` 并由 Agent 投影为 `tool_cancelled`；edit/patch 不在写入中途异步打断。文件读取在同一 capability handle 上取得 pre-metadata、读取正文并取得 post-metadata，正文摘要、规范化 workspace-relative path、regular-file 类型、对象身份和稳定元数据组成内部 `WorkspaceContentRevision`；读取中任一组成部分变化都返回 typed concurrent mutation，revision 不进入模型 payload。多文件 patch 在任何目录或文件副作用前读取并验证整批目标、expected content、no-op 与 canonical duplicate；新目录逐层记录 capability identity。每次发布使用父目录 capability 下的 unique create-new temp，写入后复制原目标权限并 sync，再复核临时源完整 revision、当前目标 Content Revision 和发布后对象 identity/正文摘要。Linux 发布只通过安全 Rust 封装在已打开父目录 FD 上执行相对名称 `renameat2`：已有目标使用 `RENAME_EXCHANGE`，不存在目标使用 `RENAME_NOREPLACE`；内核或文件系统不支持必要原语时返回 typed capability blocker，不退回普通 rename。交换后同时验证新目标正文/对象身份和换出的旧 `WorkspaceContentRevision`；验证或 hook 失败时恢复旧对象，只有成功才条件清理换出的对象。Linux 的临时文件、本批次新文件和新目录清理先以 `RENAME_NOREPLACE` 移入私有随机 quarantine，再验证 revision 或 directory identity，匹配后才通过 handle-relative `unlinkat` 删除；不匹配时恢复或返回 rollback failure，不执行 check 后按原名称 unlink。中途失败仍只逆序回滚确认已经发布的对象；并发替换、恢复冲突和缺失能力保持 typed concurrent/rollback/capability 因果。其他平台继续使用各自的 capability 实现与既有 fail-closed 复核，不把 Linux 原语保证外推为跨平台保证。文件系统无法提供可靠对象身份时归为 capability failure，不误报为路径越界。相同正文但不同文件对象也拒绝沿用旧 revision，以避免把新的 capability object 误认为模型读取的对象。

edit/patch 只有在目标字节实际变化时才返回成功；no-op 在整批写入前作为可修复 input failure 拒绝，不能更新 completion mutation 状态或生成虚假 changed-file 证据。

当策略返回 ask 时，AgentLoop 生成与 thread、turn、tool call 和类型化资源绑定的 `ApprovalRequest`，同时持久化只供 runtime 使用的 `PendingToolCall` checkpoint；pending 保存已经过 model admission、exact registry binding、workspace binding 和 profile 约束的 execution input，而不是重新暴露模型输入。resume 先用当前 workspace capability 重新绑定调用，再匹配和消费 Approval；execution validator、typed executor、exact resource set 与当前 profile 任一不一致都 fail closed。Turn Blocked、request、checkpoint 和 approval trace 在同一事务提交。Allow/Deny 是单次消费并写入 approval decision history；Defer 只写脱敏 trace event，不写最终 decision history、不消费 request，也不删除 checkpoint。只有 Allow 需要 active thread 和当前可用的 workspace：workspace 在 claim 前检查，thread active 状态还会在 Store claim 事务内重检，条件不满足时不消费 request。Deny 不执行工具，不依赖 thread 是否 archived 或 workspace 是否仍存在；它在 decision 同一事务终结 Turn 并删除 checkpoint。Defer 同样不依赖这两个执行条件，保留 Blocked Turn、request 和 checkpoint。客户端不能通过 `approval/request` 自行注入内部 approval request。

Allow 在 decision history 写入与执行认领的同一事务内把 `pending` 直接认领为 `executing`。若 Blocked Turn 已接受 `steer`，该事务改为原子删除尚未开始的 pending execution、消费有序 input、写入由同一私有 `CheckpointState` 转换出的普通 `TurnCheckpoint` 并把 Turn 恢复为 `running`；转换先为旧 Assistant ToolCall 追加 `not_executed_due_to_user_input` 的 typed ToolResult，再追加真实 UserMessage，因此允许原调用不等于执行已经被新要求废止的副作用。只有 pause control 时，事务保留 canonical pending call 于普通 checkpoint 并把 Turn 置为 `paused`；显式 resume 后仍需重新协商 capability、Policy、workspace revision 和 ToolSpec。该转换不会让 ApprovalCheckpoint 与 TurnCheckpoint 同时成为恢复权威。

没有 input/pause handoff 时，tool continuation 的 Allow 在 claim 前取得同一 workspace execution guard、登记 cancellation token，并由 Store 的 `BEGIN IMMEDIATE` 事务原子确认该 workspace 不存在其他非终态 Turn；跨进程 interrupt 若先提交则 claim 不成立，claim 若先提交则后续 interrupt 进入已登记 token 的可取消执行区。该 guard 与 token 持有到 AgentLoop outcome、terminal trace、checkpoint 删除或下一 checkpoint handoff 提交完成；Deny、Defer 和 generic approval 不启动 AgentLoop，也不占用该 guard。`approval/decision` 的 continuation 在独立 request worker 中恢复，主 stdin loop 不同步等待 AgentLoop。claim 之后的普通 AppServer continuation 错误会在当前进程归约为 Failed Turn 并原子删除 executing checkpoint；进程中断留下的 executing checkpoint 由后续安全恢复归约为 Interrupted 或 successor handoff，Store 持续不可写时则可能延迟终态提交。

启动恢复和非终态 `turn/status` 只在成功取得对应 workspace execution guard 后修改状态；锁被其他进程持有时视为 live owner 并跳过。取得 guard 后会检查该 execution scope 内的所有 logical thread，因此同 workspace 的另一个 thread 也能在 owner 丢失后清理 stale Turn。每个 logical thread 都在独立事务内先验证 Approval、checkpoint、Turn 和 decision binding，再执行该 thread 的恢复：合法的 Blocked + pending checkpoint 保持可恢复；无 owner 的 Running、CancelRequested 或非法 Blocked 归约为 Interrupted 并记录 `execution_owner_lost`。没有 successor 的遗留 `executing` 归约为 `Interrupted` 和 `approval_execution_outcome_unknown`；较早 `executing` 加一个较晚且合法的 `pending` 视为半交接，只删除旧 execution并保留下一 Approval。某个 thread 存在歧义拓扑或损坏 checkpoint 时，该 thread 的恢复事务失败且不修改其数据库状态；此前已安全恢复的 sibling thread 不回滚。所有恢复路径记录 `tool_replayed=false`，当前保证是 at-most-once execution attempt，不宣称 exactly-once。

pending Approval 等待期间没有运行中的工具；此时 `turn/interrupt` 在一个事务内把 Turn 终结为 `Interrupted/cancelled`、删除 unresolved Approval 和 checkpoint，并记录 `pending_approval_cancelled=true` 的 cancellation trace，不生成 decision history。interrupt handler 会在该事务提交后直接发送 terminal event。若 Allow 已 claim 为 `executing`，interrupt 只把 `agent_loop_status` 写为 `cancel_requested`，Turn 在 worker 收敛前仍保持原来的 `blocked`；同一个 cancellation token 会传播到 resumed AgentLoop 和在途 sandbox command。工具在收到取消前可能已经产生 workspace 内副作用，取消不宣称回滚这些副作用。最终 Approval outcome 提交把 Turn 归约为 `Interrupted/cancelled`，让取消覆盖本地晚到结果，并拒绝把下一 Approval handoff 到 terminal Turn。

`ToolOutput.content` 先经过统一的敏感文本检查与大小边界，再投影到 `ToolResult`。安全、未截断且在上限内的 JSON 保持为结构化 `content`；文本摘要、敏感结果、超限结果和 source-truncated 结果降级为有界且脱敏的 `preview`。`content` 与 `preview` 在模型 payload 中互斥，因此 `retry_inputs` 等机器字段不会被压入二次编码的 JSON 字符串。发送给模型的 tool result 只包含 `ok`、工具/调用标识、稳定 `error_code`、`failure_kind` 和截断标记；内部 result id、raw arguments、approval id、policy id、audit metadata、secret-like 文本以及任何未登记的 artifact/diff ref 都不投影。当前 WorkspaceTools 不生成 synthetic `artifact://` 或 `diff_ref`；截断、binary、diff 和 patch 结果只保留有界、脱敏的 preview 或结构化摘要，不能让模型得到不可 fetch 的引用。workspace/protected/sandbox/rollback 错误使用固定安全摘要，不回显敏感路径或底层错误。`ModelMessage.content` 按 Provider 协议承载一次序列化后的整个安全 payload。完整 `ToolResult` 只存在于当前 `AgentLoopResult`，并可在等待下一 Approval 时作为内部 checkpoint 的一部分暂存。普通 runtime 的终态 SessionStore 不建立 ToolResult history 表：Turn/assistant item 保存终态，Trace 只保存状态、计数、verification、provider diagnostic 和从 ToolResult 提取的脱敏 audit 摘要。

`ToolResult` 另保留一个只由递归脱敏安全值计算的数值 context-token accounting；它不进入模型 payload、终态结果或 trace，也不把 raw 内容写入 checkpoint，只用于 AgentLoop 判断尚未压缩的安全 tool 结果大小。ASCII/non-ASCII estimator 由 `tools` crate 提供，Agent 与 ToolResult 共用同一规则。

## 9. Windows sandbox

`sandbox::WindowsSandboxBackend` 是 `windows-sandbox` 的产品 adapter。该底层代码来自 OpenAI Codex `codex-rs/windows-sandbox-rs` 的固定提交，来源、删改范围和许可证记录在 `crates/windows-sandbox/UPSTREAM.md`。

执行路径：

```text
CommandToolInput { command, cwd?, timeout_seconds? }
  -> CommandScriptRequest
  -> validate workspace root / cwd / policy-bound modes
  -> platform adapter selects the shell dialect and builds trusted internal argv
  -> add safe toolchain roots as read/execute-only ACL roots
  -> map to Windows PermissionProfile
  -> run_windows_sandbox_capture_for_permission_profile_elevated
     -> automatic UAC setup when required
     -> offline or online restricted account
     -> restricted token + ACL + private desktop + Job Object
  -> if and only if requested profile supports it:
       run_windows_sandbox_capture (unelevated restricted token)
  -> CommandResult with enforcement metadata
```

规则：

- thread 与 sandbox backend 只表达 `read-only` 和 `workspace-write`；不存在 unsandboxed filesystem mode 或本地进程 fallback。
- network denied 映射到 restricted network，必须使用 elevated offline identity；不能走 unelevated fallback。
- network allowed 可以在 elevated 路径失败且 restricted token 足够时走 unelevated 路径。
- 产品层只表达单一 workspace root 与 `denied` / `allowed` 两种网络模式，不要求用户维护 allowlist 或额外读写根目录配置。
- 模型提交的 command string 由 adapter 以受控 shell 方言执行；shell quoting、管道、重定向和条件语义由该 adapter 负责，模型不能提交 sandbox、network 或 capability 字段。普通 argv 和 model script 在进入 elevated backend 前使用同一 existing protected-path 投影建立 OS deny-read，workspace-write 同时建立 deny-write；命令 token 检查只是更早的拒绝层，不承担运行期强制执行，因此运行时拼接或间接解析路径不能绕过 protected metadata。Policy、Approval、workspace/protected-path、超时和严格 backend 才是安全边界，不依赖禁止 shell 字符串。
- Windows 的 `.cmd`/`.bat` 工具入口（例如 npm）由适配层转换为受控调用；PATH 中不存在或绝对路径不可用的可执行文件使用独立的 executable-unavailable capability 失败边界：Agent 可在有界下一回合提交新的合法 command string，Evaluation 的固定命令仍归为 environment blocker；敏感可执行路径继续按 policy denied 拒绝。
- Windows adapter 向 child 投影非 verbatim 的 canonical cwd/argv，避免 `\\?\` 破坏 Python、pip 等依赖普通 Win32 路径语义的工具；内部 ACL 扫描仍接受长路径 API 返回的 verbatim 路径，并在对象身份、workspace 边界和去重时把普通 disk/UNC 与对应的 `\\?\` 表达归一为同一 key。
- `core` 是受保护元数据名称（`.git`、`.agents`、`.singularity`）的唯一事实源，`tools`、sandbox policy 和 Windows adapter 都从该策略投影；Windows writable root 缺失时物化并保护受保护元数据 sentinel；缺失 `.git` 若位于已有祖先仓库下则在 child 启动前 typed unsupported，避免改变 Git ancestor discovery。
- Windows path-safety 叶模块统一拥有 disk/UNC 路径解析、`NtCreateFile.RootDirectory` handle-relative/no-follow 遍历和 `FileCaseSensitiveInfo` 准入。所有基于 pathname 的 ACL read/write/allow/deny 都只能通过该共同 opener；调用方的整批预检只保证在 SID、状态或物化副作用前拒绝复合输入，不是第二套强制边界。
- 普通 `.pem` 文件先通过只读 no-follow 固定句柄做有界内容分类：仅由结构完整、base64 解码后能由 Windows 证书 API 验证为 X.509 DER 的 `CERTIFICATE` / `TRUSTED CERTIFICATE` 块及已知公开证书元数据组成时免除 deny-read；标签正确但不是证书的任意内容、私钥、未知或畸形块、非 UTF-8、超限和读取失败仍 fail closed。非公开 PEM 在取得 ACL 权限的最终固定句柄上再次分类，再用同一句柄写 deny-read；`.pem` 始终保留 deny-write，sandbox-owned 文件需要修改 DACL 时只由 typed ACL Access Denied 触发现有 setup authority 提权重试。
- Restricted-token 路径先生成一个不可变 preflight，统一固定 canonical `sandbox_home`、实际 `cwd`、capability roots 与完整 allow/deny/deny-read ACL plan，并验证 home、`.sandbox` 和 capability-state 路径；capture 与显式 preflight 入口都必须先消费该对象，之后才可创建目录/日志、持久化 SID、物化对象或写 ACL。
- Windows ACL 通过稳定句柄读取和写入，并在 reparse/最终对象身份不确定时返回 typed unsupported/error；`GetAclInformation`、`GetAce` 和真实 deny/write ACL 失败会贯穿 setup、restricted-token fallback 和 elevated caller 关闭失败，不会被解释成“ACE 不存在”。Codex 的 `NUL` 设备 allow 只用于 stdout/stderr 兼容，是 best-effort 的附加可用性，不作为 sandbox enforcement 成功条件。缺失的受保护元数据目录在 child 启动前通过 `NtCreateFile.RootDirectory` 逐层执行原生 handle-relative、no-follow、原子 create/open，父目录与最终目录都禁止 delete sharing；ACL 与失败清理绑定同一个返回句柄，不再重新解析 pathname。每层稳定目录句柄同时查询 `FileCaseSensitiveInfo`；当前 case-insensitive path/state 模型无法无歧义表达 NTFS 大小写碰撞对象，因此遇到 `FILE_CS_FLAG_CASE_SENSITIVE_DIR` 或查询失败时 typed/fail-closed。配置、持久状态和 glob 扫描产生的整批 ACL 候选都必须在首次 lowercase key 或 SID/ACL 副作用前完成该准入；现有最终 reparse target 不属于 pathname ACL 的可执行集合，必须在整批 preflight 阶段拒绝。最终原子创建后的父目录复检失败会通过新对象句柄回滚本次创建。若缺失 `.git` 位于已有祖先仓库下则以 typed unsupported 失败，避免改变 Git ancestor discovery。workspace-write command 的现有 protected paths 同时进入 deny-write，未来 secret 文件名不物化。读取边界沿用 Codex 的专用 `SingularitySandboxUsers` 主体：已有 `Users`、`Authenticated Users` 或 `Everyone` read/execute 权限时不修改 ACL，否则后台 read helper 只为该 sandbox group 补 allow ACE，不向每个 workspace capability 扩散 read ACE。deny-read 在 child 启动前同步作用于同一个真实读取主体；与 Codex 一致，状态锁内先只物化并应用当前 desired set，再撤销同一 SID 的 stale paths，已删除的历史路径不会被重新创建。由于该读取主体跨 workspace 共享，elevated caller 使用第二个 native `Global` mutex 覆盖 setup 到正常 runner cleanup，并在发送 spawn request 前登记一个随机 runner mutex；runner 在启动 child 前取得该 mutex，持有到 Job Object 完整清理后才释放。父进程正常路径仍在执行锁内消费登记；父进程崩溃或 IPC 中断时，下一 caller 必须先等待 runner mutex 并回收登记，不能在旧 Job 清理完成前重配共享 ACL；等待每 50 ms 检查取消。状态 schema v4 只把本次在无既有同 SID deny 时实际新增的显式继承保护及其完整同 SID deny-ACE 指纹记为 runtime-owned，并在写 ACL 前原子记录 pending ownership；进程中断后按当前指纹提升或丢弃 pending，已完成但未提交的 stale revoke 也可幂等恢复。撤销前必须重新读取 DACL 并与持久化指纹完全一致，否则保留 stale 记录并 fail closed。已有充分 ACE 只用于 enforcement，不取得撤销所有权，v1/v2 无指纹状态迁移为 unmanaged 并永不删除，v3 指纹状态无损迁移。Windows `REVOKE_ACCESS` 不能可靠删除 deny ACE，因此撤销原语复制现有 DACL，只删除 runtime 生成的 current-object 与 inherit-only 传播 ACE；与其他拒绝合并的 ACE 会拒绝部分削弱并 fail closed。状态文件先解析并验证 canonical 最近存在祖先，再让 mutex key 与锁内全部 I/O 共用该唯一 path identity，并原子替换，使普通、verbatim 和 junction 表达共享同一跨进程协调边界且 alias 重定向不能拆分锁与 I/O；撤销失败会保留 stale 记录，后续重试仍只撤销、不物化。`WRITE_RESTRICTED` fallback 只用 restricting SID 约束写入，不能权威执行 capability deny-read，因此存在 deny-read override 或 trusted workspace-preparation authority 时明确拒绝 fallback，只允许 elevated identity。
- Elevated 与 restricted-token 产品入口只解析一次 `sandbox_home` 的 canonical 最近存在祖先，并在完整 setup、execution mutex、runner lease、state/cap I/O 和 cleanup 生命周期复用该路径；单次 lease reconcile 也复用首次锁定的 state path，不在等待后重新跟随调用方 alias。父进程独占持久化 deny-read state，隔离 runner 只打开父进程通过受保护 IPC 交付的随机 lease mutex，不跨身份读取真实用户的 state path。
- Windows workspace-write command 在 setup/ACL 完成后、runner spawn 前注册递归 `ReadDirectoryChangesW`，并在 Job Object 收敛后把观察投影为 `Changed` / `Unchanged`；监视注册、读取、取消、缓冲溢出或无法判定均保持 `Unknown` 并由 WorkspaceTools fail closed。该 OS adapter 是 command mutation 的事实生产者，WorkspaceTools 只推进逻辑 revision，不用测试或 Evaluation 结果反推变更。
- cancellation 在权限 setup 前后及 runner spawn 边界检查；setup refresh/提权 credential setup 期间不能中断其本身，但取消不会继续启动 sandbox child，已启动 child 仍由 runner control transport 和 Job Object 收敛。
- child environment 删除 secret-like 变量，并把 pip/npm cache 隔离到可写 `TEMP` 下的 Singularity 专用目录，避免读取宿主用户 cache；输出有界并再次做敏感标记检查。普通命令使用 `host_sanitized` 环境策略；Evaluation 的 setup、baseline、Agent command、public 与 hidden 统一使用 `evaluation_isolated`，额外移除 `SINGULARITY_*` 以及会重定向或注入 Cargo/Rust、Node、Python、Go 构建行为的宿主覆盖变量，同时保留 PATH、系统目录、TEMP 和工具链 home。
- Evaluation 的固定 source preparation / evaluator isolation 操作（当前仅 `git clone`、固定 commit `checkout`、临时 `git init` 和固定参数的 evaluator `git apply` 检查/应用）使用进程内可信请求来源，使这些控制面步骤可以创建或更新自己的临时 Git metadata；test patch 在进入该边界前已经由 manifest schema 校验并由控制面直接物化，不来自模型。evaluator isolation 使用碰撞即拒绝的私有临时 Git dir，并通过固定 `--git-dir` / `--work-tree` 参数与 workspace 的受保护 `.git` 完全隔离；成功或失败都清理该控制面目录和 patch 文件，不撤销普通 setup/verification 已安装的 `.git` deny。该来源不序列化、不进入 JSON schema、不可由 task manifest command 或模型脚本选择，反序列化后强制恢复 protected-path enforcement。Windows adapter 只为该来源移除自动生成的 workspace metadata 默认项，显式 deny 仍保留，并要求 elevated identity，不在失败后转入 restricted-token fallback；其变化证据使用同一 capability snapshot，不启用会把合法 Git metadata 写入与普通 protected-path 攻击混为一谈的递归目录监视器。命令仍受相同 workspace root、network policy、Job Object、timeout 和 cancellation 约束；manifest setup/verification 与模型 command 不获得该能力。
- 父进程正常退出、timeout 或 cancel 都会在 join stdout/stderr capture reader 前关闭或终止 Job Object；elevated runner 的 control transport EOF/read error 也会终止其中的进程树。
- `local_process_fallback` 始终为 false；没有无沙箱 executor。

Linux adapter 在独立 user、mount、network 与 PID namespace 中执行 argv，设置 `no_new_privs`、seccomp 和 Landlock，并清理 secret-like 宿主环境。私有 `/dev` 只暴露 bind-mounted `/dev/null`、`/dev/zero`、`/dev/random` 和 `/dev/urandom` 字符设备；设备 staging 在 mount setup 完成后由新的私有 `/run` 覆盖，不向命令暴露宿主路径或额外设备。Executable resolution 将规范化 `execve` target 与保留最终 symlink 名称的 `argv[0]` 分离，使 Python venv 和 rustup proxy 保持真实调用身份；未知非标准 executable 只获得自身文件的 read/execute 能力，不再从 `bin` 或祖父目录猜测宽泛 runtime root。标准只读执行闭包还包含 `/usr/libexec`，使编译器等程序通过标准 helper 的嵌套 dispatch 仍受同一 Landlock read/execute 规则约束，而不授予写权限。Evaluation 把 Task Set 已声明的 smoke executable（其中 bare/absolute executable 已通过 run-level preflight）作为不进入模型输入的 `CommandScriptRequest.runtime_executables` 绑定到对应 `WorkspaceTools`；Linux adapter 只解析这些可信 executable 的既有只读 runtime closure，使受控 shell 的动态分派与 direct-argv verification 使用同一运行时资产，同时不把整个网络配置投影给普通 shell command。Python venv、rustup、NVM 和 `/usr/bin/env` shebang 只按已验证布局投影必要的只读文件或 toolchain 目录，文件与目录使用各自的 Landlock access mask；自定义 rustup 布局只有在 proxy 位于规范化 `CARGO_HOME/bin`、目标为 `rustup` 且 `RUSTUP_HOME` 可规范化为目录时才获得只读能力。child 与 derived child 继承相同的 namespace/seccomp/Landlock 边界；workspace 外访问和 denied network 继续 fail closed。network-denied seccomp 允许 Rust 等运行时创建只存在于进程树内部的 Unix `socketpair` 并传递 exec status/pidfd，但仍拒绝 socket 创建、connect/bind/listen/accept，并由独立 network namespace 隔绝外部网络。timeout/cancel 显式终止 process group；正常 PID namespace init 退出由内核清理 namespace descendants，当前实现不在 leader 已被 `waitpid` 回收后再次按旧 PGID 发送信号。回归通过 delayed marker 证明 normal exit、timeout 与 cancel 后都没有 orphan 副作用。WSL 的 namespace/mount 资源在默认并行测试下可能争用，因此当前同机全量 sandbox 验收使用 `--test-threads=1`；这不是放宽 runtime 合同。

Linux `workspace-write` 不把真实 workspace 作为普通可写目录交给不可信命令：控制面先在 transaction-owned 临时目录中物化一份绑定初始树状态的 immutable lower snapshot，child 只能看到以该快照为 lower、私有 upper/work 为写入层的 OverlayFS view。upper 通常为空；为避免依赖内核可选的 OverlayFS inode index，仅把 workspace 内部 hardlink group 复制一次并在 upper 内重建 aliases，避免 copy-up 破坏运行期 link identity，同时不让 lower 或真实 workspace inode 进入可写层。真实 workspace 不进入 child 的普通挂载视图；受保护根只在快照中保留同类型空挂载点，再以现有只读 protected-path bind mount 覆盖。命令退出后，控制面一次捕获完整 baseline、一次验证 upper 并冻结全部操作计划（路径集合、类型、正文摘要、对象身份、链接关系、权限、时间、xattr/特殊对象和 protected-path 不变量）；提交阶段不按操作重复扫描整棵 workspace，而是通过计划时固定的父目录 capability，在每个 `renameat2(RENAME_NOREPLACE)` 线性化点重新核对父目录身份并只验证目标 leaf/subtree，最后再执行一次完整树验证。`.env`、`.git`、`.agents`、`.singularity` 及同一 protected-path 合同下的未来表示始终拒绝。transaction baseline 与 final-state 复核跳过受保护目录的内部树，但继续绑定该目录本身的类型、权限、设备与 inode；因此 AppServer 对 `.singularity` 的并发持久化不会伪造 Agent mutation，也不能掩盖受保护对象替换或普通路径变化。提交期间的并发删除、替换、父目录重定向或无法证明的对象归属会返回 typed drift/rollback failure；rollback 使用相同的 pinned-parent 与 leaf-only absence 语义，只恢复能证明属于本事务的对象，不复活或覆盖并发状态。metadata 变更先移入私有 metadata 区，在 pinned handle 上验证并原子装回；嵌套 metadata 操作留下的空目录只在 transaction-owned 私有区清理。必要 OverlayFS、Landlock 或其他内核能力缺失时只返回 typed capability blocker，不 fallback 到真实 workspace 直接写入。commit area 位于 workspace 外且不进入 child 的 Landlock 可写集合。

transaction 的 lower snapshot、upper capture、stage、apply 与 rollback 都把 symlink target bytes 当作不透明对象原样复制和比较，不解析或访问目标；目标在 child 中能否访问仍由 mount namespace 与 Landlock 决定，protected path 仍按 workspace 路径组件拒绝。

### Linux 威胁模型与能力矩阵

Linux sandbox 将 command 及其全部派生进程视为不可信。受保护资产包括绑定 workspace 之外的文件、workspace protected path、宿主网络、宿主 secret 以及超出 timeout/cancellation 生命周期的派生进程。该 adapter 假设内核和调用账户本身不是恶意的，不承诺抵御特权宿主或内核被攻破；必要能力缺失时只返回明确的 unavailable/unsupported，不切换到本地进程。

| 控制项 | 运行时探测 | 强制与失败边界 | 验证入口 |
| --- | --- | --- | --- |
| User namespace | `LinuxSandboxProbe.user_namespace` | `CLONE_NEWUSER`；缺失时 strict 不可用 | `linux_probe_reports_kernel_controls_without_os_handles` |
| PID namespace | `LinuxSandboxProbe.pid_namespace` | `CLONE_NEWPID`；PID 1 退出与 process group 终止共同约束派生进程 | `linux_probe_reports_kernel_controls_without_os_handles`、`linux_timeout_and_cancellation_remove_orphaned_process_tree_side_effects` |
| Mount namespace | `mount_namespace` | `CLONE_NEWNS` 与 Landlock 路径规则隔离文件系统视图 | `linux_read_only_and_protected_mounts_reject_writes`、`linux_path_traversal_and_symlink_escape_are_denied_by_landlock` |
| Network namespace/seccomp | `network_namespace`、`seccomp` | denied 模式使用 `CLONE_NEWNET` 并拒绝网络 syscall；allowed 模式由显式策略选择 | `linux_network_seccomp_denies_and_allows_socket_creation` |
| `no_new_privs`/Landlock | `no_new_privs`、`landlock_abi >= 3` | executable/runtime 路径和 workspace 读写 mask 均 fail closed | `linux_nonstandard_executable_does_not_authorize_its_sibling_secret`、`linux_workspace_write_is_enforced_and_observed` |
| 环境与 protected path | policy-bound command preparation | 删除 secret-like 变量；执行前拒绝 protected path 及不安全 link/hardlink | `linux_child_inherits_secret_isolation_and_kernel_restrictions`、`linux_external_hardlink_is_rejected_before_execution`、`linux_read_only_and_protected_mounts_reject_writes` |
| 生命周期 | `process_tree_cleanup` | process group 终止与 PID namespace 清理覆盖正常退出、timeout 和 cancellation | `linux_normal_exit_removes_orphaned_process_tree_side_effects`、`linux_timeout_and_cancellation_kill_the_process_group` |
| cgroup v2 | `cgroup_v2`、`cgroup_delegated` | 仅观测；当前不附加或使用 cgroup controller，因此不参与 strict-ready；生命周期 enforcement 仍由 process group/PID namespace 提供 | probe 测试断言 |

AppServer 从当前绑定的 `SandboxBackend::capabilities()` 投影 AgentLoop 可用性；Agent 核心不判断宿主平台。strict 判定依赖文件系统、网络、环境、路径准入、进程树终止、超时和输出限制等平台无关保证，不要求 Windows restricted token、Job Object 或 Linux namespace 机制字段。默认 Windows adapter 声明 strict 能力时不会提前触发 UAC probe，真正 setup 和权限检查发生在第一条 command 上；Linux probe 只有在 user/mount/network/PID namespace、`no_new_privs`、seccomp、Landlock 和 process-tree cleanup 全部满足时才声明 strict。其他平台明确返回 `strict_command_sandbox_unavailable`。任何运行失败通过 tool/evaluation blocker 暴露，Evaluation 也只有执行元数据明确为 `Strict` 的命令才计入 strict sandbox 证据。

## 10. Cancel 与 Shutdown

每个活动 turn 在 app-server 内注册一个 `CancellationToken`，并由 file-backed request worker 附加独立的 cancellation monitor。monitor 的检查结果是 typed `UserCancellation` 或 `InfrastructureFailure`：SQLite `busy`/`locked` 通过 Store 的稳定 transient-contention 分类继续轮询；其他 Store 读取失败会记录基础设施失败并取消 token，不会被归约成普通 interrupted/cancelled。

`turn/interrupt`：

1. 先在同进程 registry 调用 `cancel()`。
2. 调用 `SessionStore::request_turn_cancellation`：只有纯 pending Approval 分支会在该事务内直接写成 `interrupted/cancelled` 并删除 request/checkpoint；普通运行或存在 `executing` Approval 时只写 `agent_loop_status=cancel_requested` 并追加 request trace，Turn 的持久化 `status` 暂时保持原来的 `running` / `blocked`。
3. worker 的 cancellation monitor 也轮询 SQLite，因此另一个 CLI/app-server 进程发出的 interrupt 可以传播到原 worker。

provider HTTP wait、AgentLoop 回合边界和 sandbox command 都检查同一个 token。monitor 的 `InfrastructureFailure` 在下一副作用边界前停止 AgentLoop/approval/terminal outcome 继续执行，并由 turn orchestration 交给 Store 的 typed `InfrastructureFailure` authority 原子归约为 `Failed`；即使持久 turn 已是 `cancel_requested`，该 authority 也不能伪装成 `Interrupted`、`Completed` 或其他状态。`UserCancellation` 才允许归约为 `interrupted/cancelled`。普通运行或 `executing` Approval 的 interrupt response 报告 `cancel_requested`，但不提前发送 terminal event；worker 最终把结果提交为 `interrupted/cancelled` 后才发送 terminal item/event/response。`commit_turn_outcome` / `commit_turn_outcome_and_resolve_pending_execution` 的普通 AgentLoop authority 会重新读取当前 Turn，并在 `agent_loop_status=cancel_requested` 时拒绝非 `Interrupted` outcome；基础设施 authority 只接受 `Failed`，并在 approval continuation 的同一事务中清理 executing checkpoint。AppServer 把晚到的 provider、assistant 或 tool 结果按当前 typed outcome 归约后再提交，因此晚到结果不能把持久化 Turn 改回 `completed` 或以普通错误覆盖 `Interrupted`。monitor 的 outcome 发布是 CAS：只有成功发布结果的 owner 才取消 token；终态提交前 stop/wake 后冻结结果，teardown 超时也先以基础设施失败冻结再 detach，晚到 monitor 不能改变 token 或终态。`server/shutdown` 先锁存全局 execution stop，再取消所有当前或随后登记的 turn，并在同一个全局 5 秒宽限期内等待 request worker 与 stdout writer 收敛；仍未响应时进程以失败状态退出，让 OS 释放执行锁，后续进程再按持久状态恢复，而不是无限等待。

## 11. Store、Trace 与 Artifact

`SessionStore` 使用 rusqlite bundled SQLite，开启 foreign keys、WAL、secure delete 和 busy timeout。默认路径为启动目录下 `.singularity/rust-app-server.sqlite3`。同一 Store namespace 中每个 canonical workspace 另有一个稳定、空内容、不会删除或重建的相邻 sidecar lock file；`WorkspaceExecutionGuard` 只持有标准库 `File` 锁，不把平台句柄或 sandbox 语义泄漏到 Agent、Protocol 或 Evaluation。锁文件名只包含带命名空间的 workspace identity 的 SHA-256，不保存原始路径、prompt、工具参数或用户内容；文件锁随进程/handle 关闭释放。缺少 workspace 的遗留 thread 使用 thread-scoped identity 仅供安全恢复，不能启动 AgentLoop；`:memory:` Store 不提供跨进程执行所有权并 fail closed。当前且唯一写入 schema 为 v13：除 v11 的稳定 Thread/Turn/Item/Approval plain-text enum、tagged `PermissionResource` 与 decision/checkpoint 合同外，`trace_events` 还持久化并交叉验证 `span_id`、`parent_span_id`、`span_kind`、`span_phase`、`span_status`、`duration_ms`、`time_to_first_token_ms`、`span_projection` 与 `metric_samples`。数据库约束只接受稳定 enum/phase/status 值，Start 不允许 terminal status/duration/metric，End 必须带 terminal status/duration，standalone metric event 不伪造 span；v13 另外加入每个 Turn 唯一的 `turn_checkpoints`、按 execution id 唯一的 `tool_executions`、引用 `ItemKind::UserMessage` 的 `turn_inputs`、`turns.pause_requested`，并把 `paused`/`suspended` 纳入 Turn 状态约束。v1-v10 打开时先将完整 schema fingerprint 与已发布合同匹配，再在首个写入前只读解码 enum、approval、trace 和关系字段；pending AgentLoop checkpoint 保持 opaque，不由 Store 解析。旧 schema 只要仍含未完成 AgentLoop checkpoint，就在首次 schema 写入前拒绝迁移并保持原库不变，不伪造或长期读取旧 checkpoint。没有 pending checkpoint 时，v10 的字符串 approval resource 只按已发布工具合同迁移：workspace tools 接受 canonical relative path，command 只接受完整 historical scope-digest envelope，`update_plan` 只接受自身 tool id；未知、歧义或非法非空资源在写入前拒绝。v11 基线转换、默认值、约束、索引、foreign key 与历史关系校验在一个 `BEGIN IMMEDIATE` 事务中完成；随后迁移到 v13 时把可判定的历史 trace 投影为规范列并验证 parent/lifecycle/metric，创建 checkpoint、execution、interactive-input 表和状态约束，不可判定或冲突数据会回滚。已标记的 v13 数据库每次 open 仍校验 schema marker/default/index、approval JSON 绑定、trace 列与 payload 的完整一致性；`0002_durable_ledger` 仅作为历史 migration id 保留，当前语义是 decision/event history，不是防篡改账本。`pending_tool_calls.execution_state` 只允许 `pending` 和 `executing`；`turn_checkpoints` 只接受 JSON object 和正数 checkpoint version；`tool_executions` 只允许 `running`/`unknown`；`turn_inputs` 只允许 `steer`/`follow_up` 与一致的 pending/consumed 时间状态。Store 把 checkpoint payload 作为 opaque serialized metadata，并在显式 request/thread/turn/tool-call/item 关系边界校验绑定。AppServer 在 persistence seam 通过 AgentLoop 的 typed `PendingApprovalOccurrence`/`ApprovalCheckpoint` 与 `TurnCheckpoint` codec 只解码当前版本并校验结构和 resource；旧版、未来版或错绑 checkpoint 在 approval decision 或 turn resume 前 fail closed。file-backed open 从文件系统 root 逐级持有 directory capability，所有父目录和最终文件均 no-follow；canonical path、held handle、唯一 link count 与平台 file identity 一致后，SQLite 才以 `NOFOLLOW` 打开已经创建的文件，并在任何会创建 WAL 状态的 pragma 前再次验证 namespace identity。trusted reopen 只能由已初始化的 Store 派生，复用相同 canonical identity 和 retained parent capability并执行结构快速校验，不重复全库 preflight。Windows capability/guard 句柄禁止 delete/rename 并拒绝 reparse point；Store 在实际行/事务边界继续校验显式关系绑定，不解析 checkpoint 字段。

主要表：

```text
threads
turns
items
trace_events
approvals
approval_decisions
pending_tool_calls
turn_checkpoints
tool_executions
turn_inputs
artifact_refs
schema_meta / schema_migrations
```

AppServer 先预分配 turn id，再完成 monitor Store reopen、可失败的 monitor thread spawn 和 active cancellation token 注册；monitor 在 Store 提交前保持暂停。任一准备步骤失败都不会留下 `Running` Turn。准备完成后，turn、输入 item、history page 和 started trace 在一个事务内生成，commit 后才启动 monitor 和发出事件。monitor 把 SQLite busy/locked 视为临时竞争，其他读取失败产生可检查的 `InfrastructureFailure`；终态提交前由 stop/wake 与有界等待收敛 monitor，随后冻结 outcome，超时则按基础设施失败 terminalize；guard teardown 只在短暂的内存 registry 临界区取得 `active_turns` mutex，保持有界，不让 request Drop 无界阻塞。终态 turn、可选 `ItemKind::Plan` plan item、assistant item 和 terminal trace 也在一个事务内提交。`persist_agent_approval_requests` 将整批 approval/checkpoint 交给 Store 的单一 `BEGIN IMMEDIATE` API；Store 先验证所有 request、thread/turn/tool-call/resource、唯一性和当前状态，再产生任何副作用，后续约束或 trace 写入失败会回滚整批，不留下部分 Blocked turn 或 checkpoint。`commit_turn_outcome` 返回 `CommittedTurnOutcome.plan_item`，app-server 只有在事务成功提交后才发送 `turn/plan/updated`（随后发送 assistant item events 和 `turn/completed`）；plan item 本身使用该 turn 的 item sequence 持久化。Approval continuation 的 `record_approval_decision` 在 claim 事务内返回绑定 turn 快照；claim 后的读取、provider、project-instruction 或 workspace 失败都进入统一 terminalization funnel，`commit_turn_outcome_and_resolve_pending_execution` 在同一事务内完成 typed outcome、plan/assistant/trace 写入、旧 executing checkpoint 删除和下一 approval/checkpoint handoff。turn sequence 和 item sequence 是每个父级内的严格正整数，用于恢复稳定顺序。

store 在写入 item、trace 和 artifact reference 前执行敏感文本检查。Artifact registration 必须在同一事务内确认 `run_id` 是真实 thread，若提供 `item_id` 则确认 item 属于该 thread 的 turn，并验证 kind、`artifact://` URI、SHA-256 digest、summary 和 metadata 合同；未知、跨 thread/turn/item、重复、含 ref 字段或 secret-like/protected 内容的输入 fail closed。读取时重新验证这些绑定和字段，thread 删除会级联删除其 artifact refs。当前 WorkspaceTools/AgentLoop 没有在模型可见路径前登记 backing artifact，因此不向模型投影 artifact refs；`artifact/fetch` 只返回真实已登记且仍绑定真实 thread 的脱敏 `ArtifactRef`，损坏或未绑定记录按 not found 处理，不直接提供任意文件读取。Evaluation 的 `PublicationArtifact` 仍是独立发布产物，不属于普通 runtime artifact ref。

Store 只持久化上游已经完成的 audit projection，再对 trace payload 做递归敏感文本脱敏和 hash 完整性校验；AppServer 在进入该边界前把 provider body、project-instruction 内容、workspace/raw error 和 raw arguments 投影为固定安全摘要及稳定 stage/cause/cleanup 字段，trace 只保存 typed provider diagnostic，不保存 validation 原文。`TraceEvent::for_turn` 固定 `run_id=thread_id`、`session_id=turn_id`、`task_id=turn_id`，所有 turn trace 写入在同一 `BEGIN IMMEDIATE` 事务内校验并插入，list/tail 批量预取绑定后统一验证。唯一可判定的历史 turn-shaped trace 按 `task_id` 归一化，存在歧义则拒绝；读取 roundtrip 不会恢复被投影丢弃的原始字段。`approvals` 直接保存并索引 `thread_id`/`turn_id`，approval request 每次读取都与 JSON payload 比较；Defer 保持 pending/resumable，不产生最终 decision history。`pending_tool_calls.payload` 不作为 Store 的第二 checkpoint schema，Store 只维护其显式关系、幂等和 execution-state；版本化 checkpoint 的唯一 codec seam 在 AppServer/AgentLoop。history 先解码所选 turn 的全部 status、kind 和 item status，再投影 user/agent，坏行不会被 SQL 过滤隐藏。

Trace span 的生命周期由 Store 的 typed API 负责：Turn/Approval/PromptAssembly/ProviderAttempt/ToolCall/PolicyDecision/SandboxExecution/Verification/FinalReview 等 span 只能由明确的 Start、状态变更和 End 事件组成；Start、End、metric sample 与对应 Turn 状态在同一 SQLite 事务中提交，重复 Start 只有在 identity 完全相同时幂等，identity 冲突、非法 parent、缺少 terminal duration 或未知状态均 fail closed。SQLite `trace_events` 是运行时唯一事实源，CLI、transport metrics 和后续导出只能查询或投影它，不得创建并行 collector、队列或内存 registry。Verification span 的 typed projection 保留 revision-bound plan 的 workspace `revision`，以及 bounded repair 的枚举 `repair_reason`、`attempt`/`max_attempts` 和 `required_revision`；repair 不再复用 command-count 字段，trace 只保存这些数字和稳定枚举，不保存原始 requirement、path、prompt、response 或 arguments。每次 AgentLoop execution epoch 内的 verification-plan lifecycle 从同一个私有单调 occurrence 序号源取得 identity ordinal，不能把 tool-call ordinal、completion rejection count 等独立计数域混用为同一身份；同一 occurrence 的 Start/End 仍共享 identity，checkpoint 恢复则由持久递增的 `resume_attempt` 切换 identity epoch。

Provider capability-cache metric 的 trace identity 来自同一个 typed lookup observation：绑定后的 Prompt parent、model-turn ordinal、protocol、Hit/Miss、真实 lookup 时间和 occurrence index 共同构成稳定身份。Blocked Turn 恢复时新的 model turn/parent 产生新的 identity，不会因每段局部 vector index 从 0 重启而与暂停前的 metric 冲突；同一 observation 被重复投影仍保持同一 ID，并由 Store 以相同事件内容幂等接受，内容冲突仍拒绝。`turn/resume` 在解码安全 `TurnCheckpoint` 后先持久递增 `resume_attempt` epoch；该 epoch 参与恢复段的 tool/provider occurrence identity，epoch 0 保留初始运行的兼容 identity，后续进程恢复不会覆盖旧 trace。

## 12. Evaluation

`sg eval run` 发送 `eval/run`，app-server 读取 `evaluation.task_set/v5` manifest 并执行 AgentLoop runner。task set 必须显式声明 1 到 32 的 `trial_count`；每个 task 仍声明用于任务集覆盖审计的非空 `capabilities`，Agent 输入则只声明版本化的 `required_tool_capabilities`。capability name 只受通用 snake_case 语法约束，实际合法性、版本和工具集合完全从当前 `ToolRegistry` 的 `ToolEntry.capability/version/exposure` 解析；Evaluation 不维护工具名或 capability 白名单：

```text
prepare source once as a read-only seed
  -> trial-0001/{baseline,agent,public,hidden}
  -> ...
  -> trial-N/{baseline,agent,public,hidden}
  -> publication/{result.json,report.json,evidence.json,publication.json}
```

Runner 在任何 Provider trial 前先一次性验证 Provider 配置，再按 manifest 顺序物化全部 task source；只有全部 source 成功才进入 task/trial 循环。任一 source preparation blocker 都发布一个 run-level typed blocker，`result.tasks` 与 trial 采样保持为空而 `configured_trial_count` 仍保留；已支持的 sandbox preflight 事实不会被改写成 sandbox blocker。

启动任何模型 trial 前，runner 先在本次 run 所在真实文件系统创建 run-owned scratch workspace，将它 canonicalize 为唯一绝对路径，再把同一路径作为 cwd 与 workspace root 传给选定 `SandboxBackend` 的完整 preflight 及其后续 probe。随后，runner 收集 task set 的 setup、baseline、public、hidden 和 agent smoke command 中由宿主解析的 bare 或绝对 executable，并要求同一 backend 使用 `EvaluationIsolated` 的真实净化环境完成只解析、不启动进程的 capability probe；workspace 内由前序 setup 生成的相对 executable 留到其依赖完成后的正常执行边界验证。任一固定 executable 明确不可用时发布 `sandbox_preflight_task_executable_unavailable`，backend 无法证明解析合同时发布 `sandbox_preflight_task_executable_unverified`；两者都在 Provider 创建和 trial sampling 前形成唯一 environment blocker，并把缺失 executable 或 probe capability 写入同一 preflight evidence。对 task set 中按 repository 去重后的每个 `RemoteGit`，随后在同一严格 backend 中以 `ReadOnly` filesystem 和显式 Allowed network 执行可信来源的 `git ls-remote --exit-code --no-tags <repository>`；该 probe 只验证远程仓库 transport 可达，不验证具体 commit、不修改 workspace，也不缓存或改写 manifest。只读 probe 以 completed、semantic success、零退出码、无 local-process fallback 和 strict enforcement 为成功合同；它不要求不存在的 workspace mutation evidence，但任何报告为 changed 的矛盾结果仍 fail closed。具体 commit 的存在性仍由后续真实 trusted clone/checkout 验证。只有所有远程 source probe 成功后，才用同一 trusted workspace-preparation 边界执行一次固定的断网 `git init`，证明 clone/checkout 所需的受保护 Git metadata 变化也能被严格 backend 确认；该写入路径及普通 verification 仍要求 workspace mutation 可验证。报告固定记录 profile、backend、OS/arch、kernel、filesystem，以及 OverlayFS、user/mount/PID/network namespace、no-new-privs、seccomp、Landlock、transactional workspace、network denied 和 protected-path enforcement。任何 remote source probe 失败在 provider/trial sampling 前发布 `sandbox_preflight_remote_source_unavailable` environment blocker，保留 configured trial count，但 sampled trial count 为零，且不调用 Provider、不创建 trial；trusted preparation 或其他能力缺失同样只发布一个绑定稳定错误码的 blocker。scratch 创建、canonicalization、清理或报告合同失败同样 fail closed，不在 publication 或 task workspace 上执行替代探针；没有 relaxed、local-process 或 no-sandbox fallback。

trusted workspace-preparation 的来源标志只由进程内 Rust API 构造，并传递到平台 adapter。Linux adapter 具备隔离 staging、漂移校验和失败/取消 rollback 时，才可证明该 transactional workspace 合同；当前 Windows adapter 尚未具备这些所有权不变量，因此 preflight 在任何写探针前以 `sandbox_preflight_trusted_preparation_unsupported` fail closed，并把 `transactional_workspace` 与 `trusted_workspace_preparation` 列为缺失能力，不会在真实 workspace 写入探针或启动 Evaluation provider。Windows 普通命令仍保持现有严格 sandbox、断网和 protected-path 合同；可序列化请求、manifest 命令和模型工具调用不能获得 trusted preparation 来源。

每个 stage 都通过同一个 `SandboxBackend` 执行 command。全部 task source 在 run-level barrier 中各准备一次；成功 source 只作为各 trial 的只读复制种子，任一 source 失败则不创建 trial。远程 source 的联网 clone 与断网 checkout 在 task 级只准备一次；每个 trial 的 agent、evaluator 四阶段、trace 和 patch evidence 都位于独立目录，不复用可变 workspace。baseline 和 public 使用 `public_test_patch`，hidden 只使用 `hidden_test_patch`；两者以及 baseline/public/hidden 命令都不进入 `AgentTaskProjection` 或模型 payload。Evaluation 按 capability requirement 解析最终模型可见工具并从同一注册表生成 Policy、prompt 中的工具列表与 tool schema 指纹；模型侧 smoke 输入使用 `command` 字符串、`cwd` 和 `timeout_seconds`，manifest 内部 `CommandSpec.argv` 仅供可信执行器和 scope 计算。逐命令 network/filesystem 策略在 Agent/Policy 侧绑定，不通过 exact binding 注入模型输入；完成门使用规范化后的实际 cwd 与 bound sandbox/network 计算同一 command-script scope。Windows runner 在创建 run 前把 publication、task source、最大 trial 目录、审计产物和四阶段深路径纳入 legacy `MAX_PATH=260` 的保守预算；超限直接拒绝。

模型提交结构无效的 command arguments 时，AgentLoop 在 policy 与 executor 前返回稳定的参数原因码，并投影有界的 `content.validation_code`、`content.retry_inputs` 与 schema 提示；`retry_inputs[*].command` 始终是字符串，runtime 不把错误类型自动转换或拼接成另一种输入。普通 trace 只记录原因码和未执行状态，不记录 raw arguments 或完整 content。是否允许下一模型回合只由稳定的 `failure_kind` 和少量 execution code 决定，与 task、provider、错误文本或当前 verification 分数无关；被拒绝的调用不执行，也不会因 verification 已满足而进入专用收敛分支。

`result.json` 使用 `evaluation.result/v8`，并与 `evaluation.evidence/v3` 共同绑定 sandbox preflight 和 configured/sampled trial count。采样前的 run-level blocker 使用同一 schema 表达：保留配置 task/trial 分母，`sampled_trial_count`/`trial_count` 为零，`tasks` 为空且只允许 Environment、WorkspacePreparation、ProviderConfiguration、Network 或 Sandbox 等稳定 blocker 类别；不把 ProviderResponse、ProviderAuthentication 或 AgentRuntime 等采样后原因提前归约。正常 run 的每个 task 保存按 1 起始连续编号的全部 `EvaluationTrialResult`，trial 分别记录四阶段、`agent_completed`、`tests_passed`、`evaluation_passed` 及原有 patch/tool/model/approval/plan/recovery/provider/token/latency/sandbox/fallback 摘要。单 trial 必须报告 `stable=false`；多 trial 只在不存在 blocked 且所有 trial 的状态门禁一致时报告稳定，并为非 blocked trial 的 model turns、tool calls、agent duration、provider latency、provider retry 和 total token 保存有限的 min/max/mean/population variance。task/run summary 同时重算 completed、failed、blocked trial 数；agent scored denominator 仅为 completed+failed，blocked 不计 agent success、agent failure或成功率分母，但仍保留在总 trial denominator 与 typed blocker 中。run 的核心门禁使用 task success rate（所有选定 task 为分母）；trial success rate 仅作为非 blocked trial 的稳定性观测，不能替代 task rate。

`report.json` 按 trial 保存 source/baseline/agent/public/hidden 命令诊断、scope digest、逐文件 before/after SHA-256、allowlist、patch evidence、trace 路径和脱敏 provider diagnostic。`evidence.json` 使用 `evaluation.evidence/v3`：task 级绑定 manifest、task selection、prepared source、allowed paths 和 capability requirements 摘要；run-level 零采样 blocker 只保留 task identity projection，不生成 trial evidence；正常 trial 级绑定 changed paths、patch、trace、四阶段 scope、fallback 观察、真实 prompt 的安全结构投影与 SHA-256、最终实际 tool schema SHA-256，以及不含 provider/model 原名、endpoint 或凭证的 provider/model/negotiation 指纹、API protocol 与 contract/metadata 指纹。完整脱敏 protocol contract 和 capability metadata 保存在该 trial 的 `agent-trace.json`，evidence 通过 trace digest 绑定它们；原始 prompt、raw response、raw arguments、完整 `ToolResult`、密钥和 base URL 不落盘。passed trial 必须同时具备 source、trace、prompt/schema 和已协商 provider 证据。

`report.json` 只保存诊断与 task/trial 投影，不嵌入完整 `EvaluationResult`；result、report、evidence 的唯一发布入口仍是同一 `publication/publication.json` manifest。

Evaluation runner 通过 AppServer 传入同一 file-backed `SessionStore`，每个 run/task/trial 建立显式的 external Task/Turn typed span；Agent 阶段使用 `AgentLoop::run_with_events`，回调事件和最终 provider/cache observations 由同一 `TraceProjector` 写入 SQLite。trial 目录中的 `agent-trace.json` 使用 `evaluation.agent-trace/v2`，只由 Store 查询并导出；`AgentLoopResult` 仍用于 result/report 的安全指标投影，但不是 Evaluation trace 权威。Evaluation 的 trace 写入失败会作为 infrastructure blocker 阻止 publication，不会只写文件后继续发布。

## 12.1 OpenTelemetry 边界

仓库当前没有 OpenTelemetry exporter。未确定 endpoint、TLS/auth、Collector 可用性、队列容量、重试/drop、flush 和 shutdown 合同前，不增加 no-op 或占位 exporter，也不把 SQLite trace 的成功误报为外部遥测发送成功。后续若产品合同明确要求外部导出，必须先定义这些边界并以 SQLite typed trace 作为可审计本地事实源；否则保持未实现并明确报告。

Smoke observed scope 直接来自同一份最后 mutation 后的 terminal successful-command suffix；失败 command 即使携带相同 digest 也不会进入 `observed_scope_digests`，成功但缺失/非法 digest 的 command 会清空旧后缀，其他合法成功 command 会占据后缀位置，因此 smoke 后的潜在写命令不能沿用旧验证。顺序与重复 command 保持为有序多重集合，Evaluation 不通过 set 去重或另行重算 gate 事实。
Agent workspace 的 before/after 快照保留完整 materialized tree；仅在生成 agent 变更与 patch evidence 时，Evaluation 才按闭集规则忽略非 source-owned 且由常见工具链生成的派生路径。未知路径以及 pristine source 已存在的路径仍进入 manifest allowlist 判定并 fail closed；该归因过滤不改变 source tree digest、`evaluation.evidence/v3`、`evaluation.result/v8` 或任何 gate 语义。

默认产物目录为 `work/evaluations/<run-id>`。result/report/evidence 与 `publication.json` 先完整序列化并 fsync 到同一 staging directory，再以一次 directory rename 固化为不可变 `publication/`。manifest 保存三项相对路径、逐文件 SHA-256 和 artifact-set SHA-256；消费者只把 `publication/publication.json` 视为发布入口。崩溃发生在 directory rename 前时只留下 staging，rename 后四项同时可见，因而不会暴露混合版本或半发布三元组。采样开始后的 infrastructure、publication 或 cancellation failure 不删除已经提交的 task/trial 目录；runner 在 run 根目录原子写入 `evaluation.failure/v1` 的 `failure.json`，只保存稳定 failure kind 和经敏感文本检查、长度受限的错误摘要。该文件用于诊断未发布 run，不能替代 `publication/publication.json`，也不改变 Evaluation 评分。

## 13. 失败与安全不变量

- 不支持的平台、缺失 binary、缺失 provider、无效 workspace 和 sandbox setup 失败都返回明确错误，不切换执行路径。Agent 请求的可执行文件不在宿主 `PATH` 或绝对路径不可用时返回 `command_executable_unavailable` capability，允许模型在有界下一回合提交新的合法 command string；Evaluation 自身固定命令的同一事实归为 environment blocker。两条路径都零执行、不伪装成 sandbox 不可用，也不暴露完整 PATH。
- CLI 使用 method 对应的 typed params/result 和 `JsonRpcId` 关联请求，只把 matching response 之前的 notification 与 response 关联；EOF、child exit、timeout、非法 envelope 和 JSON-RPC error 都是非零退出。
- thread workspace 必须是存在的绝对目录；archive thread 不能开始或恢复 pending turn。
- protected path、workspace 越界、非法 tool arguments 和扩大 sandbox/network 权限在执行前拒绝。
- approval 必须显式绑定 thread、turn 和 tool call，不能重放。
- approval checkpoint 缺失、版本未知、身份错绑、消息/tool-call 顺序不合法或重复消费 grant 时 fail closed。
- `tool_executions` 的 owner 丢失只将 `running` 归约为持久 `unknown`；unknown execution 会阻止 `turn/resume`，不得自动重放外部工具副作用。
- 只有包含完整安全 `TurnCheckpoint` 且不存在 unknown execution 的 `paused`/`suspended` turn 才可由单 owner `turn/resume` 认领；resume attempt epoch 递增后才生成新的 trace occurrence identity。
- `turn_inputs` 只记录 `ItemKind::UserMessage` 的消费关系；终态提交与 pending input/pause 检查在同一事务内，不能跳过已接受的用户输入。
- cancelled turn 的晚到结果不能恢复为 completed。
- evaluation 的 fake/mock 测试只用于确定性回归，不能替代真实 provider + AgentLoop 证明。

## 14. 维护规则

修改以下任一事实时同步更新本文对应部分：crate 边界、release binary、protocol method/object、thread/turn 状态映射、provider 配置、tool schema、policy/approval、sandbox、store schema、trace、evaluation stage 或 artifact 路径。

完整收口至少运行：

```text
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings
cargo test --workspace --all-targets --locked --no-fail-fast
git diff --check
```

影响 AgentLoop、provider、工具、sandbox、approval、evaluation、trace 或 completion 时，还必须运行一次真实 provider 的 `sg eval run` 并核对 result、report 和 trace。
