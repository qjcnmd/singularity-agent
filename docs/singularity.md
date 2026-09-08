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
- `rpc.rs`：版本 1 固定 RPC envelope 到 Workbench 方法的薄适配，同步文件与投影操作交给阻塞任务池；
- `static_files.rs`：嵌入 production assets；
- `workspace_files.rs`：Windows 原生文件夹选择、其他平台的有界目录浏览，以及当前任务目录内文件候选；
- `workbench.rs`：Workspace、Session、模型设置、控制和流事件的 composition root。

启动入口形如 `http://127.0.0.1:<port>/?token=<token>`。根路径交换成功后设置 host-only、HttpOnly、SameSite=Strict 的签名 cookie，并跳转到干净根地址。cookie 绑定当前 authority；RPC 与 WebSocket 同时验证 Host、Origin 与 fetch metadata，RPC 另要求 `application/json`。Host 不开放 CORS。

`Workbench` 拥有一个 generation、全局 revision、共享 `TurnRunner`、`ThreadCatalog`、`WorkspaceStore`、`ModelConfigOwner` 和 `sessionId -> ConversationSlot` 映射。每个 slot 恰有一个 `Conversation`、一个 session revision，以及至多一个活动 turn 或 compaction 投影；phase 直接来自 Conversation 的操作窗口与取消令牌。不同 Session 可并行；同一 Session 的普通提交由 `Conversation::reserve_start` 原子拒绝竞争者。

事件连接先发送 `ready`。浏览器随后读取 bootstrap/session baseline，再按 generation 与连续 revision 应用帧；空洞、回退、Host generation 改变或慢消费者落后时重新读取权威 snapshot。Mutation 每次只发送一次；响应不确定时重读状态，不自动重放。

活动快照以 `WorkbenchTurnEvent` 保存 `TurnEvent`、会话水位和时间；工具进度只保留每个运行中调用的最新一条，结束时移除该进度，开始和结束事实保留，在序列化边界复用同一事件 envelope。浏览器按 `method` 区分载荷类型，Conversation、轨迹与上下文用量直接消费该协议；Rust wire 样例同时用于前端类型检查。

断线后在后台持续指数退避重连，间隔上限8秒；停止页面连接或收到授权拒绝时停止重试。断线不增加横幅或输入卡片说明，草稿保留，发送按钮随连接状态禁用；具体用户动作失败仍提供错误反馈。

全局 revision 分配与帧发送在同一锁内完成。普通目录刷新不推进浏览器事件游标；读取 baseline 期间缓冲帧，按全局 revision 与 session revision 丢弃已包含的旧帧。bootstrap 同时提供各 Session 的 phase，连接恢复可重建后台运行状态。

### 2.2 浏览器 View

`crates/cli/web` 是单 React root。界面布局、控件、样式与交互约定见 [工作台交互](workbench.md)。

`WorkbenchStore` 统一拥有浏览器状态与通知，各视图只订阅自己使用的字段，流式水位变化不重绘任务列表。活动事件采用不可变日志，同一运行中工具只保留最新输出快照，结束后由完整结果替代进度。时间线、轨迹与上下文用量按新增事件或进度替换归约，跨多次替换时从当前有界快照重建；`viewPersistence.ts` 负责持久视图的读取、草稿迁移和保存。工具投影保留原始参数、输出和差异，展示标签与格式化文本在组件渲染时生成。

slot 在空闲读取和新执行链开始时从 ledger 刷新历史与 controls；执行链期间保留开始前的稳定快照，实时投影累计该链各 turn 的事件；settled 时再次读取 ledger，清除实时投影。空闲任务列表直接使用 catalog 的最新摘要，活动任务列表使用链开始前的稳定摘要。普通提交和空闲 send-now 共用启动门禁，旧 worker 完成 Workbench 收尾前保持 busy，拒绝的提升预订将原输入放回队列。因此 snapshot 中的稳定历史与实时事件不重叠。前端正文与侧栏按同一 Session 水位接受运行态，后续流式事件保留 stopping；bootstrap 的 RPC 响应按快照 revision、事件按 envelope revision 拒绝旧投影；投影版本与 SSE 消费水位分离。任务名称由列表投影统一提供给侧栏和页面标题；没有已有名称时，使用首次用户提示的前 8 个字符，短于 8 个字符则完整显示。前端缓存稳定历史归约，流式事件只归约新增后缀；edit/write 的 diff 直接来自成功工具结果，不从参数或前端文件缓存推测；增删行数紧接文件名显示。thinking 展开时将连续空行压成单次换行，持久化原文保持完整。

## 3. Workspace、Session 与持久事实

工作台先选择已登记项目，再创建任务；任务 cwd 使用项目根目录，文件候选按当前项目或任务 cwd 搜索。任务 RPC 必须提供 workspaceId，并校验任务 cwd 归属。所有项目使用同一导航与任务路径，Agent 的本机执行权限不随项目分组变化。


`CanonicalWorkspacePath` 是 Workspace identity 的唯一 owner。它验证存在目录、规范化 Windows verbatim/分隔符并生成稳定展示值和等价比较键。`workbench.json` 版本 1 只保存登记根，使用 owner-only 文件与 atomic replace；Session 按其规范 cwd 动态分组，移除 Workspace 不删除文件或 Session。

Session 使用严格 JSONL v6：

- header 包含 id、version、规范 cwd 与 timestamp；
- `message`、`compaction` 与 `instructions` 构成模型可见历史，`tool_result_pruned` 只替换已有工具内容；
- `metadata` 保存 thread settings/name；
- `record` 保存 operation、durable control、`model_request` 终态观测及 `request_content` 不可变内容。请求消息与工具定义按内容去重，观测通过条目 ID 引用，详情按需完整还原；模型偏好、序号、耗时、可用的 Token 统计和错误分类随观测保存，不参与模型上下文或恢复；未上报用量时保持未知。

v5 会话在打开边界转成相同的引用表示。只读打开不修改文件；首次写打开持有写者锁，将旧条目与新增内容记录原子迁移为 v6，保留原条目 ID、顺序和内容。更早版本仍拒绝打开。

`SessionManager` 是全部写入的唯一 owner。每个 Session 通过 OS 文件锁保证单写者；一个 turn 的写者覆盖 repair、operation started、消息与工具、compaction、operation finished。终态只由 `operation_finished` 表达。打开写路径时会把撕裂尾部和未终结 operation 收敛为可重开的 interrupted 事实，不重放副作用。

写者退出只释放 OS 锁，锁文件保留复用，运行期不删除锁路径，以免并发进程分别锁住新旧 inode。

`ThreadCatalog` 是 create/list/resume/rename/archive/summary/paged-read 的唯一目录入口。列表使用 ledger `ThreadSummary.updatedAt` 排序，摘要按文件长度、修改时间及本地运行状态缓存；变化时重新读取。目录只保留最近一次完整只读快照，活跃 slot 另外持有执行前的快照，空闲 slot 不保留整份历史。快照索引 Turn 的条目范围，分页只投影请求页并还原该页的请求详情。归档把 JSONL 移入 `archived/` 并从活动列表隐藏。

## 4. Turn 与控制所有权

`TurnRunner` 拥有一个 turn 的完整管线：准备 Workspace/Provider/项目指令，消费本轮冻结的模型配置并记录 `operation_started`，运行 AgentLoop，先落 durable 边界再发布 typed event，最后写唯一 `operation_finished`。

`Conversation` 拥有一个 Thread 的长驻执行状态：

- `reserve_start`：原子预订普通 turn；
- `steer`：向当前轮 inbox 注入输入；
- `followUp`：以 durable control ID 和 FIFO sequence 排队，可信终态后逐条执行为新 turn；
- withdraw：按 control ID 终结尚未消费的队列项；
- replace：更新同一 control 的文本，identity、FIFO sequence 和队列位置保持不变；
- send-now：把同一 control 原子转移到当前 inbox 或空闲 Turn 预订，失败时保留原队列项；
- interrupt：只取消当前轮，保留未消费 Follow-up；
- compact：与普通执行共用独占预订和取消入口，持有压缩期间唯一会话写者；
- update settings：校验并立即持久化下一 turn 使用的 selector，成功后才更新内存选择，活动 turn 和压缩的模型快照不变。

接受的控制由 `ControlSnapshot` 表达 channel、sequence、disposition、turn 归宿和原文。浏览器动作结果使用 `ActionReceipt`；失败结果携带恢复建议，需要保留文本的路径同时返回完整 `preservedInput`。

恢复的 Follow-up 只在 Conversation 构造时装入唯一待执行队列；执行链从该队列逐条取出，编辑与撤回修改同一对象。后台 turn、send-now 与 compaction 共享 worker 收尾入口。Runtime 预订保持到调用方完成投影收尾，销毁预订后才允许新操作；异常退出也沿用相同释放过程。

## 5. Agent、工具与事件

AgentLoop 的循环为：装配请求、发送流式模型请求、持久化 assistant/tool call、执行工具、逐项持久化结果、继续下一步。固定工具是 `read`、`glob`、`grep`、`bash`、`edit`、`write`、`skill`。相邻只读工具（read/glob/grep/skill）至多 8 个 worker 并行；bash/edit/write 按模型顺序串行，并等待此前只读组完成。每个结果完成后立即落盘，再发布结束事件；模型上下文将同批结果按调用顺序排列，实时与恢复使用同一投影。停止后尚未启动的调用返回取消结果，工具入口与启动 shell 前再次检查取消。同路径 edit/write 使用进程共享的互斥锁，覆盖版本核对、文件替换与新版本记录，跨任务和工具批次生效；观察版本仍由各 Conversation 独立维护。外部进程与 bash 的写入不受此锁约束。

文件修改使用观察版本防误覆盖。`read` 建立文件版本事实；覆盖已存在文件的 `edit`/`write` 要求版本未改变，成功后更新观察版本。写入采用临时文件与 atomic replace；替换工作区文件保留现有权限，新文件沿用系统默认权限与 umask，私有配置仍使用仅所有者可读写的创建路径。Workspace 不限制工具路径，隔离需求由进程外容器或 VM 承担。

`edit` 将 LF 与 CRLF 视为等价行尾，与 `read` 的逐行输出一致；其他字符和空白仍精确匹配，多处命中仍要求 `replaceAll`。替换文本沿用命中块的首个行尾，无换行时沿用文件的首个行尾；未命中部分保持原始字节，包含混合行尾和 UTF-8 BOM。

`TurnEvent` 是 runtime 与所有客户端的执行事件来源：

`turn/started · item/started · item/agentMessage/delta · item/agentThinking · tool/execution/start|update|end · item/completed · item/failed · agent/diagnostic · provider/attempt · turn/completed · turn/error`

Durable JSONL 先于相应事件发布。投影写失败不改变执行事实；`operation_finished` 写失败时不发布虚假终态。

`turn/started` 带该轮 `input`；Web 帧附带 session revision 与开始时间，使刷新后的多轮实时投影有明确归属。`edit` 与 `write` 由工具端使用 `similar` 生成统一差异，失败结果不携带已应用差异。目录搜索在枚举目录与文件期间均检查取消。

Skills 的发现和正文加载由 `core::skills` 统一拥有。每个 turn 按项目与用户目录形成带优先级的目录快照；模型上下文只注入名称与说明，通过 `skill` 工具按需读取正文。Web、无交互输入与运行中 steer 的开头 `/名称` 使用同一加载器，并在原用户消息后持久化 `skill_instructions`。正文保留来源与相对资源目录，恢复沿用已保存的内容。`user-invocable: false` 从手动候选隐藏，`disable-model-invocation: true` 从模型目录和工具调用隐藏。解析错误按文件显示，其他有效技能继续可用；不会自动执行技能中的脚本。目录范围与文件格式见 [安装与运行](INSTALL.md#skills)。

## 6. Provider、模型与 Compaction

Provider 配置由 `config.json` 与私有 `auth.json` 唯一拥有。模型显式声明 `chat` 或 `responses` 协议、context/output 限额与 reasoning variants；selector 为 `provider/model[#variant]`。同一 turn 捕获一份不可变模型快照，贯穿正常请求、重试和压缩。

Workbench 串行持有 `ModelConfigOwner` 完成配置读改写和 runner 快照刷新，避免并发设置请求丢失更新。新建 Session 立即保存其显式 selector。后续设置提交复用活动 turn 或压缩的共享写者，空闲时短开写者；保存失败保留此前的选择。

工作台的模型设置接收 schema 化 Provider 输入和只写 API Key；Composer 从同一 `RedactedModelCatalog` 呈现当前会话可用的模型与思考档位。获取可用模型同时读取提供方的容量和 effort 元数据，缺失时从 Models.dev 公共目录按准确 API 地址与模型 ID 补齐；公共目录请求不携带用户地址或凭据，不新增缓存或持久目录。查询使用表单当前地址，输入新密钥时优先使用新值，留空时复用该提供方的已存密钥，与 DSH 自定义提供方查询一致。候选只进入编辑草稿，由保存提交；提供方、地址、凭据或协议变更后丢弃旧发现请求的结果及候选，不锁定编辑字段；更新已有模型保留仍被支持的档位别名和默认选择，缺失字段保留原配置。仅支持 thinking 开关或 budget 的元数据不冒充 effort 档位。设置不提供打开配置文件或额外底层参数编辑界面。

发送前刷新文件指令，并按系统提示词、工具定义和当前历史的统一估价计算压力；当前 turn 内最近一次同模型请求的实测总量高于完整估价时，差值作为校正保留，后续剪枝与摘要按实际替换的内容重新计量。达到窗口 90%，或扣除安全余量后不足以留出窗口 10%（不超过模型输出上限）的回答空间时，先把超过 8192 个 Unicode 字符的工具结果保留前 4096、后 1024 字符；若仍需缩减，保留至少窗口 10% 的近期内容并摘要前缀。切点向前调整以保持整个工具调用/结果批次，允许在同一用户回合内切分。

摘要请求复用当前系统提示词、工具定义及原生历史前缀，末尾追加结构化摘要指令。输出上限为 8192 Token，复用普通请求的剩余窗口预算并受模型能力约束；空白、截断、工具调用或没有真正缩小替换区的结果不提交摘要。自动压力处理最多摘要两次；缩减后仍没有所需回答空间则明确失败，不将预算强行降到 1 Token。安全余量为窗口 5%，上限 4096 Token；手动压缩跳过压力阈值与比例保留量，保留最后一个完整消息或工具单元。Provider 精确返回 `context_length_exceeded` 时，一个 turn 最多执行一次有效缩减后的重发；没有缩减或恢复失败时保留原溢出根因，取消与存储失败单独收敛。

系统提示词和工具定义不属于历史替换区。文件指令来自当前用户数据目录的 `AGENTS.md`，再按项目根到 cwd 的层级读取，带来源路径作为 `instructions` 上下文记录注入；直接用户指令和系统规则优先于文件指令。每个模型步和摘要后核对原文件，内容相同且仍可见时不重复注入，内容变化或被压缩后重新注入当前加载快照（每文件 32KB、合并 64KB 的读取预算仍适用）。摘要不成为文件指令的权威来源。

`ContextView` 按日志顺序归约唯一模型历史：摘要替换当前前缀，`tool_result_pruned` 在原位置替换工具内容；锚点必须仍在活动历史中。原始工具消息始终留在日志和公开历史中，摘要与剪枝都不删除用户数据。连续压缩不会把旧摘要重新带入保留区。

## 7. 构建、发布与自动化入口

前端锁定 build 为 `tsc -b && vite build`。`build.rs` 将 `crates/cli/web/dist` 作为输入并嵌入 CLI binary；运行发布程序不读取源码目录，也不需要 Node.js。

发布工作流先用 Node 24 构建前端，再构建 Rust release binary。签名与打包脚本均从 `cargo metadata.target_directory` 解析 release root。归档只有一个运行时 `singularity.exe` 及 README、LICENSE、INSTALL；CycloneDX SBOM 把 Rust binary 与 npm production 依赖连接为同一交付物。

两个脚本共享 `release-common.ps1` 的 release root 解析与 workflow output 写入；SBOM 的隔离 workspace staging 保持由打包脚本拥有。

无交互状态码为 completed=0、interrupted=130、failed=1。`--json` 的准备失败也输出 failed summary；终态 stdout 写失败以失败退出，避免机器消费者把不完整输出误判为成功。

## 8. 评估与维护

`C:\Users\Lenovo\Desktop\Singularity-Evaluator` 通过 `singularity --json` 在隔离工作区运行真实任务，并以 checker 判分。评估器校验调用 binary 的绝对路径、大小和 SHA-256，并在判分前检查工具参数是否越过题面与 cell 边界。

验证范围见 [项目指令](../AGENTS.md#验证与交付)，命令见 [开发指南](development.md)。
