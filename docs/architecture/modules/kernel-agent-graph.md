# KernelBootstrap / AgentGraph / AgentKernel模块数据流

模块数据流文档 ID: kernel-agent-graph

源码证据路径:
- src/singularity/kernel/bootstrap.py
- src/singularity/kernel/graph.py
- src/singularity/kernel/agent_kernel.py
- src/singularity/kernel/models.py
- src/singularity/session/models.py
- src/singularity/session/recovery.py

关键符号:
- KernelBootstrap
- KernelBootstrap.boot
- AgentGraphBuilder
- AgentGraphBuilder.build
- AgentGraph
- AgentKernel
- AgentKernel.run_task
- RunIdentity
- AgentRun
- AgentSession
- KernelContext
- LifecycleEvent
- RecoveryGateDecision

字段清单:
- AgentGraph: config, trace, interaction_controller, workspace_state, project_index, memory_pipeline, policy_engine, approval_gate, sandbox_manager, command_executor, mutation_manager, edit_executor, tools, plugin_manager, verification_runner, review_pipeline, prompt_assembly, model_runner, context_manager, tool_executor, tool_protocol, planner, recovery_gate_decision, initialization_order, components, _evaluation_harness, _evaluation_harness_factory, _cancellation_token_factory
- RunIdentity: run_id, session_id, task_id
- AgentRun: identity, user_goal, status, started_at, ended_at, final_answer, error
- AgentSession: identity, status, started_at, ended_at, recovered_previous_run
- KernelContext: project_root, identity, run, session, status, components, diagnostics, workspace_lock_status, recovered_previous_run, uncertain_transactions, session_run_mode, recovery_gate_decision
- LifecycleEvent: event_type, run_id, session_id, task_id, timestamp, payload
- RecoveryGateDecision: session_id, mode, status, can_call_model, blockers, warnings, next_action, resume_context

## 这一层解决什么问题

Kernel 层负责启动配置、trace、workspace lock、组件图、健康检查、运行生命周期和最终关闭，把 CLI 目标转入真实 AgentLoop。

## 当前源码位置

- src/singularity/kernel/bootstrap.py
- src/singularity/kernel/graph.py
- src/singularity/kernel/agent_kernel.py
- src/singularity/kernel/models.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

`KernelBootstrap.boot()` -> `SessionStore.prepare_launch()` -> `TraceRecorder.create()` -> `CrashRecoveryManager.inspect()` -> `SessionHistoryReader.tool_protocol_report()` / `RecoveryManager.recover(previous context.sqlite3)` -> `SessionHistoryReader.build_resume_context()` -> `SessionRecoveryGate.evaluate()` -> `AgentGraphBuilder.build()` -> `AgentKernel.boot()` -> `AgentKernel.run_task()` -> `AgentLoop.run()`。失败时 `KernelBootstrapError` 或 `KernelError` 携带 final report / diagnostics。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例，真实调用链是 `KernelBootstrap.boot()` -> `SessionStore.prepare_launch()` -> `TraceRecorder.create()` -> `RunIdentity.new()` -> `SessionStore.start_run()` -> `CrashRecoveryManager.inspect()` -> `SessionHistoryReader.tool_protocol_report()` -> `RecoveryManager.recover(previous context.sqlite3)` -> `SessionHistoryReader.build_resume_context()` -> `SessionRecoveryGate.evaluate()` -> `AgentGraphBuilder.build()` -> `AgentKernel.run_task()`。这里生成对象 `RunIdentity` / `RecoveryGateDecision`，写入 sqlite store，并消费摘要 `SessionResumeContext`。`SessionStore.start_run()` 生成 run 记录并在 graph 构建前落盘到 `.singularity/session_index.sqlite3`，保证 boot 失败也能被 `sg session show` 找到。`CrashRecoveryManager.inspect()` 只检查 stale lock、unfinished mutation、leftover sandbox 和 process record，不静默清理；`RecoveryManager.recover()` 只把上一 run context 的 pending tool、pending approval、active process、open mutation 和 verification 状态摘要交给 gate，不把 raw context 注入模型；`SessionHistoryReader.build_resume_context()` 聚合 planner、workspace、tool protocol、verification 与失败摘要；`SessionRecoveryGate.evaluate()` 生成 `RecoveryGateDecision` 并写 trace/session timeline。`AgentGraphBuilder.build()` 消费 `RecoveryGateDecision` 并注入 graph，只有 `continue/resume` 会把过滤后的 `SessionResumeContext` 写入 `ContextManager`。`AgentKernel.run_task()` 在创建 `AgentLoop` 前检查 gate，若 `can_call_model=False`，直接写 `session.recovery_blocked` 并返回 blocked final report，不调用模型也不写文件。构图失败抛 `AgentGraphInitializationError` 并由 `KernelBootstrapError` 携带 diagnostics；运行失败由 kernel finalization 写 final report/trace，而不是写入模型请求。

## 真实对象完整结构

### RunIdentity（运行标识）

统一 run/session/task id，供 trace、context、kernel lifecycle 和 evaluation result 引用。**边界**：内部治理对象，其 id 字段投影进 trace event、context bundle、model request、evaluation result，但 RunIdentity 本身不作为整体发送给模型或落盘。

```python
@dataclass(frozen=True)
class RunIdentity:
    run_id: str        # "run_<uuid_hex_12>"
    session_id: str    # "session_<uuid_hex_12>"
    task_id: str       # "task_<uuid_hex_12>"
```

### KernelContext（内核运行上下文）

保存 project root、identity、run/session 状态、组件状态、diagnostics、workspace lock 和恢复信息。**边界**：内部治理对象，不进入模型请求；diagnostics 投影进 final report 和 trace lifecycle event。

```python
@dataclass
class KernelContext:
    project_root: Path
    identity: RunIdentity
    run: AgentRun
    session: AgentSession | None = None
    status: KernelStatus = KernelStatus.NEW
    components: dict[ComponentName, ComponentState] = field(default_factory=dict)
    diagnostics: list[dict[str, Any]] = field(default_factory=list)
    workspace_lock_status: str = "not_acquired"
    recovered_previous_run: bool = False
    uncertain_transactions: list[str] = field(default_factory=list)
    session_run_mode: str = "new"
    recovery_gate_decision: dict[str, Any] | None = None
```

### AgentGraph（智能体组件图）

包含配置、trace、policy、sandbox、command、tools、model、context、planner 和 evaluation harness lazy factory。**边界**：内部治理对象，不进入模型请求、不落盘、不写 trace；graph 内各组件各自产生 trace/audit/report。

```python
@dataclass
class AgentGraph:
    config: ProductionConfig
    trace: TraceRecorder
    interaction_controller: InteractionController
    workspace_state: WorkspaceStateManager
    project_index: ProjectIndex
    memory_pipeline: MemoryLearningPipeline
    policy_engine: PolicyEngine
    approval_gate: ApprovalGate
    sandbox_manager: SandboxManager
    command_executor: CommandExecutor
    mutation_manager: WorkspaceMutationManager
    edit_executor: EditExecutor
    tools: ToolRegistry
    plugin_manager: PluginManager
    verification_runner: VerificationRunner
    review_pipeline: ReviewPipeline
    prompt_assembly: PromptAssemblyPipeline
    model_runner: ModelRunner
    context_manager: ContextManager
    tool_executor: ToolExecutor
    tool_protocol: ToolProtocolEngine
    planner: Planner
    recovery_gate_decision: RecoveryGateDecision | None = None
    initialization_order: list[ComponentName] = field(default_factory=...)
    components: dict[ComponentName, ComponentState] = field(default_factory=dict)
    _evaluation_harness: EvaluationHarness | None = field(default=None, repr=False)
    _evaluation_harness_factory: Callable[[], EvaluationHarness] | None = field(default=None, repr=False)
    _cancellation_token_factory: Callable[[], Any] | None = field(default=None, repr=False)
```

### RecoveryGateDecision（会话恢复门禁决策）

恢复启动时的 fail-closed 决策。**边界**：内部治理对象；写入 trace `session.recovery_gate_completed`、session checkpoint/timeline 和 kernel final report；只有其 `resume_context` 的过滤投影可进入模型上下文。

```python
@dataclass(frozen=True)
class RecoveryGateDecision:
    session_id: str
    mode: str
    status: RecoveryGateStatus
    can_call_model: bool
    blockers: list[str]
    warnings: list[str]
    next_action: str
    resume_context: SessionResumeContext
```

### 关键枚举值域

```python
class KernelStatus(str, Enum):   # KernelContext.status
    NEW = "new"
    BOOTING = "booting"
    READY = "ready"
    RUNNING = "running"
    CANCELLING = "cancelling"
    SHUTTING_DOWN = "shutting_down"
    FINALIZED = "finalized"
    FAILED = "failed"

class RunStatus(str, Enum):      # AgentRun.status
    CREATED = "created"
    RUNNING = "running"
    COMPLETED = "completed"
    BLOCKED = "blocked"
    FAILED = "failed"
    CANCELLED = "cancelled"

class SessionStatus(str, Enum):  # AgentSession.status
    CREATED = "created"
    ACTIVE = "active"
    CLOSING = "closing"
    CLOSED = "closed"
    FAILED = "failed"
    CANCELLED = "cancelled"
    RECOVERED = "recovered"

class ComponentState(str, Enum): # AgentGraph.components values
    PENDING = "pending"
    INITIALIZED = "initialized"
    READY = "ready"
    FAILED = "failed"
    STOPPED = "stopped"

class RecoveryGateStatus(str, Enum): # RecoveryGateDecision.status
    READY_TO_CONTINUE = "ready_to_continue"
    READY_TO_RESUME = "ready_to_resume"
    NEEDS_REVIEW = "needs_review"
    BLOCKED = "blocked"
```

`ComponentName` 枚举有 22 个成员：`CONFIGURATION`、`OBSERVABILITY`、`INTERACTION`、`WORKSPACE_STATE`、`PROJECT_INDEX`、`MEMORY`、`POLICY`、`SANDBOX`、`COMMAND`、`MUTATION`、`EDIT`、`TOOLS`、`PLUGINS`、`TOOL_EXECUTOR`、`TOOL_PROTOCOL`、`VERIFICATION`、`REVIEW`、`EVALUATION`、`INSTRUCTIONS`、`MODEL`、`CONTEXT`、`PLANNER`。

### 数据流概述

`RunIdentity` 由 `KernelBootstrap.boot()` 基于 `SessionLaunch` 创建后，其 id 字段被注入到所有下游对象：`TraceRecorder` 用 `run_id` 标记 event/span，`ContextManager` 用 `run_id/session_id/task_id` 标记 `ContextItem`，`ModelTurnRequest` 携带全部三个 id，`EvaluationTaskResult` 引用 `run_id`。`KernelContext` 在 boot 过程中从 `NEW` -> `BOOTING` -> `READY`，运行时从 `RUNNING` -> `CANCELLING`/`SHUTTING_DOWN` -> `FINALIZED`/`FAILED`。`AgentGraph` 是纯内存依赖容器，graph 生命周期结束后各组件的 store（`context.sqlite3`、`planner state.json`、`events.jsonl`、`tool_protocol.sqlite3`、`session_index.sqlite3`）独立存在。

## 谁生成这些对象

`KernelBootstrap.boot()` 生成 `RunIdentity` 与 `KernelContext`，并通过 `SessionRecoveryGate.evaluate()` 生成 `RecoveryGateDecision`；`AgentGraphBuilder.build()` 按 initialization order 生成 `AgentGraph`。`RunLifecycleManager` 创建/更新 `AgentRun`、`AgentSession` 与 `LifecycleEvent`，`AgentKernel` 在 task 运行中推进状态。

## 谁消费这些对象

`AgentKernel.run_task()` 消费 graph/context/run/session 和 `RecoveryGateDecision`；gate 放行才构造 AgentLoop。identity 的 ids 进入 model request、context、trace 与 evaluation 引用；完整 graph/kernel context/run/session 不进入模型。

## 是否落盘

graph、KernelContext、RunIdentity、AgentRun/Session 本体只在内存；lifecycle event 写 trace，planner/context/workspace 子组件使用各自 store。`SessionStore` 另写 `.singularity/session_index.sqlite3`，保存 session、run、checkpoint 与 timeline 摘要。最终 run/session 状态与 diagnostics 投影进 planner `final_report.json/.md`、kernel final report 和 evaluation result。

## 是否进入 trace / audit

`KernelBootstrap` 和 `AgentKernel` 写 `session.created`、`session.continue_requested`、`session.resume_requested`、`session.recovery_gate_started`、`session.recovery_gate_completed`、`session.recovery_blocked`；`RunLifecycleManager` 发出 boot/session/task/finalization/shutdown lifecycle events，payload 来自状态、component health、diagnostics 与 final report；TraceRecorder 写 `events.jsonl`。Kernel 本身不写 policy audit，各执行组件经共享 PolicyEngine 写 audit。

## 失败路径

组件构图失败抛 `AgentGraphInitializationError`，bootstrap 包装为 `KernelBootstrapError`，同时把 run 标记为 failed 并保留可复制 session 命令；task 运行异常/取消由 `KernelError`/`CancellationError` 与 `FAILED/BLOCKED/CANCELLED` lifecycle 表达，diagnostics 保留异常类型/脱敏消息并执行 shutdown。恢复门禁发现 external change、rollback conflict、unfinished mutation、stale lock、leftover sandbox、context recovery failed 或 pending/running tool 时，`AgentKernel.run_task()` 在模型调用前返回 blocked/needs-review 报告。

## 当前结构问题

`AgentGraph` 是依赖所有权边界而非持久化 schema；session index、trace、workspace state 和 planner state 是并行持久化层。新增组件必须更新 initialization order、health/shutdown、KernelContext diagnostics、session recovery gate 和模块文档，不能只加字段后依赖隐式关闭。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
