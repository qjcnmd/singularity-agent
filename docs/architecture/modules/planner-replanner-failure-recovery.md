# Planner / Replanner / Failure Recovery模块数据流

模块数据流文档 ID: planner-replanner-failure-recovery

源码证据路径:
- src/singularity/planner/models.py
- src/singularity/planner/engine.py
- src/singularity/planner/replanner.py
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
- src/singularity/run_controller.py

## 关键类、函数、字段

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

`AgentLoop.run()` -> `planner.step()` -> tool/model/verification outcomes 写入 `EvidenceLedger` -> `RunController` reduce outcome -> `Planner.replan()` 或 `Planner.finalize()`。

## 真实对象完整结构

- `TaskState（任务状态）` 完整字段列在字段清单中，是 planner 的主状态对象。
- `FinalReport（最终报告）` 完整字段列在字段清单中，消费者是 kernel finalization、evaluation result extraction、memory learning 和 trace。

## 谁生成这些对象

这些对象由上文列出的源码组件在运行链路中生成。生成动作必须来自当前源码路径，不允许由文档、测试夹具或解释性包装层伪造。

## 谁消费这些对象

消费方是同一调用链后续组件、trace/audit 记录器、报告生成器或持久化 store。文档只列当前源码中真实调用的消费方。

## 是否落盘

落盘只通过当前源码中的 trace store、SQLite store、workspace state、evaluation output 或 manifest/report 写入路径发生。没有落盘代码的对象只在内存中传递。

## 是否进入 trace / audit

进入 trace / audit 的内容以 `TraceRecorder`、`JsonlTraceRecorder`、`TraceArtifactStore`、policy audit ledger 和相关 `record` / `emit` 调用为准。对象进入模型前必须经过当前工具协议、上下文组装和 redaction 逻辑。

## 失败路径

失败路径由当前源码中的异常、状态枚举、policy decision、verification result、planner outcome 和 result/report 字段表达。不得用旧 schema 或旧命名补充解释。

## 当前结构问题

当前结构仍大量使用字典 payload 连接组件，维护时最容易发生字段漂移。字段清单必须由源码校验脚本约束，不能只依赖人工描述。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
