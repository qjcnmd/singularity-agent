# Documentation Execution Map

Documentation Component is the architecture contract layer for the current Python CLI baseline and the next Desktop Transition AgentHost. It is not a README replacement and it does not change agent behavior. Its job is to keep component names, ownership, state, events, policy, trace, and migration contracts stable enough for a later local daemon and Tauri desktop client.

Current v0.1.x status:

- Python remains the production component.
- CLI remains the only shipped client.
- No Tauri, Electron, Rust rewrite, web search, or multi-agent execution is implemented here.
- The current desktop-transition implementation is a Python `AgentHost` facade around the existing component.
- The next implementation phase is the local daemon boundary around that facade, not a desktop UI rewrite.

## Phase 1A Fixed Behavior Status

| Behavior | Status | Source of truth |
| --- | --- | --- |
| Premature completion without required evidence returns `REPLAN_REQUIRED` and continues the agent loop | resolved | `AgentLoop._attempt_finalize()` plus regression coverage in `tests/test_agent_task_outcome.py` |
| `ExecutionOutcomeStatus.RETRYABLE` does not directly terminate a task | resolved | `AgentLoop._terminal_result_from_outcome()` plus malformed tool-call regression coverage |
| `ExecutionOutcomeStatus.REPLAN_REQUIRED` does not directly terminate a task | resolved | `AgentLoop._terminal_result_from_outcome()` plus completion-rejection regression coverage |
| `plan_verification(smoke_commands=...)` creates required `VERIFICATION_SMOKE` checks | resolved | `VerificationRunner._build_plan()` plus `tests/test_verification_runner.py` |
| `run_verification(smoke_commands=...)` executes explicit smoke commands through `VerificationRunner` / `CommandExecutor` | resolved | `VerificationToolHandlers.run_verification()` and smoke evidence regression coverage |
| `workspace_create_file` still writes through policy, `WorkspaceMutationManager`, journal, trace, and workspace state | resolved | mutation tool registration, `WorkspaceMutationManager.apply_changeset()`, and workspace-state regression coverage |

## Current Execution Path

```text
CLI
-> KernelBootstrap
-> AgentKernel
-> AgentLoop
-> Planner
-> ContextManager
-> PromptAssemblyPipeline
-> ModelRunner
-> ToolProtocolEngine
-> ToolExecutor
-> PolicyEngine / ApprovalGate
-> WorkspaceMutationManager / CommandExecutor / VerificationRunner / SandboxManager
-> WorkspaceStateManager
-> TraceRecorder / Audit / FinalReport
```

The in-process AgentHost path is:

```text
AgentHost
-> KernelBootstrap
-> AgentKernel
-> AgentLoop
-> existing agent graph
```

The CLI has not yet been migrated to call AgentHost; that is the next client-boundary step.

`AgentGraph` also boots supporting components that do not appear in every tool-call path: `InteractionController`, `ProjectIndex`, `MemoryLearningPipeline`, `EditExecutor`, `PluginManager`, `ReviewPipeline`, and lazy `EvaluationHarness`.

## Component Name Contract

Keep this list in sync with the matching block in `README.md`.

<!-- architecture-components:start -->
- `CLI`
- `KernelBootstrap`
- `AgentKernel`
- `AgentHost`
- `RunSession`
- `AgentLoop`
- `Planner`
- `ContextManager`
- `PromptAssemblyPipeline`
- `ModelRunner`
- `ToolProtocolEngine`
- `ParallelToolExecutor`
- `ToolExecutor`
- `ToolRegistry`
- `PluginManager`
- `PolicyEngine`
- `ApprovalGate`
- `WorkspaceMutationManager`
- `CommandExecutor`
- `VerificationRunner`
- `SandboxManager`
- `WorkspaceStateManager`
- `GitClient`
- `TraceRecorder`
- `Audit`
- `MemoryLearningPipeline`
- `MemoryBundleSync`
- `RemoteApprovalExchange`
- `ProjectIndex`
- `EditExecutor`
- `ReviewPipeline`
- `EvaluationHarness`
- `FinalReport`
- `DocumentationPipeline`
<!-- architecture-components:end -->

## Component Capability Status

| Capability | Status | Source of truth |
| --- | --- | --- |
| CLI and kernel boot graph | implemented | `src/singularity/cli.py`, `src/singularity/kernel/` |
| Planner task state and evidence ledger | implemented | `src/singularity/planner/` |
| `ContextManager` component class | implemented | `src/singularity/context/manager.py` |
| `ContextSource` source-component enum | implemented | `src/singularity/context/models.py` |
| Tool protocol, tool executor, policy, approval, mutation, command, verification | implemented | `src/singularity/tool_protocol/`, `src/singularity/tools/`, `src/singularity/policy/`, `src/singularity/workspace/`, `src/singularity/command/`, `src/singularity/verification/` |
| Parallel tool executor | implemented | `src/singularity/tool_protocol/parallel.py` runs validated read-only idempotent tool groups concurrently and binds results in original call order |
| Sandbox capability state | implemented | `SandboxManager.capability_summary()` writes `hard_isolation`, `soft_workspace_isolation`, and `no_isolation` into `TaskState.sandbox_capability` |
| Sandbox hard isolation | partial | `DockerSandboxBackend` can provide hard network/process/resource isolation when available; `LocalStagingBackend` is soft copy-on-write workspace staging and must not be documented as hard isolation |
| GitClient | implemented | `src/singularity/git_client/` provides local-only status, diff, and commit operations; push/PR/branch automation is out of scope |
| Remote approval | implemented | `src/singularity/policy/remote.py` exports policy request/decision JSON and imports scoped approval grants; no network approval service is implied |
| Remote memory sync | implemented | `src/singularity/memory/sync.py` exports/imports local JSON bundles and imports remote entries as candidates by default |
| Final reports | implemented | kernel `FinalReport` in `src/singularity/kernel/finalization.py`; planner `FinalReport` in `src/singularity/planner/models.py` |
| Python AgentHost facade | implemented | `src/singularity/agent_host/` exposes start/resume/cancel, approval grant submission, state snapshots, run-event projection, and artifact reads over the existing Python component |
| AgentHost daemon, Rust Core, Tauri UI | planned | architecture docs and ADRs only; HTTP, WebSocket, JSON-RPC, Rust, and Tauri remain out of scope |
| web search, multi-agent execution | planned | explicitly out of scope for the current Python CLI baseline |

## Ownership Map

| Layer | Owns | Must not own |
| --- | --- | --- |
| CLI | Argument parsing, local user IO, exit codes, command rendering | Component state, policy decisions, tool execution, protocol recovery |
| AgentHost | In-process product API over start/resume/cancel, approval grants, snapshots, event projection, and artifact reads | Tool execution shortcuts, direct policy decisions, UI rendering, daemon transport |
| KernelBootstrap / AgentKernel | Agent graph assembly, lifecycle, lock, cancellation, shutdown, recovery, finalization | Tool handler logic, model/tool protocol semantics, UI rendering policy |
| AgentLoop | Session orchestration across planner, context, model, protocol, and final answer | Direct tool execution, policy decisions, trace schema, storage layout |
| Planner | Task state, allowed actions, evidence ledger, completion assessment, final report facts | File writes, shell process execution, approval grants, provider calls |
| ContextManager | Structured context items, bundles, references, compression, model projection; `ContextSource` is only the source-component enum used on context items | Raw secret retention, policy approval, tool execution, workspace mutation |
| PromptAssemblyPipeline | Instruction collection, trust hierarchy, prompt compilation, prompt manifest | Provider calls, tool execution, approvals, workspace writes |
| ModelRunner | Provider registry, request validation, tool-call normalization, retry, budget, streaming | Tool execution, policy decisions, context storage, raw secret archival |
| ToolProtocolEngine | Tool-call validation, scheduling, replay detection, pending approval recovery, result binding | Handler execution for sequential calls, approval UI, direct storage writes outside protocol state |
| ParallelToolExecutor | Concurrent execution of validated read-only idempotent tool groups | Mutations, commands, verification, approval handling, result ordering decisions |
| ToolExecutor / ToolRegistry | Tool exposure, schema validation, policy request construction, handler dispatch after gates | Filesystem mutation, command execution, verification planning, Git operations |
| PolicyEngine / ApprovalGate | Permission decisions, approval gate, scoped grant storage and consumption, fail-closed review behavior, audit records | Tool execution, UI layout, command spawning, remote grant file transport |
| WorkspaceMutationManager | Model-authored workspace edits, changesets, atomic apply, rollback metadata | Shell execution, verification command choice, Git commit/push/reset |
| CommandExecutor | Process planning, env policy, resource limits, command output, side-effect ownership | Model-authored file edits, test selection, policy bypass, hard sandbox fallback |
| VerificationRunner | Project detection, check planning, impact analysis, result classification, repair hints | Direct subprocess calls, mutation writes, approval grants |
| SandboxManager | Isolated execution backend choice, capability checks, staged workspace, sandbox artifacts | Safety re-decision, importing sandbox writes into real workspace |
| WorkspaceStateManager | Baseline snapshots, ownership journal, health report, artifact handles, recovery facts | Authoring changes, hiding external edits, replacing trace/audit |
| GitClient | Local Git status, diff statistics, scoped staging, local commit creation | Push, pull, reset, remote branches, pull requests, workspace rollback authority |
| TraceRecorder / Audit | Append-only events, spans, artifact refs, redaction, timeline and summary | Storing raw secrets, deciding policy, mutating workspace |
| MemoryLearningPipeline | Local memory candidates, accepted memory entries, evidence refs, retrieval block | Trusting remote memory without review, private secret storage, replacing context or trace |
| MemoryBundleSync | JSON bundle export/import, digest validation, candidate-first import policy | Network transport, hidden sync daemon, remote memory as direct local truth |
| RemoteApprovalExchange | File-backed policy request export and scoped grant import | Remote server transport, model-authored approval, policy bypass |
| ProjectIndex | Read-only code intelligence, symbol/import facts, test and impact hints | Running code, changing files, owning verification or mutation |
| EditExecutor / ReviewPipeline / EvaluationHarness | Patch strategy, review evidence, benchmark/replay orchestration | Agent graph boot, policy bypass, command execution outside component gates |
| PluginManager | Local manifest discovery, enablement status, host API, plugin tool registration | Marketplace, dependency install, direct access to core component objects |
| DocumentationPipeline | Architecture contracts, ADRs, schema docs, drift tests | Product UI, daemon transport, Rust/Tauri implementation |

## Forbidden Cross-Boundary Behavior

- A client may request an action, but only owning components may execute it.
- A tool handler may not open/write/delete files unless the tool contract delegates to the owning component.
- A shell-like action must pass through `CommandExecutor`; verification-like commands must pass through `VerificationRunner`.
- `PolicyEngine` is the only permission decision source; approval text from the model is ignored.
- `ApprovalGate` may create local grants only after a policy review requirement.
- `TraceRecorder`, context, memory, and artifacts store redacted summaries and opaque refs, not raw secret payloads.
- Workspace state is authoritative for local ownership and external-change detection; it is not a Git substitute.

## Desktop Contract

Future desktop and daemon work must preserve these boundaries:

1. Tauri/TypeScript UI is a client, not the component core.
2. The Python AgentHost facade owns the current agent graph; the future local daemon wraps that facade and exposes events, commands, and state snapshots.
3. Rust Core is introduced only after the AgentHost boundary proves which contracts are stable.
4. Python remains the plugin/component compatibility layer until explicit migration removes a specific boundary.
