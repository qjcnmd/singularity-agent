# Miniharness v0.0.10

Miniharness is a tiny CLI coding agent harness. It is intentionally small so you can see how the agent loop, provider, planner, tools, context manager, trace file, local workspace state runtime, workspace mutation runtime, command runtime, and verification runtime connect without using LangChain, LangGraph, or any other agent framework.

v0.0.10 adds the Planner / Task Execution Runtime. The model no longer gets every tool and advances from a bare loop alone: each turn goes through `PlannerRuntime`, which tracks `TaskState`, phase policy, allowed action/tool gates, evidence ledger updates, execution budget, deterministic replanning, risk escalation, resume state, completion criteria, and a factual final report built from runtime evidence.

v0.0.9 adds the Local Workspace State Runtime. Miniharness can now create a non-Git session baseline, capture rich file snapshots, persist a JSONL state journal and SQLite query index, classify ownership for agent mutations and command side effects, detect external changes, store artifacts under session directories, report workspace health, recover interrupted sessions, and perform hash-checked agent-owned rollback without branches, commits, staging, push, or PR behavior.

v0.0.8 adds the Verification Runtime. Tests, lint, typecheck, builds, syntax checks, and smoke checks are no longer treated as ad-hoc shell calls. The agent can detect project shape, discover commands, analyze changed files, build a verification plan, execute checks through `CommandRuntime`, parse failures, generate repair hints, track evidence, handle flaky reruns, and produce a `CompletionAssessment` before declaring work ready.

v0.0.7 adds the Command / Shell Execution Runtime. Test, build, formatter, package manager, dev server, read-only git, and other process execution now flow through `CommandRequest`, `CommandPlan`, `CommandPolicy`, `ExecutionBackend`, `ProcessSupervisor`, resource limits, env redaction, output artifacts, workspace side-effect tracking, command observations, and structured command trace audit events.

v0.0.6 upgrades file modification from demo-style `write_file` into a Workspace Mutation Runtime. Agent-owned changes now flow through `ChangeSet`, `MutationTransaction`, snapshots, structured policy decisions, atomic writes, diffs, journal entries, rollback checks, trace audit records, and compact context observations.

v0.0.5 added the production Context Manager slice: context assembly is token-budget aware, tool observations are persisted to SQLite with references, long histories can be compressed, and run state can be recovered after interruption.

v0.0.4 added the Tool Runtime minimal production slice: tool calls execute through `ToolSpec`, `ToolRegistry`, `ToolRuntime`, `ToolPolicy`, and structured `ToolResult` / `ToolError` objects.

## Project Structure

```txt
.
├── pyproject.toml
├── README.md
├── tests/
└── src/
    └── miniharness/
        ├── __init__.py
        ├── agent.py        # agent loop
        ├── cli.py          # Typer command entry
        ├── command/        # command runtime: policy, backend, output, process sessions
        ├── config.py       # environment variable loading
        ├── context/        # token budgets, context assembly, observations, recovery
        ├── planner/        # task state machine, action gating, evidence, replan, budget, final reports
        ├── provider.py     # OpenAI-compatible HTTP call via httpx
        ├── tools/          # tool specs, registry, runtime, policy, read-only, mutation, command, verification tools
        ├── verification/   # project detection, planning, execution, evidence, repair, completion assessment
        ├── workspace_state/ # non-Git baseline, snapshot, ownership, journal, artifact, rollback, recovery
        ├── workspace/      # mutation runtime: paths, policy, snapshots, diffs, journal, rollback
        └── trace.py        # JSONL trace writer
```

Each run creates:

```txt
.miniharness/runs/<run_id>.jsonl
.miniharness/workspace_state.sqlite3
.miniharness/planner/<session_id>/
.miniharness/sessions/<session_id>/journal.jsonl
.miniharness/sessions/<session_id>/artifacts/
```

The trace records `user_goal`, `model_request`, `model_response`, `planner`, `tool_call`, `tool_result`, `workspace_state`, `mutation`, `command`, `verification`, `final_answer`, and `error` events. Planner audit entries include task id, session id, phase, action id/kind, decision, reason, evidence refs, budget state, risk level, replan decision, and completion assessment. Tool runtime audit entries include validated arguments, permission level, risk tags, start/end timestamps, duration, status, error code, truncation status, output digest, and cache hit status. Workspace state audit entries include session id, baseline id, event id, event type, path, ownership, before/after hashes, transaction id, command id, mutation id, artifact id, timestamp, and warning/error code. Mutation audit entries include transaction and changeset ids, operation id, path, operation type, policy decision, risk tags, before/after hashes, diff digest, line counts, status flags, error code, duration, artifact path, and verification status. Command audit entries include command id, argv/shell, cwd, backend, policy decision, risk tags, env policy, network/filesystem modes, resource limits, duration, exit code, output digest, artifact path, changed files, structured side effects, redaction count, semantic status, isolation report, and lightweight git state. Verification audit entries include project profile, impact analysis, plan/check ids, policy decision, command id, parsed failures, evidence artifacts, repair hints, and completion assessment.

## Install

From this project directory:

```powershell
python -m pip install -e .
```

On Windows, if pip says the script directory is not on `PATH`, you can enable it for the current PowerShell session:

```powershell
$env:PATH = "$env:APPDATA\Python\Python313\Scripts;$env:PATH"
```

Or run the module form without changing `PATH`:

```powershell
python -m miniharness.cli "请阅读 README 并总结这个项目"
```

## Configure Environment Variables

Miniharness calls an OpenAI-compatible Chat Completions API. The base URL should usually include `/v1`.

PowerShell example:

```powershell
$env:MINIHARNESS_BASE_URL = "https://api.openai.com/v1"
$env:MINIHARNESS_API_KEY = "sk-..."
$env:MINIHARNESS_MODEL = "gpt-4.1-mini"
```

For a local OpenAI-compatible server:

```powershell
$env:MINIHARNESS_BASE_URL = "http://localhost:8000/v1"
$env:MINIHARNESS_API_KEY = "local-key"
$env:MINIHARNESS_MODEL = "your-model"
```

The `.env` file is only loaded automatically by VSCode's `launch.json` debug configuration. A normal terminal session does not read `.env` by itself, so set the variables manually as shown above, or load them into the shell before running `miniharness`.

## Run

```powershell
miniharness "请阅读 README 并总结这个项目"
```

You can cap the loop:

```powershell
miniharness "找一下 agent loop 在哪里" --max-turns 6
```

You can resume an interrupted planner/workspace session by id:

```powershell
miniharness "继续刚才的任务" --resume-session <session_id>
```

## Test

Install the development dependency:

```powershell
python -m pip install -e ".[dev]"
```

Run the tests:

```powershell
python -m pytest tests --basetemp work/pytest-tmp
```

The tests use temporary files and a mock provider. They do not call a live model API and do not require `.env`.

## Agent Loop Flow

1. `cli.py` receives the user goal, creates a trace file, starts or resumes local workspace state, and starts or resumes `PlannerRuntime`.
2. `agent.py` creates a `ContextManager` with the system message, user goal, and compact planner context.
3. Each turn calls `PlannerRuntime.step()`, then exposes only tools allowed by the current phase.
4. `provider.py` sends the context-managed `messages` and phase-filtered tool schemas to the OpenAI-compatible API.
5. If the model returns tool calls, `agent.py` sends each call to `ToolRuntime.execute_tool_call`.
6. `ToolRuntime` parses JSON arguments, validates them with Pydantic, checks tool policy, asks `PlannerRuntime` whether the action is allowed, applies timeout/output limits/cache, blocks unsafe runtime bypasses, records audit trace, and reports the full `ToolResult` back to the planner.
7. Mutation, command, and verification runtimes also report rich result objects back to the planner, so the evidence ledger is not limited to truncated model-facing previews.
8. Each structured tool result is recorded as a `ToolObservation`; a preview is appended back into `messages` as a `tool` role message.
9. If the model returns no tool calls, `PlannerRuntime.assess_completion()` decides whether finalization is allowed. Coding tasks need applied change evidence and ready or ready-with-warnings verification evidence; read-only tasks can return the model answer when their read evidence criteria are met.
10. For completed coding tasks, `PlannerRuntime.finalize()` creates a factual `FinalReport`. Otherwise the loop returns a blocked completion message or stops at `--max-turns`.

## Context Manager

`ContextManager` owns system, user, assistant, and tool observation messages. In v0.0.5 it also controls the request-sized view sent to the model.

The context layer now:

- Initializes the system and user messages.
- Records assistant messages.
- Counts message and tool-schema tokens with `tiktoken`.
- Reserves output tokens and trims history to the configured model context window.
- Keeps assistant `tool_calls` and matching `tool` messages grouped during trimming.
- Records tool observations with raw results, previews, truncation status, digests, timing/cache/error metadata, and source references.
- Persists observations, messages, snapshots, and references in SQLite under the run directory.
- Sends only the first 4000 characters of long tool content back into model messages while keeping the full raw result in SQLite.
- Accepts a compact planner context message so the model sees current phase, allowed tools, latest evidence, unresolved failures, and risks without receiving full journals or raw stdout.
- Uses `tool_choice=none` during compression so summary calls cannot trigger tools.
- Provides recovery helpers that detect whether the next step should call the model or execute a pending tool.

## Tool Runtime

Miniharness v0.0.4 keeps the existing default CLI behavior: tool choice is sent as `auto`, and strict tool schemas are disabled unless a caller explicitly enables them.

The protocol and runtime layer now have:

- `ToolChoiceMode.AUTO`: the model may call tools or answer directly.
- `ToolChoiceMode.REQUIRED`: the model must call at least one tool, for providers that support this mode.
- `ToolChoiceMode.NONE`: the model must answer without tool calls.
- `ProviderCapabilities`: a small capability record for OpenAI-compatible providers, including support flags for tools, strict schemas, required tool choice, and parallel tool calls.
- `ToolRegistry.openai_tools(strict=True)`: emits `strict: true` function schemas and top-level `additionalProperties: false` parameters while still validating tool arguments locally with Pydantic.
- `ToolSpec`: declares a tool name, version, description, Pydantic input model, handler, permission level, risk tags, timeout, output limit, cacheability, and idempotency.
- `ToolRegistry`: only registered tools can be exposed or dispatched.
- `ToolRuntime`: executes model tool calls and returns structured `ToolResult` / `ToolError` payloads.
- `ToolPolicy.read_only()`: the default policy allows only read-only tools and rejects write, shell, git, and network risk.
- Runtime errors are classified as `tool_not_found`, `bad_arguments_json`, `validation_error`, `permission_denied`, `policy_denied`, `timeout`, `execution_error`, or `internal_error`.
- Cache is per run and only applies to `cacheable=true` read-only tools. The cache key includes tool name, version, normalized validated arguments, and workspace root.
- When a planner is attached, `ToolRuntime` returns `action_not_allowed`, `risk_escalated`, or `needs_review` before executing a handler that violates the current planner phase.
- Write tools must declare `uses_mutation_runtime=true`; otherwise `ToolRuntime` rejects them with `invalid_operation` before the handler can touch the filesystem.
- Shell tools must declare `uses_command_runtime=true`; otherwise `ToolRuntime` rejects them with `invalid_operation` before the handler can spawn a process.

## Planner / Task Execution Runtime

Miniharness v0.0.10 adds `PlannerRuntime` as the execution controller. It owns structured task state, phase transitions, allowed actions, evidence, budgets, risk escalation, replanning, completion assessment, interrupt/resume, and final reports.

The runtime owns:

- `TaskState`: task id, session id, user goal, normalized goal, current phase, status, risk level, completion criteria, linked transactions, linked commands, linked verifications, and final assessment.
- `TaskPlan` and `TaskPhase`: auditable phases with allowed tools/actions and required evidence.
- `AgentAction`: structured action intent, phase, tool allowance, expected evidence, risk level, status, and result reference.
- `EvidenceLedger`: inspected files, search results, applied changes, command results, verification results, parsed failures, external changes, missing evidence, unresolved failures, assumptions, and risks.
- `ExecutionBudget`, `Replanner`, `RiskEscalation`, and `FinalReport`.

Planner state persists under `.miniharness/planner/<session_id>/`. See `docs/architecture/planner-task-execution-runtime.md` for the state machine, evidence-driven completion, failure replanning, budget control, and final report design.

## Local Workspace State Runtime

Miniharness v0.0.9 adds `LocalWorkspaceStateRuntime` as the non-Git source of truth for local workspace state. It creates a baseline at CLI session start, captures rich `FileSnapshot` records, persists state to JSONL plus SQLite, stores large artifacts under the session directory, and reports workspace health without depending on branches, commits, staging, push, or pull requests.

The runtime owns:

- `WorkspaceBaseline`: session start snapshot index.
- `WorkspaceJournal`: structured JSONL events such as `baseline_created`, `file_snapshot_captured`, `file_changed_by_mutation`, `file_changed_by_command`, `external_change_detected`, rollback events, artifact events, recovery, and close.
- `WorkspaceStateStore`: `.miniharness/workspace_state.sqlite3` query index for current state.
- `ArtifactStore`: session-scoped command output, diffs, verification evidence, rollback backups, scan reports, and trace exports.
- `WorkspaceHealthReport`: compact state observation for planner, mutation, verification, context, and CLI surfaces.

Ownership is explicit: `AGENT_MUTATION`, `FORMATTER_SIDE_EFFECT`, `TEST_ARTIFACT`, `PACKAGE_MANAGER_SIDE_EFFECT`, `GENERATED_ARTIFACT`, `COMMAND_SIDE_EFFECT`, `UNKNOWN_EXTERNAL`, and `USER_OWNED` are not collapsed into one dirty-file list. Rollback is agent-owned only: before restoring, the runtime checks that the current file hash still matches the agent's last after-write hash and returns `rollback_conflict` instead of overwriting user or external edits.

The `.miniharness` state directory is excluded from scans and normal read-only tools, and `WorkspacePolicy` denies model-authored mutation attempts under that path. The `workspace_health` tool exposes compact health observations to the agent, and the agent injects that observation after tool calls so the next model turn can see external changes or rollback conflicts without receiving the full journal. CLI runs also print a separate workspace state panel after the final answer, keeping agent changes, command side effects, external changes, and rollback status distinct from the model's final text.

See `docs/architecture/local-workspace-state-runtime.md` for the full design.

## Command Runtime

Miniharness v0.0.7 does not treat shell as a normal tool. Process execution is represented by `CommandRequest` and planned through `CommandPlan` before execution. The default command tools are:

- `run_command`
- `start_process`
- `read_process_output`
- `stop_process`
- `list_processes`

The runtime includes:

- `CommandPolicy`: returns `allow`, `require_review`, or `deny` with risk tags, required backend, network/filesystem mode, and redaction rules.
- `CommandPurpose` and `CommandRisk`: classify read-only commands, project verification, lint, typecheck, format checks, formatters, builds, code generation, package managers, network operations, workspace writes, destructive commands, long-running processes, secret risk, VCS read/mutation, system mutation, project-code execution, and unknown commands.
- `ExecutionBackend`: implemented by `LocalProcessBackend`, with `SandboxBackend` reserved as an explicit interface for future isolation.
- `ProcessSupervisor`: starts processes, monitors timeout and idle timeout, and terminates process trees rather than only killing a parent process.
- `ResourceLimits`: enforces timeout, idle timeout, stdout/stderr/combined output limits, and reports memory/process/disk limits as unsupported on the local backend.
- `EnvPolicy`: avoids full parent env inheritance, allows a small safe inherited env set, denies secret-like env keys, and redacts secrets before trace or observation storage.
- `NetworkMode` and `FilesystemMode`: make network and filesystem expectations explicit. The local backend reports `network_isolation_enforced=false` and filesystem isolation as advisory because it is not a sandbox.
- `OutputCollector`: keeps stdout and stderr separate, builds ordered combined output, truncates oversized previews, records digests, and saves large output artifacts under `.miniharness/artifacts/commands/`.
- Workspace side-effect tracking: uses `LocalWorkspaceStateRuntime` when available to snapshot workspace files before and after commands, classify ownership, return structured side effects, and keep command changes separate from model-owned mutation transactions.

`CommandResult` distinguishes runtime failures, policy denials, review requirements, non-zero exits, and semantic failures such as `tests_failed`, `build_failed`, `lint_failed`, and `typecheck_failed`. Context Manager receives a compact `command_result` observation instead of raw stdout/stderr.

Direct `run_command` calls reject verification-like commands with `verification_runtime_required`. VerificationRuntime is responsible for choosing and running tests, lint, typecheck, builds, and syntax checks, while CommandRuntime remains responsible for executing each approved command.

Git read commands such as `git status`, `git diff`, and `git log` may run through the command runtime. Git mutation commands such as `git add`, `commit`, `reset`, `clean`, and `push` require review or a future dedicated GitRuntime path. Local workspace state, ownership, session recovery, and agent-owned rollback are handled by `LocalWorkspaceStateRuntime`, not Git. See `docs/architecture/command-runtime.md` and `docs/architecture/local-workspace-state-runtime.md` for design details and error taxonomy.

## Verification Runtime

Miniharness v0.0.8 adds a dedicated Verification Runtime. Verification is not a single `run_tests` helper: it is a planned workflow that connects project detection, command discovery, impact analysis, policy review, command execution, failure parsing, repair observation, flaky handling, evidence capture, and completion assessment.

The default verification tools are:

- `plan_verification`
- `run_verification`
- `get_verification_result`
- `rerun_check`

The runtime includes:

- `ProjectDetector` and `CommandDiscovery`: inspect project files such as `package.json`, lockfiles, `pyproject.toml`, pytest/ruff/tox config, `Cargo.toml`, `go.mod`, Java build files, `Makefile`, `justfile`, `tsconfig.json`, ESLint config, and GitHub workflows.
- `ImpactAnalyzer`: turns changed files, task intent, transaction id, and changeset id into affected modules, likely tests, risk reasons, and required build/typecheck/manual-review flags.
- `VerificationPlan` and `VerificationCheck`: separate required, optional, skipped, and blocked checks.
- `VerificationPolicy`: applies verification-specific risk rules before any command runs, while still using `CommandPolicy`.
- `FailureParser` implementations for pytest/Python traceback, TypeScript `tsc`, ESLint, npm build output, and generic stderr fallback.
- `RepairHintGenerator`, `RepairLoopController`, flaky rerun handling, and `CompletionAssessor`.

Verification observations are compact and include plan status, failed checks, parsed failures, repair hints, and completion assessment. Large command output remains in command artifacts and is referenced by evidence. See `docs/architecture/verification-runtime.md` for design details and extension points.

## Workspace Mutation Runtime

Miniharness v0.0.6 does not expose a raw `write_file` tool. File changes are represented as edit operations, assembled into a `ChangeSet`, checked by `WorkspacePolicy`, applied through a `MutationTransaction`, and recorded in a `MutationJournal`. In v0.0.9, successful mutations also flow into `LocalWorkspaceStateRuntime` so agent-owned changes, rollback evidence, workspace health, and trace correlation share the same local state layer as command side effects.

The runtime includes:

- `WorkspacePathResolver`: canonicalizes every path and rejects traversal, symlink escape, Windows drive or UNC escape, and any resolved path outside the workspace.
- `FileClassifier` and `WorkspacePolicy`: classify files as source, config, test, docs, build script, lockfile, secret, VCS internal, generated, binary, large artifact, or unknown; decisions are structured as `allow`, `require_review`, or `deny`.
- `FileSnapshot` and `WorkspaceIndex`: record path, sha256, size, mtime, encoding, line ending, and binary status so mutations can detect user, IDE, command, or agent races.
- `EditOperation` types: `ReplaceText`, `InsertBefore`, `InsertAfter`, `ReplaceRange`, `ApplyUnifiedDiff`, `CreateFile`, `DeleteFile`, `MoveFile`, `UpdateJson`, `UpdateYaml`, `UpdateToml`, and `FormatFile`. Parser-backed operations that are not yet implemented return structured `invalid_operation` errors instead of silently writing.
- `DiffEngine`: emits `FileDiff` and `DiffHunk` records with added and removed line counts, binary/rename flags, digest, truncation status, and artifact paths for large diffs.
- `AtomicWriter`: writes text through temporary files, flush, fsync, and `os.replace`, while preserving existing permissions and line-ending/encoding strategy when possible.
- `RollbackManager`: rolls back agent-owned transactions from the journal and returns `rollback_conflict` if the user changed a file after the transaction.
- Verification hook fields are present in results and trace so VerificationRuntime can attach formatters, lint, typecheck, tests, or builds without hard-coding them into mutation logic.

Registered mutation tools currently include `workspace_replace_text`, `workspace_create_file`, `workspace_delete_file`, and `workspace_move_file`. High-risk operations such as project config edits, build scripts, lockfiles, deletion, moving, and formatting are represented as `require_review`; in the current CLI, that state is returned structurally instead of being silently applied.

See `docs/architecture/workspace-mutation-runtime.md` for the design details and error taxonomy.

## Read-Only Tools

Miniharness still exposes these registered read-only tools:

- `list_files`: list files under the current project root.
- `read_file`: read a file inside the current project root.
- `search_text`: search text inside files under the current project root.

The read-only tools themselves cannot write files, run shell commands, run Git commands, browse the web, store long-term memory, or start other agents. Paths are resolved inside the current project root, so `../outside-file` is rejected. File mutation tools are separate and must pass the Workspace Mutation Runtime path, policy, snapshot, diff, journal, rollback, and trace checks. Command tools are also separate and must pass CommandRuntime policy, env, output, process, trace, and side-effect checks.

## VSCode Setup

This project can run inside a project-local virtual environment:

```powershell
python -m venv .venv
.\.venv\Scripts\python.exe -m pip install -e .
```

VSCode settings are in `.vscode/`:

- `settings.json` points Python to `.venv\Scripts\python.exe`.
- `launch.json` defines `Miniharness: run sample goal`.
- `tasks.json` defines `Miniharness: help` and `Miniharness: compile`.

Put local API settings in `.env`. This file is ignored by Git:

```txt
MINIHARNESS_BASE_URL=https://example.com/v1
MINIHARNESS_API_KEY=replace-with-your-api-key
MINIHARNESS_MODEL=your-model-name
```
