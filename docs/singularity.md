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
- `workspace_files.rs`：Windows 原生文件夹选择、其他平台的有界目录浏览，以及当前任务目录内文件候选；
- `workbench.rs`：Workspace、Session、模型设置、控制和流事件的 composition root。

启动入口形如 `http://127.0.0.1:<port>/?token=<token>`。根路径交换成功后设置 host-only、HttpOnly、SameSite=Strict 的签名 cookie，并跳转到干净根地址。cookie 绑定当前 authority；RPC 与 WebSocket 同时验证 Host、Origin 与 fetch metadata，RPC 另要求 `application/json`。Host 不开放 CORS。

`Workbench` 拥有一个 generation、全局 revision、共享 `TurnRunner`、`ThreadCatalog`、`WorkspaceStore`、`ModelConfigOwner` 和 `sessionId -> ConversationSlot` 映射。每个 slot 恰有一个 `Conversation`、一个 session revision、一个 phase、至多一个活动 turn 或 compaction 投影。不同 Session 可并行；同一 Session 的普通提交由 `Conversation::reserve_start` 原子拒绝竞争者。

事件连接先发送 `ready`。浏览器随后读取 bootstrap/session baseline，再按 generation 与连续 revision 应用帧；空洞、回退、Host generation 改变或慢消费者落后时重新读取权威 snapshot。Mutation 每次只发送一次；响应不确定时重读状态，不自动重放。

断线后在后台持续指数退避重连，间隔上限8秒；停止页面连接或收到授权拒绝时停止重试。断线不增加横幅或输入卡片说明，草稿保留，发送按钮随连接状态禁用；具体用户动作失败仍提供错误反馈。

全局 revision 分配与帧发送在同一锁内完成。普通目录刷新不推进浏览器事件游标；读取 baseline 期间缓冲帧，按全局 revision 与 session revision 丢弃已包含的旧帧。bootstrap 同时提供各 Session 的 phase，连接恢复可重建后台运行状态。

### 2.2 浏览器 View

`crates/cli/web` 是单 React root：

- 左栏支持按 Workspace 分组或平铺会话、最近更新或手动拖动排序，提供新建、重命名、归档与项目菜单；分组折叠和视图选项保存在浏览器 view。空会话按工作区复用；创建期间输入写入目标工作区的新草稿，完成后转入新会话，先前会话草稿独立保留；
- 中栏是连续 Conversation，user/assistant 保持完整正文，thinking/tool/diff 按实际顺序逐项呈现；思考行从 `thinking` 文字开始，只有正文超出单行或含后续行才提供展开箭头，展开时标题单独一行，正文从左侧全宽展开；工具行使用 `read`、`bash`、`edit` 等工具原名并保留类型图标，输入、输出与 diff 按其实际内容展开。长消息折叠只计算正文隐藏行；思考流与每次模型回复保留独立身份；Provider返回的可展示thinking/summary独立于opaque工具续接数据进入ModelTurnResponse.thinking，再随assistant消息持久化，普通回复和工具回复走同一条保存路径；工具 call/result 合为一项；
- 对话始终保留在主栏，右上角按钮控制可调宽度的右侧栏。打开后先显示仅含“轨迹”的选择首页；选中后进入紧凑记录表，可返回首页。轨迹支持逐轮／逐调用折叠；点击记录在面板内切换到详情，可返回列表及原阅读位置，按对象展示正文、思考、请求上下文、工具定义、选项、用量和时序。轨迹不提供顶栏统计、搜索、时间条或范围选择；历史与实时内容仍投影自同一会话事实；
- resident Composer 在运行中仍可编辑；模型选择使用用户提供的 DSH 插件式单面板，上部为模型列表、下部为推理等级滑条，不提供搜索、快速模型或恢复默认按钮；滑条拖动连续预览位置，松手提交最近档位；连续调档串行保存最后选择；保存队列由当前项目、任务和模型共同限定，切换任务或模型即丢弃旧队列中尚未提交的编辑，已发出的请求仍归原任务。模型弹层及滑块无过渡动画，轨道使用 Codex 截图中的粉色填充和浅灰底色。空会话可先选择模型，再发送保留的草稿。运行中 Enter 默认排队；队列卡片位于输入框上方，逐条提供编辑、删除与箭头提前发送。多条队列可折叠，编辑支持 Enter 保存、Escape 取消。Ctrl/Cmd+Enter 只为本次输入插话；输入为空时依次提前发送全部排队消息。运行中工具栏只显示停止球体；编辑区保留文件候选，移除命令菜单及帮助；上下文压缩通过编辑框内部左下方独立按钮触发；
- 复用空任务转移草稿时，目标含有其他草稿则创建新任务，不能覆盖目标内容。draft 按 Session 分键写入 `localStorage`，避免不同标签写入不同任务时互相覆盖；选择、栏宽和滚动锚点保存在版本化 view 记录中；storage event 同步任务草稿、列表视图和滚动锚点。旧 view 中的草稿在覆盖容器前迁入分任务键，已有分任务值优先；迁移写入失败时保留原容器，避免丢失唯一副本。

稳定历史与活动事件单向归约为 keyed timeline。活动项目原位更新；Session settled 后重读 durable history 并整体替换活动投影。处于底部时，流式更新和展开内容继续跟随底部；用户向上阅读时保留条目锚点，程序恢复位置和内容收缩不改变阅读意图。工具开始事件立即显示工具行及“进行中”，无输出时也保留执行状态。工具 call/result 以共同 ID 合并为一项；同一次模型请求的相邻工具再组成一层可折叠列表，使用既有请求 ordinal/attempt 划界，不依赖 thinking。批次运行时默认展开，进入下一请求或任务终止后默认收起，用户手动选择优先；摘要保留调用数与失败数。最终回答不折叠，消息不提供复制按钮。发送和停止共用粉白动态玻璃球，点击和键盘分别触发对应动作；空闲流动周期4.8秒、光照周期3.6秒，执行或悬停时分别加速至1.4秒和1.05秒，减少动态效果时球体静止。

slot 在空闲读取和新执行链开始时从 ledger 刷新历史与 controls；执行链期间保留开始前的稳定快照，实时投影累计该链各 turn 的事件；settled 时再次读取 ledger，清除实时投影。空闲任务列表直接使用 catalog 的最新摘要，活动任务列表使用链开始前的稳定摘要。普通提交和空闲 send-now 共用启动门禁，旧 worker 完成 Workbench 收尾前保持 busy，拒绝的提升预订将原输入放回队列。因此 snapshot 中的稳定历史与实时事件不重叠。前端正文与侧栏按同一 Session 水位接受运行态，后续流式事件保留 stopping；bootstrap 的 RPC 响应按快照 revision、事件按 envelope revision 拒绝旧投影；投影版本与 SSE 消费水位分离。任务名称由列表投影统一提供给侧栏和页面标题。前端缓存稳定历史归约，流式事件只归约新增后缀；edit/write 的 diff 直接来自成功工具结果，不从参数或前端文件缓存推测。

项目文件夹下的会话列表以高度过渡向下展开、向上收回，整体展开/收起360ms；项目列表与全部会话共用条目动画，在240ms内从0.7缩放淡入，按索引间隔50ms，关闭时淡出并随列表收回。菜单仅采用整体透明度淡入，不为菜单项叠加入场动画；工具和思考内容以高度过渡展开收起，历史正文不重复播放入场。正在运行的thinking文字使用16px蓝粉紫渐变、1.2秒扫光，其他运行名称保留低亮度扫光，整轮执行沿用DSH的“Deep diving...”持续提示，首字等待、工具执行和后续请求等待均保留，15秒后附带耗时；减少动态效果偏好下关闭动画。对话不显示底部请求统计，手动中断只显示 DSH 式安静的“已停止”标记。

Conversation 滚动只有 `following` 与 `anchored` 两态：底部自动跟随；用户向上阅读后保存稳定 item/offset，滚动触底恢复跟随，不显示回到最新按钮或新增计数。左右侧栏共用 SidebarToggle 控件、分隔条机制和一个宽度值，支持 pointer capture 和键盘方向键，宽度限制为 220–420px；调整任一侧同时更新两侧。收起侧栏后零占位，仅保留展开按钮，不显示竖栏底板或顶部横条，右侧标题无底部分隔线。

请求观测记录保存当前 `ModelTurnRequest` 的消息、工具 schema 和模型偏好，不序列化 Provider 认证配置或私有回放字段；用户消息与工具内容按其原文记录。开始事件带入请求快照，结束事件更新状态；持久记录用于重新打开后的轨迹展示。工具执行耗时由执行器测量并随结果保存；缺失测量的历史或修复结果显示未知，不从页面停留时间推算。对话不另外展示请求观测行，也没有全局 Details 面板。

模型未配置时，首次页面直接显示已有提供方的缺失密钥输入；没有提供方时直接显示添加卡片，均可稍后配置。用户正文按原文呈现；助手 Markdown 使用 remark-math 与 KaTeX 渲染行内和块级公式。终端输出泵保留 ANSI ESC，客户端解析其样式，读取结果显示源行号，搜索结果保留文件与行定位，统一 diff 展示前后行号，工具输出提供复制。

## 3. Workspace、Session 与持久事实

工作台先选择已登记项目，再创建任务；任务 cwd 使用项目根目录，文件候选按当前项目或任务 cwd 搜索。任务 RPC 必须提供 workspaceId，并校验任务 cwd 归属。所有项目使用同一导航与任务路径，Agent 的本机执行权限不随项目分组变化。


`CanonicalWorkspacePath` 是 Workspace identity 的唯一 owner。它验证存在目录、规范化 Windows verbatim/分隔符并生成稳定展示值和等价比较键。`workbench.json` 版本 1 只保存登记根，使用 owner-only 文件与 atomic replace；Session 按其规范 cwd 动态分组，移除 Workspace 不删除文件或 Session。

Session 是严格 JSONL v5：

- header 包含 id、version、规范 cwd 与 timestamp；
- `message` 与 `compaction` 构成模型可见历史；
- `metadata` 保存 thread settings/name；
- `record` 保存 operation、durable control 与 `model_request` 终态观测。请求观测包含模型、序号、耗时、可用的 Token 统计和错误分类，不参与模型上下文或恢复；未上报用量时保持未知。

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

工作台的模型设置接收 schema 化 Provider 输入和只写 API Key；Composer 从同一 `RedactedModelCatalog` 呈现当前会话可用的模型与思考档位。获取可用模型同时读取提供方的容量和 effort 元数据，缺失时从 Models.dev 公共目录按准确 API 地址与模型 ID 补齐；公共目录请求不携带用户地址或凭据，不新增缓存或持久目录。查询使用表单当前地址，输入新密钥时优先使用新值，留空时复用该提供方的已存密钥，与 DSH 自定义提供方查询一致。候选只进入编辑草稿，由保存提交；提供方、地址、凭据或协议变更后丢弃旧发现请求的结果及候选，不锁定编辑字段；更新已有模型保留仍被支持的档位别名和默认选择，缺失字段保留原配置。仅支持 thinking 开关或 budget 的元数据不冒充 effort 档位。设置不提供打开配置文件或额外底层参数编辑界面。

侧栏完成与异常圆点表示本次页面打开期间观察到的后台未读终态；打开任务即清除，当前任务和首次加载的历史任务不显示，手动停止保持无红点。未读集合只在内存中维护，不写入会话事实或浏览器持久化。

上下文圆环合并到左下角压缩按钮；提示按“上下文窗口：”“百分比已用”“已用标记，共容量”三行居中展示；悬停或聚焦时，仅在已有真实输入用量和有效模型容量时显示占用。数据来自当前所选模型最近一次有用量的请求，`provider/attempt` 结束事件即可更新，不必等待整轮结束；缓存输入已包含在输入量内，不重复相加，也不累计历次请求或加上输出。下一请求尚无用量时保留上次测量；切换模型或压缩后清除，直到取得新的测量。未知数据不显示用量提示。浮层为12px文字、7px/11px留白和12px圆角。设置入口位于输入框“＋”工具栏，和压缩、主题按钮共用胶囊，不在侧栏重复显示。发送与停止共用 28px 粉白球体。动作断线保留草稿且不自动重放，不显示连接横幅；一般动作错误短暂提示，表单和队列错误留在原位置。

会话工具栏首次发送后自动展开，此后保留本次挂载期间的手动展开/收起选择；＋/−、压缩圆环及主题按钮均为单色图标。默认浅色，太阳/月亮切换深色与浅色，主题保存在既有浏览器 view；支持同文档 View Transition 时使用220ms淡变，减少动态效果时直接切换。代码高亮随主题切换。设置入口只在输入框工具栏；新任务由项目旁的＋创建。工作区收起再展开会恢复会话列表的默认截取长度。任务正常完成显示绿点，失败或异常中断显示红点，手动停止不标红；手动停止从当前轮已接受的取消控制记录推导，不另存状态。运行中的任务名称复用 thinking 扫光。

Windows目录选择由STA线程中的系统IFileOpenDialog拥有；打开时传入当前前台窗口作为owner，使文件夹选择框直接位于前台。取消返回空结果，选择路径仍经工作区路径owner校验。对话不显示任务完成/失败终态行及通用诊断、请求错误行；手动中断保留安静的已停止标记。请求错误、活动诊断和当前终态错误详情在轨迹查看；工具本身的失败仍在工具结果中显示。工具详情不提供复制完整记录按钮。

可执行文件的 PerMonitorV2 manifest 使原生文件夹选择窗口按屏幕 DPI 绘制。

发送前使用上次真实 provider usage 加尾部估算判断是否主动压缩；usage 缺失时对上下文条目估算求和。Provider 精确返回 `context_length_exceeded` 时，一个 turn 最多强制压缩并重建请求一次。ToolCall/ToolResult 成对保留，合法切点必须指向现有模型上下文条目。

## 7. 构建、发布与自动化入口

前端锁定 build 为 `tsc -b && vite build`。`build.rs` 将 `crates/cli/web/dist` 作为输入并嵌入 CLI binary；运行发布程序不读取源码目录，也不需要 Node.js。

发布工作流先用 Node 24 构建前端，再构建 Rust release binary。签名与打包脚本均从 `cargo metadata.target_directory` 解析 release root。归档只有一个运行时 `singularity.exe` 及 README、LICENSE、INSTALL；CycloneDX SBOM 把 Rust binary 与 npm production 依赖连接为同一交付物。

两个脚本共享 `release-common.ps1` 的 release root 解析与 workflow output 写入；SBOM 的隔离 workspace staging 保持由打包脚本拥有。

无交互状态码为 completed=0、interrupted=130、failed=1。`--json` 的准备失败也输出 failed summary；终态 stdout 写失败以失败退出，避免机器消费者把不完整输出误判为成功。

## 8. 评估与维护

`C:\Users\Lenovo\Desktop\Singularity-Evaluator` 通过 `singularity --json` 在隔离工作区运行真实任务，并以 checker 判分。评估器校验调用 binary 的绝对路径、大小和 SHA-256，并在判分前检查工具参数是否越过题面与 cell 边界。

修改 Host、协议、Session、Provider、工具、上下文或输出行为时，验证顺序是：相关 owner 测试、锁定 production build、workspace 确定性门禁、真实 production 浏览器旅程；涉及 Agent 能力的变化再执行获准的真实模型评估。

工作台控件、列表行、菜单和页签采用圆角；模型弹层沿用 reasoning-slider 0.0.4 的布局尺寸，宽 220px，紧贴模型按钮上方 8px 并按插件右移 30px，模型列表最高 200px，推理等级固定在下方且用英文显示，窄窗口限制在可用视口内。

模型选择器样式来自 [qjcnmd/dsh-reasoning-slider](https://github.com/qjcnmd/dsh-reasoning-slider) 的 `lib/client.js`，MIT 声明保留在 `styles/model-picker.css`；DSH 主题变量映射为本地设计变量，模型数据与保存动作沿用 Workbench Store。选择模型后保持面板打开，可继续调整推理等级。

项目图标与颜色属于浏览器 View state，以 Workspace ID 保存；项目图标点击或项目菜单进入紧凑选择面板，预设图标使用 Lucide，支持预设颜色及自定义颜色。悬停不替换项目图标，新建任务的加号常驻。左右侧栏开合采用宽度过渡，拖动调整时直接跟随指针；减少动态效果设置关闭这些过渡。右侧栏每次打开先显示选择首页，轨迹列表与详情在同一面板内切换；切换 Session 则使用该 Session 的轨迹。桌面侧栏与对话并排，窄窗口中轨迹覆盖右侧工作区，可用关闭按钮或 Escape 返回。

轨迹角色直接显示协议种类的英文名称。列表与详情、详情栏目使用 Motion 的 `AnimatePresence` 顺序过渡；列表折叠使用布局位移动画。返回列表恢复滚动位置和触发按钮焦点，减少动态效果设置使过渡时长为零。动画只属于展示层，不参与记录、请求或工具状态。

thinking停止或完成后保留彩色渐变及当时位置，animation-play-state切为paused；重新加载历史时以静止渐变呈现，不持久化动画进度。压缩按钮只保留自定义上下文浮层及aria-label，不保留原生title提示。

输入框工具栏自动、鼠标、键盘及Escape共用changeExpanded状态入口和同一条240ms宽度/透明度动画；首次开始仅触发展开一次，后续流式更新不覆盖用户选择。
