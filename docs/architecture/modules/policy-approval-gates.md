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
