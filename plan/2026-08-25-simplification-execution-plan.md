---
goal: 执行 2026-08-25 审查裁决：修复行为正确性缺陷并收敛多余复杂度，使 Singularity 成为可靠的最小 coding harness
version: 1.0
date_created: 2026-08-25
owner: Singularity maintainer
status: 'In Progress'
tags: [architecture, refactor, reliability, retry, compaction, tools, protocol, session]
---

# Introduction

本计划把 2026-08-25 讨论记录（`C:\Users\Lenovo\Desktop\discussion\2026-08-25-15-05-discussion.md`）中用户逐项裁决的 30 项结论转换为可执行、可验证的实施合同。基线 `main@610767b4`。参照物：pi 0.84.2、Codex CLI（codex-rs）、Claude Code 文档（关键事实均经本机或源码核实）。

**跨会话续作协议**：本文件是执行阶段唯一事实源。恢复执行时先读本头部状态与各 TASK 勾选；每个 TASK 完成后立即更新勾选与提交号；执行过程中的阻断、偏差、评估对照记录追加到文末"执行日志"，不回写讨论记录。

## 执行 checkpoint

- revision: `cac8e9c`
- branch: `main`
- phase: Phase 5 — 文档同步与最终门禁
- next task: TASK-502

## 1. Requirements & Constraints

- **REQ-001（P0-1 重试收敛）**：传输层改为单次 HTTP attempt + 类型化错误；`Retry-After` 值解析后随错误对象传递；"可见输出已发出后禁止自动重试"约束保留在传输层。Agent 层 `TurnRetryConfig`（3 次、指数退避抖动、可取消等待）成为唯一重试策略，并 honor Retry-After。删除传输层重试循环及其独立的退避/抖动/attempt 计数逻辑。
- **REQ-002（P0-2 压缩触发）**：压缩触发改为 pi 式——每轮响应后以上一轮真实 provider usage 对比 `context_window − reserve_tokens` 决定下一请求前是否压缩；usage 缺失时 fallback 到本地估算，且 `estimate_assembled` 必须计入 reasoning replay 的序列化尺寸。Provider `ContextLengthExceeded` 强制压缩+同轮重试路径保留。修正 `estimate_assembled` 的"覆盖最终 wire 请求"注释与新语义一致。
- **REQ-003（P0-3 手动压缩）**：`compact_now` 使用配置的 retain_ratio（0.20）；仅 `ContextOverflow` 路径 retain_ratio=0；修正 loop.rs:893-894 与代码相反的注释。
- **REQ-004（P0-4 Git Bash）**：INSTALL.md 前置条件补充 Git for Windows；`sg` 启动时（三形态公共入口）一次性预检 bash 可用性，失败输出可行动错误（含 Git for Windows 安装指引），不再等模型首次调用 bash 工具才失败。
- **REQ-005（P1-5 状态机）**：`ConversationState` 的 `reserved: bool` 与 `active: Option<TurnControls>` 合并为单一 turn 生命周期状态（Idle/Reserved/Running，Running 携带控制面句柄）；`queue_settings`/`has_active_turn`/`steer`/`interrupt`/followUp/删除保护全部从同一状态派生。预订窗口内 `queue_settings` 必须排队（不立即生效）。
- **REQ-006（P1-6 截断信号链）**：维持 REQ-026（截断=正常终态、不自动续写）裁决，补全信号链：(1) assistant 消息条目持久化 stop_reason；(2) `--print` 遇截断终态向 stderr 输出警告；(3) `--json` summary 增加可选 `truncated` 字段（加法兼容，评估器现有解析不受影响，需验证）。
- **REQ-007（P1-8 目录移除）**：整体移除 models.dev 动态目录——网络拉取、metadata-cache.json、TTL、刷新接线（含 app-server supervisor 后台刷新）全部删除；模型限额解析收敛为 用户显式配置 → 内置静态表 → 保守默认。内置表保留。`MAX_DISCOVERY_RESPONSE_BYTES` 更名为 config/auth 专用常量。未知模型缺显式限额时的行为（保守默认/fail-closed）保持现状语义并在 README/INSTALL 说明需显式声明限额。
- **REQ-008（P1-9 脱敏收敛）**：仅保留 model/transport/http.rs 一层、仅按已知凭据值精确匹配脱敏；删除 core `contains_sensitive_text` 扫描器（标记表+token 形状识别）及 runtime/cli/app-server 的全部调用点；相关测试同步改写。
- **REQ-009（P1-10 工具事件）**：`ToolExecutionStarted` 逐个在 `execute_prepared` 之前发射；被 preflight 拒绝的调用发射 started+failed 紧凑事件对（模型仍收到 failed ToolResult）；保持"无 started 不得有终态"配对不变量。
- **REQ-010（P2 死代码）**：删除 test-hooks 故障注入机制（TestFaults、inject_terminalization_faults、consume_* 及全部 cfg 分支、feature 定义、app-server dev-dep 标记）；删除 /models 发现子系统（discovery.rs、discover_model_ids、models_endpoint、MAX_DISCOVERED_* 及错误码分支）；删除 `CompactionEntry.previous_summary` 字段及断言它的测试逻辑；删除无生产者的错误分类（如 runtime `QuotaExceeded`）与 app-server `owner_only.rs` 重复包装（改调 core fs_owner）。
- **REQ-011（P2-15 传输收敛）**：抽取协议无关的共享 `SseFrameDecoder`（帧分割/字段解析/字节上限）；引入窄协议适配接口（payload 构造、响应解析、reasoning_present），`complete_attempts` 单骨架依赖该接口，消除流式×2+非流式内联×2 的三份映射。wire 语义不变，协议测试全绿为验收。
- **REQ-012（P2-16 协议块迁移）**：`JSON_RPC_*`/`APP_ERROR_*` 常量、`ErrorCode`、`ClientInfo` 从 core 迁入 protocol；core 不再承载任何协议概念。
- **REQ-013（P2-19 metadata 强类型）**：7 种 session metadata 改为带 payload 的 typed enum；JSONL wire 扁平格式不变（serde 兼容现有文件）；store/session_index 的字符串字段解析改类型访问。
- **REQ-014（P2-20 工具参数）**：六工具各定义 serde 强类型参数结构，反序列化即校验；删除 registry 自制 JSON Schema 子集 validator；给模型的静态 Tool Schema 保留。
- **REQ-015（P2-24 home 校验统一）**：SINGULARITY_HOME 仓库边界校验收敛 core 单一入口；CLI 在创建任何目录之前调用；app-server 删除 state_paths 副本改调 core。
- **REQ-016（P2-14 遥测剥离）**：删除 `ProviderAttemptSummary` 事件、runner 聚合推导、protocol summary 事件与 14 参构造器中的聚合部分；保留轻量 started/finished attempt 事件（TUI 思考中状态依赖）。
- **REQ-017（P2 小修）**：CLI smoke 删除 app-server 二进制依赖（字段/env/存在性校验）；`Conversation::compact` 使用 Drop 守卫防 panic 卡死；assistant 终态守卫改用 `first_delta_observed`；修正注释漂移（loop.rs:144/153-155"投影失败即中止"、TUI Ctrl+C 注释、docs §8 Alt+↑ 矛盾）。
- **REQ-018（P2-18 reasoning 三态）**：settings 的 reasoning 字段改 Keep/Set/Clear 三态；wire 缺字段=Keep、`null`=Clear、字符串=Set；TUI 设置面板清空 reasoning 即 Clear（恢复模型默认）。
- **REQ-019（P2-22 测试 API 收编）**：删除真无消费者的测试专用公开 API（如 `OpenAiProviderConfig::from_env`、`ProviderConfigResolution` 导出）；测试需要的（`with_provider_override`/`with_test_provider`/`with_sessions_dir`）移入 feature 门或单元测试可达范围。
- **REQ-020（P3-27 投影收敛）**：会话元数据投影（header/metadata → thread 摘要）收敛为一份共享 API（runtime 或 agent session 层）；`store::list_threads` 与 `SessionIndex` 共用。
- **REQ-021（P3-28 TUI 簇）**：换行逻辑三处收敛为一份共享纯函数；item 身份识别去 `_assistant` 后缀猜测（事件携带类型信息）；TUI Ctrl+C 注释与实现一致。
- **REQ-022（P3-29 spill 清理）**：创建新 spill 时惰性删除同目录超过 7 天的旧文件；无后台线程、无退出钩子。
- **REQ-023（P3-30 小重复簇）**：TurnRunner 尾部纯委托包装删除；force/manual compact 装配重复提私有 helper；`split_lines` 收敛一份；tool_choice 两处 `json!("auto")` 合一；`classify_model_error` 别名删除；editor `consumed` 死变量删除；`ConversationError::Settings` 杂项变体拆分/更名；`SessionRepository::read` 改只读打开；edit patch hunk 头行号 +1。
- **REQ-024（P3-31 保留）**：压缩文件操作追踪（readFiles/modifiedFiles + details 落盘）保留现状（pi 同款设计，已核实），不删不改。
- **SEC-001**：JSONL 唯一持久事实源、durable 先于事件发布、fail-stop 终态化、单写者、有界读取、owner-only 文件安全合同不得削弱。
- **SEC-002**：凭据值不出现在任何输出/日志/事件；脱敏收敛后由 http.rs 凭据值精确匹配承担（REQ-008）。
- **CON-001**：七 crate 边界保持（P1-7 已裁决 app-server 保留）；不新增 crate/框架/依赖/工具面/协议方法。
- **CON-002**：不重开已裁决事项（环境层整体短路、三形态、spill 本体、文件操作追踪等）；执行中发现计划外问题只记录到执行日志，不实施。
- **CON-003**：每个 TASK 独立自包含提交、可回滚；structure-only 与 behavior 改动分提交。
- **CON-004**：不得通过加重试、扩预算、吞错、跳过测试、弱化断言或 test-only production 分支制造通过。
- **GUD-001**：行为修复先写能捕获原症状的回归，再实施；测试锚定行为合同而非实现细节。
- **GUD-002**：复杂度放入拥有不变量的最小正确层；同一事实只保留一个权威来源。
- **EVAL-001（执行期评估合同）**：外部 Singularity-Evaluator 的改动前后对照仅适用于 TASK-101，其既有记录保留有效且不重跑。TASK-102 已产生的评估记录保留；其余全部 TASK（包括原行为敏感项）不运行外部评估器，以确定性回归测试和定向输出检查作为充分验证。
- **EVAL-002（终局行为回归）**：全部实施 TASK 完成且 TASK-502 门禁全绿后，按 AGENTS.md 评估节运行一次终局完整评估，将 run id、通过率与失败归因写入执行日志；禁止重采样制造通过。

## 2. Implementation Steps

### Phase 0 — 基线

| Task | Description | Done | Date |
|------|-------------|------|------|
| TASK-000 | 读根 AGENTS.md、docs/singularity.md、docs/agents/workflow.md、本计划；`git rev-parse HEAD`/`git status --short` 核对基线 `610767b4`；跑一轮基线门禁（fmt/clippy/test/build）记录结果到执行日志。提交：`47b5550` | [x] | 2026-08-25 |

### Phase 1 — P0 行为正确性

| Task | Description | Done | Date |
|------|-------------|------|------|
| TASK-101 ⚡ | REQ-001 重试收敛：transport 单次 attempt + Retry-After 入错误对象；Agent 层唯一重试策略；删第二套退避/抖动。回归：持续 429 总请求数≤4、Retry-After 尊重、可见输出后禁重试、取消可中断等待。提交：`7595317` | [x] | 2026-08-25 |
| TASK-102 ⚡ | REQ-002 压缩触发 pi 式：真实 usage 优先触发；estimate_assembled 计入 replay；溢出强制路径保留；docs §3 同步。回归：usage 触发、fallback 估算含 replay、首轮无 usage 路径。提交：`e751da5` | [x] | 2026-08-25 |
| TASK-103 | REQ-003 手动压缩恢复正常保留；修注释。回归：manual 保留 0.20、overflow 保留 0。提交：`641b22b` | [x] | 2026-08-25 |
| TASK-104 | REQ-004 Git Bash 前置：INSTALL 补前置；启动预检 + 可行动错误。回归：无 bash 环境启动即失败且文案含指引。提交：`49ad545` | [x] | 2026-08-25 |

### Phase 2 — P1 合同与结构

| Task | Description | Done | Date |
|------|-------------|------|------|
| TASK-201 | REQ-005 状态机重构：Idle/Reserved/Running 单一状态派生全部判断。回归：预订窗口内 queue_settings 排队、busy 查询、steer/interrupt 路由、followUp/删除保护不回退。提交：`352fff3` | [x] | 2026-08-25 |
| TASK-202 | REQ-006 截断信号链：stop_reason 落盘 + --print stderr 警告 + summary 可选 truncated 字段；验证评估器解析兼容。提交：`ec600e7` | [x] | 2026-08-25 |
| TASK-203 | REQ-007 目录移除：删网络拉取/缓存/TTL/刷新接线（含 supervisor 段）；限额三级改两级+保守默认；常量更名；README/INSTALL 限额声明说明。回归：显式限额模型正常、内置表命中、未知模型走保守默认。提交：`aacfa2c` | [x] | 2026-08-25 |
| TASK-204 | REQ-008 脱敏收敛：删 core 扫描器与全部二次应用；http.rs 凭据值精确匹配保留。回归：凭据值脱敏仍生效、普通错误文本不再被吞。提交：`ed236bc` | [x] | 2026-08-25 |
| TASK-205 | REQ-009 工具事件生命周期：逐个 Started + 拒绝 started+failed 对；runner/TUI/app-server 投影适配。回归：串行执行时序、拒绝配对、无 started 无终态。提交：`33a666b` | [x] | 2026-08-25 |

### Phase 3 — P2 结构收敛与死代码

| Task | Description | Done | Date |
|------|-------------|------|------|
| TASK-301 | REQ-010 死代码四簇删除（与 TASK-203 协同处理 discovery/常量）。提交：`a89e902` | [x] | 2026-08-25 |
| TASK-302 | REQ-011 传输收敛：SseFrameDecoder + 窄协议适配接口；wire 不变，protocol/transport 测试全绿。提交：`4d60f17` | [x] | 2026-08-25 |
| TASK-303 | REQ-012 core 协议块迁 protocol。提交：`7512b7a` | [x] | 2026-08-25 |
| TASK-304 | REQ-013 metadata typed enum（wire 扁平不变；旧文件可读）。提交：`78398fe` | [x] | 2026-08-25 |
| TASK-305 | REQ-014 工具参数 serde 强类型；删自制 validator。提交：`07d8725` | [x] | 2026-08-25 |
| TASK-306 | REQ-015 home 校验 core 单一入口；CLI 先校验后建目录；app-server 删副本。提交：`6a3f030` | [x] | 2026-08-25 |
| TASK-307 | REQ-016 遥测剥离：删 Summary+聚合；留 started/finished。提交：`7963268` | [x] | 2026-08-25 |
| TASK-308 | REQ-017 小修四项（smoke 解耦/Drop 守卫/配对守卫/注释漂移）。提交：`766c625` | [x] | 2026-08-25 |
| TASK-309 | REQ-018 reasoning 三态（wire null=Clear；TUI 面板适配）。提交：`57d748c` | [x] | 2026-08-25 |
| TASK-310 | REQ-019 测试 API 收编。提交：`7a22517` | [x] | 2026-08-25 |

### Phase 4 — P3 一致性

| Task | Description | Done | Date |
|------|-------------|------|------|
| TASK-401 | REQ-020 会话投影共享 API。 | [x] | 2026-08-25 |
| TASK-402 | REQ-021 TUI 四项。 | [x] | 2026-08-25 |
| TASK-403 | REQ-022 spill 惰性清理（7 天）。 | [x] | 2026-08-25 |
| TASK-404 | REQ-023 小重复簇十项。 | [x] | 2026-08-25 |

### Phase 5 — 文档同步与最终门禁

| Task | Description | Done | Date |
|------|-------------|------|------|
| TASK-501 | docs/singularity.md 全量同步（§2 事件流/§3 压缩触发/§5 metadata/§6 目录移除/§8 TUI）；README（限额显式声明、Git Bash 前置）；INSTALL。只写当前事实。 | [x] | 2026-08-25 |
| TASK-502 | 最终门禁：fmt --check / clippy -D warnings / test --workspace / build --bins / git diff --check 全绿；汇总执行日志（提交清单、评估对照、偏差、遗留风险）。 | [x] | 2026-08-25 |
| TASK-503 | EVAL-002 终局完整评估，结果记入执行日志。 | [ ] | |

## 3. Alternatives（已否决方向，勿重开）

- 重试收敛到传输层（Codex 模型）——用户选 Agent 层（pi 模型）。
- 压缩触发仅修估算——用户选 pi 式机制变更。
- 设置竞态最小修复——用户选状态机重构。
- app-server 移出 workspace——用户选保留现状。
- models.dev 修接线保功能——用户选整体移除。
- 敏感文本扫描器收缩保留——用户选彻底删除。
- 会话投影删缓存按需扫描——用户选共享投影 API。

## 4. Dependencies

- Rust 1.96.0 工具链、Cargo.lock 锁定构建。
- TASK-203 与 TASK-301 协同（discovery 与目录移除共享删除面）；TASK-204 与 TASK-303 同文件（core/lib.rs）注意顺序。
- TASK-102/201 触及 Agent/Conversation 核心，建议在 TASK-101 之后进行（重试语义已稳定）。
- 外部评估器仅用于已完成的 TASK-101 对照、保留的 TASK-102 记录与 TASK-503 终局完整评估；见 EVAL-001/EVAL-002。

## 5. Files

主要触及：`crates/model/src/{transport/*,openai/*,config/*,lib.rs}`、`crates/agent/src/{loop.rs,compaction.rs,session/*,tools/*}`、`crates/runtime/src/{runner.rs,conversation.rs,store.rs,events.rs,error.rs}`、`crates/core/src/lib.rs`、`crates/protocol/src/lib.rs`、`crates/app-server/src/*`、`crates/cli/src/*`、`docs/singularity.md`、`README.md`、`docs/INSTALL.md`。

## 6. Testing

- 每 TASK：能捕获目标缺陷的回归先行（行为类）或前后等价验证（结构类）；受影响 crate 定向测试。
- TASK-104/201/202/205/309 等其余行为变化：以目标缺陷回归和定向输出检查验证，不运行外部评估器。
- 最终：`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings`、`cargo test --workspace --all-targets --locked --no-fail-fast`、`cargo build --workspace --bins --locked`、`git diff --check`。
- TASK-502 全绿后执行 TASK-503：运行一次终局完整评估，记录 run id、通过率与失败归因，禁止重采样。
- `--json` summary 形状变更（truncated 字段）必须验证评估器解析兼容。

## 7. Risks

- 状态机重构（TASK-201）在正确性敏感文件上 diff 较大——以并发回归矩阵护航，独立提交。
- 传输收敛（TASK-302）可能引入 wire 漂移——protocol/transport 测试锚定 wire 语义，禁止顺手改行为。
- 目录移除（TASK-203）后未知模型需显式限额——文档必须清晰告知，否则用户配置会意外失败。
- pi 式压缩触发首轮无新鲜 usage——fallback 路径必须有测试覆盖。

## 8. Related Specifications

- 讨论记录：`C:\Users\Lenovo\Desktop\discussion\2026-08-25-15-05-discussion.md`
- [Singularity 架构事实](../docs/singularity.md)
- pi compaction/session 设计（本机 pi 0.84.2 dist 与 GitHub 源码已核实）
- codex-rs client retry（github.com/openai/codex）
- Claude Code Windows setup 文档（Git for Windows = Bash 工具启用条件）

## 执行日志

- TASK-000 — commit `47b5550`; baseline `610767b4fba329f104dec7a7483ce20a7eee118c`; `cargo fmt --all -- --check` exit 0（无输出）；`cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings` exit 0（`Finished dev profile`）；`cargo test --workspace --all-targets --locked --no-fail-fast` exit 0（全部测试通过，`real_provider_restart_and_resume_smoke` 1 项按既有声明 ignored）；`cargo build --workspace --bins --locked` exit 0（`Finished dev profile`）；偏差：无。
- TASK-101 — commit `7595317`; TDD red：`cargo test -p singularity_model --test transport openai_provider_returns_transient_http_error_after_one_attempt --locked -- --exact` exit 1，`ProviderError` 缺少 `retry_after`/`automatic_retry_allowed`；green：`cargo test -p singularity_agent --locked` 63 passed，`cargo test -p singularity_model --locked --no-fail-fast` 41+30+23+22 passed，`cargo test --workspace --all-targets --locked --no-fail-fast` exit 0（全部通过，既有 provider smoke 1 ignored），`cargo clippy -p singularity_model -p singularity_agent --all-targets --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。评估前：smoke `run-1787648521-585` 1/1；全量 `run-1787648628-708` 已完成 10/12 均通过，进程观察中断后仅补跑未完成单元 `run-1787649023-629` 0/2，合计 10/12（83.3%），2 timed_out。评估后：smoke `run-1787651356-646` 1/1；全量 `run-1787651518-250` 11/12（91.7%），1 timed_out。超时 rollout 均停在 `find / -type d -name "shop" 2>/dev/null | head -20` 的无界磁盘搜索；记录显示 provider/model 用时不是阻断点，未重采样。偏差：评估前全量进程在 10/12 后被观察中断，仅以单次 remainder run 补齐原 run 未执行的两个单元。
- TASK-102 — commit `e751da5`; TDD red：`cargo test -p singularity_agent assembled_fallback_estimate_includes_provider_reasoning_replay --locked -- --exact` exit 1，`estimate_assembled` 尚无 reasoning replay 参数；green：`cargo test -p singularity_agent --locked` 67 passed，`cargo clippy -p singularity_agent --all-targets --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。评估前复用改动前 revision 的 `run-1787651518-250`：11/12（91.7%），1 timed_out；评估后 `run-1787652858-509`：11/12（91.7%），1 timed_out。后置超时为 `warehouse-audit × longcat`：rollout 已显示 17/17 tests 与 CLI 均成功，随后因任务描述声称存在失败而反复复核至 600 秒；`model_ms=2500`、`tool_ms=235093`、15 次工具调用，非 provider/transport 阻断，未重采样。偏差：前置 run 同时作为 TASK-101 后置证据，因其 revision 精确等于 TASK-102 修改前状态。
- TASK-103 — commit `641b22b`; TDD red：`cargo test -p singularity_agent manual_compaction_keeps_the_configured_recent_twenty_percent --locked` 1 failed，手动路径切点未保留配置的近期 20%；green：`cargo test -p singularity_agent --locked` 68 passed，`cargo clippy -p singularity_agent --all-targets --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0；既有 overflow 回归新增 first-kept 断言并通过，确认 ContextOverflow 仍为 retain ratio 0。偏差：无。
- 评估合同修订 — EVAL-001/EVAL-002 按用户 2026-08-25 补充指令更新：TASK-101 对照与 TASK-102 既有记录保留；TASK-104 起不再逐 TASK 评估；TASK-502 后由 TASK-503 运行一次终局完整评估。修订到达时 TASK-104 前置 run `run-1787653754-734` 已完成 8 个 cell（均通过）并在第 9 个完成输出后终止；该不完整 run 不作为验收证据，也不补跑。
- TASK-104 — commit `49ad545`; TDD red：`cargo test -p singularity_cli --test entry_contract startup_without_git_bash_fails_with_installation_guidance --locked` 1 failed，实际先进入 provider 配置且无安装指引；green：`cargo test -p singularity_cli --locked` 20 unit + 1 smoke passed/1 ignored + 10 entry contract + 4 TUI PTY passed，`cargo test -p singularity_agent --locked` 68 passed，`cargo test -p singularity_runtime --locked` 12 passed，`cargo clippy -p singularity_cli -p singularity_runtime -p singularity_agent --all-targets --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。定向输出检查覆盖 TUI、`--print`、`--json`：三者均在初始化前失败并输出 Git for Windows、`bash.exe` 与官方安装 URL；JSON 保留 `failed` summary。偏差：无；按修订 EVAL-001 未运行 TASK 评估。
- TASK-201 — commit `352fff3`; TDD red：`cargo test -p singularity_runtime --test conversation_tests reservation_holds_window_and_releases_on_drop --locked` 1 failed，预订窗口内 `queue_settings` 返回 `AppliedNow` 而非 `QueuedForNextTurn`；green：`cargo test -p singularity_runtime --locked --no-fail-fast` 12 passed，`cargo test -p singularity_app_server --locked --no-fail-fast` 15 lib + 19 bin + 4 integration + 3 transport passed，`cargo clippy -p singularity_runtime -p singularity_app_server --all-targets --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。回归覆盖 Reserved 期间 busy、设置排队且不提前持久化、followUp 接受、steer/interrupt 无运行控制面、释放后 FIFO 消费，以及 app-server 删除保护；状态锁中毒时 busy 查询 fail-closed。规格与标准双轴审查最终无 P0/P1/P2。偏差：无；按 EVAL-001 未运行 TASK 评估。
- TASK-202 — commit `ec600e7`; TDD red：`cargo test -p singularity_cli --test entry_contract length_truncation_is_persisted_and_projected_by_headless_modes --locked -- --exact` 1 failed，截断完成的 stderr 为空；green：`cargo test -p singularity_agent --locked --no-fail-fast` 68 passed，`cargo test -p singularity_runtime --locked --no-fail-fast` 12 passed，`cargo test -p singularity_cli --locked --no-fail-fast` 20 unit + 1 smoke passed/1 ignored + 11 entry contract + 4 TUI PTY passed，`cargo test -p singularity_app_server --locked --no-fail-fast` 15 lib + 19 bin + 4 integration + 3 transport passed，`cargo clippy -p singularity_agent -p singularity_runtime -p singularity_cli -p singularity_app_server --all-targets --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。黑盒回归确认纯文本 length 保持 Completed/exit 0、不自动续写，assistant JSONL 持久化 `stopReason: "length"`，`--print` stderr 警告，`--json` 仅截断终态输出 `summary.turn.truncated: true`；普通 summary 省略该字段。评估器 `parse_sg_json` 只读取 status/usage/threadId，定向 `cargo test parse_sg_json_detects_interrupted_and_failed --locked` 1 passed，附加字段兼容。规格与标准双轴审查无 P0/P1/P2。偏差：无；按 EVAL-001 未运行 TASK 评估。
- TASK-203 — commit `aacfa2c`; TDD red：`cargo test -p singularity_model resolution_uses_builtin_then_conservative_without_dynamic_catalog_data --locked` 1 failed，缓存模型返回 `Cache` 而非 `Conservative`；green：`cargo test -p singularity_model --locked --no-fail-fast` 37 unit + 30 config + 23 protocol + 22 transport passed，`cargo test -p singularity_app_server --locked --no-fail-fast` 15 lib + 19 bin + 4 integration + 3 transport passed，`cargo clippy -p singularity_model -p singularity_app_server --all-targets --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。生产代码中 models.dev URL/拉取、`metadata-cache.json` schema/读写、TTL、刷新 API 与 supervisor 后台接线无残留；`/models` provider discovery 保留到 TASK-301，并使用私有响应上限常量；限额回归覆盖显式值、内置表/大小写命中、未知模型保守默认，README/INSTALL 已说明未知模型显式限额。规格与标准双轴审查无 P0/P1/P2。偏差：无；按 EVAL-001 未运行 TASK 评估。
- TASK-204 — commit `ed236bc`; TDD red：`cargo test -p singularity_app_server transport_error_exposes_store_agent_and_workspace_text --locked` 1 failed，普通 `provider: unavailable` 被二次扫描替换为 `Internal error`；green：`cargo test -p singularity_core --locked` 2 unit + 7 project-instructions passed，`cargo test -p singularity_model --locked` 37 unit + 30 config + 23 protocol + 22 transport passed，`cargo test -p singularity_runtime --locked` 12 passed，`cargo test -p singularity_cli --locked` 20 unit + 1 smoke passed/1 ignored + 11 entry contract + 4 TUI PTY passed，`cargo test -p singularity_app_server --locked` 15 lib + 19 bin + 4 integration + 3 transport passed，`cargo clippy -p singularity_core -p singularity_model -p singularity_runtime -p singularity_cli -p singularity_app_server --all-targets --all-features --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。定向 HTTP 回归确认配置凭据精确命中时固定替换，含 `provider:` 普通诊断保持原文；`rg` 确认 core 扫描器和 runtime/CLI/app-server 二次调用无残留。偏差：`docs/singularity.md` 的旧脱敏描述按计划留待 TASK-501 全量同步；按 EVAL-001 未运行 TASK 评估。
- TASK-205 — commit `33a666b`; TDD red：`cargo test -p singularity_agent tool_events_pair_around_each_serial_execution_and_preflight_rejection --locked` 1 failed，实际序列为三个 started 全部先发、再依次 ended；green：目标回归 1 passed，`cargo test -p singularity_agent --locked --no-fail-fast` 69 passed，`cargo test -p singularity_runtime --locked --no-fail-fast` 12 passed，`cargo test -p singularity_cli --locked --no-fail-fast` 20 unit + 1 smoke passed/1 ignored + 11 entry contract + 4 TUI PTY passed，`cargo test -p singularity_app_server --locked --no-fail-fast` 15 lib + 19 bin + 4 integration + 3 transport passed，`cargo clippy -p singularity_agent -p singularity_runtime -p singularity_cli -p singularity_app_server --all-targets --all-features --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。回归覆盖串行 runnable 调用 start/end 紧邻配对、preflight 拒绝 start/failed 紧邻配对、拒绝结果仍进入下一轮模型上下文；runtime/TUI/app-server 沿既有同一事件投影链消费，无协议形状变化。偏差：无；按 EVAL-001 未运行 TASK 评估。
- Phase 2 审查 — revision `33a666b`; 规格与标准双轴汇总审查均无 P0/P1/P2 finding；确认 REQ-005..009 的状态机、截断信号、目录移除、脱敏边界、工具事件配对及客户端投影闭合。文档事实同步仍按计划归 TASK-501。
- TASK-301 — commit `a89e902`; structure-only 等价验证：`cargo test -p singularity_model --locked --no-fail-fast` 34 unit + 29 config + 23 protocol + 22 transport passed，`cargo test -p singularity_agent --locked --no-fail-fast` 69 passed，`cargo test -p singularity_runtime --locked --no-fail-fast` 12 passed，`cargo test -p singularity_app_server --locked --no-fail-fast` 15 lib + 19 bin + 4 integration + 3 transport passed；`cargo clippy -p singularity_model -p singularity_agent -p singularity_runtime -p singularity_app_server --all-targets --all-features --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。删除 test-hooks feature/故障注入分支与 dev-dep、完整 `/models` discovery 文件/API/endpoint/测试/专用上限、`CompactionEntry.previous_summary` 新写入与断言、无生产者 quota 分类，以及 app-server owner-only 重复模块；app-server 直接调用 core fs_owner。旧 JSONL 的 `previousSummary` 键仍作为严格读取允许字段被忽略，保持既有持久文件可读；`MAX_MODEL_ID_LENGTH` 因用户配置 schema 仍有生产消费者而保留。偏差：无；按 EVAL-001 未运行 TASK 评估。
- TASK-302 — commit `4d60f17`; structure-only 等价验证：`cargo test -p singularity_model --locked --no-fail-fast` 34 unit + 29 config + 23 protocol + 22 transport passed，`cargo test -p singularity_agent --locked --no-fail-fast` 69 passed，`cargo clippy -p singularity_model -p singularity_agent --all-targets --all-features --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。共享 `SseFrameDecoder` 统一任意 HTTP chunk 下的 SSE 行/字段/帧分割与总字节上限；静态 `ProtocolAdapter` 收敛 Chat/Responses 的 endpoint、stream/non-stream payload、response parse 与 reasoning_present；所有路径统一进入 `complete_protocol` 与单次 `complete_attempt` 骨架。现有 protocol/transport 套件确认 Chat/Responses 流式与非流式 payload、增量、tool fragment、usage、错误分类、重试安全、取消和大小上限均保持。偏差：无；按 EVAL-001 未运行 TASK 评估。
- TASK-303 — commit `7512b7a`; structure-only 等价验证：`cargo test -p singularity_core --locked` 1 cancellation + 7 project-instructions passed，`cargo test -p singularity_protocol --locked` 11 passed，`cargo test -p singularity_app_server --locked --no-fail-fast` 15 lib + 19 bin + 4 integration + 3 transport passed，`cargo clippy -p singularity_core -p singularity_protocol -p singularity_app_server --all-targets --all-features --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。`JSON_RPC_*`、`APP_ERROR_*`、`ErrorCode` 与 `ClientInfo` 归属 protocol，app-server 只从 protocol 导入；core 不再依赖 serde/serde_json，也不再承载 JSON-RPC/AppServer 类型。首次 `--locked` 因依赖图尚未更新锁文件退出 1；随后以 `cargo test -p singularity_core --offline` 重算 lock 并通过，再以 `--locked` 完整复验。偏差：无；按 EVAL-001 未运行 TASK 评估。
- TASK-304 — commit `78398fe`; structure-only 等价验证：`cargo test -p singularity_agent --locked --no-fail-fast` 70 passed，`cargo test -p singularity_runtime --locked --no-fail-fast` 12 passed，`cargo test -p singularity_app_server --locked --no-fail-fast` 15 lib + 19 bin + 4 integration + 3 transport passed，`cargo clippy -p singularity_agent -p singularity_runtime -p singularity_app_server --all-targets --all-features --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。七类 `SessionMetadata` 改为携带固定 payload 的 serde tagged enum；逐类回归确认 `metadataType` 与 payload 仍为原扁平 JSONL wire，无 provider 的既有裸 model settings 仍可读。runtime store、app-server history/session index 与 runner usage 投影均改为 typed variant 访问，不再按字段名解析字符串。一次带 `--exact` 的定向过滤因遗漏模块前缀执行 0 项，随后以非 exact 定向过滤执行目标回归 1 passed，并由完整 agent 套件复验。偏差：无；按 EVAL-001 未运行 TASK 评估。
- TASK-305 — commit `07d8725`; TDD red：`cargo test -p singularity_agent typed_preflight_rejects_zero_bash_timeout --locked` 1 failed，旧 schema 子集 validator 只检查 integer 类型，`timeout_ms: 0` 错误通过 preflight；green：目标回归 1 passed，`cargo test -p singularity_agent --locked --no-fail-fast` 71 passed，`cargo clippy -p singularity_agent --all-targets --all-features --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。read/glob/grep/bash/edit/write 各自定义 serde typed args 并拒绝未知字段；静态 Tool Schema 保留给模型，registry 自制 properties/required/type validator 删除，preflight 与 execute 共用 typed deserialization；bash 的 `timeout_ms` 自定义 serde 校验保持缺失=无限、非正整数/错误类型/null=拒绝。green 前完整 agent 首轮有 3 个旧错误文案断言失败，确认均为 serde 诊断文本变化后更新为同等强度断言并完整复验。偏差：工具参数错误诊断从手写 schema 文案统一为 serde typed 文案，拒绝语义不弱化；按 EVAL-001 未运行 TASK 评估。
- TASK-306 — commit `6a3f030`; TDD red：`cargo test -p singularity_cli --test entry_contract home_inside_repository_is_rejected_before_directory_creation --locked -- --exact` 1 failed，CLI 虽最终拒绝仓库内 home，但目录已先创建；green：目标 CLI 回归 1 passed，core 边界定向回归 3 passed，`cargo test -p singularity_model --test config --locked` 29 passed，`cargo test -p singularity_cli --locked --no-fail-fast` 20 unit + 1 smoke passed/1 ignored + 12 entry contract + 4 TUI PTY passed，`cargo test -p singularity_app_server --locked --no-fail-fast` 15 lib + 19 bin + 4 integration + 3 transport passed，`cargo clippy -p singularity_core -p singularity_model -p singularity_cli -p singularity_app_server --all-targets --all-features --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。core 单一入口统一最近 `.git` 边界、缺失尾部 canonicalization 与 Windows 大小写比较；model、CLI、app-server 共用，CLI 在 runtime/state 目录准备前拒绝，app-server `state_paths.rs` 删除。一次并行测试输出在 model config 与 CLI 终态前截断，未计通过，随后分别重跑取得 exit 0。偏差：无；按 EVAL-001 未运行 TASK 评估。
- TASK-307 — commit `7963268`; structure-only 等价验证：`cargo test -p singularity_model --locked --no-fail-fast` 32 unit + 29 config + 23 protocol + 22 transport passed，`cargo test -p singularity_agent --locked --no-fail-fast` 71 passed，`cargo test -p singularity_runtime --locked --no-fail-fast` 12 passed，`cargo test -p singularity_protocol --locked` 11 passed，`cargo test -p singularity_app_server --locked --no-fail-fast` 15 lib + 19 bin + 4 integration + 3 transport passed；`cargo clippy -p singularity_model -p singularity_agent -p singularity_runtime -p singularity_protocol -p singularity_cli -p singularity_app_server --all-targets --all-features --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。删除 `ProviderAttemptMetadata` 在 response/error/outcome 的跨层携带、Agent 聚合、runner 分组推导、`ProviderAttemptSummary` runtime/protocol/app-server/TUI 全链路；`openai_provider_observes_one_ordered_start_end_pair` 继续通过，实时 `provider/attempt` started/finished 事件与 TUI 思考中消费保留。model 首轮测试虽 exit 0 但报告 unused warnings，随后删除仅为 aggregate 搭建的参数/导入并以 clippy `-D warnings` 复验。一次并行 app-server 测试在初始工具等待时未取得终态，未计通过，随后单独重跑 exit 0。偏差：无；按 EVAL-001 未运行 TASK 评估。
- TASK-308 — commit `766c625`; TDD red：`compact_releases_its_busy_window_when_the_provider_panics` 捕获 provider panic 后发现 Conversation 仍保持 busy，`tool_appearance_does_not_create_an_assistant_terminal_item` 发现仅工具出现也生成 assistant 终态；green：`cargo test -p singularity_agent --locked --no-fail-fast` 71 passed，`cargo test -p singularity_runtime --locked --no-fail-fast` 1 unit + 13 integration passed，`cargo test -p singularity_cli --locked --no-fail-fast` 20 unit + 1 smoke passed/1 ignored + 12 entry contract + 4 TUI PTY passed，`cargo clippy -p singularity_agent -p singularity_runtime -p singularity_cli --all-targets --all-features --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。`Conversation::compact` 复用 `TurnReservation` Drop 守卫，unwind 时释放单写者窗口；assistant completed/failed 均由 `first_delta_observed` 内部守卫；CLI smoke 不再定位、校验或注入 app-server binary；定向 `rg` 确认 smoke 无 app-server 残留，loop/TUI/docs 三处注释事实一致。首次串联门禁中的 runtime 新单测暴露失败终态守卫仍在调用方，统一内收后重新完整运行 runtime、CLI 与 clippy/fmt/diff 获得 exit 0。偏差：无；按 EVAL-001 未运行 TASK 评估。
- TASK-309 — commit `57d748c`; TDD red：protocol `thread_settings_reasoning_wire_distinguishes_missing_string_and_null` 显示 `reasoning:null` 与缺字段均反序列化为 None 且重编码时消失，TUI `clearing_reasoning_in_settings_removes_the_selector_effort` 显示清空输入仍保留 `#high`；green：`cargo test -p singularity_protocol --locked --no-fail-fast` 12 passed，`cargo test -p singularity_runtime --locked --no-fail-fast` 1 unit + 14 integration passed，`cargo test -p singularity_app_server --locked --no-fail-fast` 16 lib + 19 bin + 4 integration + 3 transport passed，`cargo test -p singularity_cli --locked --no-fail-fast` 21 unit + 1 smoke passed/1 ignored + 12 entry contract + 4 TUI PTY passed，`cargo clippy -p singularity_protocol -p singularity_runtime -p singularity_app_server -p singularity_cli --all-targets --all-features --locked --no-deps -- -D warnings` exit 0，格式化后 `cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。protocol 与 runtime 各自使用窄 `ReasoningPatch::{Keep,Set,Clear}`，wire 缺字段/字符串/null 与 selector 保持/设置/清除逐项回归；app-server 映射三态并确认 null 计为更新、缺字段不更新；TUI 空 reasoning 生成 Clear。初始 app-server 回归使用未声明的 `#high` 被既有 selector 校验拒绝，改为把 wire 区分、runtime selector 三态与 TUI 实际清除拆成确定性回归，未绕过生产校验。偏差：无；按 EVAL-001 未运行 TASK 评估。
- TASK-310 — commit `7a22517`; structure-only 等价验证：`cargo test -p singularity_model --locked --no-fail-fast` 38 unit + 24 config + 23 protocol + 22 transport passed，`cargo test -p singularity_runtime --locked --no-fail-fast` 15 unit passed，`cargo test -p singularity_cli --locked --no-fail-fast` 21 unit + 1 smoke passed/1 ignored + 12 entry contract + 4 TUI PTY passed，`cargo test -p singularity_app_server --locked --no-fail-fast` 35 lib + 4 integration + 3 transport passed，`cargo build --workspace --bins --locked` exit 0，`cargo check --workspace --all-targets --locked` exit 0，`cargo clippy -p singularity_model -p singularity_runtime -p singularity_cli -p singularity_app_server --all-targets --all-features --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。删除 `OpenAiProviderConfig::from_env` 与 `ProviderConfigResolution` 公共导出；原 from_env 覆盖等量迁入 model 私有配置单测。runtime provider override 及字段仅在 cfg(test)/`test-support` feature 存在，CLI/app-server 仅由 dev-dependency 启用；runtime conversation 集成回归收编为 crate unit module。app-server stdio transport 归入 lib 私有模块，测试 provider 仅 cfg(test)，会话目录成为构造必填而非公开测试 builder，store accessor 收窄 crate 可见。首次把 `with_sessions_dir` 直接收窄时二进制 target 作为独立 crate 无法访问；改为构造时注入并把 transport 归入 lib 后完整复验。偏差：Phase 3 统一审查按用户最新“无需审查”指令取消；按 EVAL-001 未运行 TASK 评估。

- TASK-401 — commit 15ea2c3; structure-only 等价验证：cargo test -p singularity_runtime --locked --no-fail-fast 15 passed，cargo test -p singularity_app_server --locked --no-fail-fast 35 lib + 4 integration + 3 transport passed，cargo clippy -p singularity_agent -p singularity_runtime -p singularity_app_server --all-targets --all-features --locked --no-deps -- -D warnings exit 0，cargo fmt --all -- --check exit 0，git diff --check exit 0。新增 agent session 层只读 project_session 共享投影，runtime 与 app-server 仅映射本地状态类型；JSONL header/metadata、标题、model、status、usage、时间戳、turn/token 统计语义保持，未改变修复/写入路径。agent 全量测试首轮出现既有 Windows descendant cancellation 偶发失败（descendant loop must start producing ticks），未由本次改动触发；runtime/app-server、clippy、fmt、diff 复验通过。偏差：无；按 EVAL-001 未运行 TASK 评估。
- TASK-402 — commit `61d1cf5`; structure-only 等价验证：`cargo test -p singularity_cli --locked --no-fail-fast` 21 unit + 1 smoke passed/1 ignored + 12 entry contract + 4 TUI PTY passed，`cargo fmt --all -- --check` exit 0。TUI 共享 `wrapped_lines` 纯函数覆盖 transcript/app/editor 高度；工具 item 身份由事件携带的 tool call 集合判定，不再依赖 assistant 后缀猜测；Ctrl+C 文案与实现保持一致。偏差：无；按 EVAL-001 未运行 TASK 评估。
- TASK-403 — commit `ce64507`; TDD/定向验证：`cargo test -p singularity_agent tools::bash::tests::small_output_never_spills --locked` 1 passed，`cargo fmt --all -- --check` exit 0。新 spill 创建时扫描专用根目录并惰性删除超过 7 天的旧项，无后台线程或退出钩子。偏差：无；按 EVAL-001 未运行 TASK 评估。
- TASK-404 — commit `75a6d26`; structure-only 等价验证：`cargo test -p singularity_agent -p singularity_runtime -p singularity_model --locked --no-fail-fast` 全部通过（71/15/38+24+23+22），`cargo test -p singularity_app_server --locked --no-fail-fast` 35 lib + 4 integration + 3 transport passed，`cargo clippy -p singularity_agent -p singularity_runtime -p singularity_model -p singularity_app_server --all-targets --all-features --locked --no-deps -- -D warnings` exit 0，`cargo fmt --all -- --check` exit 0，`git diff --check` exit 0。删除 TurnRunner 尾部委托、合并 split_lines 与 tool_choice payload、移除 classify_model_error 别名、清理 editor consumed、SessionRepository 改只读、修正 edit hunk 起始行号，并将 ConversationError 杂项拆为 Configuration/State。偏差：无；按 EVAL-001 未运行 TASK 评估。
- TASK-501 — commit `cac8e9c`; 文档终态检查：`rg` 确认 docs/singularity.md、README.md、docs/INSTALL.md 无 models.dev/discovery/provider summary/旧脱敏描述残留，`git diff --check` exit 0。同步压缩、事件、metadata、spill 清理、Git Bash 前置与未知模型限额当前事实。偏差：无。
- TASK-502 — commit `PENDING`; 五项终局门禁全部 exit 0：`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings`、`cargo test --workspace --all-targets --locked --no-fail-fast`（provider smoke 1 ignored）、`cargo build --workspace --bins --locked`、`git diff --check`。偏差：无。
