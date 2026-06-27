# Policy / Approval Gates模块数据流

模块数据流文档 ID: policy-approval-gates

源码证据路径:
- src/singularity/policy/models.py
- src/singularity/policy/engine.py
- src/singularity/policy/approval.py
- src/singularity/policy/audit.py

关键符号:
- PolicyRequest
- PolicyDecision
- ApprovalGrant
- PolicyAuditEntry
- PolicyEngine
- ApprovalGate

字段清单:
- PolicySubject: subject_type, name
- ResourceRef: resource_type, identifier, normalized_identifier, workspace_relative, sensitive, metadata
- PolicyConstraints: filesystem_mode, network_allowed, max_duration_seconds, max_output_chars, allowed_paths, denied_paths, allowed_hosts, env_redaction, sandbox_required, hard_isolation_required
- PolicyRequest: session_id, task_id, phase_id, action_id, component, operation, capability, subject, resource, reason, request_id, proposed_by_model, risk_tags, metadata, evidence_refs, reversible, requires_network, touches_workspace, touches_secrets, destructive, long_running, interactive, workspace_root
- ApprovalScope: capabilities, path_globs, command_patterns, network_hosts, max_duration_seconds, max_files, session_only, single_use
- ApprovalRequirement: message, scope, review_kind, details
- ApprovalGrant: decision_id, request_id, approved_by, scope, session_id, approved_at, grant_id, expires_at, single_use, reason, operator_signature
- PolicyDecision: request_id, outcome, reason, risk_level, risk_tags, user_message, constraints, required_approval, rule_ids, audit_severity, context_summary, decision_id, approval_grant_id
- PolicyAuditEntry: timestamp, session_id, task_id, phase_id, action_id, request_id, decision_id, component, operation, capability, resource_summary, normalized_input_hash, risk_level, risk_tags, outcome, rule_ids, reason, approval_required, approval_grant_id, approved_by_user, user_decision, constraints, execution_result_ref

## 这一层解决什么问题

Policy 层把组件、能力、资源、风险、约束和人工 approval 统一成可审计决策，保护文件、命令、网络、secret 和高风险操作。

## 当前源码位置

- src/singularity/policy/models.py
- src/singularity/policy/engine.py
- src/singularity/policy/approval.py
- src/singularity/policy/audit.py

## 关键类、函数、字段

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

`ToolExecutor` / `CommandExecutor` / `WorkspaceMutationManager` / `VerificationRunner` 创建 `PolicyRequest` -> `PolicyEngine.evaluate()` -> `ApprovalGate` 可选人工授权 -> ledger/audit -> 执行或拒绝。

## 真实对象完整结构

- `PolicyRequest（策略请求）` 完整字段列在字段清单中，生成者是各执行组件。
- `PolicyDecision（策略决策）` 完整字段列在字段清单中，消费者是执行组件、approval gate、trace/audit 和 planner/context。

## 谁生成这些对象

ToolExecutor、CommandExecutor、mutation、verification 与 plugin manager 生成 `PolicySubject`、`ResourceRef` 和 `PolicyRequest`；rules/engine 生成 `PolicyConstraints` 与 `PolicyDecision`。`PolicyDecision.review()` 生成 `ApprovalScope`/`ApprovalRequirement`，ApprovalGate 或 remote approval生成 `ApprovalGrant`，`PolicyAuditWriter.append()` 生成 `PolicyAuditEntry`。

## 谁消费这些对象

PolicyEngine 消费 request，执行器与 ApprovalGate 消费 decision/requirement/grant。完整 request/decision不进入模型；ContextManager最多追加裁剪后的 policy reason/outcome observation。

## 是否落盘

ApprovalGate 将 grant 写受信任 policy home 的 `approval_grants.jsonl`并记录 single-use消费；workspace内 grant store不受信任。`PolicyAuditWriter` 将 `PolicyAuditEntry` 追加到 policy audit JSONL；subject/resource/constraints作为 request/decision/audit嵌套字段。

## 是否进入 trace / audit

PolicyEngine 发出 `policy_requested`、`policy_decided`、`policy_blocked`，payload仅含 ids、operation、capability、脱敏资源、outcome/risk/rules。Audit entry另保存normalized input hash、grant/result ref；trace与audit是两条记录，不互相替代。

## 失败路径

DecisionOutcome可为 deny、require_review、ask_user、escalate、sandbox_required；非交互 review必须fail-closed转deny。ApprovalGate对拒绝、缺grant、过期/签名/范围不匹配抛对应 approval错误，single-use grant消费后不可复用。

## 当前结构问题

policy decision、approval grant、trace event和audit entry各有不同敏感度与持久化目的；新增constraint或scope字段时必须同步matching、签名/audit与执行器消费。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
