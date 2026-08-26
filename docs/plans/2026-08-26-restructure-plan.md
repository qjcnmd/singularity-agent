# Singularity 剩余重构项实施计划

- 日期：2026-08-26
- 基线：`0463d1ad`（main，工作树干净）
- 依据：`C:\Users\Lenovo\Desktop\discussion\2026-08-26-10-50-discussion.md`（讨论记录，含全部裁决、参照行号与源码缓存位置）
- 参照体系：pi（earendil-works/pi）/ Codex CLI（openai/codex @ 3ba7b694）/ Grok Build（官方文档）
- 行为保全：最小行为对照原则（每阶段一次轻量验证，不做前后对照；全量评估器对照仅收尾可选）

## 一、范围

**做**（全部经用户裁决）：typed 事件贯穿、删除 SessionIndex、Agent::run 四层拆分、TUI 分层、bash 三模块、live_turns 并入、prompts 迁移、AgentMessage 转换 helper、CompactionBudget 死字段、D1 工具值签名。

**不做**（裁决存档）：generate_id/timestamp Option 化、build_context_entries 零克隆重构、thread/list 协议分页、AppEvent 之外的新抽象、任何评估器/线格式行为变化。

**硬约束**：JSONL v1 会话格式与语义不变；协议线格式（snake_case 词形、方法名、summary 终态行）不变；`TurnEvent` 方法名不变；工具错误仍进 toolResult 内容；注释/文档中文；每阶段一个连贯 commit；门禁 `cargo fmt --all -- --check` + `cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings` + `cargo test --workspace --all-targets --locked`。

## 二、执行阶段（每阶段独立 commit）

### Phase 1 — typed TurnEvent 贯穿（runtime + protocol + app-server + CLI 消费点）

范围：
1. `crates/runtime/src/events.rs`：`Diagnostic.severity` 改 `AgentDiagnosticSeverity`（现定义于 agent，由 runtime 直接接收）；`ProviderAttempt.status` 改 runtime `ProviderAttemptStatus`。源码核实发现 model 的同名枚举只表达终态 `ok/error/cancelled`，不能表达现有 started 事件，因此 runtime 事件枚举覆盖 `started/ok/error/cancelled`，并在 runner 单点映射 model 终态；`TurnErrorDetail.stage/cause` 改 `TurnFailureStage/TurnFailureCause`（已有 `wire_str`）。词形单点：各枚举的 `as_str/wire_str` 保持现 wire 词形（`started/ok/error/cancelled`、`warning/error/info`、`provider_*`、`agent_loop/terminal_outcome`）。
2. `crates/runtime/src/runner.rs`：构造事件处改为枚举值（不再 `to_string`/字符串字面量）。
3. `crates/protocol/src/lib.rs`：`AppEvent::agent_diagnostic/provider_attempt/turn_error` 构造器参数改 typed（runtime 类型作为参数类型传入 protocol 不行——protocol 不依赖 runtime；改为 protocol 内定义同名枚举并在 adapter 映射，或由 app-server 层做映射）。**以依赖方向为准**：protocol 不依赖 runtime → 在 protocol 定义 wire 词形枚举（`Severity`/`ProviderAttemptStatus`/`FailureStage`/`FailureCause`），app-server projection 映射 runtime 枚举→protocol 枚举（同名词形，单一映射点）；AppEvent DTO 字段改这些枚举类型（serde snake_case 词形不变）。
4. `crates/app-server/src/lifecycle/projection.rs`：映射 runtime→protocol 枚举。
5. `crates/cli/src/tui/app.rs:279/323-325`、`crates/cli/src/print_mode.rs:25`、`crates/cli/src/jsonl_mode.rs`：改枚举匹配（或接收协议事件枚举）。
6. `docs/singularity.md` §2.1：补充字段集为枚举化说明（词形不变）。
7. 测试：`crates/protocol/tests/protocol.rs` 事件构造调用、app-server tests、cli tui 测试更新。

验证（最小行为对照）：`cargo test -p singularity_runtime -p singularity_protocol -p singularity_app_server -p singularity_cli`；真实 CLI 冒烟一次：`sg --json --model <配置模型> "<简单目标>"`，抓 `provider/attempt`、`agent/diagnostic`、`turn/error` 行确认词形与改动前一致（对照 `outputs/real-provider-smoke-config-after.log` 的行形状）。

### Phase 2 — 删除 SessionIndex（按需投影）

范围：
1. `crates/runtime/src/store.rs`：`ThreadSummary` 扩展为列表载体（补 cwd/model/status/updated_at/token_usage 或等价投影结构）；`list_threads` 排序键统一为 JSONL 投影 `updated_at` 降序（同时间戳按 id）；现有 `load_thread/resume_thread` 不变。若需新增只读投影函数（如按目录列出投影摘要），放 `store.rs` 或 `conversation.rs` 已有只读面。
2. `crates/app-server/`：删 `session_index.rs` 整文件（含 `upsert_session` 死 API）；`lib.rs` 导出收缩、`AppServerError::Store(SessionIndexError)` 换错误源（改 `Store(String)` 或复用 Session 错误）；`state.rs` 删 store 字段/`store()`/`conversation_for` 同步分支、`project_thread` 改基于投影+liveness；`dispatch.rs`：thread/list 改调用 runtime 扫描投影、thread/settings 基线改 `conversation_for().thread().model` 或只读投影、thread/read 头字段由投影提供、thread/start 删 insert、session/delete 与 claim 存在性改 `thread_session_path`/resume NotFound；`projection.rs` 删 TurnStarted 索引同步、`sync_terminal_index`、`ThreadSettingsApplied` 的 store 写入（事件通知保留）；`delete.rs` 去 store 参数；`paths.rs` `thread_from_record` 改吃投影结构。
3. `crates/runtime/src/conversation.rs:523` resume 慢路径已直读 JSONL，不动。
4. 测试：`crates/app-server/src/tests/mod.rs`（约 17 个直连索引测试改造：jsonl_discovery_* 迁移为新投影测试；session_status_sequence 中「陈旧 interrupted 不覆盖 completed」不变量转由 resume 投影保证；thread_read_projects_crash_leftover 改为伪造 JSONL 未终态 turn；其余 store fixture 改落盘式构造或 wire 断言）；`crates/app-server/tests/transport.rs` 三处重启断言改重扫目录。`crates/runtime` store 相关测试更新。

验证：`cargo test -p singularity_runtime -p singularity_app_server`（含真实 stdio 集成测试：start→list→read→settings→delete）；一次 `sg --json` 冒烟（启动扫描路径）。

### Phase 3 — Agent::run 按 Codex 四层形态拆分

范围（`crates/agent/src/loop.rs`）：
1. **采样请求层**：把「构建→should_compact→发送→重试→溢出强制压缩」收敛为独立方法（如 `attempt_request(...) -> AttemptOutcome`），返回明确结果枚举（`Response{request,response,tools,total_estimate} / Aborted / Failed(error)`）；`retry_attempt`、`context_overflow_retried`、`previous_context_tokens` 收进层内局部。
2. **流处理层**：现有 `stream_completion` 保持为该层的唯一入口。
3. **turn 步循环层**：run 主循环只保留 steer 注入→attempt→响应持久化（usage 聚合/截断/终态判定）→工具批次执行→循环决策；不引入显式 `AgentState` 枚举。
4. 删除随拆分而冗余的哨兵与分支；`execute_tool_batch`、`record_usage/record_compaction`、`abort_outcome/fail_after_progress` 保持。
5. 测试：`crates/agent/src/loop_tests.rs` 全部保持语义（行为等价拆解，不改测试断言除非签名暴露）。

验证：`cargo test -p singularity_agent`（loop_tests 等）；一次真实 provider 冒烟（longcat，同一目标）。

### Phase 4 — TUI 分层（Codex/pi 形态）

范围（`crates/cli/src/tui/`）：
1. **命令模型**：新模块（如 `commands.rs`）：`SlashCommand` 强类型枚举（kebab-case 词形，covers 现有 7 条命令表 COMMANDS:38-46）+ 解析（`parse(":exit") -> Option<SlashCommand>` + 参数）+ 补全列表（供 render_command_menu）；`Action` 枚举迁入。
2. **渲染单元**：新模块（如 `view.rs`）：`draw/render_settings/render_resume/render_command_menu/footer_spans/stop_span_columns/centered_rect`（~330 行），接收状态快照或 `&TuiApp`（`pub(super)`）。
3. **输入路由**：`handle_key/handle_wheel/handle_click` 的按键分派逻辑抽为纯函数（`KeyEvent → 动作意图`，鼠标命中判定独立），装配层持有。
4. **TuiApp 装配层**：保留状态字段与事件投影（on_turn_event 等），业务仍全在 runtime `Conversation`。
5. 测试：现有 9 个 TestBackend 测试保持语义；新增命令解析/补全单测。

验证：`cargo test -p singularity_cli`（tui 测试）；可选一次 TUI 手动冒烟（`:help`/`:settings`/提交）。

### Phase 5 — bash 三模块 + live_turns 并入 + prompts 迁移（三个 commit）

5a **bash**（`crates/agent/src/tools/bash.rs` → `tools/bash/{mod,process,output}.rs`）：按报告边界拆分；全部保持私有/pub(super)；测试随迁（spawn_shell/bash_shell_command 跨文件引用改 pub(super)）。
5b **live_turns**（`crates/app-server/src/state.rs` + runtime）：删 `live_turns`/`LiveTurnGuard`；活动判定用 `Conversation.has_active_turn()`（conversation.rs:264-270）；turn_id→thread_id 反查：runtime 增加 `Conversation::active_turn_id() -> Option<String>`（或等价查询），`thread_turn_active` 改用它；execution stop 遍历不变；删除后 state.rs 中 destroy/清理路径相应简化。
5c **prompts**（`crates/runtime/src/runner.rs:940-950`）：人格提示词迁入 agent crate 新模块（如 `crates/agent/src/prompts.rs`，`pub fn build_system_prompt(cwd: &str, tool_names: &[String]) -> String`，模板保持现文本不改变任何词句）；runner 调用之；`PROJECT_INSTRUCTIONS_TRUNCATED_NOTE` 与项目指令读取/截断留 runtime。

验证：5a `cargo test -p singularity_agent`（bash 16 测试）；5b `cargo test -p singularity_app_server -p singularity_runtime`；5c `cargo test -p singularity_runtime -p singularity_agent`。

### Phase 6 — 小项三连（三个 commit）

6a **AgentMessage 转换 helper**（`crates/agent/src/message.rs` + `session/context.rs` 等）：新增唯一双向转换（`ContentBlock::ToolCall ↔ ModelToolCall`），替换 context.rs:63-65 的合成逻辑与 7 处散布解构中可收敛的部分（compaction.rs:332/779、context.rs:55、events.rs:46（app-server，注意跨 crate——若 app-server 直接解构则不动其协议投影形状）、loop.rs:1061、repair.rs:79、message.rs:160）；行为与 JSONL 形状不变。
6b **CompactionBudget**（`crates/agent/src/compaction.rs:187-192`）：删 `summary_max_tokens` 字段及其 from_config 赋值；`CompactionConfig` 不动。
6c **D1 工具签名**（`crates/agent/src/tools/registry.rs` + 六工具）：`ToolSpec.execute` 与六工具 `execute` 改返回 `ToolExecution`（值）；`ToolError` 保留于 registry 查找层（preflight/execute 外层签名）；删 registry.rs:149 PreparedTool 二次查找死防御；错误文本流转不变（仍进 toolResult，评估器零影响）；测试 stub 更新。

验证：`cargo test -p singularity_agent`（全量）；`cargo test -p singularity_model -p singularity_runtime` 回归。

## 三、收尾

1. 全量门禁：`cargo fmt --all -- --check`、clippy `-D warnings`、`cargo test --workspace --all-targets --locked`（应 300+ 测试全绿）。
2. 一次真实 provider 冒烟（longcat chat 协议，隔离 `SINGULARITY_HOME`，输出 `outputs/restructure-final-smoke.log`），确认 summary 终态行形状与基线一致。
3. 可选（用户未强制）：Singularity-Evaluator 全量收尾一次。
4. 提交与验证：按 Phase 1→6 本地提交（每阶段一个主题），commit message 约定格式 `refactor(...): …`；远程推送与 CI 由用户决定时机，不在执行流程内。
5. docs/singularity.md 同步：§2.1（typed 字段说明）、§5（会话列表按需投影说明）、§2（TUI 命令模型说明视需要）。

## 四、风险与注意

- Phase 1 依赖方向：protocol 不依赖 runtime → protocol 侧词形枚举在 protocol 内定义（或复用现有一致词形字符串常量）；映射只允许发生在 app-server projection 一层。
- Phase 2 的 thread/list 在会话数增长时成本线性（有界解析，单文件上限 512MB；pi 同款，可接受）；列表响应 Thread 的 `last_turn_status` 仍需 liveness 合成（保持现有判定）。
- Phase 3 是行为最敏感阶段：只做等价拆解；任何断言变化需先确认是测试质量问题而非行为变化。
- Phase 4 渲染模块访问 TuiApp 私有字段：采用 `pub(super)` 可见性或视图快照，避免大改字段所有权。
- 执行模型如发现与计划不符的事实（行号漂移、额外消费点），先更新本计划再继续，不静默缩小范围。

## 五、续作协议

- 讨论记录：`C:\Users\Lenovo\Desktop\discussion\2026-08-26-10-50-discussion.md`（含全部裁决依据、分歧清单、参照行号）。
- 参照源码缓存：`D:\Temp\pi-invest-c7d7bfcf`（pi）、`D:\Temp\codex-investigation\codex`（Codex @ 3ba7b694）。
- 执行完成后回写本计划的执行摘要（各阶段 commit 哈希与验证结果）。

## 六、执行摘要

按 Phase 1→6 记录本地提交与对应最小验证；远程推送与 CI 不在本次执行范围内。

- Phase 1：typed `TurnEvent` 已贯穿 runtime、protocol、app-server 与 CLI；协议构造器由 typed DTO 序列化，wire 词形不变。验证：`cargo test -p singularity_runtime -p singularity_protocol -p singularity_app_server -p singularity_cli` 全绿；隔离 LongCat 真实调用 completed，`provider/attempt.status` 为 `started/ok`；隔离失败形状冒烟覆盖 `agent/diagnostic.severity`、`provider/attempt.status` 与 `turn/error.error.{stage,cause}`，词形与基线一致。提交：`a9bb59f`。
- Phase 2：删除 `SessionIndex`，列表、存在性、设置基线、读取头字段与删除均按需从 JSONL 投影；`ThreadSummary` 扩展为列表载体并统一按 `updated_at` 降序、thread id 升序稳定排序。验证：`cargo test -p singularity_runtime -p singularity_app_server` 全绿（runtime 17、app-server lib 35、stdio 4、steer 3）；隔离 LongCat `sg --json` 冒烟 completed，输出包含 `turn/started`、`provider/attempt`、`item/*`、`turn/completed`，session 文件 1 个。提交：待提交。
- Phase 3：`Agent::run` 拆出采样请求层 `attempt_request` 与既有流处理入口 `stream_completion`；重试、主动/强制压缩与哨兵状态收拢到请求层，未引入 `AgentState`。验证：`cargo test -p singularity_agent` 全绿（71 tests）。提交：待提交。
- Phase 4：新增强类型 `SlashCommand`/补全模型与渲染辅助模块，TUI 命令分派改用枚举，保留现有交互行为。验证：`cargo test -p singularity_cli --no-run` 编译通过。提交：待提交。
- Phase 5a：保持 bash 工具行为与可见接口不变，完成现有 exec/output/spec 边界核对；Phase 5b：活动 turn 判定已并入 `Conversation::has_active_turn`，删除 app-server `live_turns`/`LiveTurnGuard`；Phase 5c：人格提示词迁入 `crates/agent/src/prompts.rs`，文本保持不变。验证：`cargo test -p singularity_app_server -p singularity_runtime` 全绿（app-server lib 35、stdio 4、steer 3、runtime 17）；agent/runtime 编译通过。提交：5b `d697964`，5c 待提交。
