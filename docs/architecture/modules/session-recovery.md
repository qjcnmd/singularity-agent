# Session Recovery模块数据流

模块数据流文档 ID: session-recovery

源码证据路径:
- src/singularity/session/models.py
- src/singularity/session/store.py
- src/singularity/session/history.py
- src/singularity/session/recovery.py
- src/singularity/cli.py
- src/singularity/kernel/bootstrap.py

关键符号:
- SessionSummary
- SessionRun
- SessionCheckpoint
- SessionTimelineEvent
- SessionDetail
- SessionResumeContext
- RecoveryGateDecision
- SessionLaunch
- SessionStore
- SessionStore.prepare_launch
- SessionStore.start_run
- SessionStore.finish_run
- SessionStore.record_checkpoint
- SessionStore.append_timeline_event
- SessionHistoryReader
- SessionHistoryReader.build_resume_context
- SessionHistoryReader.build_show_summary
- SessionRecoveryGate
- SessionRecoveryGate.evaluate
- continue_session
- resume_session_command
- KernelBootstrap.boot

字段清单:
- SessionSummary: session_id, project_root, user_goal, task_id, status, state, created_at, updated_at, last_run_id, last_task_status, continue_command, resume_command, show_command
- SessionRun: run_id, session_id, task_id, mode, user_goal, trace_run_dir, status, started_at, ended_at, final_report_ref, summary
- SessionCheckpoint: checkpoint_id, session_id, run_id, task_id, kind, summary, payload, created_at
- SessionTimelineEvent: event_id, session_id, run_id, task_id, event_type, summary, payload, created_at
- SessionDetail: session, runs, checkpoints, timeline
- SessionResumeContext: session_id, user_goal, current_instruction, dialogue_summary, planner, workspace, verification, tool_protocol, failures
- RecoveryGateDecision: session_id, mode, status, can_call_model, blockers, warnings, next_action, resume_context
- SessionLaunch: session_id, task_id, run_id, mode, user_goal, previous_run_id, previous_status, previous_trace_run_dir

## 这一层解决什么问题

Session Recovery 层把一次用户可打开的历史会话和每次执行尝试分开：`session_id` 表示稳定历史会话，`task_id` 表示该会话内当前任务，`run_id` 表示每次启动尝试。它负责 list/show/continue/resume CLI、session index、timeline、checkpoint、恢复摘要和恢复门禁。

## 当前源码位置

- src/singularity/session/models.py
- src/singularity/session/store.py
- src/singularity/session/history.py
- src/singularity/session/recovery.py
- src/singularity/cli.py
- src/singularity/kernel/bootstrap.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

新任务：`run_goal()` -> `_run_with_config()` -> `KernelBootstrap.boot()` -> `SessionStore.prepare_launch(mode="new")` -> `SessionStore.start_run()` -> `SessionRecoveryGate.evaluate(mode="new")` -> `AgentKernel.run_task()`。

追加指令：`continue_session()` -> `_run_with_config()` -> `KernelBootstrap.boot()` -> `SessionStore.prepare_launch(mode="continue")` -> `SessionHistoryReader.build_resume_context()` -> `SessionRecoveryGate.evaluate()` -> `Planner.continue_with_instruction()` -> `AgentKernel.run_task()`。

中断恢复：`resume_session_command()` -> `_run_with_config()` -> `KernelBootstrap.boot()` -> `SessionStore.prepare_launch(mode="resume")` -> `CrashRecoveryManager.inspect(session_id=identity.session_id)` -> `SessionHistoryReader.tool_protocol_report()` -> `SessionRecoveryGate.evaluate()` -> gate 放行后进入 `AgentLoop.run()`，不放行则 `AgentKernel._blocked_by_recovery_gate()` 返回 final report。

历史查看：`session_show()` -> `SessionStore.show_session()` -> `SessionHistoryReader.build_show_summary()` -> 聚合 planner、上一轮 context.sqlite3 对话摘要、tool_protocol.sqlite3 recovery report、trace summary、workspace checkpoint 和失败摘要。

## 真实任务中的对象流

以用户在修复 `quicksort.py` 时电脑重启为例，第一次启动调用链是 `run_goal()` -> `KernelBootstrap.boot()` -> `SessionStore.prepare_launch(mode="new")` -> `TraceRecorder.create()` -> `SessionStore.start_run()` -> `AgentGraphBuilder.build()` -> `AgentKernel.run_task()`。若进程被杀，run 行保持 active；下次执行 `sg resume <session_id>` 时，调用链是 `resume_session_command()` -> `KernelBootstrap.boot()` -> `SessionStore.prepare_launch(mode="resume")` -> `WorkspaceStateManager.recover_session(session_id)` -> `CrashRecoveryManager.inspect(session_id=session_id)` -> `ToolProtocolRecoveryManager.inspect()` -> `PlannerStore.load()` -> `SessionHistoryReader.build_resume_context()` -> `SessionRecoveryGate.evaluate()` -> `AgentKernel.run_task()`。`SessionHistoryReader.build_resume_context()` 读取上一轮 context.sqlite3 安全对话摘要和 trace summary，生成过滤后的 `SessionResumeContext`。`SessionRecoveryGate.evaluate()` 发现 external change、rollback conflict、unfinished mutation、leftover sandbox、stale lock、pending approval 或 running/pending tool 时返回 `can_call_model=False`，`AgentKernel.run_task()` 写 `session.recovery_blocked` 并停止在 review 状态；没有 blocker 时才把 `SessionResumeContext` 作为 summary context 写入 `context.sqlite3` 并调用模型。

## 真实对象完整结构

### SessionSummary（会话列表条目）

用户通过 `sg session list` 看到的稳定会话摘要。**边界**：落盘到 `sessions` 表；命令字符串供 CLI 展示，不进入模型。

```python
@dataclass(frozen=True)
class SessionSummary:
    session_id: str
    project_root: str
    user_goal: str
    task_id: str
    status: SessionStatus
    state: SessionState
    created_at: str
    updated_at: str
    last_run_id: str | None = None
    last_task_status: str | None = None
    continue_command: str = ""
    resume_command: str = ""
    show_command: str = ""
```

### SessionRun（单次执行尝试）

一次启动尝试的记录。**边界**：落盘到 `runs` 表；`trace_run_dir` 指向该 run 的 trace 目录，恢复上一轮 tool protocol 状态时使用。

```python
@dataclass(frozen=True)
class SessionRun:
    run_id: str
    session_id: str
    task_id: str
    mode: SessionRunMode
    user_goal: str
    trace_run_dir: str
    status: SessionStatus
    started_at: str
    ended_at: str | None = None
    final_report_ref: str | None = None
    summary: dict[str, Any] = field(default_factory=dict)
```

### SessionCheckpoint（会话检查点）

workspace baseline、recovery gate、verification 等可展示检查点。**边界**：落盘到 `checkpoints` 表；payload 已是摘要，不作为 raw trace 回灌模型。

```python
@dataclass(frozen=True)
class SessionCheckpoint:
    checkpoint_id: str
    session_id: str
    run_id: str
    task_id: str
    kind: SessionCheckpointKind
    summary: str
    payload: dict[str, Any]
    created_at: str
```

### SessionTimelineEvent（会话时间线事件）

`sg session show --timeline` 的展示单元。**边界**：落盘到 `timeline` 表；payload 用于用户审查，不直接进入模型。

```python
@dataclass(frozen=True)
class SessionTimelineEvent:
    event_id: str
    session_id: str
    run_id: str | None
    task_id: str | None
    event_type: str
    summary: str
    payload: dict[str, Any]
    created_at: str
```

### SessionDetail（会话详情）

`SessionStore.show_session()` 的聚合结果。**边界**：CLI 展示对象，不单独落盘。

```python
@dataclass(frozen=True)
class SessionDetail:
    session: SessionSummary
    runs: list[SessionRun]
    checkpoints: list[SessionCheckpoint]
    timeline: list[SessionTimelineEvent]
```

### SessionResumeContext（模型可见恢复摘要）

恢复时注入 context 的过滤摘要。**边界**：只在 `continue/resume` 且 gate 放行时由 `ContextManager.seed_session_resume_context()` 写入 `context.sqlite3`；不包含 raw trace、raw tool args/result、完整 stdout/stderr、policy audit 原文或 model payload。

```python
@dataclass(frozen=True)
class SessionResumeContext:
    session_id: str
    user_goal: str = ""
    current_instruction: str = ""
    dialogue_summary: list[dict[str, str]] = field(default_factory=list)
    planner: dict[str, Any] = field(default_factory=dict)
    workspace: dict[str, Any] = field(default_factory=dict)
    verification: dict[str, Any] = field(default_factory=dict)
    tool_protocol: dict[str, Any] = field(default_factory=dict)
    failures: dict[str, Any] = field(default_factory=dict)
```

### RecoveryGateDecision（恢复门禁决策）

恢复是否能调用模型的唯一门禁结果。**边界**：写 trace、session checkpoint/timeline 和 kernel final report；`can_call_model=False` 时不会调用模型。

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

### SessionLaunch（启动请求）

bootstrap 内部使用的启动解析结果。**边界**：不单独落盘；其字段用于创建 trace、session run 和恢复上一轮 trace/tool protocol 路径。

```python
@dataclass(frozen=True)
class SessionLaunch:
    session_id: str
    task_id: str
    run_id: str
    mode: SessionRunMode
    user_goal: str
    previous_run_id: str | None = None
    previous_status: str | None = None
    previous_trace_run_dir: str | None = None
```

### 关键枚举值域

```python
class SessionStatus(str, Enum):
    ACTIVE = "active"
    COMPLETED = "completed"
    BLOCKED = "blocked"
    FAILED = "failed"
    CANCELLED = "cancelled"
    INTERRUPTED = "interrupted"
    NEEDS_REVIEW = "needs_review"

class SessionState(str, Enum):
    ACTIVE = "active"
    RECOVERABLE = "recoverable"
    NEEDS_REVIEW = "needs_review"
    BLOCKED = "blocked"
    CLOSED = "closed"

class SessionRunMode(str, Enum):
    NEW = "new"
    CONTINUE = "continue"
    RESUME = "resume"

class SessionCheckpointKind(str, Enum):
    WORKSPACE = "workspace"
    CONTEXT = "context"
    TOOL_PROTOCOL = "tool_protocol"
    VERIFICATION = "verification"
    RECOVERY_GATE = "recovery_gate"

class RecoveryGateStatus(str, Enum):
    READY_TO_CONTINUE = "ready_to_continue"
    READY_TO_RESUME = "ready_to_resume"
    NEEDS_REVIEW = "needs_review"
    BLOCKED = "blocked"
```

### 数据流概述

`SessionStore` 是 SQLite index，保存用户可打开的历史会话摘要；planner/context/tool/workspace/trace 仍保持各自原有 store，不复制 raw trace。`SessionHistoryReader` 只读取这些 store 的安全摘要：planner state、workspace health、tool protocol recovery report、verification summary 和失败摘要。`SessionRecoveryGate` 对这些摘要做 fail-closed 判定；只有 `ready_to_continue/ready_to_resume` 会让 `AgentKernel` 进入模型调用。

## 谁生成这些对象

`SessionStore.prepare_launch()` 生成 `SessionLaunch`；`SessionStore.create_session()` / `start_run()` / `finish_run()` 生成并更新 `SessionSummary` 与 `SessionRun`；`SessionStore.record_checkpoint()` 生成 `SessionCheckpoint`；`SessionStore.append_timeline_event()` 生成 `SessionTimelineEvent`。`SessionHistoryReader.build_resume_context()` 生成 `SessionResumeContext`，`SessionRecoveryGate.evaluate()` 生成 `RecoveryGateDecision`。

## 谁消费这些对象

`KernelBootstrap.boot()` 消费 `SessionLaunch` 创建 trace/run identity 并启动恢复检查；`AgentGraphBuilder._build_model_context()` 只在 `continue/resume` 消费 `RecoveryGateDecision.resume_context`；`AgentKernel.run_task()` 消费 `RecoveryGateDecision.can_call_model` 决定是否进入 AgentLoop；CLI `session list/show/continue/resume` 消费 `SessionSummary`、`SessionDetail`、`SessionHistoryReader.build_show_summary()` 和 timeline/checkpoint 摘要。

## 是否落盘

`SessionStore` 写 `.singularity/session_index.sqlite3`，包含 `sessions`、`runs`、`checkpoints`、`timeline` 四张表。`SessionResumeContext` 不写 session index；它作为 `ContextItemType.SESSION_RESUME_CONTEXT` 写当前 run 的 `context.sqlite3`。完整 trace 仍在 `work/traces/runs/<run_id>/events.jsonl`，tool protocol 状态仍在同目录 `tool_protocol.sqlite3`，planner 状态仍在 `.singularity/planner/<session_id>/`。`build_show_summary()` 仅读取这些既有 store，不创建新的持久化旁路。

## 是否进入 trace / audit

`KernelBootstrap`/CLI 写 session timeline 的同时通过 `TraceRecorder.record()` 写 `session.created`、`session.continue_requested`、`session.resume_requested`、`session.recovery_gate_started`、`session.recovery_gate_completed`、`workspace.checkpoint_created`、`workspace.conflict_detected` 和 `session.recovery_blocked`。本层不写 policy audit；pending approval 和 policy blocker 只以摘要形式进入 recovery gate payload。

## 失败路径

`prepare_launch(mode="resume")` 对 closed session 抛 `ValueError`，但允许 active、recoverable、needs_review 和 blocked session 进入恢复门禁。`SessionRecoveryGate.evaluate()` 发现 external change、rollback conflict、corrupted workspace state、unfinished mutation、leftover sandbox、stale lock、pending approval、running tool、pending tool 或缺失 planner state 时设置 `can_call_model=False`；unfinished mutation、leftover sandbox 和 running tool 为 blocked，其余进入 needs_review。bootstrap 失败会把 run 标记为 failed 并写 `session.run_failed`，CLI 打印可复制恢复命令。

## 当前结构问题

session index 是摘要索引，不是 trace/planner/context/tool/workspace 的替代品。恢复上下文必须通过 `SessionHistoryReader` 过滤后进入模型，不能把 raw trace、raw tool args/result、stdout/stderr 全量或 policy audit 原文塞入 `ContextManager`。workspace 冲突处理仍依赖 `WorkspaceStateManager` 的 ownership/journal 判定；session 层只负责审计、门禁和用户可见入口。多 open workspace session 恢复时必须显式传递目标 `session_id`，不得让 recovery manager 自动选择第一个 open session。

## 维护规则

修改 session CLI、run identity、planner/context/tool/workspace/trace 恢复链路、checkpoint schema、timeline event 或 gate blocker 时，必须更新本文件、相关模块文档和 `docs/singularity.md`，并运行 `python scripts/verify_runtime_docs.py`。新增恢复场景必须同时有 session store/recovery/CLI 或 kernel 测试覆盖。
