# Policy / Approval / Permission Gate Runtime Flow

Runtime flow doc id: policy-approval-gates
Source paths:
- src/singularity/policy/models.py
- src/singularity/policy/engine.py
- src/singularity/policy/approval.py
- src/singularity/policy/audit.py
- src/singularity/policy/config.py
- src/singularity/policy/rules.py
- src/singularity/policy/remote.py
- src/singularity/policy/operator_key.py
- src/singularity/policy/ledger.py
- src/singularity/tools/executor.py
- src/singularity/planner/engine.py

Symbols:
- PolicyRequest
- PolicyDecision
- ApprovalRequirement
- ApprovalGrant
- ApprovalGrant.from_dict
- PolicyAuditEntry
- PolicyEngine
- PolicyEngine.evaluate
- PolicyEngine.enforce
- ApprovalGate
- ApprovalGate.resolve
- ApprovalGate.register_grant
- ApprovalGate.find_matching_grant
- ApprovalGate.consume_matching_grant
- ApprovalGate.consume_grant
- ApprovalGate.is_grant_consumed
- ApprovalGate.grants_store_path
- ApprovalGate.is_grant_store_trusted
- PolicyConfig
- _default_policy_home
- _targets_workspace_policy_dir
- DefaultLocalPolicyRules
- PolicyAuditWriter
- PolicyAuditWriter.append
- RemoteApprovalExchange
- RemoteApprovalExchange.import_grant
- RemoteApprovalExchange.export_grant
- RemoteApprovalExchange.register_grant
- _validate_scope_convergence
- _operator_signature_payload
- default_operator_key_path
- load_operator_key
- generate_operator_key
- sign_grant
- verify_grant_signature
- operator_fingerprint
- GrantConsumptionLedger
- GrantConsumptionLedger.is_consumed
- GrantConsumptionLedger.consume
- GrantConsumptionLedger.record_for
- GrantConsumptionLedger.all_records
- GrantConsumptionLedger.ledger_path
- GrantConsumptionRecord
- GrantConsumptionRecord.canonical_payload
- GrantConsumptionRecord.to_jsonl_line
- GrantConsumptionRecord.from_dict
- _LedgerHead
- GrantConsumptionLedgerTamperError
- GrantAlreadyConsumedError
- ToolExecutor
- Planner
- Planner.authorize_tool_call
- Planner.record_policy_observation

## Module Boundary

This module owns runtime permission decisions before side effects execute.

It is responsible for building and evaluating `PolicyRequest`, classifying risk, applying local rules, writing audit entries, resolving approval requirements, consuming matching approval grants, and surfacing denial/approval/sandbox outcomes back to tool execution and planner state.

Trust boundary: `ApprovalGrant` is an authorization declaration carrying no consumption state (the legacy `consumed: bool` field has been removed). Consumption facts live in the append-only, HMAC-chained `GrantConsumptionLedger` (`src/singularity/policy/ledger.py`), which is the single source of truth for whether a single-use grant has been consumed. Both the grant store and the consumption ledger are control-plane state persisted outside the model-writable workspace under `<policy_home>/.singularity/policy/`.

It is not responsible for provider tool schema exposure or for executing the handler after an action is allowed.

## Current Source Locations

- `src/singularity/policy/models.py`: `PolicyRequest`, `PolicyDecision`, `ApprovalRequirement`, `ApprovalGrant` (with `from_dict` deterministic id and `operator_signature` field; **no `consumed` field** — consumption state is owned by the ledger), `PolicyAuditEntry`.
- `src/singularity/policy/engine.py`: `PolicyEngine.evaluate()` and `enforce()`.
- `src/singularity/policy/approval.py`: `ApprovalGate.resolve()`, `register_grant()` (dedup by `grant_id` OR `decision_id`), `is_grant_store_trusted()`, `grants_store_path()`, `is_grant_consumed(grant_id)` (delegates to the ledger), `consume_matching_grant(request)` / `consume_grant(grant)` (pre-check `ledger.is_consumed` then delegate to `ledger.consume(...)`), and grant persistence.
- `src/singularity/policy/config.py`: `PolicyConfig` (with `operator_key_path` and `consumption_ledger_path` fields) and `_default_policy_home()` (resolves `SINGULARITY_POLICY_HOME` or `Path.home()`; default grant/audit/ledger paths live under `<policy_home>/.singularity/policy/`).
- `src/singularity/policy/rules.py`: `DefaultLocalPolicyRules._decide()` and `_targets_workspace_policy_dir()` hard-deny writes to `<workspace>/.singularity/policy/`.
- `src/singularity/policy/audit.py`: JSONL audit writer and redaction.
- `src/singularity/policy/operator_key.py`: operator key management for remote approval grant signing and ledger record signing. `default_operator_key_path()` (respects `SINGULARITY_POLICY_HOME`), `load_operator_key()`, `generate_operator_key()`, `sign_grant()` (HMAC-SHA256), `verify_grant_signature()` (constant-time compare via `hmac.compare_digest`), `operator_fingerprint()`.
- `src/singularity/policy/ledger.py`: `GrantConsumptionLedger` (append-only JSONL + HMAC-signed head pointer), `GrantConsumptionRecord` (frozen dataclass with `grant_id`/`decision_id`/`request_id`/`request_digest`/`session_id`/`consumed_at`/`previous_record_hash`/`record_hmac`), `_LedgerHead` (HMAC-signed head pointer file), `GrantConsumptionLedgerTamperError` / `GrantAlreadyConsumedError`. Records are chained via `previous_record_hash = sha256(prev_record.to_jsonl_line())` and signed via `sign_grant(record.canonical_payload(), operator_key)`. Tamper/delete/replay/rollback of any record or the head pointer is fail-closed.
- `src/singularity/policy/remote.py`: `RemoteApprovalExchange.import_grant()` (validates `operator_signature`, `request_digest`, and scope convergence; ignores explicit `grant_id`), `export_grant()` (signs grant with operator key), `register_grant()`.
- `src/singularity/tools/executor.py`: `_enforce_policy()` builds request, checks grant store trustworthiness before consuming grants, and turns outcomes into tool failures or grants.
- `src/singularity/planner/engine.py`: `Planner.authorize_tool_call()` and `record_policy_observation()`.

## Runtime Call Chain

1. `ToolExecutor.execute_request()` resolves `ToolSpec` and validates arguments.
2. It builds `PolicyRequest` from tool name, permission shape, resource refs, run/session/task/phase/action ids, risk tags, and metadata.
3. `PolicyEngine.enforce()` calls `_decide()`.
4. `_decide()` emits `POLICY_REQUESTED`, classifies risk, and calls default local policy rules. `_targets_workspace_policy_dir()` is evaluated before other allow rules: any write or command operation targeting `<workspace>/.singularity/policy/` is hard-denied with rule id `hard_deny_workspace_policy_dir_write` so the model cannot forge grants or audit entries via shell writes.
5. `PolicyEngine.enforce()` writes audit via `PolicyAuditWriter.append()` and emits policy trace. The default audit log path lives outside the workspace under `<policy_home>/.singularity/policy/audit.jsonl`.
6. If outcome is `REQUIRE_REVIEW` in non-interactive mode, the engine converts it to `DENY`.
7. `ToolExecutor` checks `ApprovalGate.is_grant_store_trusted(workspace_root)` before consuming any grant. When the configured grant store resolves inside the workspace, grants are treated as untrusted (model-forgeable) and `consume_matching_grant()` is skipped; execution falls through to `resolve()`.
8. When the store is trusted, `ToolExecutor` checks `ApprovalGate.find_matching_grant()` or `consume_matching_grant()` against grants persisted under `<policy_home>/.singularity/policy/approval_grants.jsonl`. `find_matching_grant()` skips any single-use grant whose `grant_id` is recorded as consumed in `GrantConsumptionLedger` (consistency with `consume_matching_grant`).
9. `consume_matching_grant(request)` acquires the grants file lock, reloads grants, finds a matching grant via `_find_matching_grant_unlocked`, then consults the `GrantConsumptionLedger`: if `ledger.is_consumed(grant.grant_id)` is True it returns None (refuses reconsumption); otherwise it calls `ledger.consume(grant, request=request)`, which appends a `GrantConsumptionRecord` to the append-only JSONL ledger, updates the HMAC-signed head pointer, and returns the grant. `consume_grant(grant)` follows the same path without a `PolicyRequest` (the `request_digest` falls back to `stable_hash({grant_id, decision_id, request_id, session_id})`).
10. Without a grant, `ApprovalGate.resolve()` either returns a new `ApprovalGrant` or raises `PolicyDenied`, `ApprovalRequired`, `PolicyAskUserRequired`, `PolicyEscalationRequired`, `SandboxRequired`, or `ApprovalDenied`.
11. Remote approvals flow through `RemoteApprovalExchange.import_grant()`, which first verifies the `operator_signature` (HMAC-SHA256 over the canonical grant payload using the operator key at `PolicyConfig.operator_key_path`), then validates the `request_digest` against a recomputed `stable_hash({"request", "decision"})`, checks `grant.scope` is a subset of `decision.required_approval.scope` via `_validate_scope_convergence()` across eight dimensions (capabilities/path_globs/command_patterns/network_hosts/single_use/session_only/max_duration_seconds/max_files), and ignores any explicit `grant_id` in favor of deterministic derivation, then registers the grant via `ApprovalGate.register_grant()`. `export_grant()` signs the grant payload with the operator key before writing.
12. `ApprovalGate.register_grant()` dedups by `grant_id` OR `decision_id`: a single decision can only have one active grant, so repeated imports of the same grant (which resolve to the same deterministic `grant_id` via `ApprovalGrant.from_dict`) replace the prior entry instead of appending.
13. `ToolExecutor` converts blocked outcomes into `ToolResult.failure()`.
14. `Planner.record_policy_observation()` can store policy observations into planner evidence and context.
15. `Planner.authorize_tool_call()` applies phase, repair-contract, benchmark, user-constraint, and risk gates after policy admission.

## Runtime Objects Passed

- `PolicyRequest`: `request_id`, `session_id`, `task_id`, `phase_id`, `action_id`, `component`, `operation`, `capability`, `subject`, `resource`, `reason`, `proposed_by_model`, `risk_tags`, `metadata`, `evidence_refs`, `reversible`, `requires_network`, `touches_workspace`, `touches_secrets`, `destructive`, `long_running`, `interactive`, `workspace_root`.
- `PolicyDecision`: `decision_id`, `request_id`, `outcome`, `reason`, `risk_level`, `risk_tags`, `user_message`, `constraints`, `required_approval`, `rule_ids`, `audit_severity`, `context_summary`, `approval_grant_id`.
- `ApprovalRequirement`: `message`, `scope`, `review_kind`, `details`.
- `ApprovalGrant`: `grant_id`, `decision_id`, `request_id`, `approved_by`, `scope`, `session_id`, `approved_at`, `expires_at`, `single_use`, `reason`, `operator_signature`. **No `consumed` field** — `ApprovalGrant` is a pure authorization declaration; consumption truth lives in `GrantConsumptionLedger`. `ApprovalGrant.from_dict()` generates a deterministic `grant_id` from `sha256(decision_id + ":" + request_id + ":" + approved_by)[:12]` when the payload omits `grant_id`, so repeated imports of the same grant collapse to a single entry instead of minting new random ids. Legacy `consumed` keys in persisted JSONL are silently ignored by `from_dict`. `operator_signature` is an HMAC-SHA256 hex digest produced by `sign_grant()` over the canonical grant payload; `import_grant()` ignores explicit `grant_id` values and always uses deterministic derivation.
- `PolicyAuditEntry`: timestamp and normalized request/decision summary for audit logs.
- `GrantConsumptionRecord`: frozen dataclass bound to `record_id`, `grant_id`, `decision_id`, `request_id`, `request_digest` (stable_hash of request fields, or grant fields when no request is supplied), `session_id`, `consumed_at` (ISO8601 UTC), `previous_record_hash` (SHA-256 of previous record's wire format, empty string for the first record), `record_hmac` (HMAC-SHA256 over `canonical_payload()` excluding `record_hmac`, signed with the operator key). `to_jsonl_line()` produces canonical wire format (`json.dumps(..., ensure_ascii=False, sort_keys=True, separators=(",", ":"))`).
- `GrantConsumptionLedger`: append-only JSONL consumer of `GrantConsumptionRecord` plus a sibling HMAC-signed `_LedgerHead` pointer file (`*.head.json`) recording `last_record_id`, `last_record_hash`, `record_count`, `head_hmac`. Public API: `is_consumed(grant_id)`, `consume(grant, *, request=None)`, `record_for(grant_id)`, `all_records()`, `ledger_path()`. Cross-process safety via `msvcrt.locking` (Windows) / `fcntl.flock` (POSIX).

## Model-Visible Objects (模型实际可见对象)

The model does not receive full `PolicyRequest`, `PolicyDecision`, `ApprovalGrant`, `PolicyAuditEntry`, `GrantConsumptionRecord`, or `GrantConsumptionLedger` objects. The ledger JSONL file and head pointer are never projected into model context.

The model can indirectly see policy outcomes only when they are rendered into context as:

- tool result failure payloads such as `policy_denied`, `approval_required`, `sandbox_required`, `policy_ask_user_required`, or `policy_escalation_required`;
- bounded planner/context observations generated from `Planner.record_policy_observation()`;
- prompt guidance that tells the model to use the exposed tools and not bypass policy.

Policy resource identifiers are redacted before trace/audit payloads are emitted.

## Internal Trace Debug Audit Objects (内部 trace/debug/audit 对象)

Internal-only data includes:

- complete `PolicyRequest.metadata`, including resource refs and arguments before redaction;
- `PolicyDecision.required_approval`, constraints, rule ids, audit severity, and approval grant id;
- `PolicyAuditEntry` JSONL rows persisted under `<policy_home>/.singularity/policy/audit.jsonl` (outside the workspace by default);
- approval grants persisted under `<policy_home>/.singularity/policy/approval_grants.jsonl` (outside the workspace by default; `PolicyConfig.approval_grants_path` may override it, but `ApprovalGate.is_grant_store_trusted()` will report stores inside the workspace as untrusted);
- the append-only `GrantConsumptionLedger` JSONL at `<policy_home>/.singularity/policy/grant_consumption_ledger.jsonl` plus its HMAC-signed head pointer `grant_consumption_ledger.jsonl.head.json` (outside the workspace by default; `PolicyConfig.consumption_ledger_path` may override it);
- `GrantConsumptionRecord` wire lines (canonical JSON with `record_hmac` and `previous_record_hash` chain) and the `_LedgerHead` pointer;
- `GrantConsumptionLedgerTamperError` raised on tamper/delete/replay/rollback (fail-closed);
- remote approval request/grant exchange files carrying `request_digest`, request payload, decision payload, grant payload, and `operator_signature` (HMAC-SHA256 over the canonical grant payload);
- the operator key at `<policy_home>/.singularity/policy/operator.pem` (never logged, printed, or committed; used only for HMAC signing/verification of both remote grants and ledger records);
- trace event ids, policy decision ids, approval grant ids, and redacted resource identifiers;
- `ApprovalGate` interaction prompts and user decisions.

## State Transitions And Failure Paths

- `DecisionOutcome.ALLOW` continues execution.
- `DecisionOutcome.DENY` becomes a policy-denied tool failure.
- `DecisionOutcome.REQUIRE_REVIEW` may be converted to deny in non-interactive mode.
- Write/command operations targeting `<workspace>/.singularity/policy/` are hard-denied with rule id `hard_deny_workspace_policy_dir_write` before other allow rules, so the model cannot forge grants or audit rows via shell writes.
- `ToolExecutor` skips `consume_matching_grant()` when `ApprovalGate.is_grant_store_trusted(workspace_root)` reports the grant store as inside the workspace; execution falls through to `resolve()` which fails closed without an interaction provider.
- `ApprovalGate.resolve()` can return an `ApprovalGrant`, raise an approval-required error, or raise a denial/user-input/sandbox/escalation exception.
- Matching single-use grants are consumed by appending a `GrantConsumptionRecord` to `GrantConsumptionLedger`; the grant is never marked consumed in its own dataclass (the legacy `consumed` field is gone). `find_matching_grant()` skips ledger-consumed grants so a consumed grant cannot be found again.
- `GrantConsumptionLedger` is fail-closed: tampering any field of a record breaks its `record_hmac`; deleting the last record breaks the head pointer (`record_count`/`last_record_hash` mismatch); duplicating a record line breaks the head pointer count; deleting the head pointer file or rolling back the ledger raises `GrantConsumptionLedgerTamperError`; cross-session replay is refused because `is_consumed(grant_id)` is True and `find_matching_grant()` skips the grant.
- `ApprovalGate.register_grant()` dedups by `grant_id` OR `decision_id`: a second grant sharing either value replaces the prior entry, so one decision cannot accumulate multiple active grants.
- `RemoteApprovalExchange.import_grant()` raises `ValueError` when `operator_signature` is missing or fails `verify_grant_signature()` (constant-time HMAC-SHA256 comparison), when `request_digest` is missing or does not match the recomputed `stable_hash({"request", "decision"})`, when `grant.scope` exceeds `decision.required_approval.scope` across any of the eight convergence dimensions (capabilities/path_globs/command_patterns/network_hosts/single_use/session_only/max_duration_seconds/max_files — enforced by `_validate_scope_convergence()`), or when envelope and grant `request_id`/`decision_id` disagree. Explicit `grant_id` values in the payload are always ignored in favor of deterministic derivation.
- `Planner.authorize_tool_call()` can still deny after policy allow if the tool is not phase-allowed, violates an active repair contract, violates benchmark constraints, violates user constraints, or risk escalates.
- Policy and approval events are emitted with warning severity when blocked.

## Current Structure Assessment

The current structure is production-oriented: policy decisions are typed, audited, redacted, and separated from planner authorization. Approval grants are scoped authorization declarations; consumption facts have been decoupled from `ApprovalGrant` into the append-only, HMAC-chained `GrantConsumptionLedger`, so the legacy plaintext `consumed` boolean tamper/replay vulnerability is closed.

The main integration risk is that there are two gates: `PolicyEngine/ApprovalGate` and `Planner.authorize_tool_call()`. They are intentionally different. Policy is permission/risk admission; planner authorization is task-phase and repair-contract admission.

## Production-Grade Target Structure

Current code has no single `PermissionGateResult` object that combines policy, approval, sandbox, and planner authorization.

A production-grade target could add a proposed combined gate envelope with:

- proposed `policy_decision_id`;
- proposed `approval_grant_id`;
- proposed `consumption_record_id` (from `GrantConsumptionLedger`);
- proposed `consumption_ledger_path` (resolved ledger file path for audit cross-reference);
- proposed `planner_action_id`;
- proposed `sandbox_requirement_id`;
- proposed `model_visible_error_code`.

This is proposed only. Current code stores these values across `PolicyDecision`, `ApprovalGrant`, `GrantConsumptionRecord`, `ToolResult.metadata`, planner evidence, and trace.

## Harness Usage Example

The model calls `run_command` for `python -m pytest`. `ToolExecutor` builds a `PolicyRequest` with command capability and resource identifier. Policy may allow the project-code execution with constraints or require review. If approval is required and no grant exists, `ToolResult.failure(error_code="approval_required")` is returned. The next model turn sees that tool failure, while the full decision, resource, and audit metadata remain internal.

## Maintenance Rules

Update this document when changing:

- `PolicyRequest`, `PolicyDecision`, approval, constraint, or audit models;
- `PolicyEngine.evaluate()` or `enforce()`;
- `ApprovalGate.resolve()`, `register_grant()`, grant matching/consumption, `is_grant_consumed()`, or grant store trust checks;
- `GrantConsumptionLedger` (record schema, HMAC chain, head pointer, file locking) or `GrantConsumptionRecord` fields;
- `_default_policy_home()` or default grant/audit/ledger path resolution;
- `_targets_workspace_policy_dir()` or the workspace policy dir hard-deny rule;
- `RemoteApprovalExchange.import_grant()` digest/scope/operator-signature validation or `export_grant()` signing;
- `ApprovalGrant.from_dict()` deterministic id generation or `operator_signature` field handling (including removal of legacy `consumed` keys);
- operator key management in `singularity.policy.operator_key` (path resolution, signing, verification);
- policy handling in `ToolExecutor`;
- planner authorization or policy observation recording;
- redaction of policy/audit/approval/ledger payloads.

## Verification

- `python scripts/verify_runtime_docs.py`
- `python -m pytest tests/test_policy_models.py tests/test_policy_engine.py tests/test_policy_audit.py tests/test_policy_integration.py tests/test_approval_gate.py tests/test_remote_approval.py tests/test_tool_executor_policy_approval.py tests/test_tool_executor_planner_authorization.py tests/test_security_regression.py tests/test_grant_consumption_ledger.py --basetemp work/pytest-tmp`

## Last Verified Against

Last verified against commit (pending) on 2026-06-26 (Trust Boundary Closure: `ApprovalGrant.consumed` plaintext field removed; consumption truth decoupled into append-only `GrantConsumptionLedger` (`src/singularity/policy/ledger.py`) — `GrantConsumptionRecord` chained via `previous_record_hash` (SHA-256 of prior wire format) and signed via `record_hmac` (HMAC-SHA256 over `canonical_payload()` with operator key), HMAC-signed `_LedgerHead` pointer file detects truncation/rollback (count/hash/id checks), cross-platform file locking (`msvcrt`/`fcntl`), `GrantConsumptionLedgerTamperError` fail-closed on tamper/delete/replay/rollback/cross-session replay; `ApprovalGate.find_matching_grant()` skips ledger-consumed grants; `consume_matching_grant`/`consume_grant` delegate to `ledger.consume(...)`; `test_tampered_consumed_field_cannot_be_reconsumed` xfail-strict removed and now passes; 6 new ledger tamper/replay/rollback/cross-session tests + 10 dedicated `test_grant_consumption_ledger.py` unit tests).
