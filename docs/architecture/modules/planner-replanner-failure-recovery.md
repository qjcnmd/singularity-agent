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
- ReplanDecision
- FinalReport
- Planner

字段清单:
- CompletionCriteria: required_files_inspected, required_changes_applied, required_verifications_passed, unresolved_failures_empty, workspace_health_acceptable, risks_acknowledged, final_report_ready
- TaskState: task_id, session_id, user_goal, normalized_goal, effective_goal, goal_revisions, constraints, assumptions, current_phase, status, risk_level, created_at, updated_at, completion_criteria, open_questions, blocked_reasons, linked_transactions, linked_commands, linked_verifications, final_assessment, task_contract, lifecycle_status, rolling_plan, sandbox_capability, risk_points, verification_strategies, repair_policy
- TaskPhase: phase_id, name, purpose, allowed_tools, allowed_actions, entry_conditions, exit_conditions, required_evidence, failure_policy, risk_notes
- TaskPlan: plan_id, task_id, phases, current_phase, version, updated_at
- AgentAction: kind, intent, phase_id, preconditions, allowed_tools, expected_evidence, risk_level, status, action_id, result_ref
- EvidenceLedger: inspected_files, relevant_symbols, search_results, applied_changes, command_results, verification_results, parsed_failures, assumptions, missing_evidence, unresolved_failures, external_changes, risks, tool_results, policy_observations, sandbox_observations, instruction_prompt_observations, project_index_observations, diff_observations, edit_plans, edit_results, review_results, failure_analyses, repair_plans, retrieval_results, task_outcomes
- ExecutionBudget: max_model_turns, max_tool_calls, max_command_runs, max_mutation_transactions, max_repair_iterations, max_changed_files, max_wall_time_seconds, max_repeated_failures, max_context_growth, model_turns, tool_calls, command_runs, mutation_transactions, repair_iterations, changed_files, context_growth, repeated_failures
- AuthorizationDecision: allowed, action, error_code, reason, risk_decision
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

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

`AgentLoop.run()` -> `planner.step()` -> tool/model/verification outcomes 写入 `EvidenceLedger` -> `RunController` reduce outcome -> `Planner.replan()` 或 `Planner.finalize()`。

## 真实对象完整结构

- `TaskState（任务状态）` 完整字段列在字段清单中，是 planner 的主状态对象。
- `FinalReport（最终报告）` 完整字段列在字段清单中，消费者是 kernel finalization、evaluation result extraction、memory learning 和 trace。

## 谁生成这些对象

- `Planner.start_task()` 生成 `TaskState` 与默认 `CompletionCriteria`，并通过 `_default_plan()` 生成 `TaskPhase` 列表和 `TaskPlan`。`Planner.step()` 根据当前 phase 生成 `AgentAction`。
- Planner 初始化 `EvidenceLedger`/`ExecutionBudget`；tool、mutation、command、verification、failure/review 的 `record_*`/`update_*` 方法持续更新 evidence，BudgetController 与 replan 路径更新 budget counters。
- `Planner.authorize_tool_call()` 生成 `AuthorizationDecision`，`Planner.replan()` 生成 `ReplanDecision`，`RiskEscalator.evaluate_action()` 生成 `RiskEscalation`。`Planner.finalize()` 委托 `Finalizer.build()` 生成 `FinalReport`。

## 谁消费这些对象

- Planner、AgentLoop、RunController 和 finalizer 消费 `TaskState`/`TaskPlan`/`CompletionCriteria`；tool exposure/authorization 消费当前 `TaskPhase` 与 `AgentAction`。主模型只接收 `PlannerContextRenderer` 投影的 goal、phase、allowed tools、rolling plan 与选择性 evidence，不接收完整 state/plan。
- completion/replan/finalizer 消费 `EvidenceLedger`，BudgetController 和 Planner step/replan 消费 `ExecutionBudget`；ledger/budget 全量对象不进模型。`AuthorizationDecision` 由 ToolExecutor 消费，deny reason 可经 tool observation 进入后续模型。
- planner 状态机消费 `ReplanDecision`/`RiskEscalation`；replan signal 会进入 planner-decision producer 的独立模型请求。`AgentKernel`、CLI、evaluation 和 memory learning 消费 `FinalReport`，final report 不再发送给主模型。

## 是否落盘

- `Planner._persist()` 调用 `PlannerStore.save()`，在 `.singularity/planner/<session_id>/` 写 `state.json`、`plan.json`、`evidence.json`、`budget.json` 和完成后的 `final_report.json`；human-readable finalizer 另写 `final_report.md`。
- `CompletionCriteria` 嵌在 `state.json`，`TaskPhase` 嵌在 `plan.json`。`AgentAction`、`AuthorizationDecision`、`ReplanDecision` 与 `RiskEscalation` 没有独立 JSON 文件，只在 planner event/evidence 投影中保存。
- 每个 planner event 追加到 `planner_events.jsonl`；event payload 同时经 trace recorder 进入当前 run 的 `events.jsonl`。context 只保存 planner message/observation 投影，不复制完整 PlannerStore。

## 是否进入 trace / audit

- `Planner._record_event()` 记录 phase/status、action/replan/finalization 摘要与当前 `budget_state`；`AgentAction` 的 action id/kind、`ReplanDecision` 的 decision/reason/next_action 和 final report completion 摘要均由具体 planner event 产生。
- `FinalReport` 完整 payload 由 kernel `finalization.completed` lifecycle event 记录，planner 侧 `final_report.completed` 只写摘要。`EvidenceLedger` 不整体进入 trace，只有各 producer 的增量 evidence event。
- `AuthorizationDecision` 是 planner 局部授权；实际 capability/resource policy 决策仍由 PolicyEngine 生成 `PolicyAuditEntry`。Risk/replan/state 对象不直接写 policy audit。

## 失败路径

- completion criterion 不满足时 `assess_completion()` 返回 unmet 集合；`TaskState.status`/`blocked_reasons` 表达 blocked、needs-review 或 failed。`EvidenceLedger.missing_evidence`/`unresolved_failures` 保留未解决原因。
- `TaskPlan.phase()` 找不到 phase 时失败；`TaskPhase.failure_policy` 决定该阶段后续策略。budget 对 model/tool/command/mutation/repair/repeated failure 超限时阻止继续，不通过重置 counter 绕过。
- authorization 对 phase、repair contract、benchmark tool/path、verification command 违规返回 `allowed=False` 和具体 error code；replan 可返回 ask-user、require-review 或 repeated-failure。final reviewer 拒绝或 completion 未满足时 `FinalReport.status` 非 completed，AgentLoop 不返回成功。

## 当前结构问题

Planner 同时维护 durable state、增量 events 与模型可见投影；新增 evidence bucket 或状态时必须同步 `to_dict/from_dict`、PlannerStore、PlannerContextRenderer、completion/finalizer 和 trace event，不能以“整个对象都会进入模型/trace”概括。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
