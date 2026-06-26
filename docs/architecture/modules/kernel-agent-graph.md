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

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

`KernelBootstrap.boot()` -> `AgentGraphBuilder.build()` -> `AgentKernel.boot()` -> `AgentKernel.run_task()` -> `AgentLoop.run()`。失败时 `KernelBootstrapError` 或 `KernelError` 携带 final report / diagnostics。

## 真实对象完整结构

- `AgentGraph（智能体组件图）` 完整字段列在字段清单中，包含配置、trace、policy、sandbox、command、tools、model、context、planner 和 evaluation harness lazy factory。
- `KernelContext（内核运行上下文）` 保存 project root、identity、run/session 状态、组件状态、diagnostics、workspace lock 和恢复信息。
- `RunIdentity（运行标识）` 统一 run/session/task id，供 trace、context、kernel lifecycle 和 evaluation result 引用。

## 谁生成这些对象

这些对象由上文列出的源码组件在运行链路中生成。生成动作必须来自当前源码路径，不允许由文档、测试夹具或解释性包装层伪造。

## 谁消费这些对象

消费方是同一调用链后续组件、trace/audit 记录器、报告生成器或持久化 store。文档只列当前源码中真实调用的消费方。

## 是否落盘

落盘只通过当前源码中的 trace store、SQLite store、workspace state、evaluation output 或 manifest/report 写入路径发生。没有落盘代码的对象只在内存中传递。

## 是否进入 trace / audit

进入 trace / audit 的内容以 `TraceRecorder`、`JsonlTraceRecorder`、`TraceArtifactStore`、policy audit ledger 和相关 `record` / `emit` 调用为准。对象进入模型前必须经过当前工具协议、上下文组装和 redaction 逻辑。

## 失败路径

失败路径由当前源码中的异常、状态枚举、policy decision、verification result、planner outcome 和 result/report 字段表达。不得用旧 schema 或旧命名补充解释。

## 当前结构问题

当前结构仍大量使用字典 payload 连接组件，维护时最容易发生字段漂移。字段清单必须由源码校验脚本约束，不能只依赖人工描述。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
