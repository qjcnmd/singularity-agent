# Documentation Runtime Map

Documentation Runtime is the architecture contract layer for the current Python CLI baseline and the next Desktop Transition Runtime. It is not a README replacement and it does not change runtime behavior. Its job is to keep runtime names, ownership, state, events, policy, trace, and migration contracts stable enough for a later local daemon and Tauri desktop client.

Current v0.1.x status:

- Python remains the production runtime.
- CLI remains the only shipped client.
- No Tauri, Electron, Rust rewrite, remote approval, Git Runtime, or remote memory sync is implemented here.
- The next implementation phase is Desktop Transition Runtime: a RuntimeHost/local-daemon boundary around the existing Python runtime.

## Current Execution Path

```text
CLI
-> KernelBootstrap
-> AgentKernel
-> SingularityAgent
-> PlannerRuntime
-> ContextRuntime / ContextManager
-> InstructionRuntime
-> ModelRuntime
-> ToolCallingProtocolRuntime
-> ToolRuntime
-> PolicyRuntime / ApprovalGate
-> MutationRuntime / CommandRuntime / VerificationRuntime / SandboxRuntime
-> WorkspaceStateRuntime
-> TraceRuntime / Audit / FinalReport
```

`RuntimeGraph` also boots supporting components that do not appear in every tool-call path: `InteractionRuntime`, `ProjectIndexRuntime`, `MemoryRuntime`, `EditRuntime`, `PluginRuntime`, `ReviewRuntime`, and lazy `EvaluationRuntime`.

## Runtime Name Contract

Keep this list in sync with the matching block in `README.md`.

<!-- runtime-names:start -->
- `CLI`
- `KernelBootstrap`
- `AgentKernel`
- `SingularityAgent`
- `PlannerRuntime`
- `ContextRuntime`
- `InstructionRuntime`
- `ModelRuntime`
- `ToolCallingProtocolRuntime`
- `ToolRuntime`
- `ToolRegistry`
- `PluginRuntime`
- `PolicyRuntime`
- `ApprovalGate`
- `MutationRuntime`
- `CommandRuntime`
- `VerificationRuntime`
- `SandboxRuntime`
- `WorkspaceStateRuntime`
- `TraceRuntime`
- `Audit`
- `MemoryRuntime`
- `ProjectIndexRuntime`
- `EditRuntime`
- `ReviewRuntime`
- `EvaluationRuntime`
- `FinalReport`
- `DocumentationRuntime`
<!-- runtime-names:end -->

## Ownership Map

| Layer | Owns | Must not own |
| --- | --- | --- |
| CLI | Argument parsing, local user IO, exit codes, command rendering | Runtime state, policy decisions, tool execution, protocol recovery |
| KernelBootstrap / AgentKernel | Runtime graph assembly, lifecycle, lock, cancellation, shutdown, recovery, finalization | Tool handler logic, model/tool protocol semantics, UI rendering policy |
| SingularityAgent | Session orchestration across planner, context, model, protocol, and final answer | Direct tool execution, policy decisions, trace schema, storage layout |
| PlannerRuntime | Task state, allowed actions, evidence ledger, completion assessment, final report facts | File writes, shell process execution, approval grants, provider calls |
| ContextRuntime / ContextManager | Structured context items, bundles, references, compression, model projection | Raw secret retention, policy approval, tool execution, workspace mutation |
| InstructionRuntime | Instruction collection, trust hierarchy, prompt compilation, prompt manifest | Provider calls, tool execution, approvals, workspace writes |
| ModelRuntime | Provider registry, request validation, tool-call normalization, retry, budget, streaming | Tool execution, policy decisions, context storage, raw secret archival |
| ToolCallingProtocolRuntime | Tool-call validation, scheduling, replay detection, pending approval recovery, result binding | Handler execution, approval UI, direct storage writes outside protocol state |
| ToolRuntime / ToolRegistry | Tool exposure, schema validation, policy request construction, handler dispatch after gates | Filesystem mutation, command execution, verification planning, Git operations |
| PolicyRuntime / ApprovalGate | Permission decisions, scoped local approval grants, fail-closed review behavior, audit records | Tool execution, UI layout, command spawning, persistent remote approval |
| MutationRuntime | Model-authored workspace edits, changesets, atomic apply, rollback metadata | Shell execution, verification command choice, Git commit/push/reset |
| CommandRuntime | Process planning, env policy, resource limits, command output, side-effect ownership | Model-authored file edits, test selection, policy bypass, hard sandbox fallback |
| VerificationRuntime | Project detection, check planning, impact analysis, result classification, repair hints | Direct subprocess calls, mutation writes, approval grants |
| SandboxRuntime | Isolated execution backend choice, capability checks, staged workspace, sandbox artifacts | Safety re-decision, importing sandbox writes into real workspace |
| WorkspaceStateRuntime | Baseline snapshots, ownership journal, health report, artifact handles, recovery facts | Authoring changes, hiding external edits, replacing trace/audit |
| TraceRuntime / Audit | Append-only events, spans, artifact refs, redaction, timeline and summary | Storing raw secrets, deciding policy, mutating workspace |
| MemoryRuntime | Local memory candidates, accepted memory entries, evidence refs, retrieval block | Remote sync, private secret storage, replacing context or trace |
| ProjectIndexRuntime | Read-only code intelligence, symbol/import facts, test and impact hints | Running code, changing files, owning verification or mutation |
| EditRuntime / ReviewRuntime / EvaluationRuntime | Patch strategy, review evidence, benchmark/replay orchestration | Runtime graph boot, policy bypass, command execution outside runtime gates |
| PluginRuntime | Local manifest discovery, enablement status, host API, plugin tool registration | Marketplace, dependency install, direct access to core runtime objects |
| DocumentationRuntime | Architecture contracts, ADRs, schema docs, drift tests | Product UI, daemon transport, Rust/Tauri implementation |

## Forbidden Cross-Boundary Behavior

- A client may request an action, but only runtimes may execute it.
- A tool handler may not open/write/delete files unless the tool contract delegates to the owning runtime.
- A shell-like action must pass through `CommandRuntime`; verification-like commands must pass through `VerificationRuntime`.
- `PolicyRuntime` is the only permission decision source; approval text from the model is ignored.
- `ApprovalGate` may create local grants only after a policy review requirement.
- `TraceRuntime`, context, memory, and artifacts store redacted summaries and opaque refs, not raw secret payloads.
- Workspace state is authoritative for local ownership and external-change detection; it is not a Git substitute.

## Desktop Contract

Future desktop work must preserve these boundaries:

1. Tauri/TypeScript UI is a client, not the runtime core.
2. A local daemon or RuntimeHost owns the current runtime graph and exposes events, commands, and state snapshots.
3. Rust Core is introduced only after the RuntimeHost boundary proves which contracts are stable.
4. Python remains the plugin/runtime compatibility layer until explicit migration removes a specific boundary.
