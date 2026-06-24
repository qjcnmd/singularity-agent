# Naming And Concept Map

This document maps common coding-agent harness terms to the current Singularity implementation. The architecture vocabulary uses role names that describe ownership: loop, runner, executor, manager, controller, pipeline, store, recorder, registry, checkpoint, and harness.

Architecture components tracked by `DocumentationPipeline`:

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
- `ModelTurnRequestBuilder`
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
- `AuditLog`
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

## Naming Table

| Industry term | Singularity object | Location | Data-flow position |
| --- | --- | --- | --- |
| Agent loop | `AgentLoop` | `src/singularity/agent_loop.py` | Per-turn orchestration after `AgentKernel` starts a task |
| Run controller | `RunController` | `src/singularity/run_controller.py` | Applies `ExecutionOutcome` and max-turn handling inside `AgentLoop.run()` |
| Kernel / lifecycle controller | `KernelBootstrap`, `AgentKernel` | `src/singularity/kernel/bootstrap.py`, `src/singularity/kernel/agent_kernel.py` | Boot, graph assembly, locks, cancellation, shutdown, recovery, final report |
| Agent host facade | `AgentHost` | `src/singularity/agent_host/` | Product boundary above Python components for CLI, future daemon, and desktop clients |
| Planner | `Planner` | `src/singularity/planner/engine.py` | Task state, phase policy, evidence ledger, completion assessment |
| Context management layer | `ContextManager`, `ObservationStore`, `ContextBundle`, `ContextSnapshot` | `src/singularity/context/manager.py`, `src/singularity/context/store.py`, `src/singularity/context/models.py` | Context item storage, retrieval, compaction, usage reporting, model projection |
| Prompt assembly | `PromptAssemblyPipeline`, `InstructionResolver` | `src/singularity/instructions/prompt_assembly.py`, `src/singularity/instructions/resolver.py` | Instruction resolution and provider-ready prompt sections |
| Model turn request builder | `ModelTurnRequestBuilder` | `src/singularity/model/request_builder.py` | Converts context, prompt frame, tools, and ids into `ModelTurnRequest` |
| Model runner | `ModelRunner` | `src/singularity/model/runner.py` | Executes provider calls, validates model output, emits model request/response trace |
| Tool protocol engine | `ToolProtocolEngine` | `src/singularity/tool_protocol/engine.py` | Binds model tool calls, protocol state, replay handling, and observations |
| Tool registry | `ToolRegistry`, `ToolSpec` | `src/singularity/tools/registry.py`, `src/singularity/tools/models.py` | Declares tool schemas, permissions, side effects, cache and idempotency policy |
| Tool executor | `ToolExecutor`, `ParallelToolExecutor` | `src/singularity/tools/executor.py`, `src/singularity/tool_protocol/parallel.py` | Enforces schema, policy, approval, dry-run, backend contract, execution, trace |
| Command executor | `CommandExecutor` | `src/singularity/command/executor.py` | Process execution, command policy, sandbox handoff, output capture, process sessions |
| Workspace mutation manager | `WorkspaceMutationManager`, `EditExecutor` | `src/singularity/workspace/mutation_manager.py`, `src/singularity/edit/executor.py` | File changes, patches, rollback ledger, edit strategy |
| Verification runner | `VerificationRunner` | `src/singularity/verification/runner.py` | Verification planning, check execution through `CommandExecutor`, completion assessment |
| Sandbox manager | `SandboxManager` | `src/singularity/sandbox/manager.py` | Selects Docker or local staging backend for `hard_isolation`, `soft_workspace_isolation`, or `no_isolation`; fails closed when required isolation is unavailable |
| Checkpointing / recovery | `WorkspaceStateManager`, `RunCheckpointStore`, `CrashRecoveryManager` | `src/singularity/workspace_state/manager.py`, `src/singularity/run_controller.py`, `src/singularity/kernel/recovery.py` | Session baselines, ownership journal, rollback plan, interrupted-run recovery |
| Observability / tracing | `TraceRecorder`, `TraceStore`, `TraceTimelineBuilder`, `AuditLog` | `src/singularity/observability/recorder.py`, `src/singularity/observability/store.py`, `src/singularity/observability/timeline.py`, `src/singularity/policy/audit.py` | Structured events, spans, artifacts, timelines, audit decisions |
| Policy and approval | `PolicyEngine`, `ApprovalGate`, `RiskClassifier`, `ApprovalGrant` | `src/singularity/policy/engine.py`, `src/singularity/policy/approval.py`, `src/singularity/policy/risk.py`, `src/singularity/policy/models.py` | Risk assessment, policy decision, local review, grant registration and consumption |
| Memory store and learning | `MemoryStore`, `MemoryLearningPipeline`, `MemoryBundleSync` | `src/singularity/memory/store.py`, `src/singularity/memory/pipeline.py`, `src/singularity/memory/sync.py` | Candidate extraction, selected memory retrieval, local bundle import/export |
| Evaluation harness | `EvaluationHarness`, `BenchmarkTask`, `EvalReport` | `src/singularity/evaluation/harness.py`, `src/singularity/evaluation/models.py`, `src/singularity/evaluation/reports.py` | Offline scoring, trace replay, suites, A/B runs, regression reports |
| Project index | `ProjectIndex` | `src/singularity/code_index/index.py` | Code intelligence, impact/test mapping, retrieval hints |
| Git client | `GitClient` | `src/singularity/git_client/client.py` | local-only status, diff, and commit; Push, pull, reset, remote branches, pull requests, and remote automation stay out of scope |
| Documentation pipeline | `DocumentationPipeline` | `tests/test_docs_consistency.py`, `docs/architecture/` | Documentation contract checks and architecture drift detection |

## Core Data Flow

```text
CLI
-> KernelBootstrap.boot()
-> AgentGraphBuilder.build()
-> AgentKernel.run_task()
-> AgentLoop.run()
-> RunController.start()
-> Planner.step()
-> ModelRunner.build_request_from_context()
-> ModelTurnRequestBuilder.build_request()
-> PromptAssemblyPipeline.build_for_model_turn()
-> ContextManager.messages()
-> ContextManager.build_bundle()
-> ModelRunner.run_turn()
-> ToolProtocolEngine.process_model_turn()
-> ToolExecutor.execute_request()
-> PolicyEngine.enforce() / ApprovalGate.consume_matching_grant() / ApprovalGate.resolve()
-> WorkspaceMutationManager / CommandExecutor / VerificationRunner / SandboxManager
-> ContextManager.add_tool_protocol_result()
-> TraceRecorder.emit()
-> Planner.assess_completion()
-> FinalReport
```

## Layer Semantics

| Layer | Meaning | Canonical vocabulary |
| --- | --- | --- |
| Run | One user-requested execution with a run id, session id, task id, trace directory, context store, and protocol store | run, session, task |
| Turn | One model request/response cycle | model turn, `ModelTurnRequest`, `ModelTurnResult` |
| Step | Planner state transition inside a task | planner step, phase, action |
| Tool call | Model-requested function call | `ToolCallEnvelope`, `ToolProtocolResultEnvelope`, `ToolResult`, `ToolObservation` |
| Observation | Data injected back into context | `ToolObservation`, `PolicyObservation`, `CommandObservation`, `VerificationEvidence` |
| Checkpoint | Persisted recovery boundary | `ContextSnapshot`, workspace baseline, journal event, run checkpoint |
| Trace | Structured observability record | `TraceEvent`, `TraceSpan`, `TraceRecorder`, `RunTimeline` |

## Removed Old Names

| Old name | New name |
| --- | --- |
| `AgentRuntime` / `src/singularity/agent.py` | `AgentLoop` / `src/singularity/agent_loop.py` |
| `RuntimeGraph` | `AgentGraph` |
| `RuntimeFactory` | component factory / `AgentGraphBuilder` |
| `KernelRuntime` | `AgentKernel` |
| `ModelRuntime` | `ModelRunner` |
| `ToolRuntime` | `ToolExecutor` |
| `CommandRuntime` | `CommandExecutor` |
| `ContextRuntime` | `ContextManager` / context management layer |
| `PromptRuntime` | `PromptAssemblyPipeline` / `ModelTurnRequestBuilder` |
| `MemoryRuntime` | `MemoryLearningPipeline` / `MemoryStore` |
| `EvaluationRuntime` | `EvaluationHarness` |
| `DocumentationRuntime` | `DocumentationPipeline` |
| `TraceRuntime` | `TraceRecorder` / observability layer |
| `PolicyApprovalRuntime` | `PolicyEngine` / `ApprovalGate` |
| `WorkspaceStateRuntime` | `WorkspaceStateManager` |
| `WorkspaceRuntime` | `WorkspaceMutationManager` |
| `VerificationRuntime` | `VerificationRunner` |
| `SandboxRuntime` | `SandboxManager` |
| `GitRuntime` | `GitClient` |
| `PluginRuntime` | `PluginManager` |

## Remaining Non-Architecture Uses

`RuntimeError` and `RuntimeWarning` remain Python built-in exception/warning names. They are language primitives, not Singularity architecture names.
