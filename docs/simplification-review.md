# 全项目精简与代码质量审查

审查日期：2026-10-27。对象为当前工作区源码，包含现有未提交改动。

实施状态：六项缺陷和八项等价精简已完成，静态检查及受影响操作的生产工作台验证通过。`generation_ms` 和取消通知超时后关闭共享连接两项按用户决定保留现状。已打包并整体替换 `Desktop/Singularity`。以下现状描述为审查时的证据。

整体职责划分、实现规模和依赖选择基本符合全局指令；主要差距在错误分类、失败后的状态反馈，以及少量重复计算与重复规则。建议先修复失败路径，再做保持功能不变的精简。没有证据支持重写核心执行流程。

| 类别 | 结论 |
| --- | --- |
| 真实缺陷 | 6 项，涉及 MCP、后台命令、历史图片、技能候选和模型发现 |
| 可直接精简 | 8 项，删除重复计算、临时集合、无效订阅或重复规则 |
| 需要取舍 | 删除一个无消费者的 RPC 累计字段；取消通知超时后的连接处置 |
| 文档质量 | 架构图存在过时源码符号 |
| 有依据的保留 | 现有必要并发、停止提交边界、会话写者、协议差异和交互机制继续保留 |

## 真实缺陷

### 1. 本地 MCP 管道阻塞时，停止和超时仍可能等待取消通知

位置：[client.rs:182](../crates/mcp/src/client.rs:182)、[client.rs:191](../crates/mcp/src/client.rs:191)。

已初始化的 stdio MCP 服务不再读取 stdin，工具调用参数超过管道可用缓冲时，发送任务可阻塞。用户点击停止后，代码等待 `notify_cancelled` 发完；取消通知又等待同一写入锁和管道刷新，因此不能完成。SDK 的工具超时也会等待取消通知，并不能解除这个等待。

| 当前流程 | 建议流程 |
| --- | --- |
| 发请求 → 等回复或停止 → 等取消通知写完 → 返回 | 对整个调用设置本地截止时间；停止后只有限等待取消通知，随后返回明确结果 |
| 工具超时交给 SDK；SDK 超时后仍等通知 | 外层等待覆盖发送和回复，不能把 SDK 超时等同于整个操作已结束 |

实际影响：当前会话可一直停在“停止中”，删除任务也会等待。完全退出有 Electron 的十秒强制终止上限，因此不应把它描述为整个应用无限无法退出；但正常收尾不能完成。

证据已追到锁定依赖 rmcp 3.5.0：`src/service.rs:564-571` 超时后等待取消通知；`:835-844` 等待通知发送结果；`src/transport/async_rw.rs:111-115` 在写锁内等待 `send/flush`。HTTP 的控制请求另有默认五秒限制，此无界管道结论限定于 stdio。

净收益：修复停止与超时的真实边界，不增加通用重试或恢复框架。取消通知也失败后是否关闭共享连接，见“需要取舍”。

### 2. MCP HTTP 会话过期时，SDK 默认重发工具调用

位置：[client.rs:132](../crates/mcp/src/client.rs:132)。

当前直接使用 `StreamableHttpClientTransportConfig::with_uri` 默认配置。rmcp 3.5.0 的 `reinit_on_expired_session` 默认为 `true`，普通 POST 收到表示会话过期的 HTTP 404 后，会重新初始化并将原请求重发一次；`tools/call` 属于普通请求。

这与项目约定“调用失败直接返回、不重放”不一致。该发现并不意味着所有 404 都已经执行过工具，也不等同于已经证明发生了重复副作用；确定的问题是实际调用策略与已确认契约不同。

最小修法：显式设 `reinit_on_expired_session = false`。保留下一轮发现时正常重新建立连接的能力，删除本次调用内的隐式恢复和重发步骤。

依赖证据：rmcp 3.5.0 `src/transport/streamable_http_client.rs:1057-1065`、`:1622-1649`、`:2064-2075`、`:2175`。

### 3. 后台命令终态保存失败后，界面仍显示运行中

位置：[tasks.rs:319](../crates/agent/src/tools/bash/tasks.rs:319)、[background.rs:33](../crates/app/src/runtime/conversation/background.rs:33)、[sync.ts:26](../apps/desktop/src/sync.ts:26)。

```mermaid
flowchart LR
    A[进程已结束] --> B[终态日志保存失败]
    B --> C[内存 task 改为 failed]
    C --> D[只发布会话错误]
    D --> E[前端保留旧 running task]
    E --> F[重新读取任务才更新]
```

失败分支只发送 `BashTaskUpdate::Failed(String)`。后端运行快照不包含后台任务列表，前端 `session_changed` 保留旧列表；结果是顶栏数量、后台面板的“运行中”和停止按钮、消息区 Bash 卡片都沿用旧状态。

最小修法：失败通知携带已保留的任务快照；会话先暂停自动继续，再发布失败任务和会话错误。不要把失败任务直接送进当前会唤醒下一轮的成功终态路径。日志继续保持失败事实，不伪造保存成功。

净收益：内存状态与可见状态重新一致，现有界面即可正确显示，无需增加轮询或新入口。

### 4. 历史图片读取失败被误归为日志写入失败

位置：[context.rs:310](../crates/agent/src/session/context.rs:310)、[request.rs:160](../crates/agent/src/agent/request.rs:160)、[compaction.rs:55](../crates/agent/src/compaction.rs:55)、[agent/mod.rs:75](../crates/agent/src/agent/mod.rs:75)、[turn.rs:95](../crates/app/src/runtime/conversation/turn.rs:95)。

历史图片副本丢失或不可读时，读取错误转成 `AgentError::Session`，继而统一归为 `Store`。普通回合遇到这一分类就不再写结束记录，即使日志仍完全可写。当前页面看到图片错误，重新打开后却只剩没有结束记录的“中断”，原失败原因消失。自动压缩读取图片也经过同一错误分类。

最小修法：在读取请求材料的边界区分“图片读取失败”与“日志追加失败”；只有真正的日志保存故障才跳过结束记录。保留真实路径和底层错误，不添加图片兜底或静默跳过。

净收益：去掉错误用途混淆，失败原因能随普通回合恢复。独立压缩继续沿用自身已有的收尾规则。

### 5. 等待回答时，隐藏的技能候选仍拦截键盘

位置：[Composer.tsx:30](../apps/desktop/src/components/Composer.tsx:30)、[Composer.tsx:64](../apps/desktop/src/components/Composer.tsx:64)、[Composer.tsx:104](../apps/desktop/src/components/Composer.tsx:104)、[Composer.tsx:162](../apps/desktop/src/components/Composer.tsx:162)。

输入框有 `/技能前缀`，随后模型发起提问时，候选 DOM 因 `pendingQuestion` 被隐藏，但 `showCandidateSurface` 和键盘处理没有同一条件。Enter/Tab 仍可插入不可见的候选，方向键仍被拦截，ARIA 也可能指向不存在的列表。

最小修法：把“允许显示和操作候选”集中成同一条件，供目录请求、浮层、键盘与 ARIA 使用。保留等待回答时原有的输入框与停止操作，不增加控件或改变已确认交互。

净收益：删掉分散且不一致的规则，消除不可见选项改变草稿的问题。

### 6. 模型发现丢弃提供方错误正文

位置：[discovery.rs:31](../crates/model/src/config/discovery.rs:31)、[discovery.rs:76](../crates/model/src/config/discovery.rs:76)。

模型目录接口返回非 2xx 时，代码直接丢弃响应正文，只显示 HTTP 状态。提供方已经说明“密钥撤销”“目录权限不足”等原因时，工作台仍无法区分；前端只转发该文本，没有补回原因的路径。

最小修法：保留当前 HTTP 分类与手动重试规则，读取并使用已有错误正文提取、截断逻辑附上原因。正文读取失败时保留 HTTP 状态并报告读取错误。正常模型请求已有对应实现：[provider.rs:126](../crates/model/src/openai/provider.rs:126)。

净收益：符合“查询失败显示实际原因”的现有要求，不新增重试、提示入口或配置。

## 可直接精简

| 项目与位置 | 当前实现 | 最简单替代 | 净收益与行为差异 |
| --- | --- | --- | --- |
| 历史用量重复扫描：[thread_catalog.rs:152](../crates/app/src/runtime/thread_catalog.rs:152)、[view.rs:93](../crates/app/src/runtime/conversation/view.rs:93) | 历史投影逐条累计用量，丢弃最终值；随即再次扫描整份日志求合计 | 历史投影返回历史及最终累计值，`Conversation::read` 直接使用 | 少一次全日志遍历；仍从同一写者快照重新统计，保持保存与事件交错窗口的正确性，不移除实时累计 |
| 水球重复粒子投影：[WaterOrb.tsx:104](../apps/desktop/src/components/orb/WaterOrb.tsx:104) | 同一批粒子按同序向两个清零数组执行相同 `splat` | 只投影到 `physicalField`；水平模糊直接读它，垂直模糊仍写 `field` | 删除一次清零和整次粒子×邻域循环，无额外复制；两份输入逐元素相同，保留物理湿区判断和显示结果，不宣称未经测量的帧率提升 |
| 尾部截断重复判断：[output.rs:121](../crates/agent/src/tools/output.rs:121)、[truncate.rs:49](../crates/agent/src/tools/truncate.rs:49) | 调用方已确认超限，截断函数又完整计数并支持“不截断”，结果用 `Option` 再 `expect` | 截断函数只接收已超限缓冲，直接选取尾部并返回确定的截断原因 | 删除重复全量行计数、不可达返回分支和 `Option/expect`；保留行数、字节、UTF-8 边界及提示规则 |
| UTF-8 前缀多余能力：[core/lib.rs:54](../crates/core/src/lib.rs:54)、[read.rs:137](../crates/agent/src/tools/read.rs:137) | 公共 helper 返回切片和是否截断；唯一调用已确定超限且忽略布尔值 | 在已超限分支直接按 `floor_char_boundary` 取前缀 | 删除 helper、重复判断及未使用返回值，移除一次跨 crate 依赖；不是仅因单一调用方而删函数 |
| 模型目录中间集合：[discovery.rs:54](../crates/model/src/config/discovery.rs:54) | 两种目录先收集 `Vec<(&str, &Value)>`，再遍历插入 `BTreeSet` | 两种输入形状直接迭代写同一个集合，共用必要的校验和插入 | 删除一份临时集合、分配和遍历；排序、去重、非法条目跳过及两种目录支持不变 |
| 消息时间重复生成：[agent/mod.rs:190](../crates/agent/src/agent/mod.rs:190)、[agent/mod.rs:268](../crates/agent/src/agent/mod.rs:268)、[request_execution.rs:100](../crates/agent/src/request_execution.rs:100) | 保存已返回时间，但实时事件另取 `now_iso`；失败片段还在保存前取时间 | 继续传递 `append_message_with_id` 返回的保存时间，三条事件路径共用 | 删除三处独立取时规则；实时与历史时间一致，避免跨显示时间边界时刷新后时间变化；不改日志格式、不新增消息克隆 |
| Composer 无效订阅：[Composer.tsx:18](../apps/desktop/src/components/Composer.tsx:18) | 订阅 `questionDrafts`、`actionErrors`、`theme`，组件及接收 state 的子组件没有消费它们；强调色已由参数传入 | 删除这三个订阅，保留真正参与输入、选择与禁用状态的字段 | 减少问答编辑等无关状态触发的输入区重绘；行为不变 |
| 动效接口遗留能力：[motion.ts:16](../apps/desktop/src/motion.ts:16)、[motion.ts:48](../apps/desktop/src/motion.ts:48) | `disclosureTransition` 的 `_expanded` 不使用，唯一调用总传 true；`--disclosure-ease` 只写不读 | Composer 直接选择减少动态效果时长或既有 `smooth`；只写实际消费的收起 easing | 删除薄包装、无效参数及无消费者变量写入；沿用原弹簧与减少动态效果行为 |

## 需要取舍

| 决策 | 现在 | 建议与实际影响 |
| --- | --- | --- |
| 删除累计字段 `generation_ms` | [params.rs:223](../crates/protocol/src/params.rs:223) 定义，[history.rs:276](../crates/app/src/runtime/history.rs:276) 累计，前端只有生成声明；没有消费者 | **建议删除**字段、累加及生成声明。少一项状态和传输数据；原始每次请求的 `duration_ms` 仍在日志中，计时事实保留。改变 RPC 形状，需要确认；前后端须一起更新 |
| MCP 取消通知发送超时后关闭连接 | 无限等通知会卡住停止；仅退出本地等待，堵塞的传输可能继续留在连接池 | **建议只在取消通知也无法发送时关闭该失效连接**，正常工具响应超时不一律断连。影响同一目录、同一服务器上的并行调用；下轮重新发现时重建连接。这个影响需要确认，不应悄悄扩大为所有超时都断连 |

没有建议删除两种模型协议、后台命令、MCP、技能、图片、动画或现有交互。

## 有依据的保留

| 对象 | 更简单方案 | 保留依据和前提 |
| --- | --- | --- |
| 删除任务的关闭状态与 guard | 直接停完删除，失败处各自复原 | 旧异步对象不能重新启动已删任务；删除失败又需开放入口。guard 集中处理这些结果，减少遗漏 |
| 模型配置串行读改写与边界转换 | 去锁、缓存配置，或表单直接使用落盘类型 | 并发 RPC 仍可能丢更新；缓存多出同步状态；未填写容量和密钥展示与持久化形状确有差异 |
| Chat/Responses 分开编码，共用真正共同的传输 | 一套带协议分支的通用状态机 | 工具图片、内容顺序、完成信号和私有续接形状不同；合并只会搬移复杂度 |
| Responses 终态与失败片段分别保留 | 只留终态或只留文本增量 | 前者丢取消片段，后者丢完整有序块和续接材料，两者服务不同结果 |
| SSE 换行及 BOM 状态 | 用双换行切字符串 | 无法处理合法 CR、跨块 CRLF、首行 BOM；属于当前外部协议边界 |
| 输入队列确认与多个交付点 | 取出即删除、每轮只交付一次 | 保存成功才消费；准备、压缩和请求之间可能收到新输入，删掉会改变交付时机 |
| 工具完成即保存、请求时按 ID 配对 | 按调用顺序等待全部完成 | 当前并发与即时反馈有明确要求；改法新增结果缓冲并让慢工具拖住其他结果 |
| 单次请求定义、Token 估算与压缩准备边界 | 每次重新加载，或跨轮缓存完整请求 | 前者重复发现，后者引入失效状态；压缩和新输入会改变预算，不能直接缓存先前数值 |
| 用户消息的小段公开投影 | 为统一 `public_items` 克隆保存用消息 | 当前映射固定为一段文本加附件；单为去重而复制完整技能正文或改通用保存职责，净收益不足。保存时间应单独统一 |
| Bash 前台等待后移交同一进程 | 所有命令一开始就持久登记为后台任务 | 短命令多两次记录及事件，还需抑制通知并改变列表呈现；当前按期限移交符合需求 |
| UTF-8 残字节、输出保存失败、进程回收错误分类 | 删除中间状态或合并全部失败 | 分块字符、损坏完整输出文件、是否阻止任务删除是不同事实，删掉会丢正确行为 |
| 前端读取序号、事件暂存、草稿与答案局部状态 | 只用最新返回值、直接依赖组件状态 | 异步响应可晚到，刷新和切换有既定保留语义；已确认取舍前提不变 |
| 原生浮层、展开动画、滑块提交队列和拖选保护 | 删除控制状态、同时提交每次拖动 | 会改变已确认键盘、焦点、动画、保存顺序和选字操作；正常任务切换已用 scope key 和卸载清理丢弃排队编辑 |
| 类型生成的方法表及递归依赖导出 | 手工维护 TypeScript DTO | 多维护一份同一契约；现有生成方式已经足够，没有新框架需求 |

新的保留结论已同步到 [架构说明的精简审查结论](architecture.md#精简审查结论)。已确认取舍中的限制前提未变，未重新当作缺陷报告。

## 文档质量

架构图源码文件路径和连线端点均存在，但悬停符号仍有陈旧内容：

| 位置 | 当前标注 | 实际入口 |
| --- | --- | --- |
| [architecture.html:1922](../architecture.html:1922) | `SessionManager::operation_finished` | `Conversation::run_single_turn` 构造结束记录，调用 `SessionManager::append_record` |
| [architecture.html:1914](../architecture.html:1914) | `SessionManager::append_message` | `SessionManager::append_message_with_id` |
| [architecture.html:1883](../architecture.html:1883) | `RequestContext::generate_response` | `Agent::generate_response` |
| [architecture.html:2253](../architecture.html:2253) | 管理页入口包含 `read_skill` | Skills 列表与通用 `file.read`，没有该方法 |

建议只更新原节点的源码说明和对应文字，保持画布、布局和交互。不是因大图行数多而建议拆图。

## 全局代码质量要求对照

| 要求 | 审查结论 |
| --- | --- |
| 文件不超过 500 行 | 186 个 Rust/TS/TSX/CTS/MJS/CSS 文件，共 19,662 行；没有超过 500 行或达到 450 行的文件。生成声明包含在统计内，HTML 架构文档不按源码文件限制评价 |
| 职责、状态与数据流清楚 | App 装配、Conversation 生命周期、Agent 循环、模型编解码、MCP、日志写者和前端状态分工明确；图片错误用途混淆是具体需要修正的例外 |
| 同一规则归属一致 | 用量、工具结果、类型导出等已有真实复用；候选显隐、消息时间和重复统计仍可收敛 |
| 不过度设计或防御 | 未发现需要删掉整套核心架构的依据；可删项集中在局部重复分支、临时集合和无消费者字段 |
| 返回真实错误与反馈 | MCP 停止、后台保存、图片读取和模型发现存在上述缺口；用户已接受的草稿静默失败等不重复报告 |
| 控制前端渲染与请求 | 有事件分批、稳定历史引用、按需加载及局部状态；水球同帧重复投影可直接删除，隐藏技能候选应统一请求条件 |
| 注释能定位行为与修改入口 | 关键取消、写入和锁边界有原因说明；架构图的过时符号增加维护成本，应原位校正 |
| 不增加无需求工具链 | 建议均可使用现有库及标准库；不需要新依赖、插件层或通用框架 |

## 操作覆盖与证据边界

| 操作组 | 已读到的具体路径 |
| --- | --- |
| 启动、刷新、托盘与退出 | Electron main/preload/backend、私有协议、IPC 来源检查、单实例、后端管道 EOF、已接受 RPC 收尾、前台/后台停止及十秒退出限制 |
| 项目和任务管理 | 项目登记/重复/移除忙碌检查；首次发送创建、打开写者、改名、模型设置、删除等待及失败恢复、附件删除、历史与摘要 |
| 输入和交互 | 草稿加载/保存/失败/清理、附件、技能引用与候选、排队、停止、提问回答、切换页面与晚到请求 |
| 模型执行 | 两种协议编码/流解码/结束/截断/失败/取消、重试、用量、续接来源、配置保存/删除/发现/导入 |
| 上下文 | 指令与技能加载、MCP 发现、预览取消、Token 预算、工具配对、自动/手动/超限压缩、摘要提交和历史恢复 |
| 工具与进程 | 注册/参数预检/并发调度；read/图片；Bash 管道、输出截断、前后台移交、列表/输出/停止/完成通知；MCP stdio/HTTP、调用/超时/重连/退出 |
| 界面显示 | 实时与历史合并、用量、时间线分组、后台状态、Markdown/KaTeX/Mermaid、文件标签、主题、浮层、滚动锚点、水球与动效 |
| 配套文件 | Cargo/锁文件使用约束、npm 及锁文件根声明、TypeScript/Vite/Electron 打包、runtime 拷贝脚本、CI、依赖策略、源码导航与架构图交互脚本 |
| 反查覆盖 | Rust 模块与 RPC 表、前端组件及样式入口、版本控制文件清单；生成物追到 Rust 类型导出；仓库没有单独测试目录或嵌入测试 |

本次验证：`cargo fmt --all -- --check`、全仓 `cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings`、`npm --prefix apps/desktop run build` 均通过。npm 包声明与锁文件根依赖一致。前端构建提示 LiquidFun 的 Node 模块被外置及部分 chunk 大于 500 kB；仅凭这些提示不能证明运行缺陷。

源码修改完成后，已在 production Electron 工作台使用真实 `Codex / codex/gpt-6-luna / max` 验证：提问期间候选停用与回答后恢复、缺失图片错误保存及刷新、恢复验证附件后继续对话、长行读取与 Bash 输出截断、真实 XERJ MCP 查询、消息实时保存时间一致、后台终态保存失败立即更新界面、真实模型压缩、停止后重开历史与累计用量一致、减少动态效果下的工作台绘制，以及真实模型目录 404 响应的具体原因。没有新增测试；MCP 管道堵塞和 HTTP 会话过期仅由源码及锁定 SDK 调用链验证。图片错误修正只使用既有失败记录及错误文案，没有新增恢复流程或专用提示。

## XERJ 索引修复

旧 generation 中 `surfaces.css` 从 `txt-lines` 变为 `txt-prose`，增量同步被拒绝。备份本项目状态、映射、文档和 catalog 后，精确清理 Harness 记录并按原位置重建。

| 检查 | 结果 |
| --- | --- |
| 前缀及状态目录 | `harness`；`D:\XERJ\autoindex\harness` |
| 索引 | `harness-docs` 1,924 条；`harness-apps-desktop` 11 条 |
| 文件 | 213 个全部入库，172 个代码文件成功，解析跳过 0 |
| 增量复验 | 再次同步退出码 0，记录数一致 |
| 查询复验 | 当前 `bootstrap_with_catalog`、`surfaces.css` 的 `position-try-fallbacks` 均命中；已删除 `workspace_files.rs` 的旧记录清除 |
| 备份 | `D:\XERJ\autoindex\harness-backup-20261027-160825` |

修复只涉及本项目索引和状态，其他项目 catalog 保留。
