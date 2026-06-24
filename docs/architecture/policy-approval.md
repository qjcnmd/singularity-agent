# Policy And Approval Contract

`PolicyEngine` is the single component permission decision source. `ApprovalGate` owns the approval gate: local review prompts, scoped grant storage, matching, and single-use consumption. This contract is binding for CLI, future daemon, and desktop UI.

## Decision Inputs

Every policy-controlled action must provide:

- session, task, phase, action ids
- component name
- operation kind
- capability
- subject
- resource
- reason
- risk tags
- reversibility
- network/workspace/secret/destructive flags
- workspace root when path matching is needed

## Decision Outcomes

Supported outcomes:

- `allow`
- `deny`
- `require_review`
- `ask_user`
- `escalate`
- `sandbox_required`

`allow` is the only outcome that permits handler execution without more work.

## Approval Modes

| Mode | Contract |
| --- | --- |
| `interactive` | prompt locally when review is required |
| `review_all` | route meaningful actions through review |
| `auto_safe` | allow low-risk local operations, review or deny risky ones |
| `read_only` | allow only workspace read capabilities |
| `non_interactive` | fail closed when review or approval is required |

## Approval Grants

An approval grant is scoped, not a boolean.

Grant scope may include:

- capabilities
- path globs
- command patterns
- network hosts
- duration limit
- file count limit
- session-only flag
- single-use flag

Defaults:

- session-only
- single-use
- local user as approver

The model cannot approve its own action. Text such as "the user approved" has no policy effect.

## Dry Run

Dry-run blocks mutation, command, verification, and other side-effect handlers before they run, even when policy would otherwise allow them.

Dry-run may still produce:

- validation results
- policy decisions
- planner observations
- trace events
- synthetic blocked tool results

It must not produce real workspace side effects.

## Strict Mode

Strict mode tightens schema, protocol, and redaction expectations. It does not relax policy.

Expected behavior:

- invalid tool schemas or arguments fail earlier
- raw secret-like payloads are rejected or redacted
- protocol violations become structured failures

## Sandbox Required

When policy returns `sandbox_required`, the owning execution component must call `SandboxManager`. It must not run through a normal local process backend as a fallback.

If the requested sandbox capability is unavailable, the result is a fail-closed component failure. Unsupported hard network isolation, process limits, or memory limits are not silently ignored.

## Fail Closed

Fail closed applies when:

- policy component is missing
- approval gate is unavailable and review is required
- non-interactive mode needs approval
- sandbox is required but unavailable
- resource cannot be normalized safely
- a tool declares high-risk side effects without the owning backend contract
- a component cannot write an audit record for a controlled decision

## Audit

Every decision should have:

- decision id
- request id
- outcome
- reason
- risk level/tags
- rule ids when available
- approval requirement or grant id when applicable
- constraints
- redacted resource summary

Policy audit is append-only local state. Desktop may render summaries, but must not rewrite audit records.
