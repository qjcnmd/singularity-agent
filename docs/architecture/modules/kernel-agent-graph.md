# KernelBootstrap / AgentGraph / AgentKernel模块数据流

模块数据流文档 ID: kernel-agent-graph

源码证据路径:
- src/singularity/kernel/bootstrap.py
- src/singularity/kernel/graph.py
- src/singularity/kernel/agent_kernel.py
- src/singularity/kernel/models.py

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

字段清单:
- AgentGraph: config, trace, interaction_controller, workspace_state, project_index, memory_pipeline, policy_engine, approval_gate, sandbox_manager, command_executor, mutation_manager, edit_executor, tools, plugin_manager, verification_runner, review_pipeline, prompt_assembly, model_runner, context_manager, tool_executor, tool_protocol, planner, initialization_order, components, _evaluation_harness, _evaluation_harness_factory, _cancellation_token_factory
- RunIdentity: run_id, session_id, task_id
- AgentRun: identity, user_goal, status, started_at, ended_at, final_answer, error
- AgentSession: identity, status, started_at, ended_at, recovered_previous_run
- KernelContext: project_root, identity, run, session, status, components, diagnostics, workspace_lock_status, recovered_previous_run, uncertain_transactions
- LifecycleEvent: event_type, run_id, session_id, task_id, timestamp, payload

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

`KernelBootstrap.boot()` -> `AgentGraphBuilder.build()` -> `AgentKernel.boot()` -> `AgentKernel.run_task()` -> `AgentLoop.run()`。失败时 `KernelBootstrapError` 或 `KernelError` 携带 final report / diagnostics。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例：`KernelBootstrap.boot()` -> `AgentGraphBuilder.build()` 先生成对象 `RunIdentity`、`KernelContext` 和 `AgentGraph`，并把 trace、policy、sandbox、tools、model、context、planner 按 initialization order 放入 graph。`AgentKernel.run_task()` 读取 `KernelContext.run/session`，创建 `AgentLoop.run()` 并消费返回的 `AgentLoopResult`；`RunLifecycleManager` 生成 `AgentRun`、`AgentSession` 和 `LifecycleEvent`，这些 lifecycle event 写入 trace `events.jsonl`。构图失败抛 `AgentGraphInitializationError` 并由 `KernelBootstrapError` 携带 diagnostics；运行失败由 kernel finalization 写 final report/trace，而不是写入模型请求。

## 真实对象完整结构

- `AgentGraph（智能体组件图）` 完整字段列在字段清单中，包含配置、trace、policy、sandbox、command、tools、model、context、planner 和 evaluation harness lazy factory。
- `KernelContext（内核运行上下文）` 保存 project root、identity、run/session 状态、组件状态、diagnostics、workspace lock 和恢复信息。
- `RunIdentity（运行标识）` 统一 run/session/task id，供 trace、context、kernel lifecycle 和 evaluation result 引用。

## 谁生成这些对象

`KernelBootstrap.boot()` 生成 `RunIdentity` 与 `KernelContext`；`AgentGraphBuilder.build()` 按 initialization order 生成 `AgentGraph`。`RunLifecycleManager` 创建/更新 `AgentRun`、`AgentSession` 与 `LifecycleEvent`，`AgentKernel` 在 task 运行中推进状态。

## 谁消费这些对象

`AgentKernel.run_task()` 消费 graph/context/run/session 并构造 AgentLoop。identity 的 ids 进入 model request、context、trace 与 evaluation 引用；完整 graph/kernel context/run/session 不进入模型。

## 是否落盘

graph、KernelContext、RunIdentity、AgentRun/Session 本体只在内存；lifecycle event 写 trace，planner/context/workspace 子组件使用各自 store。最终 run/session 状态与 diagnostics 投影进 planner `final_report.json/.md` 和 evaluation result。

## 是否进入 trace / audit

`RunLifecycleManager` 发出 boot/session/task/finalization/shutdown lifecycle events，payload 来自状态、component health、diagnostics 与 final report；TraceRecorder 写 `events.jsonl`。Kernel 本身不写 policy audit，各执行组件经共享 PolicyEngine 写 audit。

## 失败路径

组件构图失败抛 `AgentGraphInitializationError`，bootstrap 包装为 `KernelBootstrapError`；task 运行异常/取消由 `KernelError`/`CancellationError` 与 `FAILED/BLOCKED/CANCELLED` lifecycle 表达，diagnostics 保留异常类型/脱敏消息并执行 shutdown。

## 当前结构问题

`AgentGraph` 是依赖所有权边界而非持久化 schema；新增组件必须更新 initialization order、health/shutdown、KernelContext diagnostics 和模块文档，不能只加字段后依赖隐式关闭。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
