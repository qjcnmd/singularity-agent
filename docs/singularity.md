# Singularity 架构说明文档

> **本文档描述 Singularity 当前有效架构事实**，以当前源码与协议为权威依据。
>
> **维护规则**：修改以下任一事实时同步更新本文：进程边界、事件流、会话格式、Compaction、工具面与工具语义、Provider/模型能力声明、配置 schema、评估工具、发布二进制。

## 1. 总览与进程架构

产品有三种形态，共享同一运行时语义：

- **无交互单次入口**：`sg --print <goal>` / `sg --json <goal>`，行为参照 pi。`--print` 只向 stdout 输出最终 assistant 文本；`--json` 输出逐行 JSONL 事件并以 `{"summary":{…}}` 终态行收尾。`--model` 只覆盖本次执行；`--session <id>` 恢复既有 Thread；`--no-session` 以临时 home 关闭持久化；默认持久化会话。
- **交互式 TUI**：`sg` 无参数进入长驻终端界面；界面交互以 Grok Build 为主参照，功能以 pi、Codex CLI 和 Grok Build 为参照。
- **桌面端**：参照 Codex Desktop；app-server 是桌面端连接共享 runtime 的 stdio JSON-RPC 后端，不构成独立用户入口。

分层：

- **crates/runtime**：Thread/Turn 生命周期协调与单轮执行管线，是 Turn 执行的唯一所有者。
  - [`TurnRunner`]：一个 turn 的完整管线——会话打开/修复（单写者所有权贯穿全程）、项目指令装配、Agent 执行、typed 事件投影、终态 metadata 与 usage 落盘、fail-stop 收敛。准备阶段 fail-fast：任何失败不留 turn 痕迹。
  - [`Conversation`]：一个 Thread 的长驻协调器——单活动 turn 链窗口（`reserve_start` 原子预订，路由层可在负载线程启动前确定先到先得）、steer 注入当前轮 inbox、followUp FIFO 队列（当前轮可信终态后自动逐条执行为独立新 turn，每条恰好一次）、取消令牌按轮独立（取消只作用于当前轮）、设置时序（活动期间排队，终态后自动校验持久化，无公开手工应用接口）。
- **crates/cli**：入口解析与三种渲染（TUI / 文本 / JSONL）。TUI 与无交互模式进程内调用 runtime 的 `Conversation`；渲染只消费 typed `TurnEvent`，投影失败只丢弃投影，不影响执行事实。
- **headless core（库）**：
  - `AgentLoop`（双层循环）：单一原子 `TurnInbox` 承载 steer；每轮**请求前**以装配成品估算对比 `context_window − reserve_tokens` 决定主动压缩；Provider 显式 `ContextLengthExceeded` 时强制压缩并同轮重试一次；
  - `session` 子系统：严格 JSONL v1（format/file/manager/context/repair/repository）；会话 JSONL 是唯一持久事实源；
  - `compaction`：摘要引擎与切点策略（ToolCall/ToolResult 成对保留）;
  - `tools`：固定六工具注册表（read/glob/grep/bash/edit/write），多工具批按模型给定顺序串行；
  - 资源加载：AGENTS.md root→cwd 逐层合并，预算超限截断并向客户端发诊断；
  - `singularity_model`：types/error/provider/openai(chat,responses)/transport/config/discovery。
- **crates/app-server**：桌面端的 stdio JSON-RPC 后端适配层，把 runtime 作为唯一执行核心：`turn/start` 经 `Conversation::reserve_start` 同步裁定并发（先到先得、后到立即 invalid-state），随后在 worker 线程以预订执行为整条链；事件由适配器把 typed `TurnEvent` 一一映射为协议通知（索引在 JSONL 落盘后同步）；`turn/steer`/`turn/followUp`/`turn/interrupt` 控制 lane 与设置、删除全部路由到共享 `Conversation`。协议类型只存在于 crates/protocol 与适配器；runtime 不依赖 protocol/UI。
- **共享事实**：`~/.singularity/config.json`（全局配置）、`~/.singularity/auth.json`（私有认证）、`~/.singularity/sessions/<uuid>.jsonl`（会话正文）。
- **依赖方向**：cli → runtime → {core, model, agent}；app-server → {runtime, protocol}；runtime 与 agent 不依赖 protocol/UI；protocol 类型仅服务 app-server 适配器。

## 2. 无交互主调用链

```
sg --print|--json <goal>
  ├─ 解析参数（--model/--session/--no-session）
  ├─ 解析 SINGULARITY_HOME（或临时 home）并准备 sessions/backups 目录
  ├─ ProviderConfigSnapshot::capture(env)（进程层任一变量出现即整体短路用户配置）
  ├─ Thread 来源：--session 恢复并修复 | 新建 uuid v7 会话文件
  ├─ Conversation::run_turn(goal)   # 内部 = reserve_start() 原子预订 → 执行链
  │    ├─ TurnRunner::run（每轮独立控制面；当前轮可取消/可转向）
  │    │    ├─ fail-fast 准备（workspace/provider/config/项目指令/会话修复）
  │    │    ├─ turn_started metadata 落盘 → 发布 turn/started
  │    │    ├─ AgentLoop 执行（压缩、工具批、steer 注入）
  │    │    ├─ 终态 metadata + usage 落盘（有界重试一次，失败即 fail-stop）
  │    │    └─ 发布 item/turn 终态事件
  │    ├─ 终态后自动持久化待生效设置（若有）→ 下一轮用新 selector
  │    └─ 按 FIFO 启动已接受的 followUp 为独立新 turn，直到队列清空
  └─ 渲染：--print 只写最终文本；--json 逐行事件 + summary 行
```

退出码：completed=0、interrupted=130（第一次 Ctrl+C 中断当前 turn；第二次强制退出）、failed=1。`--json` 的所有失败路径（含准备阶段失败）都输出 failed 终态 summary 行，保证机器解析总能看到终态。

### 2.1 事件流（TurnEvent 单一事实源）

runtime 的 typed `TurnEvent` 枚举是全部客户端渲染的唯一事件来源，方法名稳定：

`thread/started · turn/started · item/started · item/agentMessage/delta · tool/execution/start|update|end · item/completed · item/failed · agent/diagnostic · provider/attempt(/summary) · turn/completed · turn/error · thread/settingsApplied`

`thread/settingsApplied` 在活动 turn 期间排队的设置于可信终态后成功持久化时发布（位于该轮终态事件之后），payload 为应用后的完整 Thread 投影；app-server 据此把索引行的 model 同步到已落盘值。空闲路径无此事件（提交点内已立即持久化）。

`--json` 行形状为 `{"method": <名>, "params": <TurnEvent 字段，snake_case>}`；终态行为
`{"summary":{"thread":{"threadId"},"turn":{"threadId","status","usage"}}}`，
其中 `status ∈ completed|failed|interrupted`，`usage` 含 input/output/total/cached/reasoning tokens 与 `usagePresent/usageComplete`。外部评估器仅依赖该 summary 合同。

## 3. Compaction（请求前两道闸门）

- **第一道（请求前主动）**：每轮请求发出前，以本轮装配成品估算（消息+reasoning replay+预算，含 max_output_tokens）对比 `context_window − reserve_tokens`；超过则先以 Threshold 原因压缩再装配请求。`reserve_tokens` 默认 16_384，表示给模型回答预留的空间；配置非法 fail closed。
- **第二道（Provider 精确拒绝后）**：非 2xx body 有界读取并结构化解析，**仅当** `error.code == "context_length_exceeded"` 时分类为 `ContextLengthExceeded`（不可重试）；此时以 ContextOverflow 原因强制压缩一次并同轮重试；二次失败原样返回。其余错误体保持状态码分类并附有界脱敏短诊断。
- 切点策略：从最新往回累积至保留预算（retain_ratio），取其后最近合法切点；toolResult 永不作切点且 ToolCall/ToolResult 成对保留；尾部跨预算时回退到当前轮起点——摘要更早历史，当前轮完整保留；只有当前轮即全部历史时才返回 NotNeeded。
- 摘要调用的 usage 计入 turn 总量；强制压缩的 tokensBefore 取真实重建估算。

## 4. 工具执行要点

- 固定注册 read/glob/grep/bash/edit/write 六工具；多工具批按模型给定顺序串行，`parallel_tool_calls` 恒 false。
- **grep**：先对完整原始行做正则匹配（CRLF 容忍），命中后才按 1 KiB char 边界安全前缀截断展示；跳过 .git/target/node_modules、二进制与符号链接目录；include 按 basename 过滤；上限 500 条。
- **bash**：显式 `timeout_ms` 生效、未提供不超时；Windows Job Object / Unix 进程组整树终止；增量 UTF-8 carry；内存尾部窗口（2000 行/50 KiB 预览，内部 100 KiB）；截断发生时完整输出 spill 到 `%TEMP%/singularity-tool-output/<uuid>/<slug>.log`；输出泵有界排空（2s 宽限）。
- **read/glob/edit/write**：有界读取（满 limit 即停、4 MiB 单行）、200 条结果上限、edit 20 MiB 门限与局部 diff、in-place 写入。

## 5. 会话持久化与恢复

- 严格 JSONL v1：首行 header（id/version/cwd/timestamp），未知字段写入即拒绝；metadata 条目（turn_started/completed/failed/interrupted、thread_settings、usage）不进入模型上下文。
- **单写者**：一个 turn 打开一次 `SessionManager` 并独占贯穿 repair→turn_started→对话→工具→压缩→终态→usage；turn 结束关闭写者。
- 发布次序：durable JSONL 先于事件发布；terminal metadata 经有界重试仍无法落盘时不发布虚假终态，转 fatal 存储诊断（fail-stop）。
- 崩溃恢复：重开时未终态 turn 补 synthetic interrupted；孤立 tool call 补 synthetic failed ToolResult，绝不重试执行。
- 设置：`thread_settings` metadata 记录 provider/model/reasoning；`Conversation::queue_settings` 是唯一入口——空闲时立即校验并持久化（`AppliedNow`），活动 turn 期间合并为单份待生效意图（`QueuedForNextTurn`）并在轮终态收敛后自动应用（下一轮读取生效，当前轮保持启动时 selector），空 patch 返回 `NothingToApply`；设置持久化失败保留意图并中止链条返回可行动错误。不改写全局配置。app-server 的 `thread/settings` 在排队时同步返回 `queued=true` 且索引行保持旧 model，终态后随 `thread/settingsApplied` 事件同步——任何时刻 thread/list 与 thread/read 都只读已落盘值。

## 6. Provider 与模型

- 运行时解析只取用户配置持久化值或进程环境层（`SINGULARITY_MODEL/SINGULARITY_BASE_URL/SINGULARITY_API_KEY/...` 任一出现即整体短路用户配置）；环境层下 api_protocol 由 base_url 是否以 `/responses` 结尾推断，用户配置层必须显式声明 api_protocol。
- Chat SSE：仅按序 visible content delta 上抛；可见 delta 后禁止自动重试。Responses 协议独立 wire。传输层单次 complete ≤6 attempts，Retry-After clamp 60s，缺失时全抖动退避。
- 错误：非 2xx body 有界读取（≤8 MiB）；结构化解析仅精确识别 context_length_exceeded；普通错误附 ≤256 字符单行化短诊断，命中敏感文本或凭据形态整体替换固定文案；凭据绝不进入错误文本或任何输出。

## 7. 评估（外部黑盒评估器）

独立仓库 `Singularity-Evaluator` 黑盒调用 `sg --json <instruction> --model <model>`（隔离 cell workspace 与独立 `SINGULARITY_HOME`），逐行解析 JSONL 并以最终 summary 行判定 turn.status/usage；checker.sh exit 0/1/2 = passed/failed/partial。评估器不依赖 Harness 内部 crate。

## 8. 交互式 TUI 契约

`sg` 无参数进入长驻 TUI，只依赖 `Conversation` 与 typed `TurnEvent`；业务状态不复制在客户端：

- **布局**：主会话流（滚动区）＋底部多行编辑器（高度=内容折行行数，上限半屏）＋状态行＋提示行。
- **滚动**：严格双态（钉底跟随 / 上翻脱离）。PgUp、Alt+↑ 或滚轮上滚会脱离跟随并统计底部新增行（`↓ N new`）；下滚触底、End 或发送输入恢复跟随。resize 不改变语义，只钳制位置。
- **鼠标**：滚轮浏览会话流；点击输入框按显示位置定位编辑光标。
- **编辑器**：光标/插入/删除/Home/End/上下行；Shift+Enter 或 Ctrl+J 换行。空闲时 Enter 启动新 turn；运行中 Enter 注入当前 turn，输入在工具调用完成后、下一段模型生成前送达；Alt+Enter 排队到当前 turn 结束，Alt+Up 撤回最近一条排队消息。
- **斜杠命令**：`/model`、`/settings`、`/resume`、`/new`、`/session`、`/compact`，并提供 `/` 补全菜单；`/name` 修改当前会话名称。`/model` 和 `/settings` 复用设置面板，`/resume` 与 `/new` 在进程内换绑 `Conversation`。
- **Esc 阶梯**：运行中 Esc 停止生成；空闲时浏览态 Esc 回底跟随 → 非空草稿 Esc 清空 → 其余 no-op；临时菜单 Esc 关闭。
- **工具块**：运行中就地刷新；Ctrl+O（兼容 Alt+O）在折叠、截断、完整三档间循环。运行态使用动画强调色，成功态使用常规色，失败态使用红色；完成时短暂闪烁。
- **思考块**：Ctrl+T 折叠或展开思考内容，不改变运行中输入路由。
- **状态行**：显示当前活动（思考中、等待模型、执行工具或终态收敛）、本轮经过时间、thread id、模型、token usage 与 followUp 队列计数；浏览态显示 `viewing history`。
- **取消/退出**：Esc 中断当前轮；Ctrl+C 第一次清空输入，输入已经为空时进入退出确认，第二次退出；输入为空时 Ctrl+D 退出。
- **设置模态**：`/settings` 打开，Tab 切换字段，Enter 应用（空闲立即生效 / 运行中排队到下一轮），Esc 关闭；开关前后滚动位置与编辑器内容不变。
- **终端生命周期**：alternate screen + raw mode + 鼠标捕获；正常路径与 panic 钩子共用同一恢复实现；退出后无残留 raw/alt-screen。

## 9. 当前维护边界

- 本文只描述当前有效的进程边界、事件流、会话格式、AgentLoop、Compaction、工具语义、Provider 能力声明、配置、TUI 契约和评估入口。
- app-server 的协议细节（命令/事件集、握手、控制 lane、并发 turn 裁定）作为 GUI 适配面的内部合同，由 crates/app-server 与 crates/protocol 自身的适配器测试维护（协议事件名/字段/终态/取消/会话恢复不漂移）；协议类型不进入 runtime。
- 已移除机制、迁移过程和历史提交由 Git 保存；修改上述任一事实时必须同步更新对应章节并跑受影响验证。
