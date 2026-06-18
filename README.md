# Miniharness v0.0.17

Miniharness is a tiny CLI coding agent harness. It is intentionally small so you can see how the agent loop, model runtime, provider adapter, planner, policy runtime, tools, context manager, observability trace runtime, local workspace state runtime, workspace mutation runtime, command runtime, verification runtime, and sandbox runtime connect without using LangChain, LangGraph, or any other agent framework.

v0.0.17 upgrades the Context Manager into a production-grade Context Runtime control plane. Context is now stored as typed `ContextItem` ledger entries, assembled into budgeted `ContextBundle` requests, redacted before model rendering, linked to traceable `ContextReference` records, and recoverable through SQLite checkpoints and runtime evidence. This release does not add new tools and does not implement Git Runtime behavior.

v0.0.15 adds the Instruction / Prompt Runtime. Model requests now flow through `src/miniharness/instructions/` before reaching `ModelRuntime`: instruction sources are collected, assigned priority and trust, checked for prompt injection, resolved for conflicts, compiled into a `PromptBundle`, and summarized in a redacted `PromptManifest`. Project instructions such as `AGENTS.md` are treated as `project_declared`, not as user commands, and project files, logs, tool output, command output, summaries, and model output remain data unless a higher-trust runtime classifies them otherwise. Full prompts are not written to trace or final reports by default; trace records only manifest ids, hashes, token estimates, counts, conflicts, and warning metadata.

v0.0.14 adds the Model / Inference Runtime. Model calls now flow through `src/miniharness/model/` instead of direct provider calls in the agent loop. `ModelRuntime` owns stable request/result objects, provider selection, OpenAI-compatible adaptation, message conversion, tool schema rendering, response validation, token budget checks, retry/fallback handling, stream aggregation, redacted trace metadata, context export policy checks, and structured model failures. It does not execute tools, mutate files, run commands, stage commits, or implement a Git Runtime.

v0.0.13 adds the Observability / Trace Runtime. Planner, tool dispatch, policy, approval, command execution, workspace mutation, sandbox execution, verification, context rendering, and final reporting now write structured `TraceEvent`, `TraceSpan`, and `TraceArtifact` records into an append-only local trace store. Large stdout, stderr, diffs, reports, model messages, and sandbox logs are represented as artifacts instead of being embedded directly in event payloads. Trace payloads and artifacts are redacted before storage, can be queried by run/session/task/action/runtime identifiers, can produce a timeline, and feed a compact Execution Trace Summary into final reports and model context. This is local telemetry only; there is no remote telemetry exporter.

v0.0.12 adds the Sandbox / Isolation Runtime. Policy outcomes such as `sandbox_required` now route command and verification execution into `SandboxRuntime` instead of the bare local process backend. The implemented `LocalStagingBackend` creates a copy-on-write workspace under `work/sandboxes/<sandbox_id>/`, filters and redacts environment variables, enforces timeout/output limits, captures artifacts, detects sandbox-only file changes, writes append-only sandbox trace, and carries isolation evidence into command results, verification evidence, planner context, and final reports. It is practical local staging isolation, not Docker/Podman/WSL or a kernel-level security boundary.

v0.0.11 adds the Policy / Approval Runtime. Tool dispatch, workspace mutation, command execution, verification checks, and planner failure handling now pass through `PolicyRuntime`, which builds structured `PolicyRequest` objects, classifies risk, returns auditable `PolicyDecision` values, records append-only policy JSONL, supports scoped single-use `ApprovalGrant` objects, and carries policy observations into planner context and final reports. Local CLI approval is modeled, but real remote approval, Git policy, and container-backed sandboxing are still intentionally out of scope.

v0.0.10 adds the Planner / Task Execution Runtime. The model no longer gets every tool and advances from a bare loop alone: each turn goes through `PlannerRuntime`, which tracks `TaskState`, phase policy, allowed action/tool gates, evidence ledger updates, execution budget, deterministic replanning, risk escalation, resume state, completion criteria, and a factual final report built from runtime evidence.

v0.0.9 adds the Local Workspace State Runtime. Miniharness can now create a non-Git session baseline, capture rich file snapshots, persist a JSONL state journal and SQLite query index, classify ownership for agent mutations and command side effects, detect external changes, store artifacts under session directories, report workspace health, recover interrupted sessions, and perform hash-checked agent-owned rollback without branches, commits, staging, push, or PR behavior.

v0.0.8 adds the Verification Runtime. Tests, lint, typecheck, builds, syntax checks, and smoke checks are no longer treated as ad-hoc shell calls. The agent can detect project shape, discover commands, analyze changed files, build a verification plan, execute checks through `CommandRuntime`, parse failures, generate repair hints, track evidence, handle flaky reruns, and produce a `CompletionAssessment` before declaring work ready.

v0.0.7 adds the Command / Shell Execution Runtime. Test, build, formatter, package manager, dev server, read-only git, and other process execution now flow through `CommandRequest`, `CommandPlan`, `CommandPolicy`, `ExecutionBackend`, `ProcessSupervisor`, resource limits, env redaction, output artifacts, workspace side-effect tracking, command observations, and structured command trace audit events.

v0.0.6 upgrades file modification from demo-style `write_file` into a Workspace Mutation Runtime. Agent-owned changes now flow through `ChangeSet`, `MutationTransaction`, snapshots, structured policy decisions, atomic writes, diffs, journal entries, rollback checks, trace audit records, and compact context observations.

v0.0.5 added the production Context Manager slice: context assembly is token-budget aware, tool observations are persisted to SQLite with references, long histories can be compressed, and run state can be recovered after interruption.

v0.0.16 tightens the Tool Runtime into a production-grade slice: tool calls now flow through `ToolSpec`, `ToolRegistry`, `ToolRuntime`, `ToolPolicy`, `ToolResult`, `ToolError`, approval gating, planner authorization, output validation, cache invalidation, replay tracking, and redacted trace records.

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
        ├── instructions/   # instruction sources, hierarchy, prompt compilation, manifest
        ├── model/          # model runtime: request/result schema, provider registry, validation, budget, retry
        ├── observability/  # structured trace events, spans, artifacts, timeline, summary
        ├── policy/         # unified local policy, approval, risk, audit runtime
        ├── planner/        # task state machine, action gating, evidence, replan, budget, final reports
        ├── provider.py     # OpenAI-compatible HTTP call via httpx
        ├── sandbox/        # local staging sandbox runtime: COW workspace, env filtering, artifacts, trace
        ├── tools/          # tool specs, registry, runtime, policy, read-only, mutation, command, verification tools
        ├── verification/   # project detection, planning, execution, evidence, repair, completion assessment
        ├── workspace_state/ # non-Git baseline, snapshot, ownership, journal, artifact, rollback, recovery
        ├── workspace/      # mutation runtime: paths, policy, snapshots, diffs, journal, rollback
        └── trace.py        # legacy JSONL trace writer compatibility
```

Each run creates:

```txt
work/traces/runs/<run_id>/events.jsonl
work/traces/runs/<run_id>/spans.jsonl
work/traces/runs/<run_id>/artifacts.jsonl
work/traces/runs/<run_id>/artifacts/
.miniharness/policy/audit.jsonl
.miniharness/sandbox/trace.jsonl
.miniharness/workspace_state.sqlite3
.miniharness/planner/<session_id>/
.miniharness/sessions/<session_id>/journal.jsonl
.miniharness/sessions/<session_id>/artifacts/
```

The structured trace records `task.started`, `action.*`, `instruction.*`, `prompt.*`, `model.*`, `tool.*`, `policy.*`, `approval.*`, `command.*`, `sandbox.*`, `mutation.*`, `verification.*`, `context.*`, and `final_report.*` events. Planner audit entries include task id, session id, phase, action id/kind, decision, reason, evidence refs, budget state, risk level, replan decision, completion assessment, and compact policy/sandbox/instruction observations. Instruction audit entries include source counts, source hashes, prompt injection warnings, conflicts, prompt manifest ids, prompt hashes, token estimates, trust summaries, and priority summaries without full prompt text. Model runtime audit entries include request/response/failure events, message/tool counts, request and content hashes, schema hash, usage, finish reason, proposed tool call metadata, and optional redacted `model_message` artifacts. Tool runtime audit entries include validation, dispatch start/completion/failure, redacted argument summaries, permission level, risk tags, duration, status, error code, truncation status, output digest, and cache hit status. Policy audit entries are written as append-only JSONL under `.miniharness/policy/audit.jsonl` and mirrored into structured trace events with request id, decision id, outcome, risk level, constraints, approval grant references, and secret redaction. Sandbox audit entries are still compatible with `.miniharness/sandbox/trace.jsonl` and are also emitted as structured trace events when `TraceRuntime` is installed. Workspace state audit entries include session id, baseline id, event id, event type, path, ownership, before/after hashes, transaction id, command id, mutation id, artifact id, timestamp, and warning/error code. Mutation audit entries include transaction and changeset ids, operation id, path, operation type, policy decision, risk tags, before/after hashes, diff digest, line counts, status flags, error code, duration, artifact path, and verification status. Command audit entries include command id, argv/shell, cwd, backend, policy decision, risk tags, env policy, network/filesystem modes, resource limits, duration, exit code, output digest, artifact path, changed files, structured side effects, redaction count, semantic status, isolation report, sandbox metadata, and lightweight git state. Verification audit entries include project profile, impact analysis, plan/check ids, policy decision, command id, parsed failures, evidence artifacts, sandbox evidence, repair hints, and completion assessment.

Basic trace CLI commands are available:

```powershell
miniharness trace list
miniharness trace show <run_id>
miniharness trace timeline <run_id>
miniharness trace errors <run_id>
miniharness trace artifacts <run_id>
```

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

## Policy Runtime

Miniharness v0.0.11 adds a unified `PolicyRuntime` in `src/miniharness/policy/`. It classifies risk, returns auditable policy decisions, records scoped approval grants, writes append-only policy audit JSONL, and feeds compact policy observations into planner context and final reports. In v0.0.12, generated-code and verification execution can return `sandbox_required`, which CommandRuntime enforces through SandboxRuntime.

The runtime boundary is:

```txt
ToolRuntime / MutationRuntime / CommandRuntime / VerificationRuntime
  -> PolicyRequest
  -> PolicyRuntime
  -> PolicyDecision
  -> allow / deny / require_review / sandbox_required / escalate / ask_user
```

Policy decisions are local-only. There is no Git policy or remote approval flow in this slice. The current sandbox backend is local staging only; hard network, memory, and process isolation are reserved for future backends.

## Sandbox Runtime

Miniharness v0.0.12 adds `src/miniharness/sandbox/`. `SandboxRuntime` owns backend selection, capability checks, local staging setup, execution, cleanup, artifact collection, change detection, and sandbox trace.

The only implemented backend is `LocalStagingBackend`. It can:

- copy the workspace into `work/sandboxes/<sandbox_id>/workspace`
- exclude `.git`, `node_modules`, virtualenvs, caches, build outputs, coverage, and nested sandboxes
- reject cwd values outside the workspace
- filter and redact secret-like environment variables
- enforce timeout and output preview limits
- attempt process-tree cleanup
- capture stdout/stderr and declared artifacts
- report created, modified, and deleted files inside the sandbox copy

It cannot enforce hard network denial, hard memory limits, hard process-count limits, or container-level filesystem security. If policy requires one of those unsupported capabilities, Miniharness returns `backend_unavailable` / `sandbox_unavailable` and does not run the command naked.

Sandbox changes are not imported into the real workspace. Future import must go through `MutationRuntime` and `PolicyRuntime`.

## Agent Loop Flow

1. `cli.py` receives the user goal, creates a `TraceRuntime` run under `work/traces/runs/<run_id>/`, starts or resumes local workspace state, and starts or resumes `PlannerRuntime`.
2. `cli.py` creates `ModelRuntime` around the OpenAI-compatible provider; old `Provider.chat(...)` remains available as a compatibility wrapper.
3. `agent.py` creates a `ContextManager` with the system message, user goal, and compact planner context.
4. Each turn calls `PlannerRuntime.step()`, then exposes only tools allowed by the current phase.
5. `ModelRuntime.build_request_from_context()` asks `ContextManager` for the request-sized OpenAI chat view, renders `ToolRegistry` tools into `ModelToolSchema`, and records policy/trace metadata.
6. `ModelRuntime.run_turn()` checks context export policy, provider capabilities, token budgets, retry/fallback rules, response shape, tool choice, allowed tools, JSON arguments, Pydantic schema, duplicate ids, and empty output.
7. If the validated model result contains tool calls, `agent.py` sends each canonical call to `ToolRuntime.execute_tool_call`.
8. `ToolRuntime` parses JSON arguments, validates them with Pydantic, checks tool policy, asks `PlannerRuntime` whether the action is allowed, applies timeout/output limits/cache, blocks unsafe runtime bypasses, records audit trace, and reports the full `ToolResult` back to the planner.
9. Mutation, command, and verification runtimes also report rich result objects back to the planner, so the evidence ledger is not limited to truncated model-facing previews.
10. Each structured tool result is recorded as a `ToolObservation`; a preview is appended back into `messages` as a `tool` role message.
11. If the model returns no tool calls, `PlannerRuntime.assess_completion()` decides whether finalization is allowed. Coding tasks need applied change evidence and ready or ready-with-warnings verification evidence; read-only tasks can return the model answer when their read evidence criteria are met.
12. For completed coding tasks, `PlannerRuntime.finalize()` creates a factual `FinalReport` with verification, policy, sandbox, execution trace, and model usage summaries. Otherwise the loop returns a blocked completion message or stops at `--max-turns`.

## Model / Inference Runtime

Miniharness v0.0.14 adds `src/miniharness/model/` as the model protocol boundary. The runtime owns model request construction, provider selection, message conversion, tool schema exposure, response validation, budget checks, retry/fallback handling, stream aggregation, redacted trace events, and structured failure results.

The key files are:

- `models.py`: stable dataclasses and enums for purpose, role, messages, tool schemas, tool calls, capabilities, preferences, budgets, usage, errors, requests, and results.
- `providers.py`: `ModelProvider`, `ProviderRequest`, `ProviderResponse`, `MockModelProvider`, legacy chat adapter, and OpenAI-compatible model provider.
- `registry.py`: provider registration, default selection, and capability checks.
- `messages.py`: model-message to provider-message conversion, developer fallback metadata, `tool_call_id` preservation, and token estimation.
- `tools.py`: `ToolRegistry` to model tool schema rendering, allowed-tool filtering, schema hashing, and canonical tool call normalization.
- `validation.py`: tool choice, unknown tool, JSON/Pydantic schema, duplicate id, empty response, max tool call, and provider capability validation.
- `budget.py`, `retry.py`, and `streaming.py`: token budgeting, usage merge, retryable error handling, fallback model selection, and text/tool delta aggregation.
- `config.py`: env/config defaults, raw prompt/response storage controls, redacted trace defaults, and context export policy.

ModelRuntime never executes a tool. It only returns validated canonical tool calls to the agent loop. `ToolRuntime` remains the only tool execution path, with planner and policy checks still applied as a second boundary.

## Instruction / Prompt Runtime

Miniharness v0.0.15 adds `src/miniharness/instructions/` as the prompt compilation boundary. Every agent-loop model request now asks `InstructionRuntime` to build a `PromptBundle` before `ModelRuntime.run_turn()` sends anything to a provider.

The runtime owns:

- `InstructionSource`: a typed source record with origin, source type, priority, trust level, scope, content hash, metadata, and redaction flag.
- `InstructionHierarchy`: a deterministic priority order: system invariant, harness developer, user session, user task, project instruction, runtime observation, retrieved content, model generated.
- `ProjectInstructionLoader`: loads `AGENTS.md`, `.miniharness/instructions.md`, and `.miniharness/AGENTS.md` inside the workspace only, with size limits and `project_declared` trust.
- `PromptInjectionDetector`: detects common English and Chinese injection patterns such as ignoring system instructions, bypassing policy/approval/sandbox, reading secrets, deleting files, or pretending the user approved.
- `InstructionResolver`: converts sources to frames, filters by purpose, keeps untrusted summaries untrusted, and records conflicts instead of letting the model resolve them.
- `PromptCompiler`: emits system/developer/user/context messages, fences untrusted data and tool output, folds developer sections into system when the provider lacks developer-message support, and computes a stable prompt hash and token estimate.
- `PromptManifest`: the trace/report-safe representation with counts, trust/priority summaries, conflict and injection-warning counts, prompt hash, token estimate, and fold status.

`InstructionRuntime` does not execute tools, call models, approve actions, inspect Git status, mutate files, or expose full policy tables. `ContextManager` still chooses and stores compact context observations, but it exports them to InstructionRuntime with source metadata. `ModelRuntime` still owns provider calls and validation, but request messages come from `PromptBundle.messages`. `PlannerRuntime` records compact instruction prompt observations for final reports without storing full prompts.

## Context Manager

`ContextManager` still exposes the small API used by the agent loop: `messages()`, `add_assistant_message()`, `add_tool_result()`, and `add_trace_summary()`. Internally, v0.0.17 turns those operations into a typed context ledger rather than appending raw chat messages everywhere.

The context layer now:

- Stores `ContextItem` records with run/session/task/phase ids, layer, source runtime, item type, authority, freshness, sensitivity, token count, references, digest, and metadata.
- Builds `ContextBundle` objects through `ContextAssembler`, including included/excluded item ids, `ContextBudgetPlan`, render policy, bundle digest, and lost-context warnings.
- Uses phase-aware retrieval and ranking across system instructions, user goal, planner state, policy observations, workspace state, evidence, tool observations, verification results, recent dialogue, compressed history, failures, and references.
- Keeps assistant `tool_calls` and matching `role=tool` messages paired during window trimming; the pair is retained or removed together.
- Counts message tokens, tool-schema tokens, output reserve, and reasoning reserve, and raises a structured `ContextOverflowError` when pinned context cannot fit.
- Persists context items, append-only context events, bundles, references, snapshots, summaries, recovery checkpoints, legacy messages, and tool observations in SQLite.
- Redacts secret-like content before storage/render by default; raw secret storage is opt-in and normal model rendering never receives unredacted secrets.
- Records tool observations with raw digests, previews, truncation metadata, timing/cache/error metadata, source references, and 4000-character message previews.
- Validates structured compression output: verified facts require reference ids, invalid JSON is rejected, policy constraints are drift-checked, and raw evidence remains preserved.
- Resolves references by id, file path, mutation transaction, policy decision, and verification id; references can be marked stale when a target changes.
- Reconstructs interrupted runs from SQLite, messages, latest bundle, trace tail, checkpoints, pending tool calls, policy approvals, open mutation transactions, active command observations, and verification status.
- Emits context trace events for item addition, bundle build, rendering, compaction, stale references, and recovery without writing raw content to trace payloads.

## Tool Runtime

Miniharness v0.0.16 keeps tool execution behind a single runtime boundary, but the contract is now explicit:

- `ToolSpec` carries `name`, `version`, `description`, `input_model`, `output_model`, `handler`, `permission_level`, `risk_tags`, `timeout_seconds`, `max_output_chars`, `cacheable`, `idempotent`, backend hints, and sensitivity metadata.
- `ToolRegistry` only registers admitted tools, exports strict OpenAI tool schema, and refuses the old bypass-style dispatch path.
- `ToolRuntime` performs JSON parsing, Pydantic validation, policy evaluation, optional approval gating, planner authorization, per-run replay tracking, bounded cache, backend checks, handler execution, output validation, truncation, and trace/audit emission. It must be constructed with the session `PolicyRuntime`; it no longer creates a fallback policy runtime internally.
- Default policy stays read-only. Write, shell, git, and other high-risk tools are rejected unless they are registered through the proper mutation/command/verification backends.
- Sensitive files such as `.env`, `id_rsa`, `*.pem`, `*.key`, and similar names are hidden from directory listing and excluded from directory search results.
- Legacy `TraceWriter` now redacts payloads before writing JSONL so older runtime paths do not leak raw tool arguments, sensitive path names, or secret-like values.
- Tool errors are classified as `tool_not_found`, `bad_arguments_json`, `validation_error`, `permission_denied`, `policy_denied`, `approval_required`, `sandbox_required`, `timeout`, `execution_error`, `output_validation_error`, `conflicting_replay`, `replay_not_allowed`, or `internal_error`.

Tool runtime cache is per run and only applies to `cacheable=true`, read-only, idempotent tools with non-sensitive results. Cache keys include tool name, version, schema fingerprint, normalized arguments, workspace root, and path snapshots or directory fingerprints so file changes invalidate stale entries. Duplicate `tool_call_id` values are checked before cache lookup for every tool, so same-id/different-argument calls are rejected with `conflicting_replay` even for cacheable tools.

## Planner / Task Execution Runtime

Miniharness v0.0.10 adds `PlannerRuntime` as the execution controller. It owns structured task state, phase transitions, allowed actions, evidence, budgets, risk escalation, replanning, completion assessment, interrupt/resume, and final reports.

The runtime owns:

- `TaskState`: task id, session id, user goal, normalized goal, current phase, status, risk level, completion criteria, linked transactions, linked commands, linked verifications, and final assessment.
- `TaskPlan` and `TaskPhase`: auditable phases with allowed tools/actions and required evidence.
- `AgentAction`: structured action intent, phase, tool allowance, expected evidence, risk level, status, and result reference.
- `EvidenceLedger`: inspected files, search results, applied changes, command results, verification results, parsed failures, external changes, missing evidence, unresolved failures, assumptions, risks, and policy observations.
- `ExecutionBudget`, `Replanner`, `RiskEscalation`, and `FinalReport`.

Planner state persists under `.miniharness/planner/<session_id>/`. Policy observations are rendered into compact context summaries and final reports include a `policy_approval_summary`. See `docs/architecture/planner-task-execution-runtime.md` for the state machine, evidence-driven completion, failure replanning, budget control, and final report design.

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

`CommandResult` distinguishes runtime failures, policy denials, review requirements, sandbox backend failures, non-zero exits, and semantic failures such as `tests_failed`, `build_failed`, `lint_failed`, and `typecheck_failed`. Context Manager receives a compact `command_result` observation instead of raw stdout/stderr.

When policy requires sandboxing, CommandRuntime calls SandboxRuntime and maps the `SandboxResult` back into `CommandResult`. Sandbox metadata appears under `isolation_report.sandbox` and `metadata.sandbox_*`.

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

Verification observations are compact and include plan status, failed checks, parsed failures, repair hints, sandbox evidence, and completion assessment. Large command output remains in command or sandbox artifacts and is referenced by evidence. See `docs/architecture/verification-runtime.md` for design details and extension points.

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
