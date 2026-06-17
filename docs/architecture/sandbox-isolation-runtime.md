# Sandbox / Isolation Runtime

Miniharness v0.0.12 adds a local Sandbox Runtime. It turns policy outcomes such as `sandbox_required` and `PolicyConstraints.sandbox_required=True` into an actual isolated execution path instead of treating sandbox as a placeholder.

The compact boundary is:

```txt
PolicyRuntime
  -> sandbox_required decision and constraints
  -> CommandRuntime
  -> SandboxRuntime
  -> LocalStagingBackend
  -> copy-on-write workspace
  -> filtered environment
  -> resource and output limits
  -> artifact capture and change detection
  -> sandbox trace
  -> CommandResult / VerificationEvidence / Planner evidence / FinalReport
```

This slice does not implement Git Runtime, Docker, Podman, WSL, remote approval, persistent sandbox sessions, hard network isolation, hard memory limits, or automatic import of sandbox changes.

## Runtime Contract

`PolicyRuntime` decides whether an action needs a sandbox. `SandboxRuntime` does not re-decide safety; it selects a backend, checks capabilities, prepares the isolated workspace, runs the command, collects evidence, and fails closed when required capabilities are unavailable.

Any command marked `SANDBOX_REQUIRED` or `constraints.sandbox_required=True` must not run in the real workspace through `LocalProcessBackend`.

Sandbox results are structural:

```txt
success
failed
timeout
policy_blocked
violation
backend_unavailable
setup_failed
cleanup_failed
```

`backend_unavailable` means the current backend cannot enforce the required policy. It is not downgraded to normal local execution.

## LocalStagingBackend

The only implemented backend is `LocalStagingBackend`.

It guarantees:

```txt
copy-on-write workspace staging
default exclusion of .git, node_modules, venv, caches, build outputs, and harness sandboxes
cwd mapping only when the original cwd is inside the workspace
filtered and redacted environment
timeout and output preview limits
best-effort process tree cleanup
artifact capture
created/modified/deleted file detection inside the sandbox copy
append-only sandbox JSONL trace
```

It does not guarantee:

```txt
kernel-level process isolation
hard network denial
hard memory limits
hard process-count limits
filesystem isolation against malicious OS-level escape attempts
container-like security boundaries
```

Its capabilities explicitly report `network_isolation=False`, `memory_limit=False`, and `process_limit=False`. If policy requires hard network isolation or unsupported limits, the backend returns `backend_unavailable`.

## Filesystem

Each run creates a sandbox root under:

```txt
work/sandboxes/<sandbox_id>/
```

The staged workspace lives at:

```txt
work/sandboxes/<sandbox_id>/workspace/
```

Artifacts live at:

```txt
work/sandboxes/<sandbox_id>/artifacts/
```

The default copy excludes:

```txt
.git
node_modules
.venv
venv
__pycache__
.pytest_cache
.mypy_cache
.ruff_cache
dist
build
coverage
.coverage
work/sandboxes
.miniharness/sandboxes
```

The runtime records a baseline before execution and compares it afterward. Changes are returned as `SandboxChangeSummary` only. They are not written back to the real workspace. Future import must go through `MutationRuntime` and `PolicyRuntime`.

## Environment

`SandboxEnvironmentBuilder` defaults to a minimal child environment. It only inherits parent environment variables when the profile explicitly asks for inheritance.

Even with inheritance enabled, secret-like names are filtered and redacted:

```txt
*KEY*
*TOKEN*
*SECRET*
*PASSWORD*
AUTHORIZATION
COOKIE
NPM_TOKEN
GITHUB_TOKEN
OPENAI_API_KEY
ANTHROPIC_API_KEY
```

Trace and audit surfaces record redacted values only.

## Profiles

Default profiles are provided by `default_sandbox_profile(...)`:

```txt
readonly_analysis       copy-on-write, network denied, redacted env, 30s timeout
isolated_verification   copy-on-write, network denied, redacted env, 120s timeout
generated_code          empty temp workspace, network denied, redacted env, 30s timeout
package_operation       copy-on-write, network policy metadata only, 180s timeout
long_running_service    copy-on-write, network denied, timeout lease, process cleanup
```

`package_operation` does not imply hard network containment. If hard network isolation is required, `LocalStagingBackend` fails closed.

## Runtime Integration

`CommandRuntime` evaluates `PolicyRuntime` first. If the decision requires sandbox, it resolves cwd inside the real workspace, builds a `SandboxRequest`, and calls `SandboxRuntime.run(...)`. The returned `SandboxResult` is mapped into `CommandResult` with:

```txt
backend=local_staging
isolation_report.sandbox
metadata.sandbox_id
metadata.sandbox_backend
metadata.sandbox_status
metadata.sandbox_trace_id
metadata.sandbox_artifacts
metadata.sandbox_changed_files
metadata.sandbox_violations
```

`VerificationRuntime` still executes checks through `CommandRuntime`. Verification evidence now records sandbox id, backend, status, artifacts, changed files, and violations. Sandbox unavailable and sandbox violations are classified separately from test failures.

`PlannerRuntime` records sandbox observations in `EvidenceLedger.sandbox_observations`. The context renderer exposes compact summaries such as:

```txt
[sandbox] command ran in isolated copy-on-write workspace via local_staging, exit_code=0.
[sandbox] command blocked: backend cannot enforce required isolation.
```

`FinalReport` includes `sandbox_isolation_summary`:

```txt
sandboxed_commands_count
verification_commands_run_in_sandbox_count
backend_unavailable_count
sandbox_violation_count
timeout_count
artifact_count
changed_files_in_sandbox_count
imported_changes_count
```

`imported_changes_count` is currently always `0`.

## Trace

Sandbox trace is append-only JSONL:

```txt
.miniharness/sandbox/trace.jsonl
```

Each entry records sandbox id, session, task, action, backend, profile, capabilities, command summary, cwd, workspace root, sandbox root, filesystem mode, network mode, redaction flag, time/output limits, status, exit code, duration, artifacts, changed-file count, violations, cleanup status, and policy decision id.

Sensitive values are redacted before trace write.

## Tests

Focused coverage lives in:

```txt
tests/test_sandbox_models.py
tests/test_sandbox_environment.py
tests/test_sandbox_filesystem.py
tests/test_sandbox_backend_local.py
tests/test_sandbox_runtime.py
tests/test_sandbox_integration.py
```

Run:

```powershell
python -m pytest tests -q --basetemp work/pytest-tmp
python -m compileall -q src tests
```
