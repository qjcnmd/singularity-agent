# Singularity 架构说明

本文描述当前有效的产品、对象所有权和运行契约。源码、协议类型和可复现运行是事实依据。

## 1. 产品与进程边界

`singularity` 是单进程本地 Coding Agent：

- 无参数启动本地 Web 工作台；`--port` 选择监听端口，`--no-open` 只关闭浏览器自动交接。
- `--print <goal>` 只输出最终 assistant 文本。
- `--json <goal>` 输出逐行 `TurnEvent` JSONL，并以终态 `summary` 行收尾。
- Web 与无交互入口复用同一 `TurnRunner`、`Conversation`、Session、Provider 和工具实现。

工作台 Host 只绑定 `127.0.0.1`。浏览器是控制面，不拥有 Agent 事实；关闭、刷新或新增标签不会停止 Host 中的任务。

依赖方向为：

```text
cli/web -> runtime -> {core, model, agent, protocol}
agent   -> {core, model, protocol}
protocol 无内部 crate 依赖
```

## 2. Web 工作台

### 2.1 Host 与浏览器会话

`crates/cli/src/web` 组成唯一 Web adapter：

- `host.rs`：loopback Axum listener、WebSocket、同源边界和安全响应头；
- `auth.rs`：进程级 32-byte launch token、持久 64-byte signing key、30 天签名 cookie；
- `rpc.rs`：版本 1 固定 RPC envelope 到 Workbench 方法的薄适配；
- `static_files.rs`：嵌入 production assets；
- `workspace_files.rs`：有界目录浏览与已登记 Workspace 内文件候选；
- `workbench.rs`：Workspace、Session、模型设置、控制和流事件的 composition root。

启动入口形如 `http://127.0.0.1:<port>/?token=<token>`。根路径交换成功后设置 host-only、HttpOnly、SameSite=Strict 的签名 cookie，并跳转到干净根地址。cookie 绑定当前 authority；RPC 与 WebSocket 同时验证 Host、Origin 与 fetch metadata，RPC 另要求 `application/json`。Host 不开放 CORS。

`Workbench` 拥有一个 generation、全局 revision、共享 `TurnRunner`、`ThreadCatalog`、`WorkspaceStore`、`ModelConfigOwner` 和 `sessionId -> ConversationSlot` 映射。每个 slot 恰有一个 `Conversation`、一个 session revision、一个 phase、至多一个活动 turn 或 compaction 投影。不同 Session 可并行；同一 Session 的普通提交由 `Conversation::reserve_start` 原子拒绝竞争者。

事件连接先发送 `ready`。浏览器随后读取 bootstrap/session baseline，再按 generation 与连续 revision 应用帧；空洞、回退、Host generation 改变或慢消费者落后时重新读取权威 snapshot。Mutation 每次只发送一次；响应不确定时重读状态，不自动重放。

全局 revision 分配与帧发送在同一锁内完成。普通目录刷新不推进浏览器事件游标；读取 baseline 期间缓冲帧，按全局 revision 与 session revision 丢弃已包含的旧帧。bootstrap 同时提供各 Session 的 phase，连接恢复可重建后台运行状态。

### 2.2 浏览器 View

`crates/cli/web` 是单 React root：

- 左栏按 Workspace 分组 Task，使用会话语义标题或稳定的新任务编号，提供搜索、新建、命名、归档、目录添加和对象级项目管理；当前 Task 已是未使用空任务时，新建动作聚焦其输入区；
- 中栏是连续 Conversation，user/assistant 保持完整正文，thinking/tool/diff/diagnostic 聚合成可展开活动组；
- Details 只显示用户明确选择项目的完整参数、输出、错误或 diff；
- resident Composer 在运行中仍可编辑；左侧提供添加引用、完整本机权限和投递意图，右侧使用一个组合入口选择会话模型与思考程度，并提供发送或停止动作；文件候选、固定命令和 `/compact` 由同一编辑区承载；
- draft 按 Session 分键写入 `localStorage`，避免不同标签写入不同任务时互相覆盖；选择、投递意图、栏宽和滚动锚点保存在版本化 view 记录中，通过 storage event 同步。

稳定历史与活动事件单向归约为 keyed timeline。活动项目原位更新；Session settled 后重读 durable history 并整体替换活动投影。工具 call/result 以共同 ID 合并为一项，最终回答不折叠。

slot 在空闲读取和新执行链开始时从 ledger 刷新历史与 controls；执行链期间保留开始前的稳定快照，实时投影累计该链各 turn 的事件；settled 时再次读取 ledger，清除实时投影。空闲任务列表直接使用 catalog 的最新摘要，活动任务列表使用链开始前的稳定摘要。普通提交和空闲 send-now 共用启动门禁，旧 worker 完成 Workbench 收尾前保持 busy，拒绝的提升预订将原输入放回队列。因此 snapshot 中的稳定历史与实时事件不重叠。前端正文与侧栏按同一 Session 水位接受运行态，后续流式事件保留 stopping；bootstrap 的 RPC 响应按快照 revision、事件按 envelope revision 拒绝旧投影；投影版本与 SSE 消费水位分离。任务名称由列表投影统一提供给侧栏和页面标题。前端缓存稳定历史归约，流式事件只归约新增后缀；edit/write 的 diff 直接来自成功工具结果，不从参数或前端文件缓存推测。

Conversation 滚动只有 `following` 与 `anchored` 两态：底部自动跟随；用户向上阅读后保存稳定 item/offset 并累计新增项，触底或点击回到最新恢复跟随。Sidebar 与 Details 分隔条支持 pointer capture 和键盘方向键，宽度分别限制为 220–420px 与 300–640px；窄窗口使用 52px rail 与 Details overlay。

## 3. Workspace、Session 与持久事实

`CanonicalWorkspacePath` 是 Workspace identity 的唯一 owner。它验证存在目录、规范化 Windows verbatim/分隔符并生成稳定展示值和等价比较键。`workbench.json` 版本 1 只保存登记根，使用 owner-only 文件与 atomic replace；Session 按其规范 cwd 动态分组，移除 Workspace 不删除文件或 Session。

Session 是严格 JSONL v5：

- header 包含 id、version、规范 cwd 与 timestamp；
- `message` 与 `compaction` 构成模型可见历史；
- `metadata` 保存 thread settings/name；
- `record` 保存 operation、provider/tool attempt 与 durable control 事实。

`SessionManager` 是全部写入的唯一 owner。每个 Session 通过 OS 文件锁保证单写者；一个 turn 的写者覆盖 repair、operation started、消息与工具、compaction、operation finished。终态只由 `operation_finished` 表达。打开写路径时会把撕裂尾部和未终结 operation 收敛为可重开的 interrupted 事实，不重放副作用。

写者退出只释放 OS 锁，锁文件保留复用，运行期不删除锁路径，以免并发进程分别锁住新旧 inode。

`ThreadCatalog` 是 create/list/resume/rename/archive/summary/paged-read 的唯一目录入口。列表使用 ledger `ThreadSummary.updatedAt` 排序；分页使用 Turn cursor。归档把 JSONL 移入 `archived/` 并从活动列表隐藏。

## 4. Turn 与控制所有权

`TurnRunner` 拥有一个 turn 的完整管线：准备 Workspace/Provider/项目指令，记录冻结的模型配置与 `operation_started`，运行 AgentLoop，先落 durable 边界再发布 typed event，最后写唯一 `operation_finished`。

`Conversation` 拥有一个 Thread 的长驻执行状态：

- `reserve_start`：原子预订普通 turn；
- `steer`：向当前轮 inbox 注入输入；
- `followUp`：以 durable control ID 和 FIFO sequence 排队，可信终态后逐条执行为新 turn；
- withdraw：按 control ID 终结尚未消费的队列项；
- replace：更新同一 control 的文本，identity、FIFO sequence 和队列位置保持不变；
- send-now：把同一 control 原子转移到当前 inbox 或空闲 Turn 预订，失败时保留原队列项；
- interrupt：只取消当前轮，保留未消费 Follow-up；
- compact：独立可取消的上下文压缩；
- update settings：校验并更新下一 turn 使用的 selector。

接受的控制由 `ControlSnapshot` 表达 channel、sequence、disposition、turn 归宿和原文。浏览器动作结果使用 `ActionReceipt`；失败结果携带恢复建议，需要保留文本的路径同时返回完整 `preservedInput`。

恢复的 Follow-up 只在 Conversation 构造时装入唯一待执行队列；执行链从该队列逐条取出，编辑与撤回修改同一对象。后台 turn、send-now 与 compaction 共享 worker 收尾入口，异常退出也会刷新 Session 状态。

## 5. Agent、工具与事件

AgentLoop 的循环为：装配请求、发送流式模型请求、持久化 assistant/tool call、并发执行工具、持久化结果、继续下一步。固定工具是 `read`、`glob`、`grep`、`bash`、`edit`、`write`。单批工具至多 8 个 worker；同文件 edit/write 互斥，结果按模型给定顺序回填。

文件修改使用观察版本防误覆盖。`read` 建立文件版本事实；覆盖已存在文件的 `edit`/`write` 要求版本未改变，成功后更新观察版本。写入采用临时文件与 atomic replace。Workspace 不限制工具路径，隔离需求由进程外容器或 VM 承担。

`TurnEvent` 是 runtime 与所有客户端的执行事件来源：

`turn/started · item/started · item/agentMessage/delta · item/agentThinking · tool/execution/start|update|end · item/completed · item/failed · agent/diagnostic · provider/attempt · turn/completed · turn/error`

Durable JSONL 先于相应事件发布。投影写失败不改变执行事实；`operation_finished` 写失败时不发布虚假终态。

`turn/started` 带该轮 `input`；Web 帧附带 session revision 与开始时间，使刷新后的多轮实时投影有明确归属。`edit` 与 `write` 由工具端使用 `similar` 生成统一差异，失败结果不携带已应用差异。目录搜索在枚举目录与文件期间均检查取消。

## 6. Provider、模型与 Compaction

Provider 配置由 `config.json` 与私有 `auth.json` 唯一拥有。模型显式声明 `chat` 或 `responses` 协议、context/output 限额与 reasoning variants；selector 为 `provider/model[#variant]`。同一 turn 捕获一份不可变模型快照，贯穿正常请求、重试和压缩。

Workbench 串行持有 `ModelConfigOwner` 完成配置读改写和 runner 快照刷新，避免并发设置请求丢失更新。新建 Session 直接保留 catalog 创建的 Thread 及其显式 selector。

工作台的“模型连接”表面接收 schema 化 Provider 输入和只写 API Key；Composer 的组合选择器从同一 `RedactedModelCatalog` 按 Provider 呈现当前会话可用的模型、reasoning、默认值与生效时机。目录投影不含 credential、header 或 secret。

发送前使用上次真实 provider usage 加尾部估算判断是否主动压缩；usage 缺失时对上下文条目估算求和。Provider 精确返回 `context_length_exceeded` 时，一个 turn 最多强制压缩并重建请求一次。ToolCall/ToolResult 成对保留，合法切点必须指向现有模型上下文条目。

## 7. 构建、发布与自动化入口

前端锁定 build 为 `tsc -b && vite build`。`build.rs` 将 `crates/cli/web/dist` 作为输入并嵌入 CLI binary；运行发布程序不读取源码目录，也不需要 Node.js。

发布工作流先用 Node 24 构建前端，再构建 Rust release binary。签名与打包脚本均从 `cargo metadata.target_directory` 解析 release root。归档只有一个运行时 `singularity.exe` 及 README、LICENSE、INSTALL；CycloneDX SBOM 把 Rust binary 与 npm production 依赖连接为同一交付物。

两个脚本共享 `release-common.ps1` 的 release root 解析与 workflow output 写入；SBOM 的隔离 workspace staging 保持由打包脚本拥有。

无交互状态码为 completed=0、interrupted=130、failed=1。`--json` 的准备失败也输出 failed summary；终态 stdout 写失败以失败退出，避免机器消费者把不完整输出误判为成功。

## 8. 评估与维护

`C:\Users\Lenovo\Desktop\Singularity-Evaluator` 通过 `singularity --json` 在隔离工作区运行真实任务，并以 checker 判分。评估器校验调用 binary 的绝对路径、大小和 SHA-256，并在判分前检查工具参数是否越过题面与 cell 边界。

修改 Host、协议、Session、Provider、工具、上下文或输出行为时，验证顺序是：相关 owner 测试、锁定 production build、workspace 确定性门禁、真实 production 浏览器旅程；涉及 Agent 能力的变化再执行获准的真实模型评估。
