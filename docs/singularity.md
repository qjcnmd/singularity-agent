# Singularity 架构说明文档

> **本文档描述 Singularity 当前有效架构事实**，以当前源码与协议为权威依据。
>
> **维护规则**：修改以下任一事实时同步更新本文：进程边界、事件流、会话格式、Compaction、工具面与工具语义、Provider/模型能力声明、配置 schema、评估工具、发布二进制。

## 1. 总览与进程架构

产品形态与共享运行时语义：

- **无交互单次入口**：`sg --print <goal>` / `sg --json <goal>`。`--print` 只向 stdout 输出最终 assistant 文本；`--json` 输出逐行 JSONL 事件并以 `{"summary":{…}}` 终态行收尾。`--model` 只覆盖本次执行；`--session <id>` 恢复既有 Thread；`--no-session` 以临时 home 关闭持久化；默认持久化会话。
- **交互式 TUI**：`sg` 无参数进入长驻终端界面。
- **桌面端**：规划形态。接入时以 stdio JSON-RPC 适配层把桌面端接到共享 runtime（协议设计历史与并发约定见决策记录），不构成独立执行核心。

分层：

- **crates/runtime**：Thread/Turn 生命周期协调与单轮执行管线，是 Turn 执行的唯一所有者。
  - [`TurnRunner`]：一个 turn 的完整管线——会话打开/修复（单写者所有权贯穿全程）、项目指令装配、`operation_started` 记录（携带本 turn 冻结的模型配置快照）落盘后发布 turn/started、Agent 执行、typed 事件投影、`operation_finished` 单条终态（status/usage/truncated）落盘后发布终态事件、fail-stop 收敛。准备阶段 fail-fast：任何失败不留 operation 痕迹。
  - [`Conversation`]：一个 Thread 的长驻协调器——单活动 turn 链窗口（`reserve_start` 原子预订，路由层可在负载线程启动前确定先到先得）、steer 注入当前轮 inbox、followUp FIFO 队列（当前轮可信终态后自动逐条执行为独立新 turn；队列是进程内存态，当前进程存活期间按 FIFO 消费已接受输入）、取消令牌按轮独立（取消只作用于当前轮）、设置时序（提交点只校验并更新内存投影，运行中同样接受；落盘由下一 turn 开始时由 turn 在自己的会话写者上记录，见 D-056）。
  - [`ThreadCatalog`]：持久化 Thread 目录与只读投影的唯一入口（create/list/resume/rename/archive/read_thread_summary/paged_read），会话目录与进程级写者锁协调器由目录自身持有；目录操作直接实现在本对象上。只读状态以 ledger 为事实、以协调器中已落盘但未终结的本进程 run 为活动证明：该 run 投影为 `running`，无活动证明的开放 operation 投影为 `interrupted`。`Conversation` 不持有线程目录 CRUD。归档拒绝条件分两层且各自拥有、互不合并：跨进程写者（`ThreadCatalog::archive` 持锁拒绝 `WriterActive`）、当前绑定会话（TUI UI 策略）。
- **crates/cli**：入口解析与三种渲染（TUI / 文本 / JSONL）。TUI 与无交互模式进程内调用 runtime 的 `Conversation`；渲染只消费 typed `TurnEvent`，投影失败只丢弃投影，不影响执行事实。
- **headless core（库）**：
  - `AgentLoop`(三层分层循环):turn 步循环(steer 注入→轮步→响应持久化→工具批次→循环决策)→ **轮步层**(发送前基于上一轮真实 provider usage 主动压缩,usage 缺失时回退上下文条目估算求和;Provider 显式 `ContextLengthExceeded` 时强制压缩并重建请求,预算按 turn 计、每 turn 至多一次)→ **采样请求层**(按 `TurnRequestSpec` 装配请求一次,独立重试包装:可取消的固定指数退避、≤3 次、尊重 Retry-After,内部仅重发同一请求)→ **发送层**(`stream_completion` 纯发送,不感知压缩与重试);单一原子 `TurnInbox` 承载 steer;每个执行边界（step attempt、provider 观测、tool 启动、已注入转向）先落 durable ledger 记录再发对应事件；已发布的 assistant 文本在请求失败或取消时仍以该 attempt 预分配的结果 id 落入会话，终态由 operation 独立表达；每轮**请求后**保存真实 provider usage 供下一轮发送前判定;
  - `session` 子系统：严格 JSONL v4（format/file/manager/context/repair/operation/writer_lock/test_support）；线性消息与压缩条目是模型可见历史，metadata 条目只承载 thread_settings/thread_name，`type:"record"` 条目承载单 lane operation ledger 事实：`operation_started`（run 意图携带本 turn 冻结的模型配置快照与规范化不可变的用户输入）/`operation_finished`（run 的唯一终态事实）/`step_attempt`/`provider_attempt`/`write_deferred`/`write_abandoned`/`tool_started`（含 replay 分类与预分配结果 id）/`control_accepted`（FIFO 接受序号与归宿）；会话 JSONL 是唯一持久事实源；
  - `compaction`：摘要引擎与切点策略（ToolCall/ToolResult 成对保留）；每个 `firstKeptEntryId` 必须指向既有的模型上下文条目，否则以 `invalid_compaction_anchor` 拒绝；独立压缩无论完成、失败或中断都先写入匹配的 `operation_finished` 再返回;
  - `tools`：固定六工具注册表（read/glob/grep/bash/edit/write），多工具批并发执行（单批至多 8 worker），同文件 `edit`/`write` 互斥，结果与落盘仍按模型给定 source order；
  - 资源加载：AGENTS.md root→cwd 逐层合并，预算超限截断并向客户端发诊断；
  - `singularity_model`：types/error/provider/openai(chat,responses)/transport/config。
- **共享事实**：`~/.singularity/config.json`（全局配置）、`~/.singularity/auth.json`（私有认证）、`~/.singularity/sessions/<uuid>.jsonl`（会话正文）。`auth.json` 的 owner-only 权限校验为 Unix 语义（0600）；Windows 上依赖用户目录 ACL，不额外检查文件权限。
- **依赖方向**：cli → runtime → {core, model, agent, protocol}；agent → {core, model, protocol}——agent 只使用 protocol 的共享词形类型（`TurnModelUsage`/`TurnStatus`/`DiagnosticSeverity`/`ProviderAttemptStatus`），不依赖 runtime/UI；protocol 不依赖 runtime/model/agent。runtime 的对外表面：协调器与目录经 crate 根，事件出口经 `events` 模块接缝，公开对象类型经 `objects` 模块接缝（均为 protocol 类型的导出，无第二份定义）。

## 2. 无交互主调用链

```
sg --print|--json <goal>
  ├─ 解析参数（--model/--session/--no-session）
  ├─ 解析 SINGULARITY_HOME（或临时 home）并准备 sessions 目录
  ├─ ProviderConfigSnapshot::capture(runtime)（读取用户配置目录 config.json + auth.json）
  ├─ Thread 来源：--session 恢复并修复 | 新建 uuid v7 会话文件
  ├─ Conversation::run_turn(goal)   # 内部 = reserve_start() 原子预订 → 执行链
  │    ├─ TurnRunner::run（每轮独立控制面；当前轮可取消/可转向）
  │    │    ├─ fail-fast 准备（workspace/provider/config/项目指令/会话修复）
  │    │    ├─ thread_settings metadata 记录（与最后已记录值不同才写，先于 operation_started，D-056）
  │    │    ├─ operation_started 记录落盘（run 意图携带本 turn 冻结的模型配置快照
  │    │    │    与规范化不可变的用户输入；followUp 启动的 turn 同时落其
  │    │    │    control_accepted(started_as_new_turn)）→ 发布 turn/started
  │    │    ├─ AgentLoop 执行(三层分层:轮步/采样请求/纯发送,工具批、steer 注入;
  │    │    │    每个执行边界 step_attempt/provider_attempt/tool_started 先落盘再发事件)
  │    │    ├─ operation_finished 单条落盘（status/usage/truncated 合一；每轮恰好一条；失败即 fail-stop）
  │    │    └─ 发布 item/turn 终态事件
  │    ├─ 可信终态后按 FIFO 启动已接受的 followUp 为独立新 turn，直到队列清空
   │    ├─ 控制接受即 durable：steer/followUp/cancel 共用协调器一个接受序号
   │    │    计数器，各落一条 control_accepted 记录携带归宿（injected /
   │    │    started_as_new_turn / cancelled）；cancel 记录先于 interrupted
   │    │    终态落盘，撤回且从未启动的 followUp 不留记录
   │    ├─ 每次外呼模型的请求是一个 attempt：started 与终态观测经 protocol
   │    │    ProviderAttemptStatus 单点投影为 provider/attempt 事件，同一观测
   │    │    落 durable provider_attempt 记录；已交付可见文本的失败不透明重试
  └─ 渲染：--print 只写最终文本；--json 逐行事件 + summary 行
```

退出码：completed=0、interrupted=130（第一次 Ctrl+C 中断当前 turn；第二次强制退出）、failed=1。`--json` 的所有失败路径（含准备阶段失败）都输出 failed 终态 summary 行，保证机器解析总能看到终态。终态 summary 行或 `--print` 最终文本写 stdout 失败时，即使本轮已正常完成，进程也以失败退出码收敛——机器解析方不会看到缺失终态的成功退出；事件行写失败只置投影破损标志并跳过后续事件行，不改变执行事实。

### 2.1 事件流（TurnEvent 单一事实源）

protocol 的 typed `TurnEvent` 枚举是 runtime 与全部客户端渲染的唯一执行事件来源；方法名由 `TurnEvent::method()` 单点定义：

`turn/started · item/started · item/agentMessage/delta · item/agentThinking · tool/execution/start|update|end · item/completed · item/failed · agent/diagnostic · provider/attempt · turn/completed · turn/error`

`TurnEvent` 的 params 由该枚举自身的 serde derive 单点生成（camelCase；`tool/execution/end` 的 result 包成 `content:[{type:text,text}]`；`provider/attempt` 的可选字段恒出现、无值为 null；`turn/error` 平铺 `threadId/turnId/error`）直接构成 `--json` 事件行的 `params`；行级 envelope `{"method","params"}` 由 protocol `turn_event_envelope` 单点生成，协议 golden 测试表逐字钉住。

`agent/diagnostic.severity`、`provider/attempt.status` 与 `turn/error.error.{stage,cause}` 由 protocol 枚举单点定义，runtime 直接使用。runtime 诊断 code 由 protocol 常量定义，Agent 内部诊断 code 由 Agent 事件模块常量定义；线格式词形不变。`provider/attempt` 的字段集为「threadId/turnId/modelTurnOrdinal/provider/model/protocol/status/attemptDurationMs + 按分类可选的 errorCategory/diagnosticCode」。两张错误词表各自独立：`turn/error.error.cause` 用 protocol `TurnFailureCause` 的 13 词（provider 来源带 `provider_` 前缀：`provider_rate_limited`、`provider_network`、`provider_timeout`、`provider_auth`、`provider_validation`、`provider_overloaded`、`provider_cancelled`、`provider_context_overflow`、`provider_unknown`，另有 `store`、`project_instructions`、`workspace`、`internal`），model 具体失败类型（`ModelErrorKind` 12 类）到 provider cause 的分组映射由 runtime 单点拥有；`provider/attempt.errorCategory` 用 model `ModelErrorCategory` 的 snake_case 词形（`cancelled`、`authentication`、`network`、`model_configuration`、`invalid_request`、`context_length_exceeded`、`json_schema`、`content_filter`、`unsupported_capability`、`provider_unavailable`、`unknown_provider_error`，无 `provider_` 前缀），是 attempt 观测分类而非 turn 失败原因。wire 与 serde 词形的一致性由 protocol 测试钉住。

`--json` 行形状为 `{"method": <名>, "params": <TurnEvent serde 投影>}`；终态行为
`{"summary":{"thread":{"threadId"},"turn":{"threadId","status","usage"}}}`，
其中 `status ∈ completed|failed|interrupted`，`usage` 含 input/output/total/cached/reasoning tokens 与 `usagePresent/usageComplete`。截断终态（`turn.truncated`）的 `turn` 额外携带可选字段 `truncated: true`，普通终态省略该字段——该字段仅截断终态出现，外部评估器仅依赖原有字段即可解析。准备阶段失败的 summary 省略 `thread` 字段（turn 尚未启动，无已确定的 thread 可投影）。

## 3. Compaction（请求前两道闸门）

- **第一道（请求前主动）**：[`ContextView`]（`agent/session/context.rs`，上下文与计量的唯一 owner）每轮成功响应后保存其真实 provider usage（usage 基线），上报后追加的条目以 token 估算累计尾部增量；下一请求发出前优先以基线+尾部增量对比 `context_window − reserve_tokens`。首轮、压缩重写后或 usage 缺失时回退为「上下文条目的 token 估算求和」（唯一内容计量由 `entry_token_estimate` 单点拥有，metadata 与 ledger 记录计 0）。超过阈值则先以 Threshold 原因压缩再装配请求。`reserve_tokens` 默认 16_384，表示给模型回答预留的空间。
- **第二道（Provider 精确拒绝后）**：错误体（非 2xx 或 200 内嵌）有界读取并结构化解析，**仅当** `error.code == "context_length_exceeded"` 时分类为 `ContextLengthExceeded`（不可重试）；此时以 ContextOverflow 原因强制压缩并重建请求——预算按 turn 计、每 turn 至多一次（不是每个模型步一次）；二次失败保留原始根因并以失败终态收敛。其余错误体保持状态码分类并附有界单行短诊断。
- 切点策略：从最新往回累积至固定近期保留预算（`keep_recent_tokens`，默认 20,000），取其后最近合法切点；toolResult 永不作切点且 ToolCall/ToolResult 成对保留；尾部跨预算时回退到当前轮起点——摘要更早历史，当前轮完整保留；只有当前轮即全部历史时才返回 NotNeeded。
- 摘要调用的 usage 唯一落点为 `CompactionEntry.usage`，经会话累计投影计入总量；强制压缩的重建估算规模经完成回调交付客户端，不落盘。摘要请求走与正常请求同一模型配置快照和重试包装（可取消退避、尊重 Retry-After）；独立压缩取消以 `interrupted` 终结，其他错误以 `failed` 终结。

## 4. 工具执行要点

- 固定注册 read/glob/grep/bash/edit/write 六工具。一次模型响应内的多个工具调用**并发**执行：`Started` 事件与返回结果按模型给定 source order 排列，`Update`/`Ended` 按实际完成顺序发出；单批至多 8 个 worker，窗口之间顺序推进。同一文件的 `edit`/`write` 由批内按路径键持有的互斥锁串行化，只读工具与 bash 不加锁。provider 请求不携带 `parallel_tool_calls` 字段，一次响应内可发多少个调用由端点默认决定。
- 每次 turn 的提示词工具名单、请求 schema 与执行分发共用同一个注册表快照。压缩文件清单按 read/edit/write 的 ToolCall 参数推导。
- 系统提示词由 `PromptAssembly` 单点装配，顺序是：基础人格与工具约定、项目指令、末尾独立成行的 `Current working directory: <会话 cwd>`。环境事实不混入自然语言句子、行尾不带句读，模型可逐字复制到命令中。
- read 与 grep 共用同一有界行读取原语；不可信超长行 fail closed。session JSONL 解析为普通行迭代（撕裂尾部在解析层识别为修复状态），append 侧另有增长守卫。
- **grep**：先对完整原始行做正则匹配（CRLF 容忍），命中后才按 1 KiB char 边界安全前缀截断展示；跳过 .git/target/node_modules、二进制与符号链接目录；include glob 同时按相对路径与文件名匹配；上限 500 条。
- **bash**：每次调用都有执行界——`timeout_ms` 缺省为 300000 ms，显式传值可放宽（该参数无上限）；界到点即整树终止（Windows Job Object / Unix 进程组），并把界前已捕获的输出连同 `Command timed out after N ms and was terminated…` 一并返回、标记为失败，使模型可以就地改用更大预算或收窄命令；一次调用不得无限期占住整个 turn。增量 UTF-8 carry；内存尾部窗口（2000 行/50 KiB 预览，内部 100 KiB）；截断发生时完整输出 spill 到 `%TEMP%/singularity-tool-output/<uuid>/<slug>.log`，创建新 spill 时惰性删除同目录超过 7 天的旧文件；输出泵有界排空（2s 宽限）。
- **read/glob/edit/write**：有界读取（满 limit 即停、4 MiB 单行）、200 条结果上限、edit 20 MiB 门限与局部 diff。glob 模式：`*` 匹配除 `/` 外的任意字符，不跨目录层；`**` 跨任意目录层（含零层），尾部 `**` 同样跨目录递归。跳过 .git / target / node_modules 目录。
- **edit/write 落盘**：临时文件 + 原子替换（`singularity_core::atomic_replace_bytes`，跨平台：Windows MoveFileExW / 其他 rename），崩溃不出现半写撕裂。跨进程工作区协调不做（属已知限制）。

## 5. 会话持久化与恢复

- 严格 JSONL v4：首行 header（id/version/cwd/timestamp），header 的 `cwd` 是会话工作目录的唯一呈现形状——正斜杠绝对路径，写入与读回都经 `normalize_cwd_string` 剥除 Windows verbatim 前缀；Thread 投影、`/resume` 列表与系统提示词逐字复用同一字符串，不按调用方拼法重新派生。header version 必须等于当前版本，非当前版本直接拒绝打开；未知字段写入即拒绝。条目四型：`message`/`compaction`（模型可见历史）、`metadata`（仅 thread_settings、thread_name）与 `record`（`recordType` 标签的单 lane operation ledger 事实：operation_started/operation_finished/step_attempt/provider_attempt/write_deferred/write_abandoned/tool_started/control_accepted）；metadata 与 record 条目不进入模型上下文。turn 的终态唯一落盘位置是 `operation_finished`：其 usage 为 `TurnModelUsage` 的 camelCase 形状、七个键全部必填，完整性标志由 `usage.usageComplete` 单点承载。全部持久写入由持有写者锁的 `SessionManager` 执行，runtime 与 TUI 共用同一只读投影 API。
- 会话列表（`/resume` 菜单）只读每个文件的 header 首行与文件 mtime，不解析条目、不做聚合；列表统一按 `updated_at` 降序排列，同一时间戳按 thread id 升序稳定排序。标题、模型、状态与回合/用量聚合属单会话事实，按需经单文件读取（`ThreadCatalog::read_thread_summary`、`paged_read`、`/session`）获取。
- **单写者（OS 写者锁）**：同一会话同一时刻至多一个存活写者，由文件锁跨进程强制执行：每会话一把锁文件（sessions 同级 `thread-writer-locks/<id>.lock`），`File::try_lock` 快速失败；排他性来自锁句柄上的 OS 文件锁，Guard 释放句柄即释放锁，锁文件保留供复用，无人持有的残留由每个进程首次获取时的一次性清扫回收。一个 turn 打开一次 `SessionManager` 并独占贯穿 repair→operation_started→对话→工具→压缩→operation_finished；turn 结束随实例释放写者锁。协调器只在 run 的 `operation_started` 已落盘且匹配的 `operation_finished` 尚未落盘时维护本进程活动投影，普通目录写操作不构成活动 turn。只读投影（列表、摘要、分页读取、设置基线）走无锁路径，不参与写者竞争。
- 发布次序：durable JSONL 先于事件发布；`operation_finished` 单条落盘失败时不发布虚假终态，转 fatal 存储诊断（fail-stop）。
- 崩溃恢复：打开写路径时把 durable 前缀归约为 operation 事实，未终结的 run operation 收敛为一条 `operation_finished(interrupted)`，未解决的工具调用（含 `replay: never` 的已启动调用）补写模型可见的 synthetic failed ToolResult——绝不自动重放任何副作用；修复记录先于恢复后的终态可见，重开幂等（第二次打开无新记录）。撕裂尾部在解析层截去不完整尾行后再归约；全部记录由持写者锁的单一写者顺序追加产生，归约只折叠事实、不审计写入者：无法归约的记录无害跳过，开放 operation 各自独立收敛，header 版本不符仍在解析层拒绝打开。
- 归档：会话删除是归档保留——在持写者锁与 live-turn 拒绝护栏下，把会话文件从 sessions 顶层 rename 进 `archived/` 子目录；列表、摘要与分页只扫描顶层 `.jsonl`，归档会话自动隐藏，重复归档按未找到收敛。
- 设置：`thread_settings` metadata 记录 provider/model/reasoning；`Conversation::update_settings` 是唯一变更入口——提交点只做校验与内存投影更新（`AppliedNow`，运行中同样接受；变更从下一 turn 读取生效），空 patch 返回 `NothingToApply`；落盘发生在下一 turn 开始时，由 turn 在自己的会话写者上记录当前 selector（与最后一条已记录值相同则跳过），落盘走 turn 边界记录——提交点不写文件，运行中改设置与空闲时同路径，不会因写者锁被占用而失败。不改写全局配置。reasoning 是字段级三态 patch：wire 缺字段 = Keep（保持当前值）、`null` = Clear（清除显式 effort、恢复模型默认）、字符串 = Set（设置显式 effort）。任何时刻会话列表与分页读取都只读已落盘值（变更后、下一 turn 运行前显示旧值）。

## 6. Provider 与模型

- provider 配置只来自用户配置目录的 provider 目录（`config.json` + `auth.json`）；每个模型条目必须显式声明 `api_protocol: chat|responses`，运行时不做端点推断或跨协议 fallback。selector 统一为 `provider_id/model_id[#variant]`，每个 selector 都指向目录中声明的一个模型条目。runtime 在 turn 准备边界解析一次 `ModelConfigurationSnapshot`，同一不可变快照贯穿 Agent、请求模型身份、能力上限、重试、压缩与 reasoning replay，不再从 Provider 重读竞争配置。
- 两协议的 wire 分派收口在 transport 的 `ProtocolAdapter`：集中于单文件的薄转发表（端点、请求载荷、reasoning 在场判定、响应解析、SSE 读取），各协议实现体位于各自文件 `openai/chat.rs` 与 `openai/responses.rs`。协议选择是运行时事实（模型目录声明），trait 化不减少分支总数、只把分派表拆进实现文件，故不采用；接入第三 wire 协议时重评该形状。
- Chat SSE：仅按序 visible content delta 上抛；可见 delta 后禁止自动重试。Responses 协议独立 wire。`Provider` trait 唯一必需方法 `complete_stream` 每次执行一个 HTTP attempt（SSE 流式是唯一读取路径），并把类型化错误与最多 60 秒的 Retry-After 交给 Agent 层；Agent 层最多重试 3 次，采用可取消的指数退避并优先遵循 Retry-After。
- 错误：非 2xx body 有界读取（≤8 MiB）；结构化解析按 wire 错误码精确映射——`context_length_exceeded` → ContextLengthExceeded、`rate_limit_exceeded` → RateLimited、`insufficient_quota` → AuthError；200 内嵌错误对象（Chat `error`、Responses `error`/`response.failed`）与 `incomplete_reason == "content_filter"` 同样投影为类型化错误，保留 provider 原文（有界）与 `provider_error_code` 诊断；普通错误附 ≤256 字符单行化短诊断。

## 7. 评估（外部黑盒评估器）

独立仓库 `Singularity-Evaluator` 黑盒调用 `sg --json <instruction> --model <model>`（隔离 cell workspace 与独立 `SINGULARITY_HOME`），逐行解析 JSONL 并以最终 summary 行判定 turn.status/usage；checker.sh exit 0/1/2 = passed/failed/partial。评估器不依赖 Harness 内部 crate。每个 cell 有 `timeout_secs`（当前 1800 秒）预算，到点终止 `sg` 并记 `timed_out`；该预算必须显著大于单次 bash 调用的默认执行界（300 秒，见 D-065），使一次调用的失控不可能吃掉整轮预算，也使命中 `timed_out` 只表示整轮真的用尽时间。

## 8. 交互式 TUI 契约

`sg` 无参数进入长驻 TUI，只依赖 `Conversation` 与 typed `TurnEvent`；业务状态不复制在客户端：

- **布局**：主会话流（滚动区）＋底部多行编辑器（高度=内容折行行数，上限半屏）＋状态行＋提示行。
- **滚动**：严格双态（钉底跟随 / 上翻脱离）。PgUp 或滚轮上滚会脱离跟随并统计底部新增行（`↓ N new`）；下滚触底、End 或发送输入恢复跟随；恰好落底不恢复，需再次滚动才回到跟随（overscroll）。提交新消息后视口钉在新内容首行，回复填满一屏后自动回底（page-flip）。resize 不改变语义，只钳制位置。
- **鼠标**：滚轮按事件间隔归一化加速（区分滚轮/触控板）并按指针位置路由——输入框内滚轮只滚动编辑区（任何编辑或光标移动立即回到跟随），会话流上滚轮滚动会话流；点击输入框按显示位置定位编辑光标；运行中点击状态行右侧 `[stop]` 中断当前轮（与 Esc 同一路径）。命中判定基于渲染帧登记的点击矩形表（`mouse.rs`，`(Rect, ClickTarget)` 对），取代文本列反查。
- **编辑器**：光标/插入/删除/Home/End/上下行；Shift+Enter 或 Ctrl+J 换行。空闲时 Enter 启动新 turn；运行中 Enter 注入当前 turn，输入在工具调用完成后、下一段模型生成前送达；Alt+Enter 排队到当前 turn 结束，Alt+Up 撤回最近一条排队消息。
- **粘贴**：进入终端时启用 bracketed paste，粘贴文本整段插入光标处（CRLF/CR 归一为换行，字节上限按整字符截断并在提示行警告）；设置/命名菜单激活时粘贴落入当前字段（剔除换行）。
- **输入历史**：空闲相位且光标在可视首行时，↑/↓ 回溯本会话已提交的输入（含 steer 与 followUp 成功路径，相邻重复折叠）；回溯中任何编辑退出历史并保持当前文本，未编辑退出恢复原草稿；运行中 ↑/↓ 仍是编辑器光标移动。历史为会话内内存态，不持久化。
- **斜杠命令**：`/model`、`/settings`、`/resume`、`/new`、`/session`、`/compact`，并提供 `/` 补全菜单；`/name` 修改当前会话名称。`/model` 和 `/settings` 复用设置面板，`/resume` 与 `/new` 在进程内换绑 `Conversation`（统一 `rebind_conversation`）。`/resume` 换绑后按 `paged_read` 重放最近历史：物化 user/assistant/thinking/tool 条目为会话流，压缩点与设置变更分别投影为 `context compacted` 与 `settings updated for this thread: <provider>/<model> · reasoning <值>` 两行 note（轮次上限与 `paged_read` 单页上限一致），`/new` 与首启保持空流。`/resume` 菜单内可对非当前会话按 Ctrl+D 触发归档（两阶段确认：确认态只接受 Enter 归档、Esc 取消，其余键忽略；当前活动会话拒绝归档），归档走 `ThreadCatalog::archive`，归档后列表自动隐藏该行。`/compact` 异步执行：后台线程运行压缩，压缩期间界面持续渲染，Esc 取消本次压缩。压缩期间文本输入排队（Enter 走 steer 通道、Alt+Enter 走 followUp 通道，斜杠命令立即执行）：压缩结束后首条按普通提交启动新回合，其余在该回合启动时按通道注入，注入失败的退回队列不丢输入；Alt+Up 把队列整体倒回编辑器，状态行显示 `queued:N` 计数。
- **Esc 阶梯**：运行中 Esc 停止生成；压缩进行中 Esc 取消本次压缩；空闲时浏览态 Esc 回底跟随 → 非空草稿 Esc 清空 → 其余 no-op；临时菜单 Esc 关闭。
- **工具块**：运行中就地刷新；Ctrl+O（兼容 Alt+O）在折叠、截断、完整三档间循环，截断档以 `… N more lines (Ctrl+O expand)` 出口提示。运行态使用动画强调色，成功态使用常规色，失败态使用红色；完成时短暂闪烁。
- **思考块**：思考内容经 `item/agentThinking` 事件流实时到达客户端（assistant 消息持久化后逐块发布），不回查持久层；思考默认折叠，Ctrl+T 折叠或展开，不改变运行中输入路由。
- **状态行**：显示当前活动（思考中、等待模型、执行工具或终态收敛）、本轮经过时间、thread id、模型、会话累计 token usage（会话投影 `read_thread_summary().total_tokens` 的缓存，在轮次终态、压缩完成与 resume 时刷新）与 followUp 队列计数；浏览态显示 `viewing history`。
- **取消/退出**：Esc 中断当前轮；Ctrl+C 第一次在需要时清空输入并进入退出确认，第二次退出；输入为空时 Ctrl+D 退出。
- **设置模态**：`/settings` 打开，Tab 切换字段，Enter 应用（提交点立即校验并更新内存投影，运行中同样接受；落盘由下一 turn 开始时记录），Esc 关闭；开关前后滚动位置与编辑器内容不变。reasoning 字段预填当前 effort，清空后应用即 Clear（恢复模型默认），非空则 Set 为该值。
- **终端生命周期**：alternate screen + raw mode + 鼠标捕获 + bracketed paste；正常路径与 panic 钩子共用同一恢复实现；退出后无残留 raw/alt-screen/括号粘贴。

## 9. 当前维护边界

- 本文只描述当前有效的进程边界、事件流、会话格式、AgentLoop、Compaction、工具语义、Provider 能力声明、配置、TUI 契约和评估入口。
- protocol 的事件合同（事件名/字段/终态/取消/会话恢复不漂移）由 crates/protocol 的 wire golden 测试与 runtime 的并发安全护栏测试维护，端到端行为以外部评估器黑盒为准；runtime 只依赖 protocol 的稳定类型，不依赖客户端适配器。桌面端接入层重建时的协议细节见决策记录。
- 已移除机制、迁移过程和历史提交由 Git 保存；修改上述任一事实时必须同步更新对应章节并跑受影响验证。
