# Tool Protocol Contract

All model tool calls flow through `ToolProtocolEngine` and `ToolExecutor`. This contract is the stable bridge for future desktop resume, approval, and replay behavior.

## Input

The protocol receives:

- model request id
- model response id
- assistant message id
- allowed tool names
- raw OpenAI-style tool calls
- parsed/normalized arguments when available
- run/session/task/phase/action ids

The protocol stores a `ToolCallEnvelope` before execution.

## Lifecycle

```text
proposed
-> validated
-> scheduled
-> running
-> succeeded|failed|cancelled
-> result_appended
```

Approval path:

```text
validated -> waiting_approval -> approved -> scheduled
```

Recovery path:

```text
waiting_approval|running|succeeded -> recovered
```

## Invalid Calls

Invalid tool calls never disappear. They become protocol records and synthetic tool results so the model receives a valid tool-response shape.

Invalid cases include:

- missing tool call id
- duplicate tool call id
- unknown tool
- disallowed tool
- invalid JSON
- arguments not an object
- schema mismatch
- protocol violation
- policy denied
- approval denied
- sandbox required at the wrong layer
- result binding failure

## Scheduling

Default execution is sequential. Read-only parallel execution is allowed only when:

- provider capabilities allow parallel tool calls
- every tool declares read-only side effects
- every tool is idempotent
- each call has passed protocol validation and replay checks
- the scheduler can preserve deterministic result binding

Side-effect tools are not parallelized. Mutation, command, verification, approval-required, unknown, and non-idempotent tools stay sequential.

When a batch is scheduled as `parallel_readonly`, `ParallelToolExecutor` runs the read-only handlers concurrently and `ToolProtocolEngine` binds and appends results in the original tool-call order.

## Pending Approval

When policy requires local review:

- the tool record moves to `waiting_approval`
- the turn result is `pending_approval`
- the component reports `pending_approval_count`
- next action is `resume_pending_approval`
- no handler runs before a scoped grant exists

Desktop must display approval from the policy requirement, not from model prose. Grant submission goes to AgentHost and is consumed by `PolicyEngine`.

## Replay

Replay detection compares tool call id, normalized arguments digest, tool schema hash, side-effect kind, and idempotency contract.

Statuses:

- `read_only_replay`: safe to return previous result for idempotent read-only calls.
- `side_effect_replay`: blocked unless a future explicit idempotent side-effect contract exists.
- `conflicting_replay`: blocked when the same tool call id has different arguments or schema.

Replay output must use prior result bindings and artifact refs, not rerun the handler silently.

## Resume

Resume must load:

- batches
- tool call records
- result bindings
- pending approvals
- succeeded-but-not-appended calls
- assistant messages missing tool messages

The component may append missing safe results, but it must not rerun side effects without an explicit replay contract.

## Strict Mode

Strict mode tightens:

- rendered tool schemas
- `additionalProperties: false`
- protocol validation
- redaction expectations
- invalid assistant output handling

Strict mode does not authorize extra side effects.

## Result Binding

Every result envelope must include:

- tool call id
- tool name
- ok/status
- error code/kind when failed
- content preview
- content digest
- artifact refs when output is external
- policy decision id when applicable
- approval grant id when applicable
- redacted/truncated flags

Raw handler results are not context messages. Context receives `ToolProtocolResultEnvelope.to_context_message()`.
