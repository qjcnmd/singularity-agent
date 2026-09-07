# 正式 Rust 接线合同

本文件定义完整目标，不把 Python 原型描述成正式功能。`engine.py` 验证控制器的语义；它没有真实模型、跨进程锁、provider wire、异步工具或 Rust 接线。不要增加一个永久 Python sidecar，也不要把实验 Journal 作为生产事实源。

## 1. 当前代码依据

基线 `b031ff411b334ccae72a03e9688a551ba680a0aa`：

| 现有 owner | 本次用途 |
|---|---|
| `crates/agent/src/session/format.rs` | 严格 v5、CompactionEntry、LedgerRecord；新增版本化上下文事件的归属 |
| `session/manager.rs` / `session/repair.rs` | 复用单写者、原子追加和 interrupted 修复，不自建 SessionStore |
| `session/context.rs` | 当前是摘要节点+tail+后续新增；这里改为 strategy-aware 的 ContextPlan |
| `crates/agent/src/request.rs` | `prepare_request/build_request/assemble_messages`；统一预算、投影和 replay |
| `crates/agent/src/loop.rs` | 观察结果追加、手动 compact、overflow 恢复与正常工具批次 |
| `crates/agent/src/compaction.rs` | 原样保留 Summary 的生产实现；不能由原型模拟替换 |
| `crates/runtime/src/store.rs` | `ThreadCatalog::create_thread` 和历史只读投影 |
| `crates/runtime/src/runner.rs` | 从同一会话得到策略、工具快照和 Agent；手动路径也一致 |
| `crates/protocol/src/workbench.rs` | `ThreadSummary/ThreadReadPage` 增加策略及 typed 状态预览 |
| `crates/cli/src/web/workbench.rs` / `crates/cli/web/src/store.ts` | 创建前选择；当前复用空白会话的逻辑必须比较策略 |
| `crates/agent/src/tools/bash/capture.rs` | 当前临时 spill 的七天清理；改为会话拥有 payload 的引用式保存 |

上述文件在本轮或此前同一研究中读取过；正式落地前以当前 checkout 重新核对，尤其不要覆盖用户尚未 push 的修改。AGENTS 引用的 `.specify/memory/constitution.md` 本次远端读取返回 404，这是规格材料缺口，不应伪称已取得其内容。

## 2. 完整目标数据模型（拟议，不是现有定义）

```rust
// 由 Session header 唯一持久化；不是每 turn 可修改的 AgentConfig 开关。
#[serde(tag = "strategy", rename_all = "snake_case")]
enum ContextPolicy {
    Summary,
    StateWorkspace { version: u32, budgets: ContextBudgets },
    EvidenceWorkspace { version: u32, budgets: ContextBudgets },
}

struct SourceRef {
    entry_id: String,
    part: ContentPart,
    byte_start: u64,
    byte_end: u64,
    digest: ContentDigest,
}

// 执行事实与模型判断分属不同写入口。
struct WorkClaim {
    id: ClaimId,
    text: String,
    kind: ClaimKind, // hypothesis/decision/next_action；不是 tool_success
    sources: Vec<SourceRef>,
    scope: WorkScope,
}

struct WorkspaceUpdate {
    // 实际 request manifest 在 host 绑定，不能由模型随意改写其授权范围。
    expected_revision: u64,
    based_on: RequestManifestId,
    consumed_units: Vec<ProtocolUnitId>,
    claim_changes: Vec<ClaimChange>,
    retain: Vec<SourceRef>,
    release: Vec<SourceRef>,
    boundary_requested: bool,
}

struct ContextCheckpoint {
    source_cut: EntryId,
    state_revision: u64,
    epoch: u64,
    visible_parts: Vec<ContextPart>,
    renderer_version: u32,
    frozen_render: Option<PayloadRef>,
}

struct ContextPlan {
    policy: ContextPolicy,
    checkpoint: Option<EntryId>,
    ordered_parts: Vec<ContextPart>,
    pending_units: Vec<ProtocolUnitId>,
    // 不把 opaque provider state 拼成摘要或开放给 history_search。
    continuation_units: Vec<ProviderContinuationRef>,
    manifest: RequestManifest,
}

// 保留现有各 variant；按仓库既有 envelope 布局新增，不复制历史。
enum ContextRecord {
    WorkspaceUpdated(WorkspaceUpdate),
    CheckpointCommitted(ContextCheckpoint),
    // 实际出站请求的上下文身份可并入已有 StepAttempt，不必另建 attempt ledger。
}
```

原始消息、工具结果和关联 payload 是 durable truth。已经提交的模型判断只能证明“模型提出了这一判断”。工作状态、checkpoint 是 durable derived projection；FTS、热度和token估算是可重建缓存；Web选项草稿/展开状态是 UI-only。

不要求所有源字节都塞进 JSONL：大 payload 可以是日志引用的会话资产；它不是第二套可编辑历史。先保存完整资产再提交引用。保留、归档和导出以 Session 的引用关系为依据，而不是 TTL 清理仍被使用的资产。

## 3. Session 与用户操作

新 v6 header 保存唯一 `ContextPolicy`。v5 decoder 明确映射 Summary，不原地伪装成新策略；新未知版本或策略拒绝，不静默降级。恢复必须读取 header，而非读取当前全局默认值。必要的历史格式迁移保持旧文件可审计。

UI 在创建前以 draft 保存策略，确认后传入 create_thread/create_session；已有空白 Session 也已具有固定策略，不能拨按钮改 header。空会话复用要求策略完全一致。研究界面可三选（Summary+两候选），默认产品二选可在评测后确定；都只发生在创建前。

CLI 增加创建参数，恢复时不允许覆盖。正式名称由接线时确定，但测试必须检查 `--json`、Web、手动 compact、恢复四条路径使用同一策略。

ThreadSummary 添加只读策略；ThreadReadPage 保留 compaction_summary 给 Summary，另加 typed context_state。人类 history 仍从原始 Session 投影，不能从被缩减的 prompt 生成。checkpoint 显示 epoch、来源范围、处理状态与真实使用量，不标注虚假的“无损压缩率”。

## 4. 两策略如何共存

一个 `ContextController` enum 分派到现有 Summary 引擎或同一 WorkspaceEngine 的两种 policy。没有动态插件/strategy object 的现实需求。

WorkspaceEngine 共用 source store、facts reducer、working set、budget/epoch controller；语义 state reducer 只在 StateWorkspace 激活。EvidenceWorkspace 的模型判断入口为空；对象槽位依据 adapter 能可靠提取的真实 metadata，而不是猜测命令含义。

建议文件归属：`crates/agent/src/context/{mod,policy,state,working_set,projection,history}.rs`。不是必须一个职责一个文件；按当前模块规模调整，但 owner 不分裂。保留现有 `session/context.rs` 对外调用接缝，消除重复的请求拼装路径。

## 5. 实际模型输出与工具合同

不能在 ModelTurnResponse 虚构 provider 未提供的字段。首选利用模型已经支持的 native tool call，增加一个 `workspace_update` 管理工具，参数是变更意图而不是全状态快照。允许与独立任务工具同批返回；一次响应至多一个更新，host 统一校验来源和 revision。

StateWorkspace 使用 claim_changes；EvidenceWorkspace 只使用消费/保留/释放/边界。host facts 不需要模型调用。未产生语义变化时无需填写假 patch。模型没有提交承接/消费意图时，相关观察继续在待处理区，不能悄悄按时间删除。

**不承诺模型总会合并调用。** 单独产生 workspace_update 导致下一轮请求、schema失败修复、提醒引起的往返都计入管理成本。如果单工具模型无法有效使用该合同，可以另测结构化 `agent_step` 同时携带管理与行动，但必须用同样封装的 Summary 对照，不能把工具接口收益误归为状态管理。不要直接强制所有模型改为全 REPL。

无每步额外 summarizer/critic 请求。预算提示附在正常请求的可控尾部，合并源材料重建快照由代码执行；只有实际失败/不完整承接才按明确有界策略修复。

## 6. 更新、消费与淘汰的原子性

1. 正常工具结果先按现有规则落盘；host facts 从实际结果更新，状态为 unknown 时不猜成功。
2. 请求 manifest 固定可见来源、state revision、source cut、模型配置和渲染版本。
3. 模型更新只引用过去的合法来源，引用内容必须确实暴露过。形状验证不能证明语义正确。
4. 更新成功后，才把对应观察标记为已处理；未闭合工具批次、provider必需的continuation、用户有效约束不得淘汰。
5. 同步/存储失败不发布成功状态。无效patch不导致材料退役，也不凭空取消已经真实发生的工具副作用。
6. 后续新观察不在旧cut里，下一请求继续携带；不能被旧的批量更新吞掉。
7. 选择关系和checkpoint先持久化再用于出站请求，保证恢复能知道实际用了哪份视图。

Python 参考只接受闭合 protocol unit，真实并行与异步批次必须由现有 Agent loop 管理。被引用的旧 tool call 作为历史数据展示，绝不交给 ToolExecutor。普通 tool result 保持合法 call_id 配对；资料包不能伪装成没有对应调用的 ToolResult。

## 7. 缓存、计量与单源状态

ContextPlan 物化一次，同时产出 messages、compatible private replay 和预算，不给 three pathways 各维护不同列表。删除/替换后废弃旧的仅追加 usage 基线，按真实最终请求重新估算，下一 provider usage 校准。cached input 仍占窗口；usage缺失是unknown，不是0。

状态逻辑更新在事件发生时生效，prompt布局使用epoch快照+有界增量。重建不改策略，不再调用LLM重述状态。入场预览保留完整源数据与明示截断范围；大结果需要进一步展开，不把一次预览标成全文已经看过。

provider可用缓存断点、模型/路由/工具schema、隐式续接均绑定请求manifest。需要脱离旧隐式history链时，使用该provider真正支持的新请求语义，不能只修改客户端 Vec。private reasoning 不进入FTS，也不从可见文本反推。没有验证过的组合明确不支持实验策略，不自动退回Summary。

## 8. 手动、overflow 和恢复

`/compact` 在 Summary 下走原引擎；两候选下做确定性 workspace rebase。无可回收内容则NotNeeded；新视图确实合法且更小才重发overflow请求。保留现有turn级有界恢复预算，不用无限尝试掩盖错误。

崩溃发生在patch提交前：恢复旧状态，尚未处理观察仍在。
提交后、checkpoint前：从已提交增量恢复最新逻辑状态与旧快照+增量。
checkpoint后、operation终态前：checkpoint有效；operation按原合同修复为interrupted。
工具副作用后但结果未知：保留unknown、不自动重放；状态或计划文本不覆盖真实恢复合同。
索引损坏：重建；若源payload缺失则明确缺失，不通过重新执行旧命令“恢复”。

原型没有生产OS锁、目录同步或provider发送步骤。这些必须复用和验证现有实现，不能通过23项Python测试宣布完成。

## 9. 接线完成的最低事实标准（不是削弱目标）

两策略能在真实 `singularity --json` 和Web创建/恢复中选定且固定；手动/overflow/中断恢复通过；工具合法、来源可回读、baseline行为保持；指标包含所有实际出站请求与管理成本。还需要真实模型开发任务评测，才可决定默认策略。
