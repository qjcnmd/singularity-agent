# AgentLoop 主循环模块数据流

模块数据流文档 ID: agent-loop

源码证据路径:
- src/singularity/agent_loop.py
- src/singularity/agent_loop_completion.py
- src/singularity/agent_loop_failure_recovery.py
- src/singularity/agent_loop_turns.py
- src/singularity/error_codes.py

关键符号:
- AgentLoop
- AgentLoop.run
- AgentLoopResult
- AgentLoopStatus
- CompletionGate
- FailureRecoveryCoordinator
- TurnCoordinator

字段清单:
- AgentLoopResult: status, final_answer, turn, error_code, diagnostics

## 这一层解决什么问题

AgentLoop（智能体主循环）负责把 planner 状态、上下文、模型单轮请求、工具协议结果和最终报告串成一个可中断、可重试、可追踪的执行循环。

## 当前源码位置

- src/singularity/agent_loop.py
- src/singularity/agent_loop_completion.py
- src/singularity/agent_loop_failure_recovery.py
- src/singularity/agent_loop_turns.py
- src/singularity/error_codes.py

## 关键类、函数、字段

关键符号和字段清单按源码声明顺序列出，便于和对象流小节对照。

## 真实运行时调用链

`AgentKernel.run_task()` 构造 `AgentLoop` -> `AgentLoop.run()` 创建 `RunController` 并启动 planner 状态；`run()` 内部的 `run_turn()` callback 委托 `TurnCoordinator.run_turn()`，逐 turn 调用 `planner.step()`、`ModelRunner.build_request_from_context()`、`ModelRunner.run_turn()`、`ToolProtocolEngine.process_model_turn()`。completion/final review 由 `CompletionGate.attempt_finalize()` 调用 `Planner.finalize()` 或生成 completion outcome；工具、验证和 completion 失败的 repair/replan 由 `FailureRecoveryCoordinator.maybe_analyze_failure()` 协调。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例：`AgentKernel.run_task()` -> `AgentLoop.run()` -> `TurnCoordinator.run_turn()` -> `ModelRunner.build_request_from_context()` 先生成对象 `ModelTurnRequest`，`ModelRunner.run_turn()` 返回 `ModelTurnResult` 后交给 `ToolProtocolEngine.process_model_turn()`。每个 turn 在构造模型请求前先生成 `turn_action_id="turn_N"` 并传给 `Planner.filtered_tools()`；Planner 先通过 `PlannerPolicy.is_allowed()` 对当前 phase 的 allowed tools、allowed actions、permission level、mutation manager 和 command executor 要求求交，得到与 `authorize_tool_call()` 同源的可授权集合，再叠加 benchmark constraints 与 repair contract 生成同一份 deterministic projection，并把 `tool.exposure_decided` trace event 与 `ModelTurnRequest.action_id` 绑定。模型可见 tool schema、`ToolChoicePolicy.allowed_tool_names` 和 `tool.exposure_decided.selected_tools` 必须来自同一 selected tool 集合；semantic rolling plan 只能进入 planner context，不会扩大当前 phase 可暴露工具。工具结果通过 `ContextManager.add_tool_protocol_result()` 和 `Planner.update_from_tool_result()` 写入 `context.sqlite3`、planner evidence 和 trace 事件；关键 tool/verification/policy/sandbox/task outcome evidence 由 `EvidenceLedger.add_*()` typed helper 写入 JSON 投影。当 completion gate 通过时，`CompletionGate.attempt_finalize()` 调用 `Planner.finalize()`，`Finalizer.build()` 通过 typed evidence helper 生成 `FinalReport`，然后写入 `final_answer` trace event。若模型失败，`AgentLoop._outcome_from_model_failure()` 归类 provider 错误，`AgentLoop._terminal_result_from_outcome()` 返回带 `error_code` 的 `AgentLoopResult`。

## 真实对象完整结构

### AgentLoopResult（智能体主循环结果）

AgentLoop 执行的最终返回值。**边界**：内部治理对象，不落盘为独立文件；投影进 evaluation `result.json`、`report.json`、`report.md` 和 trace `final_answer` event。

```python
@dataclass(frozen=True, eq=False)
class AgentLoopResult:
    status: AgentLoopStatus
    final_answer: str
    turn: int
    error_code: str | None = None
    diagnostics: dict[str, Any] | None = None
```

### AgentLoopStatus（主循环状态枚举）

`AgentLoopResult.status` 的枚举类型，由 `AgentLoop` 内部各终止分支选择。

```python
class AgentLoopStatus(StrEnum):
    COMPLETED = "completed"
    BLOCKED = "blocked"
    MAX_TURNS_EXCEEDED = "max_turns_exceeded"
    FAILED = "failed"
```

### ErrorCode（错误码注册表）

`AgentLoop` 不再在终止分支中维护分散的错误码字面量集合。`max_turns_exceeded`、`completion_rejected`、`final_review_rejected`、`model_runner_failed`、`repair_budget_exceeded` 以及 failure-analysis 排除集合来自 `singularity.error_codes.ErrorCode` 和 `FAILURE_ANALYSIS_EXCLUDED_ERROR_CODES`；对外仍序列化为同名字符串。工具 observation 的 `error_code` 由 `RunOutcomeReducer.protocol_result_to_outcome()` 按 `TOOL_BLOCKING_ERROR_CODES`、`TOOL_REPLAN_ERROR_CODES` 和 `TOOL_RETRYABLE_ERROR_CODES` 归类为 blocked、replan 或 retry，不在 `AgentLoop.run()` 内重复判定。

### 数据流概述

`AgentKernel.run_task()` 构造 `AgentLoop`，其 `run()` 内部创建 `RunController`，再把每一轮执行封装在局部 `run_turn()` callback 中并委托 `TurnCoordinator.run_turn()`：`planner.step()`、`ModelRunner.build_request_from_context()`、`ModelRunner.run_turn()`、`ToolProtocolEngine.process_model_turn()`。completion gate 通过时 `CompletionGate.attempt_finalize()` 通过 `AgentLoop` 注入的 result factory 构造 `status=completed` 的 `AgentLoopResult`；不可重试失败由 `AgentLoop._terminal_result_from_outcome()` 构造 `blocked`/`failed`；turn 达上限由 `on_max_turns()` 构造 `max_turns_exceeded`。`AgentLoopResult` 不进入模型请求；evaluation 运行投影进 `result.json`/`report.json`/`report.md`，CLI 输出 `final_answer` 给用户。

完成判定只消费 AgentLoop 内部 evidence：`Planner.update_from_verification()` 通过 `EvidenceLedger.add_verification_result()` 写入的最新 `completion_assessment.status` 必须为 `ready` 或 `ready_with_warnings`，`Planner.assess_completion()` 必须没有 unmet，`Planner.finalize()` 的 final review 必须没有 blocking finding 且 `FinalReport.status=completed`。`Finalizer.build()` 通过 `latest_verification_result()`、`policy_records()`、`sandbox_records()`、`tool_result_records()` 汇总 completion/report 关键 bucket，避免从裸 dict 随意读取缺字段。 当这些条件满足时，`Planner.finalize()` 会清理已经被最新 final report 解决的 completion blocker（例如 `required_verifications_passed`、`unresolved_failures_empty`、旧的 sandbox backend unavailable 记录），再由 `CompletionGate.attempt_finalize()` 返回 `AgentLoopStatus.COMPLETED`。未被最新证据解决的 policy、approval、workspace conflict、sandbox/backend unavailable 当前失败仍保持 fail-closed，不会被 reducer 或 finalizer 改写成 completed。

## 谁生成这些对象

- `AgentLoop.run()` 内部的 `run_turn()` 委托 `TurnCoordinator.run_turn()`；当 completion gate 通过时，`CompletionGate.attempt_finalize()` 通过注入的 result factory 构造 `status=completed` 的 `AgentLoopResult`；`on_max_turns()` 构造 `status=max_turns_exceeded` 的结果。
- `_terminal_result_from_outcome()` 把不可重试的 `ExecutionOutcome` 映射成 `blocked` 或 `failed`，同时把 `outcome.to_dict()` 放入 `diagnostics`。`AgentLoopStatus` 由这些构造点直接选择，不存在第二套字符串状态 alias。

## 谁消费这些对象

- `AgentKernel.run_task()` 接收 `AgentLoopResult`，更新 `AgentRun`/`AgentSession` 生命周期并返回 CLI；`EvaluationRunner.run_task()` 读取其 `status`、`turn`、`final_answer` 和 `error_code` 生成 `EvaluationTaskResult`。
- `AgentLoopResult` 不进入模型请求。模型只在结果生成前接收 `ModelRunner.build_request_from_context()` 构造的 request；结果生成后执行已经终止。
- `RunOutcomeReducer` 只把 `ExecutionOutcomeStatus.SUCCESS` 且 `next_action="finalize"` 的 outcome 归约为 terminal completed；`sandbox_unavailable`、policy denied、protected path、cwd denied 等 runtime error code 仍归约为 blocked。
- CLI 将 `final_answer` 输出给用户，并依据最终状态/内核错误确定退出；targeted replay 读取同一结果生成 `TargetedFailureReplayResult`。

## 是否落盘

- `AgentLoopResult` 没有独立 store。`CompletionGate.attempt_finalize()` / `_terminal_result_from_outcome()` / `on_max_turns()` 先写 `final_answer` trace event；evaluation 运行再把结果投影进 `<evaluation_run>/result.json`、`report.json` 和 `report.md`。
- 主循环创建或复用的 `ContextManager` 把消息、观察和 bundle 写入当前 trace run 目录下的 `context.sqlite3`；该数据库保存的是循环输入证据，不是 `AgentLoopResult` 序列化副本。

## 是否进入 trace / audit

- `_record_outcome_context()` 写 `execution_outcome` event，并把同一 outcome 加入 planner context；模型失败由 `_record_model_failure()` 写 `model_failure`，超 turn 另写 `error(type=MaxTurnsExceeded)`。
- `Planner.filtered_tools()` 在每个模型 turn 前写 `tool.exposure_decided` trace event，payload 只包含 `selected_tools`、blocked/deferred/suppressed tool 名称、`reason_code`、`stage_basis`、phase、policy/sandbox/constraint factors 和 action id；不包含 raw prompt、raw response、raw patch text、secret、文件内容或 evaluator-only metadata。
- 所有终止分支写 `final_answer` event，payload 来源是 `turn` 与最终文本；trace 记录的是这些事件和 outcome，而不是完整 `AgentLoopResult` 对象。
- AgentLoop 自身不写 policy audit。tool/command/verification 触发的 `PolicyRequest`/`PolicyDecision` 由相应执行器和 policy audit ledger 记录。

## 失败路径

- 模型失败先由 `_outcome_from_model_failure()` 区分 retryable、外部依赖阻塞与 fatal，并设置 `model_runner_failed`、`invalid_json`、`unknown_tool` 或 `schema_mismatch`；retryable/replan 不终止，blocked/fatal 经 `_terminal_result_from_outcome()` 返回结果。
- completion evidence 不足生成 `completion_rejected` 并继续；final review 未通过生成 `final_review_rejected`，可继续时 replan，不可继续时返回 `blocked`。repair contract 不满足可返回带具体 error code 的 blocked outcome。
- turn 达到上限返回 `max_turns_exceeded`。未在这些 outcome 分支内转换的异常继续向 `AgentKernel.run_task()` 传播，由 kernel 生成失败 lifecycle/final report，而不是伪造 completed 结果。

## 当前结构问题

`AgentLoop.run()` 仍是对外 facade，负责 context/controller 生命周期和 max-turn callback；单 turn 编排已由 `TurnCoordinator` 承担，completion/final review 已由 `CompletionGate` 承担，failure-analysis/replan 已由 `FailureRecoveryCoordinator` 承担。新增状态时仍必须同时检查 kernel、evaluation 和 targeted replay 的消费分支，避免状态存在但报告层无法分类。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
