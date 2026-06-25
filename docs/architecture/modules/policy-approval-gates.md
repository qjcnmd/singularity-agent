# Policy / Approval / Permission Gate Runtime Flow

Runtime flow doc id: policy-approval-gates
Source paths:
- src/singularity/policy/models.py
- src/singularity/policy/engine.py
- src/singularity/policy/approval.py
- src/singularity/policy/audit.py
- src/singularity/tools/executor.py
- src/singularity/planner/engine.py

Symbols:
- PolicyRequest
- PolicyDecision
- ApprovalRequirement
- ApprovalGrant
- PolicyAuditEntry
- PolicyEngine
- PolicyEngine.evaluate
- PolicyEngine.enforce
- ApprovalGate
- ApprovalGate.resolve
- ApprovalGate.find_matching_grant
- ApprovalGate.consume_matching_grant
- PolicyAuditWriter
- PolicyAuditWriter.append
- ToolExecutor
- Planner
- Planner.authorize_tool_call
- Planner.record_policy_observation

## Module Boundary

This module owns runtime permission decisions before side effects execute.

It is responsible for building and evaluating `PolicyRequest`, classifying risk, applying local rules, writing audit entries, resolving approval requirements, consuming matching approval grants, and surfacing denial/approval/sandbox outcomes back to tool execution and planner state.

It is not responsible for provider tool schema exposure or for executing the handler after an action is allowed.

## Current Source Locations

- `src/singularity/policy/models.py`: `PolicyRequest`, `PolicyDecision`, `ApprovalRequirement`, `ApprovalGrant`, `PolicyAuditEntry`.
- `src/singularity/policy/engine.py`: `PolicyEngine.evaluate()` and `enforce()`.
- `src/singularity/policy/approval.py`: `ApprovalGate.resolve()` and grant persistence.
- `src/singularity/policy/audit.py`: JSONL audit writer and redaction.
- `src/singularity/tools/executor.py`: `_enforce_policy()` builds request and turns outcomes into tool failures or grants.
- `src/singularity/planner/engine.py`: `Planner.authorize_tool_call()` and `record_policy_observation()`.

## Runtime Call Chain

1. `ToolExecutor.execute_request()` resolves `ToolSpec` and validates arguments.
2. It builds `PolicyRequest` from tool name, permission shape, resource refs, run/session/task/phase/action ids, risk tags, and metadata.
3. `PolicyEngine.enforce()` calls `_decide()`.
4. `_decide()` emits `POLICY_REQUESTED`, classifies risk, and calls default local policy rules.
5. `PolicyEngine.enforce()` writes audit via `PolicyAuditWriter.append()` and emits policy trace.
6. If outcome is `REQUIRE_REVIEW` in non-interactive mode, the engine converts it to `DENY`.
7. `ToolExecutor` checks `ApprovalGate.find_matching_grant()` or `consume_matching_grant()` when applicable.
8. Without a grant, `ApprovalGate.resolve()` either returns a new `ApprovalGrant` or raises `PolicyDenied`, `ApprovalRequired`, `PolicyAskUserRequired`, `PolicyEscalationRequired`, `SandboxRequired`, or `ApprovalDenied`.
9. `ToolExecutor` converts blocked outcomes into `ToolResult.failure()`.
10. `Planner.record_policy_observation()` can store policy observations into planner evidence and context.
11. `Planner.authorize_tool_call()` applies phase, repair-contract, benchmark, user-constraint, and risk gates after policy admission.

## Runtime Objects Passed

- `PolicyRequest`: `request_id`, `session_id`, `task_id`, `phase_id`, `action_id`, `component`, `operation`, `capability`, `subject`, `resource`, `reason`, `proposed_by_model`, `risk_tags`, `metadata`, `evidence_refs`, `reversible`, `requires_network`, `touches_workspace`, `touches_secrets`, `destructive`, `long_running`, `interactive`, `workspace_root`.
- `PolicyDecision`: `decision_id`, `request_id`, `outcome`, `reason`, `risk_level`, `risk_tags`, `user_message`, `constraints`, `required_approval`, `rule_ids`, `audit_severity`, `context_summary`, `approval_grant_id`.
- `ApprovalRequirement`: `message`, `scope`, `review_kind`, `details`.
- `ApprovalGrant`: `grant_id`, `decision_id`, `request_id`, `approved_by`, `scope`, `session_id`, `approved_at`, `expires_at`, `single_use`, `consumed`, `reason`.
- `PolicyAuditEntry`: timestamp and normalized request/decision summary for audit logs.

## Model-Visible Objects (模型实际可见对象)

The model does not receive full `PolicyRequest`, `PolicyDecision`, `ApprovalGrant`, or `PolicyAuditEntry` objects.

The model can indirectly see policy outcomes only when they are rendered into context as:

- tool result failure payloads such as `policy_denied`, `approval_required`, `sandbox_required`, `policy_ask_user_required`, or `policy_escalation_required`;
- bounded planner/context observations generated from `Planner.record_policy_observation()`;
- prompt guidance that tells the model to use the exposed tools and not bypass policy.

Policy resource identifiers are redacted before trace/audit payloads are emitted.

## Internal Trace Debug Audit Objects (内部 trace/debug/audit 对象)

Internal-only data includes:

- complete `PolicyRequest.metadata`, including resource refs and arguments before redaction;
- `PolicyDecision.required_approval`, constraints, rule ids, audit severity, and approval grant id;
- `PolicyAuditEntry` JSONL rows;
- approval grants persisted under the configured policy path;
- trace event ids, policy decision ids, approval grant ids, and redacted resource identifiers;
- `ApprovalGate` interaction prompts and user decisions.

## State Transitions And Failure Paths

- `DecisionOutcome.ALLOW` continues execution.
- `DecisionOutcome.DENY` becomes a policy-denied tool failure.
- `DecisionOutcome.REQUIRE_REVIEW` may be converted to deny in non-interactive mode.
- `ApprovalGate.resolve()` can return an `ApprovalGrant`, raise an approval-required error, or raise a denial/user-input/sandbox/escalation exception.
- Matching grants can be consumed and written back as consumed.
- `Planner.authorize_tool_call()` can still deny after policy allow if the tool is not phase-allowed, violates an active repair contract, violates benchmark constraints, violates user constraints, or risk escalates.
- Policy and approval events are emitted with warning severity when blocked.

## Current Structure Assessment

The current structure is production-oriented: policy decisions are typed, audited, redacted, and separated from planner authorization. Approval grants are scoped and consumable.

The main integration risk is that there are two gates: `PolicyEngine/ApprovalGate` and `Planner.authorize_tool_call()`. They are intentionally different. Policy is permission/risk admission; planner authorization is task-phase and repair-contract admission.

## Production-Grade Target Structure

Current code has no single `PermissionGateResult` object that combines policy, approval, sandbox, and planner authorization.

A production-grade target could add a proposed combined gate envelope with:

- proposed `policy_decision_id`;
- proposed `approval_grant_id`;
- proposed `planner_action_id`;
- proposed `sandbox_requirement_id`;
- proposed `model_visible_error_code`.

This is proposed only. Current code stores these values across `PolicyDecision`, `ApprovalGrant`, `ToolResult.metadata`, planner evidence, and trace.

## Harness Usage Example

The model calls `run_command` for `python -m pytest`. `ToolExecutor` builds a `PolicyRequest` with command capability and resource identifier. Policy may allow the project-code execution with constraints or require review. If approval is required and no grant exists, `ToolResult.failure(error_code="approval_required")` is returned. The next model turn sees that tool failure, while the full decision, resource, and audit metadata remain internal.

## Maintenance Rules

Update this document when changing:

- `PolicyRequest`, `PolicyDecision`, approval, constraint, or audit models;
- `PolicyEngine.evaluate()` or `enforce()`;
- `ApprovalGate.resolve()` or grant matching/consumption;
- policy handling in `ToolExecutor`;
- planner authorization or policy observation recording;
- redaction of policy/audit/approval payloads.

## Verification

- `python scripts/verify_runtime_docs.py`
- `python -m pytest tests/test_policy_models.py tests/test_policy_engine.py tests/test_policy_audit.py tests/test_approval_gate.py tests/test_tool_executor_policy_approval.py tests/test_tool_executor_planner_authorization.py --basetemp work/pytest-tmp`

## Last Verified Against

Last verified against commit `5f2202bd8cfcc2a4e4a66c025891550e52f3556e` on 2026-06-25.
