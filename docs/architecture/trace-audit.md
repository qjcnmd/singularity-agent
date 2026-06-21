# Trace And Audit Contract

Trace and audit are the evidence layer for local recovery, debugging, desktop rendering, and evaluation. They are not a raw data dump.

## Trace Records

Current durable objects:

- `TraceEvent`
- `TraceSpan`
- `TraceArtifact`
- policy audit entries
- legacy `TraceWriter` events for compatibility paths

Every event should include identity ids, runtime name, event type, timestamp, severity, summary, payload hash when available, redaction flag, and related artifact refs.

## Spans

Spans represent duration:

```text
running -> success|failed|cancelled|timeout|skipped|blocked
```

Crash recovery must repair incomplete spans or mark them clearly. A desktop timeline must not infer success from an unclosed span.

## Artifacts

Artifacts store large or sensitive data behind refs:

- stdout/stderr/command logs
- diffs
- reports
- snapshots
- sandbox and verification evidence
- model messages and prompt manifests
- policy audit refs

Artifact contract:

- artifact id is the external handle
- internal absolute path is private
- sha256 and size are recorded
- sensitive artifacts are redacted before write
- metadata contains summaries, not raw secrets

## Redaction

Redaction happens before storage and rendering. It applies to:

- API keys and tokens
- cookies and auth headers
- `.env` content
- secret-like command output
- raw tool arguments and results
- raw provider payloads
- absolute paths that expose private internals when a relative handle is enough

When redaction removes content, the record should retain a stable digest or artifact ref so debugging can still correlate events.

## Secret Handling

Secrets are not valid long-term memory, context, trace, or documentation payloads.

Rules:

- do not store API keys as CLI flags
- do not render secret content into model context
- do not write raw secret values into artifacts
- do not include secret values in policy audit
- do not store secret values in memory entries

If a runtime needs to mention a secret, it should use a class name such as `api_key`, a digest, or a redacted placeholder.

## Audit Boundaries

Policy audit:

- records decisions and grants
- is append-only
- is local-first
- must be enough to explain why execution happened or did not happen

Workspace audit:

- records ownership and before/after hashes
- separates user-owned changes, agent mutations, command side effects, and generated artifacts

Tool audit:

- records validated call shape, policy decision ids, handler outcome, redaction/truncation flags, and result binding ids

## Desktop Rendering

Desktop should render trace through RuntimeHost event/state APIs:

- timeline from events and spans
- artifacts by artifact ref
- approvals from policy decisions
- health and recovery from final report summaries

Desktop must not depend on private trace directory layout as its primary API.
