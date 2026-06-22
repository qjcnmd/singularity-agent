# Runtime Kernel / Session Lifecycle Runtime

Singularity now has a runtime kernel layer in `src/singularity/kernel/`. The kernel is the process-level control plane: it owns boot, runtime graph assembly, run/session lifecycle, workspace locking, cancellation, shutdown, crash recovery, health checks, and final report aggregation. PlannerRuntime remains focused on task planning and completion evidence; it no longer owns system lifecycle.

## CLI Entry

The CLI path is:

```text
CLI -> KernelBootstrap -> AgentKernel -> SingularityAgent -> PlannerRuntime
```

`src/singularity/cli.py` still parses user flags into `ProductionRuntimeConfig`, but runtime construction is delegated to `KernelBootstrap`. The CLI then calls `AgentKernel.run_task()` and prints both the model final answer and the kernel `FinalReport`.

Supported CLI inputs still flow through the config object and override lower-priority sources:

- `--max-turns`
- `--profile`
- `--resume`
- `--approval-mode`, including `read_only`
- `--trace-dir`
- `--context-db`
- `--model`
- `--base-url`
- `--raw-artifacts`
- `--dry-run`
- `--strict`

The full runtime config precedence is:

```txt
explicit CLI flag > SINGULARITY_* > .singularity/config.toml > defaults
```

`ProductionRuntimeConfig.effective_config()` returns the redacted effective values and source map. `KernelBootstrap.boot()` records that payload in trace with runtime `config`, and kernel final reports include the same source-aware summary. `SINGULARITY_API_KEY` remains environment-only and is not written to the effective config payload.

## RuntimeGraph

`RuntimeFactory` builds a `RuntimeGraph` in this declared order:

1. Configuration
2. Observability
3. Interaction
4. WorkspaceState
5. ProjectIndex
6. Memory
7. Policy
8. Sandbox
9. Command
10. Mutation
11. Edit
12. Tools
13. Plugins
14. ToolRuntime
15. ToolProtocol
16. Verification
17. Review
18. Evaluation
19. Instructions
20. Model
21. Context
22. Planner

Each component is recorded as `runtime.initialized` in trace when its boot-time boundary is ready. `RuntimeFactory.build()` assembles the graph in explicit phases: infra, policy/sandbox, execution core, tools/plugins/protocol, verification/review, model/context, then planner wiring. The plugin boundary runs after built-in tools are registered and before `ToolRuntime` / `ToolCallingProtocolRuntime` are created, so enabled local tool plugins become ordinary `ToolSpec` entries and still execute through the same policy, approval, sandbox, trace, and protocol layers. The evaluation boundary is registered during boot but the full `EvaluationRuntime` object is created only when evaluation functionality is actually accessed, so normal agent runs do not pay for benchmark scoring, replay, and artifact-writer setup. The graph creates `ContextManager` before Planner, then `AgentKernel` passes that context into `SingularityAgent`. Command, Mutation, Tool, Review, Evaluation, and Verification runtimes are wired back to the session PlannerRuntime after Planner creation so execution evidence still lands in the existing planner ledger.

## Lifecycle

`RunLifecycleManager` creates `AgentRun`, `AgentSession`, and `LifecycleEvent` records for:

- `lifecycle.run.started`
- `lifecycle.session.started`
- `lifecycle.task.started`
- run completed, failed, or cancelled

Lifecycle events are written to trace and summarized into `FinalReport.lifecycle_summary`.

## Cancellation

`CancellationManager` owns a root `CancellationToken` and child tokens. `RuntimeGraph` owns the list of cancellation-aware runtime targets, and `AgentKernel` asks the graph to attach child tokens so downstream layers can honor cancellation without owning process shutdown. Lazy evaluation construction uses the same graph-level token factory when evaluation is accessed after boot.

`KeyboardInterrupt` is converted into:

```text
Ctrl+C -> kernel.cancel(user_interrupted) -> graceful shutdown -> finalization path
```

No KeyboardInterrupt path is expected to bypass shutdown/finalization.

Shutdown also cancels the root token, so later Planner, Model, Command, Sandbox, Tool, Protocol, Context, Edit, Review, and Verification entrypoints fail through their cancellation checks instead of accepting new actions.

## Workspace Lock

`WorkspaceLockManager` stores its default lock at:

```text
.singularity/locks/workspace.lock
```

Behavior:

- write mode blocks all concurrent runs
- read-only mode allows shared read-only holders
- stale locks are detected by timestamp and PID liveness; stale-lock facts are retained in the recovery report even when acquiring a new lock has to remove the stale file
- lock state is independent of Git
- shutdown releases the active holder even if previous cleanup steps fail

## Health Check

`RuntimeHealthChecker` checks the runtime graph components, including plugins and deferred components such as evaluation, without forcing lazy runtimes to instantiate. Missing components become diagnostics; critical missing components fail closed. Results are written to trace as `runtime.health_checked` and included in `FinalReport.runtime_health_summary`.

## Shutdown

`ShutdownManager` executes cleanup in this order and continues after failures:

```text
stop planner
-> reject actions
-> cancel model
-> terminate commands
-> terminate sandbox
-> finalize mutations
-> checkpoint
-> flush trace
-> write report
-> release lock
```

The `write report` step generates a kernel final report before lock release. After all cleanup steps finish, `AgentKernel` refreshes the in-memory report with the full shutdown summary. The cleanup result is included in `FinalReport.shutdown_summary`.

## Recovery

`CrashRecoveryManager` checks for stale workspace locks, incomplete trace spans, recoverable workspace-state sessions, unfinished mutation journals, leftover sandboxes, and running process records. Recovery marks mutation journals with `recovered.json`, cleans leftover sandbox directories, stops recorded running command sessions, and repairs trace spans when the trace store supports it. It does not automatically continue an unfinished planner action. The recovery result is included in `FinalReport.recovery_summary`.

Bootstrap failures still release the workspace lock and carry a partial kernel `FinalReport` on `KernelBootstrapError`; the CLI prints that report before exiting.

## FinalReport

The kernel-level `FinalReport` includes:

- `run_id`
- `session_id`
- `task_id`
- `kernel_status`
- `shutdown_reason`
- `diagnostics_count`
- `cleanup_status`
- `recovered_previous_run`
- `uncertain_transactions`
- `workspace_lock_status`
- `runtime_health_summary`
- `shutdown_summary`
- `recovery_summary`
- `lifecycle_summary`

Planner `FinalReport` also accepts the four new summary fields so planner-level reports can carry kernel summaries when needed. All final report payloads go through trace redaction before serialization.

## Extension Points

This layer is ready to support later daemon, TUI, remote attach, external supervisor, and multi-session capabilities because runtime construction and process lifecycle now sit above PlannerRuntime instead of inside the CLI command body.
