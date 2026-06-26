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
- RepairPlan
- RepairReplanSignal
- FailureAnalyzer
- RepairPlanner

字段清单:
- FailureAnalysisRequest: request_id, run_id, session_id, task_id, phase_id, workspace_root, failure_source, failure_summary, failure_sources, context_references, recent_tail, verification_log_refs, changed_files, evidence_refs, metadata, risk_points, repair_policy, verification_strategies
- FailureAnalysisResult: analysis_id, request_id, root_cause, failure_category, affected_files, evidence_refs, repair_strategy, next_actions, verification_plan, confidence, needs_user_input, blocked_reason, raw_response_ref, verification_contract
- RepairContract: contract_id, analysis_id, failure_category, target_files, evidence_refs, action_candidates, verification_plan, confidence, allowed_tool_names, needs_user_input, blocked_reason, validation_errors, verification_contract
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
