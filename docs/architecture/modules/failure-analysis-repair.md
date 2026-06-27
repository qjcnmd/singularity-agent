# Failure Analysis / Repair模块数据流

模块数据流文档 ID: failure-analysis-repair

源码证据路径:
- src/singularity/failure_analysis/request.py
- src/singularity/failure_analysis/result.py
- src/singularity/failure_analysis/analyzer.py
- src/singularity/repair/contract.py
- src/singularity/repair/plan.py
- src/singularity/repair/planner.py
- src/singularity/repair/signal.py

关键符号:
- FailureAnalysisRequest
- FailureAnalysisResult
- RepairContract
- RepairActionCandidate
- RepairPlan
- RepairReplanSignal
- FailureAnalyzer
- RepairPlanner

字段清单:
- FailureAnalysisRequest: request_id, run_id, session_id, task_id, phase_id, workspace_root, failure_source, failure_summary, failure_sources, context_references, recent_tail, verification_log_refs, changed_files, evidence_refs, metadata, risk_points, repair_policy, verification_strategies
- FailureAnalysisResult: analysis_id, request_id, root_cause, failure_category, affected_files, evidence_refs, repair_strategy, next_actions, verification_plan, confidence, needs_user_input, blocked_reason, raw_response_ref, verification_contract
- RepairContract: contract_id, analysis_id, failure_category, target_files, evidence_refs, action_candidates, verification_plan, confidence, allowed_tool_names, needs_user_input, blocked_reason, validation_errors, verification_contract
- RepairActionCandidate: candidate_id, action_type, target_file, rationale, tool_hints, verification_ref, confidence
- RepairPlan: plan_id, analysis_id, strategy, summary, action_candidates, next_actions, verification_plan, evidence_refs, confidence, needs_user_input, blocked_reason, repair_contract, verification_contract
- RepairReplanSignal: signal_id, repair_plan_id, analysis_id, contract_id, failure_fingerprint, failure_category, target_files, action_candidates, verification_plan, confidence, needs_user_input, blocked_reason, repair_contract, error_code, verification_failed, verification_contract

## 这一层解决什么问题

失败分析与修复层把失败证据转换为根因、修复计划、修复契约和 replanner signal，避免模型在同一失败上盲目循环。

## 当前源码位置

- src/singularity/failure_analysis/request.py
- src/singularity/failure_analysis/result.py
- src/singularity/failure_analysis/analyzer.py
- src/singularity/repair/contract.py
- src/singularity/repair/plan.py
- src/singularity/repair/planner.py
- src/singularity/repair/signal.py

## 关键类、函数、字段

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

`AgentLoop._maybe_analyze_failure()` -> `FailureAnalysisRequest.from_planner()` -> `FailureAnalyzer.analyze()` -> `RepairPlanner.plan()` -> `RepairPlanner.to_replan_signal()` -> `Planner.record_failure_analysis()` -> `Planner.replan()`。

## 真实对象完整结构

- `FailureAnalysisRequest（失败分析请求）` 完整字段列在字段清单中，生成者是 AgentLoop。
- `RepairContract（修复契约）` 完整字段列在字段清单中，消费者是 replanner、verification contract satisfaction 和 targeted replay。

## 谁生成这些对象

- `AgentLoop._maybe_analyze_failure()` 调用 `FailureAnalysisRequest.from_planner()`，从 planner evidence、context references、recent tail、changed files 和当前 outcome 生成 request。
- `FailureAnalyzer.analyze()` 将 `request.to_model_payload()` 发送给失败分析模型，成功响应由 `FailureAnalysisResult.from_model_payload()` 生成；invalid JSON、schema 或不可用模型由 `FailureAnalysisResult.blocked()` 生成明确 blocked result。
- `RepairPlanner.plan()` 先用 `_action_candidates()` 生成 `RepairActionCandidate`，再由 `RepairContract.from_analysis()` / `blocked()` 与 `RepairPlan` 构造修复边界；`RepairPlanner.to_replan_signal()` 调用 `RepairReplanSignal.from_contract()` 生成 replanner 输入。

## 谁消费这些对象

- `FailureAnalyzer` 消费 `FailureAnalysisRequest`；只有 `to_model_payload()` 的有界失败证据进入 failure-analysis 模型，workspace root、raw log 和完整 metadata 不直接发送。
- `RepairPlanner`、Planner、ContextManager 消费 `FailureAnalysisResult`；`RepairContract`/candidate 限定 target files、allowed tools 和 verification。Planner authorization 与 `VerificationRunner` 是生产消费者，targeted replay 只是读取证据的评估消费者。
- Planner 与 planner-decision producer 消费 `RepairReplanSignal`，因此 signal 进入的是独立 replanner 模型请求；`RepairPlan`/contract 的安全摘要通过 planner context 进入后续主模型 turn。

## 是否落盘

- request 不保存完整副本；requested trace 只记录 id、failure source、evidence refs 与 changed files。`FailureAnalysisResult`、`RepairActionCandidate`、`RepairContract`、`RepairPlan` 写入 `.singularity/planner/<session_id>/evidence.json` 的对应 evidence bucket。
- ContextManager 将 failure analysis/repair plan/signal 投影为 failure item 写当前 trace run 的 `context.sqlite3`；contract/candidate/verification contract 作为父对象嵌套 JSON，不另建表。
- `RepairReplanSignal` 的安全字典写 `planner_events.jsonl` 的 `replan_signal`，并由 planner recorder 投影到 observability `events.jsonl`；没有独立 repair report 文件。

## 是否进入 trace / audit

- `FailureAnalyzer._record()` 写 `failure_analysis_requested`、`failure_analysis_completed`、`failure_analysis_failed`；requested payload 是 request 摘要，completed payload 是 result 的安全投影，raw model response 只通过 `raw_response_ref` 引用。
- `RepairPlanner._record_contract_validation()` 写 `repair_contract_validation`，只含 contract/analysis id、category、target count、allowed tools 与 validation errors；`Planner._record_repair_signal_consumed()` 写 `repair_signal_consumed`。
- 本层对象本身不写 policy audit；后续候选工具/验证执行生成各自的 `PolicyRequest`/`PolicyDecision` 后，才由 policy ledger 记录实际授权结果。

## 失败路径

- 无失败、重复 fingerprint 或没有新增证据时 AgentLoop 不重复分析；failure-analysis invalid JSON/schema、低 confidence、越权 target、缺 evidence/target/action 或不可执行 verification 产生 blocked result/contract 与 `needs_user_input`/`blocked_reason`。
- `RepairActionCandidate` 校验 action type、target、tool hint、rationale 和 confidence；`RepairContract.validation_errors` 明确记录 unsupported tool、unauthorized target 与 invalid verification contract，Planner fail-closed 拒绝越权工具/路径。
- replanner 对 ask-user、repeated-failure budget、policy/sandbox/permission 类失败返回阻塞或人工输入；verification failure 保持 `verification_failed=True` 并继续受 contract 限制，不清空旧失败证据。

## 当前结构问题

`FailureAnalysisResult.root_cause` 在 dataclass 中是字符串，但 `to_dict()` 将其投影成包含 description/evidence/confidence 的对象；这是序列化边界，不应通过新增 alias 字段解决。repair candidate、contract、plan、signal 均为不同授权阶段，文档与代码不得合并成一个宽松字典。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
