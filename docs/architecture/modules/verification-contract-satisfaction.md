# Verification / Contract Satisfaction模块数据流

模块数据流文档 ID: verification-contract-satisfaction

源码证据路径:
- src/singularity/verification/models.py
- src/singularity/verification/contract.py
- src/singularity/verification/runner.py
- src/singularity/verification/satisfaction.py
- src/singularity/verification/assessor.py
- src/singularity/tools/verification.py
- src/singularity/planner/engine.py

关键符号:
- VerificationCheck
- VerificationResult
- CompletionAssessment
- VerificationStep
- VerificationContract
- StepEvidence
- ContractSatisfaction
- CompletionAssessor
- VerificationRunner

字段清单:
- VerificationCheck: kind, command, scope, required, timeout, risk_tags, failure_policy, id, policy_decision, policy_reasons, skip_reason, source, contract_step_id
- VerificationResult: check_id, kind, status, failure_type, evidence, repair_hints, confidence_impact, duration_ms, attempts, policy_decision
- CompletionAssessment: status, confidence, passed_checks, failed_checks, skipped_checks, warnings, remaining_risks
- VerificationStep: step_id, command, kind, required
- VerificationContract: contract_id, steps, status, validation_errors
- StepEvidence: step_id, check_id, command_id, status, artifact_ref
- ContractSatisfaction: contract_id, satisfied, completed_steps, failed_steps, skipped_steps, reason, step_evidence

## 这一层解决什么问题

Verification 层发现、计划并执行验证命令，把命令结果转换为 `VerificationResult（验证结果）` 和 completion assessment，同时约束 repair contract 的允许命令。

## 当前源码位置

- src/singularity/verification/models.py
- src/singularity/verification/contract.py
- src/singularity/verification/runner.py
- src/singularity/verification/satisfaction.py
- src/singularity/verification/assessor.py
- src/singularity/tools/verification.py
- src/singularity/planner/engine.py

## 关键类、函数、字段

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

`run_verification` tool -> `VerificationToolHandlers.run_verification()` -> `VerificationRunner.plan_verification()` / `run_plan()` -> `CommandExecutor.run()` -> parsers -> `CompletionAssessor.assess()` -> planner evidence -> `Planner.assess_verification_contract_satisfaction()` -> `AgentLoop._attempt_finalize()`。

## 真实对象完整结构

- `VerificationCheck（验证检查）` 完整字段列在字段清单中，描述要执行或跳过的验证动作。
- `VerificationContract（验证契约）` 完整字段列在字段清单中，约束 repair 阶段允许的验证命令。

## 谁生成这些对象

- `VerificationRunner._check()` 生成 `VerificationCheck`；执行、blocked、skipped、budget 与 policy 分支生成 `VerificationResult`。`CompletionAssessor.assess()` 从 verification plan 和 result 列表生成 `CompletionAssessment`。
- `VerificationContract.from_plan_strings()` 或 Planner 的 benchmark/repair contract 路径生成 `VerificationStep` 与 `VerificationContract`；contract validation errors 在构造时确定。
- `Planner.assess_verification_contract_satisfaction()` 将 contract step 与 evidence 对齐，逐步生成 `StepEvidence`，再生成 `ContractSatisfaction`；这不是 `VerificationRunner` 的输出。

## 谁消费这些对象

- verification planner、policy gate、`CommandExecutor` 消费 `VerificationCheck`；assessor、failure analysis 和 Planner 消费 `VerificationResult`。`run_verification` tool 将安全 result/assessment 投影写入 context，因此下一轮模型可见该投影，而不是内部 command/policy 对象。
- `AgentLoop._attempt_finalize()` 通过 Planner completion state 消费 `CompletionAssessment` 的结论；assessment 本体也进入 verification tool result 与 planner `final_assessment`。
- Planner tool authorization、`VerificationRunner.plan_verification()` 和 contract satisfaction 消费 `VerificationContract`/`VerificationStep`。repair planner context 可把 contract 摘要送入模型；`StepEvidence`/`ContractSatisfaction` 用于 completion/report，不直接作为 provider message。

## 是否落盘

- verification check/result/assessment 作为 tool result 与 planner evidence 写 `.singularity/planner/<session_id>/evidence.json`；相关 tool observation/message 另写当前 trace run 的 `context.sqlite3`。
- `VerificationContract`、`VerificationStep`、`StepEvidence` 和 `ContractSatisfaction` 嵌入 failure analysis、repair plan、planner evidence/final report 的 JSON，不设独立 store。命令长输出使用 `CommandResult.artifact_path` 指向 trace artifact。
- evaluation runner 的独立 public/hidden verification 使用自己的 `CommandEvalResult` 和 evaluation report，不应与本层 `VerificationResult` store 混称。

## 是否进入 trace / audit

- verification runner 记录 plan、check/result、`verification.evidence_recorded` 与 completion assessment 摘要；legacy `verification` record 的 payload 来自 result/assessment 的安全 `to_dict()` 投影，并关联 verification/command ids 与 artifact refs。
- 每个执行检查都先产生 `PolicyRequest`/`PolicyDecision`，decision 进入 policy audit ledger；blocked/denied check 的 `policy_decision` 和 reasons 同时进入 `VerificationCheck`/`VerificationResult`。
- Planner 对 contract satisfaction 的结果进入 planner event/final report；不存在单独的 `VerificationRunner.contract_satisfaction` recorder。

## 失败路径

- `VerificationResult.status` 区分 passed、failed、timeout、blocked、skipped、flaky；policy、budget、missing command 与 parser failure分别由 helper 生成对应 result，而不是抛弃该 check。
- `CompletionAssessor` 对 required failure 返回 `failed`，required blocked 返回 `blocked`，缺结果/高风险人工复核返回 `needs_review`，warning/flaky 返回 `ready_with_warnings`，全部满足才是 `ready`。
- contract 的 invalid/pending、missing step、command 不在 contract 或 step evidence failed/skipped 会使 `ContractSatisfaction.satisfied=False` 并给出 reason；Planner completion gate据此拒绝完成或进入 repair。

## 当前结构问题

执行验证与契约满足度是两个边界：`VerificationRunner.run_plan()` 产生命令证据，`Planner.assess_verification_contract_satisfaction()` 判断 repair contract 是否兑现；维护时必须同步这两条链的字段和失败语义。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
