# Policy / Approval Runtime

Miniharness v0.0.11 adds a unified local `PolicyRuntime`. It is the permission, risk, approval, and audit boundary for runtime actions. The model and planner can propose actions, but they do not own execution permission.

The compact boundary is:

```txt
ToolRuntime / MutationRuntime / CommandRuntime / VerificationRuntime
  -> PolicyRequest
  -> RiskClassifier
  -> DefaultLocalPolicyRules
  -> PolicyDecision
  -> ApprovalGate / ApprovalGrant when local CLI review is needed
  -> PolicyAuditWriter
  -> runtime result or planner policy observation
  -> ContextManager compact summary
  -> FinalReport Policy & Approval Summary
```

Git policy, remote approval, persistent approval profiles, and container sandbox backends are intentionally not implemented in this slice. Miniharness v0.0.12 adds a local staging sandbox backend, but it is not a Docker/Podman/WSL or kernel-level security boundary.

## Core Objects

`PolicyRequest` is built before a runtime reads or writes files, runs commands, accesses network, changes config, starts long-lived processes, or executes verification. It includes session, task, phase, action, runtime name, operation kind, capability, subject, resource, reason, risk hints, metadata, reversibility, network/workspace/secret/destructive flags, and workspace root.

`PolicyDecision` is the runtime answer. Its outcome can be:

```txt
allow, deny, require_review, ask_user, escalate, sandbox_required
```

It also records risk level, risk tags, user-facing reason, constraints, rule ids, optional approval requirement, audit severity, and compact context summary.

`ApprovalGrant` is a scoped approval, not a boolean. The current CLI gate only creates single-use, session-only grants from a local user action. The grant scope contains capabilities, path globs, command patterns, network hosts, duration limits, file limits, and single-use/session-only flags. Model text such as "the user approved" is never accepted as approval.
Grant consumption is owned by `PolicyRuntime`, so a local approval is converted into one audited allow decision without re-running the same policy request through risk classification and rules.

`PolicyAuditWriter` writes append-only JSONL to:

```txt
.miniharness/policy/audit.jsonl
```

The audit log redacts token, API key, Authorization, password, and secret-like content before writing.

## Risk Classification

`RiskClassifier` handles file, command, package, network, verification, generated-code, and long-running risk.

File examples:

```txt
workspace source read -> low
.env / token / credential path -> high
id_rsa / browser profile / system key area -> critical
outside-workspace path -> high
delete inside workspace -> high
delete outside workspace -> critical
```

Command examples:

```txt
pytest / npm test / pnpm test -> medium, executes project code
npm install / pip install / uv pip install -> high, package manager, network, supply chain
rm -rf / del / rmdir / Remove-Item -Recurse -> critical
curl ... | sh -> critical
PowerShell EncodedCommand / sudo / runas -> denied or escalated
npm run dev / vite / python -m http.server -> long running
```

Verification commands are still policy-controlled. They are not allowed merely because the runtime label says "verification".

## Default Local Rules

The default config is local and conservative:

```txt
approval_mode = interactive
workspace reads = allow
workspace mutation = require_review
command execution = require_review
verification command execution = require_review
network = require_review
package install = require_review
secret access = deny by default for highly sensitive paths
outside-workspace write/delete = deny
non_interactive review = fail closed
```

`PolicyConfig.runtime_default(...)` uses `auto_safe` for existing local runtime compatibility. It still routes every action through `PolicyRuntime`, but can allow low-risk runtime-owned operations used by current tests and CLI flows. High-risk package/network/delete/config/secret actions remain reviewed or denied.

Hard-deny rules run before review rules:

```txt
outside-workspace write/delete
secret plus network risk
private key or browser cookie reads
remote script piped to interpreter
encoded shell commands
critical destructive commands
```

`SANDBOX_REQUIRED` is returned for generated-code execution and verification command execution. `CommandRuntime` maps that decision into `SandboxRuntime` and must not fall back to normal local process execution. The current backend is `LocalStagingBackend`, which provides copy-on-write workspace staging, env filtering, timeout/output limits, artifact capture, change detection, and trace. It fails closed for hard network isolation or unsupported memory/process limits.

## Runtime Integration

`ToolRuntime` constructs a request before dispatch. It maps read tools to workspace-read capabilities, mutation tools to mutation/create/delete capabilities, command tools to command or process capabilities, and verification tools to project-code verification capabilities. A denied or review-required decision becomes a structured `ToolResult.failure(...)`.

`MutationRuntime` constructs a request after a `ChangeSet` is built and current snapshots are checked, but before atomic writes. Metadata includes diff summary, changed files, created files, deleted files, reversibility, transaction id, and changeset id. Non-allow decisions become structured `MutationResult` values.

`CommandRuntime` constructs a request before process spawn or long-running process start. Metadata includes command preview, cwd, env policy, network policy, filesystem mode, timeout, long-running flag, and risk acceptance reason. Non-allow decisions become `CommandResult` / `ProcessSession` policy-blocked results.

`VerificationRuntime` constructs a verification-scoped request before each executable check, then still calls `CommandRuntime.run(...)`. This keeps both verification policy and command policy in the execution path. When policy requires sandboxing, the actual process is executed by `SandboxRuntime`, not by the bare local process backend.

`PlannerRuntime` records compact policy observations into `EvidenceLedger.policy_observations`. The context renderer exposes summaries such as:

```txt
[policy] Command denied: package install requires review but session is non-interactive.
```

`FinalReport` includes `policy_approval_summary` with allowed low-risk action count, reviewed action count, denied action count, sandbox-required count, user-approved actions, high-risk commands, and skipped actions due to policy. It also includes `sandbox_isolation_summary` from sandbox execution evidence.

## Failure Handling

Policy and approval failures are represented with explicit exception types:

```txt
PolicyError
PolicyDenied
ApprovalRequired
ApprovalDenied
SandboxRequired
PolicyEscalationRequired
PolicyAskUserRequired
```

Runtime entrypoints convert policy decisions into structured runtime results. They should not leak unhandled policy exceptions to the CLI loop.

## Extension Points

The first implementation keeps rules centralized in `DefaultLocalPolicyRules`, but the shape supports future layers:

```txt
project policy file
user policy file
session policy
persistent approval profile
container or VM sandbox backend
remote approval workflow
GitRuntime-specific policy
```

Those are reserved extension points, not hidden behavior in this release.
