# Planner / Replanner / Failure Recovery模块数据流

模块数据流文档 ID: planner-replanner-failure-recovery

源码证据路径:
- src/singularity/planner/models.py
- src/singularity/planner/engine.py
- src/singularity/planner/replanner.py
- src/singularity/planner/store.py
- src/singularity/planner/context.py
- src/singularity/planner/finalizer.py
- src/singularity/planner/risk.py
- src/singularity/run_controller.py

关键符号:
- TaskState
- TaskPlan
- AgentAction
- EvidenceLedger
- VerificationEvidenceRecord
- SandboxObservationRecord
- PolicyObservationRecord
- ToolResultRecord
- TaskOutcomeRecord
- ReplanDecision
- Replanner
- FinalReport
- _ToolScope
- Planner

字段清单:
- CompletionCriteria: required_files_inspected, required_changes_applied, required_verifications_passed, unresolved_failures_empty, workspace_health_acceptable, risks_acknowledged, final_report_ready
- TaskState: task_id, session_id, user_goal, normalized_goal, effective_goal, goal_revisions, constraints, assumptions, current_phase, status, risk_level, created_at, updated_at, completion_criteria, open_questions, blocked_reasons, linked_transactions, linked_commands, linked_verifications, final_assessment, task_contract, lifecycle_status, rolling_plan, sandbox_capability, risk_points, verification_strategies, repair_policy
- TaskPhase: phase_id, name, purpose, allowed_tools, allowed_actions, entry_conditions, exit_conditions, required_evidence, failure_policy, risk_notes
- TaskPlan: plan_id, task_id, phases, current_phase, version, updated_at
- AgentAction: kind, intent, phase_id, preconditions, allowed_tools, expected_evidence, risk_level, status, action_id, result_ref
- VerificationEvidenceRecord: completion_assessment, check_status, results, tool_call_id, plan, extra
- SandboxObservationRecord: source, backend, status, sandbox_enforcement, enforcement_status, execution_backend, fallback_used, fallback_reason, elevated_available, elevated_blocker_summary, network_denied_verified, process_tree_kill, job_killed, timeout_enforced, artifact_count, artifact_refs, changed_files_count, violations, imported_changes_count, extra
- PolicyObservationRecord: outcome, component, operation, reason, risk_level, resource, approval_grant_id, approved_by_user, extra
- ToolResultRecord: tool_call_id, tool_name, action_id, ok, status, error_code, failure, extra
- TaskOutcomeRecord: status, error_code, summary, reason, next_action, retry_allowed, missing_evidence, extra
- EvidenceLedger: inspected_files, relevant_symbols, search_results, applied_changes, command_results, verification_results, parsed_failures, assumptions, missing_evidence, unresolved_failures, external_changes, risks, tool_results, policy_observations, sandbox_observations, instruction_prompt_observations, project_index_observations, diff_observations, edit_plans, edit_results, review_results, failure_analyses, repair_plans, retrieval_results, task_outcomes
- ExecutionBudget: max_model_turns, max_tool_calls, max_command_runs, max_mutation_transactions, max_repair_iterations, max_changed_files, max_wall_time_seconds, max_repeated_failures, max_context_growth, model_turns, tool_calls, command_runs, mutation_transactions, repair_iterations, changed_files, context_growth, repeated_failures
- AuthorizationDecision: allowed, action, error_code, reason, risk_decision
- _ToolScope: allowed_tools, phase_allowed, repair_allowed, repair_evidence_allowed, repair_execution_block, benchmark_allowed
- ReplanDecision: decision, reason, next_action
- RiskEscalation: decision, risk_level, reasons
- FinalReport: user_goal, status, files_changed, agent_changes, command_side_effects, verification_summary, unresolved_issues, risks, rollback_status, policy_approval_summary, artifacts, next_steps, sandbox_isolation_summary, execution_trace_summary, model_usage_summary, context_usage_diagnostic, instruction_prompt_summary, component_health_summary, shutdown_summary, recovery_summary, lifecycle_summary, review_summary, failure_repair_summary, contract_satisfaction

## 这一层解决什么问题

Planner 层维护任务状态、阶段、行动、证据、预算、重规划决策和最终报告，决定何时继续、修复、阻塞或完成。

## 当前源码位置

- src/singularity/planner/models.py
- src/singularity/planner/engine.py
- src/singularity/planner/replanner.py
- src/singularity/planner/store.py
- src/singularity/planner/context.py
- src/singularity/planner/finalizer.py
- src/singularity/planner/risk.py
- src/singularity/run_controller.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

`AgentLoop.run()` -> `planner.step()` -> tool/model/verification outcomes 写入 `EvidenceLedger` -> `RunController` reduce outcome -> `Planner.replan()` 或 `Planner.finalize()`。`Planner.replan()` 先把 signal 投给 `Replanner.decide()` 做 repair contract、blocked reason、fresh-file、review 和 verification failure 等规则判定；Planner 自身继续负责 `TaskState` 状态转换、budget、persist 和 event recording。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例：`Planner.start_task()` -> `Planner.step()` -> `Planner.update_from_tool_result()` / `update_from_command()` / `update_from_verification()` 先生成对象 `TaskState`、`TaskPhase`、`TaskPlan` 和 `AgentAction`，再通过 `EvidenceLedger.add_tool_result()`、`add_verification_result()`、`add_sandbox_observation()`、`add_policy_observation()`、`add_task_outcome()` 把关键 evidence bucket 规范化为 typed record 的 JSON 投影，并更新 `ExecutionBudget`；`Planner._persist()` 再把 state/plan/evidence/budget 写入 `.singularity/planner/<session_id>/state.json`、`plan.json`、`evidence.json`、`budget.json`。completion gate 不满足时 `Planner.replan()` 先调用 `Replanner.decide()` 取得 `ReplanDecision`，再在同一方法内应用状态转换、重复失败预算和 event recording；失败分析的 `RepairReplanSignal` 通过 `Planner.record_failure_analysis()` 进入同一 evidence/report 链。
## 真实对象完整结构

### TaskState（任务状态）

planner 的主状态对象，维护任务全生命周期。**边界**：落盘到 `.singularity/planner/<session_id>/state.json`；投影进 planner context 和 trace event，不作为整体进入模型请求。

```python
@dataclass
class TaskState:
    task_id: str
    session_id: str
    user_goal: str
    normalized_goal: str
    effective_goal: str | None = None
    goal_revisions: list[dict[str, Any]] = field(default_factory=list)
    constraints: list[str] = field(default_factory=list)
    assumptions: list[str] = field(default_factory=list)
    current_phase: str = "understanding_task"
    status: TaskStatus = TaskStatus.INITIALIZED
    risk_level: RiskLevel = RiskLevel.LOW
    created_at: str = field(default_factory=lambda: _now())
    updated_at: str = field(default_factory=lambda: _now())
    completion_criteria: CompletionCriteria = field(default_factory=CompletionCriteria)
    open_questions: list[str] = field(default_factory=list)
    blocked_reasons: list[str] = field(default_factory=list)
    linked_transactions: list[str] = field(default_factory=list)
    linked_commands: list[str] = field(default_factory=list)
    linked_verifications: list[str] = field(default_factory=list)
    final_assessment: dict[str, Any] = field(default_factory=dict)
    task_contract: dict[str, Any] = field(default_factory=dict)
    lifecycle_status: str = "created"
    rolling_plan: dict[str, Any] = field(default_factory=dict)
    sandbox_capability: dict[str, Any] = field(default_factory=dict)
    risk_points: list[dict[str, Any]] = field(default_factory=list)
    verification_strategies: list[dict[str, Any]] = field(default_factory=list)
    repair_policy: dict[str, Any] | None = None
```

### AgentAction（智能体行动）

planner 当前阶段的具体行动决策。**边界**：内部治理对象，不落盘为独立文件；action id/kind 进入 trace event。

```python
@dataclass
class AgentAction:
    kind: ActionKind
    intent: str
    phase_id: str
    preconditions: list[str]
    allowed_tools: list[str]
    expected_evidence: list[str]
    risk_level: RiskLevel = RiskLevel.LOW
    status: ActionStatus = ActionStatus.PROPOSED
    action_id: str = field(default_factory=lambda: f"action_{uuid4().hex[:12]}")
    result_ref: str | None = None
```

### _ToolScope（工具范围）

Planner 内部的工具范围快照，统一 phase policy、active repair contract、repair evidence block 和 benchmark constraints 对工具集合的收敛。**边界**：仅为 `Planner._active_tool_scope()` 的内存对象，不落盘，不进入模型请求，不直接进入 trace；消费者只使用它投影出的 model-visible tool schemas、ToolChoicePolicy allowed names、`tool.exposure_decided.selected_tools` 和授权判断。

```python
@dataclass(frozen=True)
class _ToolScope:
    allowed_tools: set[str]
    phase_allowed: set[str]
    repair_allowed: set[str]
    repair_evidence_allowed: set[str]
    repair_execution_block: tuple[str, str] | None
    benchmark_allowed: set[str]
```

### Evidence typed records（关键证据记录）

completion、verification、sandbox、policy、tool result 和 task outcome 相关 bucket 的 typed projection。**边界**：运行时仍以 `EvidenceLedger` 的 list-of-dict JSON shape 落盘；`add_*()` 和 `*_records()` helper 在写入与 Finalizer/CompletionGate 读取时提供字段稳定性。

```python
@dataclass(frozen=True)
class VerificationEvidenceRecord:
    completion_assessment: dict[str, Any] = field(default_factory=dict)
    check_status: list[dict[str, Any]] = field(default_factory=list)
    results: list[dict[str, Any]] = field(default_factory=list)
    tool_call_id: str | None = None
    plan: dict[str, Any] = field(default_factory=dict)
    extra: dict[str, Any] = field(default_factory=dict)

@dataclass(frozen=True)
class SandboxObservationRecord:
    source: str | None = None
    backend: str | None = None
    status: str | None = None
    sandbox_enforcement: str | None = None
    enforcement_status: str | None = None
    execution_backend: str | None = None
    fallback_used: bool | None = None
    fallback_reason: str | None = None
    elevated_available: bool | None = None
    elevated_blocker_summary: str | None = None
    network_denied_verified: bool | None = None
    process_tree_kill: bool | None = None
    job_killed: bool | None = None
    timeout_enforced: bool | None = None
    artifact_count: int = 0
    artifact_refs: list[str] = field(default_factory=list)
    changed_files_count: int = 0
    violations: list[dict[str, Any]] = field(default_factory=list)
    imported_changes_count: int = 0
    extra: dict[str, Any] = field(default_factory=dict)

@dataclass(frozen=True)
class PolicyObservationRecord:
    outcome: str | None = None
    component: str | None = None
    operation: str | None = None
    reason: str | None = None
    risk_level: str | None = None
    resource: str | None = None
    approval_grant_id: str | None = None
    approved_by_user: bool | None = None
    extra: dict[str, Any] = field(default_factory=dict)

@dataclass(frozen=True)
class ToolResultRecord:
    tool_call_id: str | None = None
    tool_name: str | None = None
    action_id: str | None = None
    ok: bool | None = None
    status: str | None = None
    error_code: str | None = None
    failure: dict[str, Any] | None = None
    extra: dict[str, Any] = field(default_factory=dict)

@dataclass(frozen=True)
class TaskOutcomeRecord:
    status: str
    error_code: str | None = None
    summary: str | None = None
    reason: str | None = None
    next_action: str | None = None
    retry_allowed: bool | None = None
    missing_evidence: list[str] = field(default_factory=list)
    extra: dict[str, Any] = field(default_factory=dict)
```

### FinalReport（最终报告）

任务完成时的结构化总结。**边界**：落盘到 `.singularity/planner/<session_id>/final_report.json` 和 `final_report.md`；完整 payload 由 kernel `finalization.completed` lifecycle event 记录，不发送给主模型。

```python
@dataclass
class FinalReport:
    user_goal: str
    status: TaskStatus
    files_changed: list[str]
    agent_changes: list[dict[str, Any]]
    command_side_effects: list[dict[str, Any]]
    verification_summary: dict[str, Any]
    unresolved_issues: list[Any]
    risks: list[Any]
    rollback_status: dict[str, Any]
    policy_approval_summary: dict[str, Any]
    artifacts: list[str]
    next_steps: list[str]
    sandbox_isolation_summary: dict[str, Any] = field(default_factory=dict)
    execution_trace_summary: dict[str, Any] = field(default_factory=dict)
    model_usage_summary: dict[str, Any] = field(default_factory=dict)
    context_usage_diagnostic: dict[str, Any] = field(default_factory=dict)
    instruction_prompt_summary: dict[str, Any] = field(default_factory=dict)
    component_health_summary: dict[str, Any] = field(default_factory=dict)
    shutdown_summary: dict[str, Any] = field(default_factory=dict)
    recovery_summary: dict[str, Any] = field(default_factory=dict)
    lifecycle_summary: dict[str, Any] = field(default_factory=dict)
    review_summary: dict[str, Any] = field(default_factory=dict)
    failure_repair_summary: dict[str, Any] = field(default_factory=dict)
    contract_satisfaction: dict[str, Any] = field(default_factory=dict)
```

### 关键枚举值域

```python
class TaskStatus(str, Enum):         # TaskState.status
    INITIALIZED = "initialized"
    UNDERSTANDING_TASK = "understanding_task"
    INSPECTING_WORKSPACE = "inspecting_workspace"
    PLANNING_CHANGES = "planning_changes"
    APPLYING_CHANGES = "applying_changes"
    RUNNING_VERIFICATION = "running_verification"
    REPAIRING_FAILURES = "repairing_failures"
    FINALIZING = "finalizing"
    COMPLETED = "completed"
    BLOCKED = "blocked"
    FAILED = "failed"
    NEEDS_REVIEW = "needs_review"
    INTERRUPTED = "interrupted"
    RECOVERING = "recovering"

class ActionKind(str, Enum):         # AgentAction.kind
    INSPECT_WORKSPACE = "InspectWorkspace"
    READ_RELEVANT_FILES = "ReadRelevantFiles"
    SEARCH_CODE = "SearchCode"
    ANALYZE_ISSUE = "AnalyzeIssue"
    PROPOSE_CHANGE_SET = "ProposeChangeSet"
    APPLY_MUTATION = "ApplyMutation"
    RUN_VERIFICATION = "RunVerification"
    PARSE_FAILURE = "ParseFailure"
    REPAIR_CHANGE = "RepairChange"
    ASK_USER = "AskUser"
    REQUIRE_REVIEW = "RequireReview"
    FINALIZE = "Finalize"
    ABORT = "Abort"

class ActionStatus(str, Enum):       # AgentAction.status
    PROPOSED = "proposed"
    ALLOWED = "allowed"
    RUNNING = "running"
    SUCCEEDED = "succeeded"
    FAILED = "failed"
    BLOCKED = "blocked"

class RiskDecisionKind(str, Enum):   # AuthorizationDecision.risk_decision
    CONTINUE = "continue"
    REQUIRE_REVIEW = "require_review"
    ASK_USER = "ask_user"
    DENY_ACTION = "deny_action"
    ABORT = "abort"
```

### 数据流概述

`Planner.start_task()` 生成 `TaskState` 和 `TaskPlan`，`Planner.step()` 生成 `AgentAction`。工具/命令/验证结果写入 `EvidenceLedger`（25 个 evidence bucket），其中 `verification_results`、`sandbox_observations`、`policy_observations`、`tool_results`、`task_outcomes` 通过 typed helper 写入和读取；持久化仍保持 list-of-dict JSON shape。`ExecutionBudget` 跟踪计数器。`Planner._persist()` 写 `state.json`/`plan.json`/`evidence.json`/`budget.json`。`Planner._active_tool_scope()` 生成 `_ToolScope`，把 phase policy、repair contract、repair evidence block 和 benchmark constraints 收敛为同一份 `allowed_tools`；`Planner.decide_tool_exposure()`、`Planner.filtered_tools()` 和 `Planner.authorize_tool_call()` 共用该结果，确保模型可见工具、ToolChoicePolicy allowed names、`tool.exposure_decided.selected_tools` 和执行授权来自同一来源，同时授权拒绝仍按 repair block、repair allowed、benchmark allowed、phase policy 的原有顺序返回具体 error code。`Finalizer.build()` 通过 `latest_verification_result()`、`sandbox_records()`、`policy_records()`、`tool_result_records()` 聚合关键 summary 并生成 `FinalReport`，落盘 `final_report.json`/`.md`。`PlannerContextRenderer` 只投影 goal、phase、allowed tools、rolling plan 与选择性 evidence 进入模型上下文。

`FinalReport.status` 的完成边界来自三层内部证据：最新 `verification_summary.status` 必须是 `ready` 或 `ready_with_warnings`，final review `latest_decision` 必须是 `accept`，active repair `contract_satisfaction.satisfied` 不能为 false。三者满足时 `Planner.finalize()` 把 `TaskState.status` 置为 `completed`、设置 `completion_criteria.final_report_ready=True`，并清理已由本次 final report 解决的历史 completion blocker。`contract_satisfaction` 随 report 落盘和进入 kernel planner summary；它不进入主模型请求，也不由 evaluation 后验 verification 改写。

`Finalizer._sandbox_summary()` 从 `SandboxObservationRecord` 聚合 `selected_backends`、`backend_unavailable_count`、`local_process_backend_count`、`reduced_backend_count`、`reduced_backends`、`elevated_blocker_summaries`、network/process proof、artifact refs 和 change counts。`windows_unelevated` 会作为 reduced backend 进入 summary；`local_process` 仍单独计入 `local_process_backend_count`，用于 evaluation 的 public task sandbox enforcement audit。

## 谁生成这些对象

- `Planner.start_task()` 生成 `TaskState` 与默认 `CompletionCriteria`，并通过 `_default_plan()` 生成 `TaskPhase` 列表和 `TaskPlan`。`Planner.step()` 根据当前 phase 生成 `AgentAction`。
- Planner 初始化 `EvidenceLedger`/`ExecutionBudget`；tool、mutation、command、verification、failure/review 的 `record_*`/`update_*` 方法持续更新 evidence。关键 bucket 由 `EvidenceLedger.add_*()` 生成 typed record 后再投影回 dict；BudgetController 与 replan 路径更新 budget counters。
- `Planner._active_tool_scope()` 生成 `_ToolScope`，`Planner.authorize_tool_call()` 生成 `AuthorizationDecision`，`Replanner.decide()` 生成规则层面的 `ReplanDecision`，`Planner.replan()` 应用该 decision 并处理持久化/事件/预算状态，`RiskEscalator.evaluate_action()` 生成 `RiskEscalation`。`Planner.finalize()` 委托 `Finalizer.build()` 生成 `FinalReport`。

## 谁消费这些对象

- `Planner.step()`、`AgentLoop.run()` 内部 `run_turn()`、`RunController.apply_protocol_result()` / `apply_outcome()` 和 `Finalizer.build()` 消费 `TaskState`/`TaskPlan`/`CompletionCriteria`；`Planner.decide_tool_exposure()`、`Planner.filtered_tools()` 和 `Planner.authorize_tool_call()` 消费 `_ToolScope`；`ToolExecutor.execute_request()` 消费当前 `TaskPhase` 与 `AgentAction`。主模型只接收 `PlannerContextRenderer.render()` 投影的 goal、phase、allowed tools、rolling plan 与选择性 evidence，不接收完整 state/plan。
- `Planner.assess_completion()` 消费 `EvidenceLedger`，`Finalizer.build()` 通过 typed helper 消费 verification/policy/sandbox/tool result 关键 bucket，`BudgetController.check_budget()` 和 `Planner.step()` 消费 `ExecutionBudget`；ledger/budget 全量对象不进模型。`AuthorizationDecision` 由 `ToolExecutor.authorize()` 消费，deny reason 可经 tool observation 进入后续模型。`InteractionController.build_final_report()` 可从 kernel final report 的 `planner_summary` 读取 planner report 投影，用于把 completed + ready verification 映射为 interaction `success`。
- `Planner.replan()` 消费 `Replanner.decide()` 生成的 `ReplanDecision`/`RiskEscalation` 并更新 state；replan signal 会进入 planner-decision producer 的独立模型请求。`AgentKernel.run_task()`、CLI、evaluation 和 `MemoryLearningPipeline.ingest_final_report()` 消费 `FinalReport`，final report 不再发送给主模型。

## 是否落盘

- `Planner._persist()` 调用 `PlannerStore.save()`，在 `.singularity/planner/<session_id>/` 写 `state.json`、`plan.json`、`evidence.json`、`budget.json` 和完成后的 `final_report.json`；human-readable finalizer 另写 `final_report.md`。
- `CompletionCriteria` 嵌在 `state.json`，`TaskPhase` 嵌在 `plan.json`。`_ToolScope`、`AgentAction`、`AuthorizationDecision`、`ReplanDecision` 与 `RiskEscalation` 没有独立 JSON 文件，只在 planner event/evidence 投影中保存。
- 每个 planner event 追加到 `planner_events.jsonl`；event payload 同时经 trace recorder 进入当前 run 的 `events.jsonl`。context 只保存 planner message/observation 投影，不复制完整 PlannerStore。

## 是否进入 trace / audit

- `Planner._record_event()` 记录 phase/status、action/replan/finalization 摘要与当前 `budget_state`；`AgentAction` 的 action id/kind、`ReplanDecision` 的 decision/reason/next_action 和 final report completion 摘要均由具体 planner event 产生。`_ToolScope` 不整体进入 trace，只有 `decide_tool_exposure()` 产生的 `tool.exposure_decided.selected_tools` 与 `tool_choice.allowed_tool_names` 作为工具暴露投影进入 trace。
- `FinalReport` 完整 payload 由 kernel `finalization.completed` lifecycle event 记录，planner 侧 `final_report.completed` 只写摘要。`EvidenceLedger` 不整体进入 trace，只有各 producer 的增量 evidence event。
- `AuthorizationDecision` 是 planner 局部授权；实际 capability/resource policy 决策仍由 PolicyEngine 生成 `PolicyAuditEntry`。Risk/replan/state 对象不直接写 policy audit。

## 失败路径

- completion criterion 不满足时 `assess_completion()` 返回 unmet 集合；`TaskState.status`/`blocked_reasons` 表达 blocked、needs-review 或 failed。`EvidenceLedger.missing_evidence`/`unresolved_failures` 保留未解决原因。
- `TaskPlan.phase()` 找不到 phase 时失败；`TaskPhase.failure_policy` 决定该阶段后续策略。budget 对 model/tool/command/mutation/repair/repeated failure 超限时阻止继续，不通过重置 counter 绕过。
- benchmark constraints 中的 `verification_command` 会替换 task contract 里的推断 verification requirements，成为唯一 required smoke command；公共 capability task 不会把 goal 文本或 rules builder 推断出的 pytest 命令扩展进 AgentLoop 内验证。
- authorization 对 phase、repair contract、benchmark tool/path、verification command 违规返回 `allowed=False` 和具体 error code；tool exposure 与 authorization 共用 `_active_tool_scope()` 的工具范围收敛，避免模型可见工具和执行授权使用不同来源。`Replanner.decide()` 可返回 ask-user、read-fresh-file、repair-failure 或 require-review，重复失败预算由 `Planner.replan()` 记录后再走同一 decision/event 边界。final reviewer 拒绝或 completion 未满足时 `FinalReport.status` 非 completed，AgentLoop 不返回成功。

## 当前结构问题

Planner 同时维护 durable state、增量 events 与模型可见投影；新增 evidence bucket 或状态时必须同步 `to_dict/from_dict`、typed helper、PlannerStore、PlannerContextRenderer、completion/finalizer 和 trace event，不能以“整个对象都会进入模型/trace”概括。关键 completion/report bucket 不应绕过 `EvidenceLedger.add_*()` 直接随意写裸 dict。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
