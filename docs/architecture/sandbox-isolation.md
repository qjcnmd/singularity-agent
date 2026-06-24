# Sandbox Isolation

Singularity v0.0.12 adds a local Sandbox Management. It turns policy outcomes such as `sandbox_required` and `PolicyConstraints.sandbox_required=True` into an actual isolated execution path instead of treating sandbox as a placeholder.

The compact boundary is:

```txt
PolicyEngine
  -> sandbox_required decision and constraints
  -> CommandExecutor
  -> SandboxManager
  -> DockerSandboxBackend when available and required
  -> LocalStagingBackend
  -> copy-on-write workspace
  -> filtered environment
  -> resource and output limits
  -> artifact capture and change detection
  -> sandbox trace
  -> CommandResult / VerificationEvidence / Planner evidence / FinalReport
```

Docker is the preferred hard-isolation backend when the Docker CLI and daemon are available. Docker is not a required development dependency: if it is unavailable, Singularity keeps `LocalStagingBackend` for copy-on-write execution. If policy requires hard network isolation, memory limits, or process limits and no capable backend is available, the component returns `backend_unavailable` and does not fall back to real local execution.

This slice does not implement GitClient, Podman, WSL, remote approval, persistent sandbox sessions, host allowlist enforcement inside Docker, or automatic import of sandbox changes.

## Component Contract

`PolicyEngine` decides whether an action needs a sandbox. `SandboxManager` does not re-decide safety; it selects a backend, checks capabilities, prepares the isolated workspace, runs the command, collects evidence, and fails closed when required capabilities are unavailable.

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

At task boot, `SandboxManager.capability_summary()` writes a snapshot into `TaskState.sandbox_capability`:

```txt
hard_isolation
soft_workspace_isolation
no_isolation
network_blocked
write_scope
approval_mode
available_backends
capabilities
```

On a local-only machine this snapshot must show `soft_workspace_isolation=True` and `hard_isolation=False` unless Docker is available. That evidence is the source for user-facing sandbox claims; local staging must not be described as container-grade isolation.

## Backend Selection

`SandboxManager` builds the default backend list as:

```txt
DockerSandboxBackend, LocalStagingBackend   when docker info succeeds
LocalStagingBackend                         when Docker CLI or daemon is unavailable
```

For each request, the component picks the first available backend whose declared `SandboxCapabilities` satisfy the request profile. Capability mismatches are fail-closed: unsupported hard network isolation, memory limits, or process limits return `backend_unavailable`.

## DockerSandboxBackend

`DockerSandboxBackend` reuses the same staged workspace, redacted environment, timeout, output limit, artifact capture, and change detection contract as `LocalStagingBackend`. It runs `docker run --rm` with the staged workspace mounted at `/workspace`. Network is set to `--network none` unless the sandbox profile explicitly allows network. Memory and process limits are mapped to Docker CLI flags when requested.

Docker output, artifacts, and changed files are collected from the staged workspace only. Sandbox changes are not imported into the real workspace.

Real Docker integration tests are skipped when the Docker CLI or daemon is unavailable; mock CLI tests still cover command construction and fail-closed behavior.

## LocalStagingBackend

`LocalStagingBackend` remains the default fallback backend.

It guarantees:

```txt
copy-on-write workspace staging
default exclusion of .git, node_modules, venv, caches, build outputs, and sandbox workdirs
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

Its capabilities explicitly report `network_isolation=False`, `memory_limit=False`, and `process_limit=False`. If policy requires hard network isolation or unsupported limits and Docker is unavailable, the component returns `backend_unavailable`.

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
.singularity/sandboxes
```

The component records a baseline before execution and compares it afterward. Changes are returned as `SandboxChangeSummary` only. They are not written back to the real workspace. Future import must go through `WorkspaceMutationManager` and `PolicyEngine`.

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

## Component Integration

`CommandExecutor` evaluates `PolicyEngine` first. If the decision requires sandbox, it resolves cwd inside the real workspace, builds a `SandboxRequest`, and calls `SandboxManager.run(...)`. The returned `SandboxResult` is mapped into `CommandResult` with:

```txt
backend=docker | local_staging
isolation_report.sandbox
metadata.sandbox_id
metadata.sandbox_backend
metadata.sandbox_status
metadata.sandbox_trace_id
metadata.sandbox_artifacts
metadata.sandbox_changed_files
metadata.sandbox_violations
```

`VerificationRunner` still executes checks through `CommandExecutor`. Verification evidence now records sandbox id, backend, status, artifacts, changed files, and violations. Sandbox unavailable and sandbox violations are classified separately from test failures.

`Planner` records sandbox observations in `EvidenceLedger.sandbox_observations`. The context renderer exposes compact summaries such as:

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
.singularity/sandbox/trace.jsonl
```

Each entry records sandbox id, session, task, action, backend, profile, capabilities, command summary, cwd handle, workspace handle, sandbox handle, filesystem mode, network mode, redaction flag, time/output limits, status, exit code, duration, artifacts, changed-file count, violations, cleanup status, and policy decision id. Absolute sandbox paths stay internal to the backend.

Sensitive values are redacted before trace write.

## Tests

Focused coverage lives in:

```txt
tests/test_sandbox_models.py
tests/test_sandbox_environment.py
tests/test_sandbox_filesystem.py
tests/test_sandbox_backend_local.py
tests/test_sandbox_backend_docker.py
tests/test_sandbox_manager.py
tests/test_sandbox_integration.py
```

Run:

```powershell
python -m pytest tests -q --basetemp work/pytest-tmp
python -m compileall -q src tests
```
