# Command Runtime

Singularity routes process execution through a Command Runtime. A shell is not a normal tool: it can execute project code, mutate the workspace, leak environment secrets, access the network, start background processes, and leave child processes behind. The runtime makes those effects explicit before a command is started and keeps the result auditable afterward.

The compact production boundary is:

```txt
ToolRuntime
  -> registered command tool
  -> CommandRuntime
  -> CommandPlan
  -> CommandPolicy
  -> PolicyRuntime sandbox check
  -> EnvPolicy + ResourceLimits + OutputCollector
  -> ExecutionBackend or SandboxRuntime
  -> ProcessSupervisor
  -> workspace side-effect tracking
  -> trace + context observation
```

Tool handlers are not allowed to spawn processes directly. `ToolRuntime` rejects any `SHELL` tool unless its `ToolSpec` declares `uses_command_runtime=true`.

## Why Shell Is Not A Normal Tool

Most read-only tools have a bounded input/output shape. Shell does not. Even a short command string can expand through shell parsing, execute arbitrary binaries, read secrets from inherited env, write files, download code, run test suites, or start a server that survives the tool call. Treating shell as a generic function tool would hide the highest-risk behavior behind a plain string.

`CommandRequest` therefore supports structured `argv` first. `shell` is still represented, but it is classified as high-risk and defaults to `require_review`. This keeps future support possible without making stringly shell execution the default agent path.

## Runtime Boundaries

`ToolRuntime` owns tool registration, schema validation, tool policy, handler timeout, and compact `ToolResult` wrapping. It only lets command tools enter the command layer when `uses_command_runtime=true`.

`CommandRuntime` owns command planning, command policy, env policy, network/filesystem declarations, process execution, resource limits, output collection, side-effect tracking, command observations, and command audit events.

`MutationRuntime` owns model-authored file edits. Command-generated changes are tracked as command side effects and are not mixed with model apply operations or mutation journals.

`VerificationRuntime` decides what checks to run, while CommandRuntime runs the approved command and returns `CommandResult` with semantic statuses such as `tests_failed`, `build_failed`, `lint_failed`, and `typecheck_failed`. Direct `run_command` tool calls reject verification-like commands with `verification_runtime_required`; this keeps tests, lint, typecheck, builds, and syntax checks on the verification planning path instead of ad-hoc shell execution.

When `PolicyRuntime` returns `sandbox_required`, CommandRuntime calls `SandboxRuntime` and does not spawn the command through `LocalProcessBackend`.

`GitRuntime` is implemented as a local-only status/diff/commit wrapper. It invokes the configured git executable directly, never executes user-provided shell command strings, never pushes or pulls, and stages only explicit paths scoped to the configured workspace root.

## Request And Plan

`CommandRequest` is the user-facing execution request:

```txt
command_id
argv
shell
cwd
purpose
timeout_seconds
idle_timeout_seconds
env_request
network_mode
filesystem_mode
resource_limits
expected_outputs
risk_acceptance_reason
```

`cwd` is resolved inside the workspace before execution. If it escapes the root, the command returns `cwd_outside_workspace` or `cwd_denied` and no process starts.

`CommandPlan` is the pre-execution plan. It contains the request, policy decision, resolved cwd, backend, allowed and denied env names, network/filesystem modes, resource limits, and isolation report.

## Policy

`CommandPolicy` returns:

```txt
allow | require_review | deny
```

It also returns reasons, risk tags, required backend, required network mode, required filesystem mode, and redaction rules.

Risk classification uses the whole request, not just the executable name. It considers argv, cwd, shell usage, requested network mode, filesystem mode, env request, and purpose. Risk tags include:

```txt
READ_ONLY_COMMAND
PROJECT_VERIFICATION
FORMATTER
BUILD
CODE_GENERATION
PACKAGE_MANAGER
NETWORK
WRITE_WORKSPACE
DESTRUCTIVE
LONG_RUNNING
SECRET_RISK
VCS_READ
VCS_MUTATION
SYSTEM_MUTATION
EXECUTES_PROJECT_CODE
UNKNOWN
```

Current default behavior:

- Shell strings require review.
- Destructive and system mutation commands are denied.
- Network-risk commands are denied when `network_mode=DISABLED`.
- Package manager commands are tagged `PACKAGE_MANAGER`, `NETWORK`, and `WRITE_WORKSPACE`, and require review.
- Test commands are tagged `PROJECT_VERIFICATION` and `EXECUTES_PROJECT_CODE`.
- Git mutation commands require review; destructive git commands such as `clean` and `reset` are denied by destructive classification.
- Workspace-writing commands require an explicit `risk_acceptance_reason`, except formatter purpose, which is still tracked as a workspace write.

## Env And Secret Redaction

The local backend does not inherit the full parent environment. `EnvPolicy` allows a small inherited set such as `PATH`, `PATHEXT`, `COMSPEC`, `SYSTEMROOT`, `TEMP`, `TMP`, `HOME`, and `USERPROFILE`.

Requested env keys matching secret patterns are denied:

```txt
*_TOKEN
*_KEY
*_SECRET
PASSWORD
DATABASE_URL
AWS_*
GITHUB_TOKEN
OPENAI_API_KEY
```

Denied values are added to the redactor. Stdout and stderr are redacted before they enter trace, observations, previews, or artifacts. The trace records redaction rules, denied env keys, and redaction count, not secret values.

## Network And Filesystem Modes

Network modes:

```txt
DISABLED
ALLOW_PACKAGE_REGISTRIES
ALLOW_GIT_HOSTS
ALLOW_ALL
```

Filesystem modes:

```txt
READ_ONLY_WORKSPACE
READ_WRITE_WORKSPACE
READ_WRITE_SELECTED_PATHS
EPHEMERAL_WORKDIR
NO_HOME_ACCESS
CACHE_MOUNT
```

The current `LocalProcessBackend` cannot enforce real network or filesystem isolation. Results and trace therefore report:

```txt
network_isolation_enforced=false
filesystem_isolation=workspace_cwd_advisory
home_access_blocked=false
```

This is intentional. Unsupported isolation is explicit in `isolation_report` instead of being implied.

Commands that require sandboxing use `LocalStagingBackend` instead. That backend copies the workspace into `work/sandboxes/<sandbox_id>/workspace`, filters env, runs there, captures artifacts and changes, then cleans up. Sandbox file changes are reported as sandbox evidence only and are not written back to the real workspace.

## Execution Backends

`ExecutionBackend` is the abstraction boundary.

`LocalProcessBackend` is implemented. It applies policy before spawn, receives a resolved workspace cwd, receives a reduced env, streams stdout/stderr into `OutputCollector`, and uses `ProcessSupervisor` for timeout and cleanup.

`LocalStagingBackend` is implemented through `SandboxRuntime`. It provides practical local copy-on-write isolation, not a hard OS security boundary. It reports `network_isolation=false`, `memory_limit=false`, and `process_limit=false`, and fails closed when policy requires those unsupported capabilities.

Future backends can implement the same interface:

```txt
ContainerBackend
RemoteIsolatedBackend
```

They should strengthen `isolation_report` rather than changing `CommandResult`.

## Process Supervision

`ProcessSupervisor` starts commands in a new process group when the platform allows it. On timeout, idle timeout, or stop, it attempts to terminate the process tree:

- Windows uses `taskkill /T /F`.
- POSIX uses process groups with `SIGTERM`, then `SIGKILL` if needed.

Results record:

```txt
signal
exit_code
killed_reason
timed_out
idle_timed_out
```

Non-zero exits are not internal errors. They become `exit_nonzero` or a semantic failure depending on command purpose.

## Resource Limits

Supported today:

```txt
timeout_seconds
idle_timeout_seconds
max_stdout_bytes
max_stderr_bytes
max_combined_output_bytes
```

Reserved interfaces:

```txt
max_memory_mb
max_processes
max_disk_write_mb
```

The local backend reports reserved limits under `resource_limits_unsupported` when they are requested.

## Output Handling

`OutputCollector` streams stdout and stderr separately. It also keeps combined ordered output for model-facing diagnostics.

The collector:

- Redacts secrets before storage.
- Tracks stdout and stderr byte counts.
- Truncates stdout, stderr, and combined previews independently.
- Marks `output_truncated`.
- Computes an output digest.
- Saves large output artifacts under `.singularity/artifacts/commands/`.

Context Manager receives a compact `command_result` observation with status, exit code, semantic status, duration, summary, key output, changed files, truncation state, artifact path, and error code. Full stdout/stderr are not appended to messages.

## Long-Running Sessions

Long-running commands use process sessions instead of blocking `run_command`.

Registered tools:

```txt
start_process
read_process_output
stop_process
list_processes
```

`ProcessSession` records process id, OS pid, argv/shell, cwd, status, ports, health check placeholder, logs artifact, and owner transaction. Sessions can be read, listed, stopped, and cleaned up.

This is the path for dev servers such as Vite, `npm run dev`, Uvicorn, Django runserver, or similar commands.

## Workspace Side Effects

The command runtime captures a workspace snapshot before and after execution. It skips VCS internals, runtime artifacts, caches, virtual environments, `node_modules`, and bytecode directories.

`CommandResult.changed_files` reports files created, removed, or changed by the command. These changes are command side effects, not agent-authored mutation transactions.

This separation matters because a formatter, package manager, test, or code generator can alter files without passing through `MutationRuntime`.

## Git Awareness

Command trace includes lightweight git summaries before and after execution. The current implementation reads `.git/HEAD` and branch refs without running extra git commands from inside the trace path. It does not claim full dirty-file status there.

`GitRuntime` owns local Git status, diff statistics, scoped staging, and local commits. CommandRuntime trace git summaries are only command side-effect context, not a replacement for GitRuntime.

## Trace Fields

Each command audit event records:

```txt
command_id
tool_call_id
transaction_id
argv
shell
cwd
backend
sandbox_id
policy_decision
policy_reasons
risk_tags
env_policy
network_mode
filesystem_mode
resource_limits
started_at
ended_at
duration_ms
exit_code
signal
stdout_bytes
stderr_bytes
output_digest
artifact_path
changed_files
secret_redactions
error_code
semantic_status
isolation_report
git_before
git_after
```

## Error Taxonomy

Command error codes are part of the runtime contract:

```txt
command_parse_error
command_not_found
cwd_outside_workspace
cwd_denied
policy_denied
review_required
sandbox_unavailable
backend_error
env_denied
network_denied
timeout
idle_timeout
output_limit_exceeded
memory_limit_exceeded
disk_limit_exceeded
process_killed
spawn_failed
permission_error
exit_nonzero
semantic_failure
secret_redacted
internal_error
```

Not every reserved code is emitted by the current compact implementation, but the names are reserved for stable handling.

## Verification

The test suite covers:

```txt
argv execution and structured CommandResult
CommandPlan generation
shell strings requiring review
cwd escape denial
secret env denial and output redaction
separate stdout/stderr collection
large output truncation and artifact creation
timeout cleanup
idle timeout cleanup
non-zero exit classification
test command semantic failure
pytest and package manager risk classification
destructive command denial
long-running process start/read/list/stop
workspace side-effect tracking
compact command_result context observation
structured command audit trace
ToolRuntime command tool registration path
```

Run:

```powershell
.\.venv\Scripts\python.exe -m pytest tests -q --basetemp work/pytest-tmp
```
