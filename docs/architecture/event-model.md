# Event Model

This event model is the subscription contract for a future local daemon and desktop UI. It is derived from current trace, lifecycle, interaction, policy, and tool protocol records, but it is not a raw trace-file API.

## Event Envelope

Every daemon-visible event uses the `run-event` schema:

```json
{
  "event_id": "evt_...",
  "event_type": "tool_protocol.call_completed",
  "schema_version": "1.0",
  "run_id": "run_...",
  "session_id": "session_...",
  "task_id": "task_...",
  "phase_id": "phase_...",
  "action_id": "action_...",
  "component": "tool_protocol",
  "severity": "info",
  "timestamp": "2026-06-21T00:00:00Z",
  "sequence": 42,
  "summary": "read_file completed",
  "payload": {},
  "artifact_refs": [],
  "redaction_applied": true
}
```

Fields are additive. Desktop clients must ignore unknown payload keys.

## Ordering

AgentHost must provide per-run monotonic `sequence` values. Trace timestamps are useful for display, but clients should order by `(run_id, sequence)` when available.

## Topics

Desktop subscriptions should support these topic groups:

- `lifecycle.*`
- `component.*`
- `planner.*`
- `context.*`
- `model.*`
- `tool_protocol.*`
- `tool.*`
- `policy.*`
- `approval.*`
- `command.*`
- `sandbox.*`
- `mutation.*`
- `edit.*`
- `review.*`
- `verification.*`
- `workspace_state.*`
- `project_index.*`
- `memory.*`
- `plugin.*`
- `evaluation.*`
- `final_report.*`

## Required Event Classes

Lifecycle:

- run/session/task started
- run completed, failed, cancelled
- shutdown started/completed
- recovery detected/completed

Component:

- boot started/completed/failed
- component initialized
- health checked
- cancellation requested

Model:

- request created
- response received
- request failed
- tool call proposed
- output rejected

Tool protocol:

- batch created
- call validated/rejected/scheduled/started/completed
- synthetic result created
- replay detected
- recovery started/completed
- result bound

Policy and approval:

- policy requested/decided/blocked
- approval requested/granted/denied
- user decision recorded

Execution:

- command requested/started/output chunk/completed/failed/timeout/killed
- sandbox requested/prepared/started/completed/violation/cleaned
- mutation proposed/transaction started/applied/failed/rollback completed
- verification plan/check/evidence/failure

State and reporting:

- context item added/rendered/compacted
- workspace state changed or health refreshed
- memory candidate/accepted/rejected
- final report section/completed

## Replay And Resume

AgentHost must support:

- initial snapshot: current session/run state plus last sequence
- event replay from `after_sequence`
- pending approval recovery
- idempotent resubscription after UI reload

If a run is recovered after crash, the first desktop-visible events must include recovery warnings and the next recommended action before any model request is resumed.

## Backpressure

Large payloads must be artifact refs, not inline events. Command output may stream as bounded chunks, but full stdout/stderr belongs in artifacts.

## Redaction

Events are display-safe by default:

- no raw API keys, tokens, cookies, `.env` content, or full secret-like output
- no raw tool args/results when sensitivity is secret
- opaque artifact refs instead of internal absolute paths

AgentHost may expose privileged local artifact reads later, but only by artifact ref and with the same redaction contract.
