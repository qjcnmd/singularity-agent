# Singularity 主链路完整调用链（全部分支路径）

> 基于 `origin/main@ab3080f3` + Phase 7 工作树源码核对：`agent_loop.py`(891行)、`run_controller.py`(479行)、`execution_outcome.py`(60行)、`error_codes.py`(123行)、`kernel/agent_kernel.py`(414行)、`tool_protocol/engine.py`(946行)。
> `[成功]` / `[失败]` / `[阻断]` 为关键分叉点；缩进表示嵌套层级。

---

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                        CLI 入口 (cli.py)                                     │
│         sg run "task" / sg continue <session_id> "..." / sg resume <id>      │
│         sg session list / sg session show <session_id> --timeline            │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│              ProductionConfig.from_cli()  (config.py:167-339)                │
│  四层配置解析：CLI args → 环境变量 → config.toml → 默认值                       │
│  输出：ProductionConfig { max_turns, sandbox, policy, model, … }              │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│              KernelBootstrap.boot(goal)  (kernel/bootstrap.py)               │
│                                                                             │
│  ├── SessionStore.prepare_launch()       → new/continue/resume 解析          │
│  │   ├── new      → 新 session_id/task_id/run_id                             │
│  │   ├── continue → 复用 session_id/task_id，新 run_id，追加用户指令           │
│  │   └── resume   → 复用 session_id/task_id，新 run_id，要求可恢复状态          │
│  ├── TraceRecorder.create()              → trace run 初始化                  │
│  ├── RunIdentity.new()                   → run / session / task id          │
│  ├── SessionStore.start_run()            → session_index.sqlite3 run 行       │
│  ├── RunLifecycleManager.create_run()    → 生命周期记录                     │
│  ├── WorkspaceLockManager.acquire_lock() → 工作区文件锁                      │
│  ├── WorkspaceStateManager.begin/recover_session()                           │
│  ├── CrashRecoveryManager.inspect()      → 只检查 stale lock / unfinished     │
│  │                                          mutation / leftover sandbox，     │
│  │                                          启动恢复时不静默清理              │
│  ├── SessionHistoryReader.build_resume_context()                             │
│  │      → 过滤聚合 planner/context/tool/workspace/trace/verification 摘要      │
│  ├── SessionRecoveryGate.evaluate()                                           │
│  │      → ready_to_continue / ready_to_resume / needs_review / blocked        │
│  └── AgentGraphBuilder.build()  (kernel/graph.py)                          │
│      ├── _build_infra()          → InteractionController + WorkspaceState    │
│      │                             + ProjectIndex + MemoryPipeline           │
│      ├── _build_policy_sandbox() → PolicyEngine + ApprovalGate              │
│      │                             + SandboxManager                         │
│      ├── _build_execution_core() → CommandExecutor                          │
│      │                             + WorkspaceMutationManager                │
│      │                             + EditExecutor                           │
│      ├── _build_tools_protocol() → ToolRegistry + PluginManager             │
│      │                             + ToolExecutor + ToolProtocolEngine      │
│      ├── _build_verification()   → VerificationRunner + ReviewPipeline      │
│      ├── _build_model_context()  → PromptAssemblyPipeline + ModelRunner     │
│      │                             + ContextManager                         │
│      │                             + SessionResumeContext(仅 continue/resume)│
│      ├── _create_planner()       → create_or_resume_planner()               │
│      │                             + Planner.continue_with_instruction()     │
│      └── _wire_planner()         → 注入依赖 + attach_producers              │
│                                                                             │
│  输出：AgentGraph（22+ 组件完整 wiring）                                     │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                                   ▼
╔═════════════════════════════════════════════════════════════════════════════╗
║           AgentKernel.run_task(goal)  (kernel/agent_kernel.py:95-177)       ║
║                        ★ 内核生命周期入口 ★                                   ║
╚═════════════════════════════════════════════════════════════════════════════╝
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  1. KernelStatus: READY → RUNNING                                           │
│  2. lifecycle.start_task(goal)                                              │
│  3. cancellation.throw_if_cancelled()                                       │
│                                                                             │
│  4. if recovery_gate_decision.can_call_model == False:                      │
│     ├── trace.record("session.recovery_blocked", decision)                  │
│     ├── planner.interrupt()/abort()                                         │
│     ├── shutdown(BLOCKED)                                                   │
│     └── return RunResult(BLOCKED)                                           │
│     该分支不创建 AgentLoop、不调用模型、不继续写 workspace。                 │
│                                                                             │
│  5. 组装 AgentLoop（注入 14 个依赖）                                          │
│     agent = AgentLoop(                                                      │
│         model_runner    = graph.model_runner,                               │
│         tools           = graph.tools,         # ToolRegistry               │
│         trace           = graph.trace,         # TraceStorageProtocol       │
│         console         = self.console,                                      │
│         max_turns       = graph.config.max_turns,                            │
│         planner         = graph.planner,                                     │
│         tool_executor   = graph.tool_executor,                               │
│         tool_protocol   = graph.tool_protocol,  # ToolProtocolEngine        │
│         prompt_assembly = graph.prompt_assembly,                             │
│         interaction_controller = self.interaction_controller,                │
│         context_manager = graph.context_manager,                             │
│         context_db_path = graph.config.context_db_path(...),                 │
│         strict          = graph.config.strict,                               │
│     )                                                                       │
│                                                                             │
│  6. agent_result = agent.run(user_goal)    ← 进入 AgentLoop（下详）          │
│     │                                                                       │
│     ▼  ★★★ AgentLoopStatus → RunStatus 映射 ★★★                            │
│     │                                                                       │
│     ┌─ AgentLoopStatus.COMPLETED ──────────────────────────────────────┐    │
│     │  │ lifecycle.mark_completed(final_answer)                        │    │
│     │  │ shutdown_reason = ShutdownReason.NORMAL                       │    │
│     │  │ result_status     = RunStatus.COMPLETED                       │    │
│     │  └──────────────────────────────────────────────────────────────┘    │
│     │                                                                       │
│     ┌─ AgentLoopStatus.BLOCKED ────────────────────────────────────────┐    │
│     │  │ lifecycle.mark_blocked(error_code + final_answer)              │    │
│     │  │ context.diagnostics.append({type, status, error_code, message})│    │
│     │  │ shutdown_reason = ShutdownReason.BLOCKED                      │    │
│     │  │ result_status     = RunStatus.BLOCKED                         │    │
│     │  └──────────────────────────────────────────────────────────────┘    │
│     │                                                                       │
│     ┌─ AgentLoopStatus.MAX_TURNS_EXCEEDED ─────────────────────────────┐    │
│     │  │ → 落入 else 分支（非 COMPLETED 也非 BLOCKED）                    │    │
│     │  │ lifecycle.mark_failed(error_code + final_answer)               │    │
│     │  │ context.diagnostics.append(...)                                │    │
│     │  │ shutdown_reason = ShutdownReason.ERROR                        │    │
│     │  │ result_status     = RunStatus.FAILED                          │    │
│     │  └──────────────────────────────────────────────────────────────┘    │
│     │                                                                       │
│     └─ AgentLoopStatus.FAILED ─────────────────────────────────────────┐    │
│        │ lifecycle.mark_failed(error_code + final_answer)               │    │
│        │ context.diagnostics.append(...)                                │    │
│        │ shutdown_reason = ShutdownReason.ERROR                        │    │
│        │ result_status     = RunStatus.FAILED                          │    │
│        └──────────────────────────────────────────────────────────────┘    │
│                                                                             │
│  7. shutdown(shutdown_reason) → ShutdownManager.shutdown()                  │
│     ├── planner.checkpoint()                                                │
│     ├── model_runner.close()                                                │
│     ├── command_executor.cleanup()                                          │
│     ├── sandbox_manager.cleanup()                                           │
│     ├── mutation_manager.rollback_pending()                                 │
│     ├── workspace_state.close_session()                                     │
│     ├── trace.close()                                                       │
│     └── workspace_lock.release()                                            │
│                                                                             │
│  8. final_report() → KernelFinalizer.finalize()                             │
│     ├── planner.finalize() → FinalReport                                    │
│     │     ├── VerificationRunner.assess() → VerificationSummary             │
│     │     ├── ReviewPipeline.assess()     → ReviewSummary                   │
│     │     └── Finalizer.build()           → FinalReport                     │
│     ├── workspace_state.get_workspace_health()                              │
│     ├── trace.final_report_summary(task_id)                                 │
│     ├── session_summary / checkpoint_summary / recovery_gate_summary         │
│     ├── lifecycle.summary()                                                 │
│     ├── memory_pipeline.ingest_session_end(final_reports, trace_summary)    │
│     └── trace.record("finalization.completed", final_report.to_dict())      │
│                                                                             │
│  9. return RunResult(final_answer, final_report, status, interaction_report)│
│                                                                             │
│  =======================================================================   │
│  异常路径（Kernel.run_task 的 try/except 块）                                  │
│  =======================================================================   │
│                                                                             │
│  ┌─ KeyboardInterrupt ─────────────────────────────────────────────────┐   │
│  │  │ interaction_controller.handle_command(CANCEL, "KeyboardInterrupt")│   │
│  │  │ cancellation.cancel(USER_INTERRUPTED, "KeyboardInterrupt")        │   │
│  │  │ KernelStatus → CANCELLING                                        │   │
│  │  │ lifecycle.mark_cancelled("KeyboardInterrupt")                     │   │
│  │  │ shutdown(KEYBOARD_INTERRUPT)                                     │   │
│  │  │ _finalize_after_shutdown("keyboard_interrupt", cancelled=True)    │   │
│  │  │ raise CancellationError("Cancelled by KeyboardInterrupt.")        │   │
│  │  └──────────────────────────────────────────────────────────────────┘   │
│  │                                                                         │
│  ┌─ CancellationError ─────────────────────────────────────────────────┐   │
│  │  │ interaction_controller.handle_command(CANCEL, "cancelled")        │   │
│  │  │ if not token.cancelled: cancel(USER_INTERRUPTED, "cancelled")     │   │
│  │  │ KernelStatus → CANCELLING                                        │   │
│  │  │ lifecycle.mark_cancelled("cancelled")                             │   │
│  │  │ shutdown(CANCELLED)                                              │   │
│  │  │ _finalize_after_shutdown("cancelled", cancelled=True)             │   │
│  │  │ re-raise                                                         │   │
│  │  └──────────────────────────────────────────────────────────────────┘   │
│  │                                                                         │
│  └─ Exception ─────────────────────────────────────────────────────────┐   │
│     │ lifecycle.mark_failed(exc)                                        │   │
│     │ context.diagnostics.append({type: exc.__name__, message: str(exc)})│   │
│     │ shutdown(ERROR)                                                  │   │
│     │ _finalize_after_shutdown("error", error=exc)                      │   │
│     │ if KernelError: re-raise else raise                               │   │
│     └──────────────────────────────────────────────────────────────────┘   │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                                   ▼
╔═════════════════════════════════════════════════════════════════════════════╗
║              AgentLoop.run(user_goal)  (agent_loop.py:150-327)              ║
║                        ★★★ 核心循环入口 ★★★                                 ║
╚═════════════════════════════════════════════════════════════════════════════╝
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  前置初始化                                                                  │
│                                                                             │
│  ├── controller = RunController(planner, trace)                             │
│  │   ├── if planner.state is None: controller.start(user_goal)              │
│  │   │   ├── planner.start_task(user_goal)                                  │
│  │   │   │   → TaskState { session_id, task_id, current_phase,              │
│  │   │   │       completion_criteria, lifecycle_status, … }                 │
│  │   │   └── RunLifecycleStatus: CREATED → RUNNING                          │
│  │   └── effective_goal = planner.state.effective_goal or user_goal         │
│  │                                                                          │
│  ├── context = ContextManager( … )  (如未注入)                               │
│  │   ├── system_prompt = SYSTEM_PROMPT (类常量，见 agent_loop.py:32-47)       │
│  │   ├── provider → ContextUsageReporter (仅诊断，不影响执行)                  │
│  │   ├── db_path → context.sqlite3 (ObservationStore, 9 张表)                │
│  │   └── continue/resume 时包含 ContextItemType.SESSION_RESUME_CONTEXT       │
│  │       只含历史摘要、planner 阶段、workspace 分类、工具/验证摘要和失败摘要，  │
│  │       不回灌 raw trace、raw tool args/result、完整 stdout/stderr。          │
│  │                                                                          │
│  ├── tool_schemas = tools.openai_tools(strict=self.strict)                  │
│  │   → [{"type":"function","function":{name,description,parameters}}, …]    │
│  └── model_runner = self.model_runner                                       │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                                   ▼
╔═════════════════════════════════════════════════════════════════════════════╗
║        RunController.run_loop()  (run_controller.py:395-431)                ║
║                      ★ Turn 循环生命周期 ★                                   ║
╚═════════════════════════════════════════════════════════════════════════════╝
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                                                                             │
│  if planner.state is None: start(user_goal)                                 │
│  else: _set_status(RUNNING)                                                 │
│                                                                             │
│  for turn in range(1, max_turns + 1):                                       │
│      │                                                                      │
│      ├── result = run_turn(turn)     ← 核心闭包（下方详图）                   │
│      │   │                                                                  │
│      │   ├── result is not None ───────────────────────────────────────┐   │
│      │   │   │  ★ AgentLoopResult 被返回 → 终止循环 ★                   │   │
│      │   │   │                                                          │   │
│      │   │   ├── current_status ∈ {CREATED, RUNNING, VERIFYING,        │   │
│      │   │   │      REPAIRING, FINAL_REVIEW, REPORTING}                │   │
│      │   │   │   │  这些是非终端状态                                      │   │
│      │   │   │   └── controller.complete()                              │   │
│      │   │   │       ├── RunLifecycleStatus → COMPLETED                 │   │
│      │   │   │       ├── planner.state.lifecycle_status = "completed"   │   │
│      │   │   │       ├── planner.state.touch()                          │   │
│      │   │   │       ├── planner.checkpoint()                           │   │
│      │   │   │       └── trace.record("task_lifecycle", …)              │   │
│      │   │   │                                                          │   │
│      │   │   │   (如果已经是终端状态如 BLOCKED/FAILED/CANCELLED，        │   │
│      │   │   │    则不调 complete())                                    │   │
│      │   │   │                                                          │   │
│      │   │   └── return result  → 回到 AgentKernel.run_task()           │   │
│      │   └──────────────────────────────────────────────────────────────┘   │
│      │                                                                      │
│      └── result is None ───────────────────────────────────────────────┐   │
│          │  ★ 继续下一 turn ★                                           │   │
│          └── next iteration                                             │   │
│                                                                             │
│  # 循环耗尽——所有 max_turns 次 turn 均返回 None                               │
│  result = on_max_turns(max_turns)                                           │
│  │                                                                          │
│  ├── apply_event(OUTCOME_RECORDED,                                          │
│  │       to_status=BLOCKED, terminal=True,                                  │
│  │       metadata={error_code: MAX_TURNS_EXCEEDED})                         │
│  └── return result   → AgentLoopResult(MAX_TURNS_EXCEEDED)                  │
│                                                                             │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                                   ▼
╔═════════════════════════════════════════════════════════════════════════════╗
║                run_turn(turn) 闭包 — 单 Turn 完整流程                        ║
║              (agent_loop.py:177-297, 闭包捕获 8+ 外部变量)                    ║
╚═════════════════════════════════════════════════════════════════════════════╝
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  步骤 ① —— 前置准备                                                          │
│                                                                             │
│  ├── _publish_progress(turn)                                                │
│  │   └── InteractionController.publish(ProgressEvent(phase, status, …))     │
│  │                                                                          │
│  ├── planner.step()              → 状态机推进 + completion gate 初检          │
│  │   ├── _auto_advance_before_step()                                        │
│  │   │   └── 根据 evidence 自动推进 current_phase                            │
│  │   └── assess_completion(mark_blocked=True)                               │
│  │                                                                          │
│  ├── context.set_user_goal(effective_goal)                                  │
│  │                                                                          │
│  ├── active_tool_schemas = planner.filtered_tools(tool_schemas, …)          │
│  │   └── 基于当前 phase / permission_level / benchmark 约束过滤工具 schema   │
│  │                                                                          │
│  └── allowed_tool_names = [name for tool in active_tool_schemas             │
│          if tool.get("function",{}).get("name")]                             │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  步骤 ② —— 构建模型请求                                                       │
│                                                                             │
│  request = model_runner.build_request_from_context(                         │
│      context,                                                               │
│      run_id          = trace.run_id,                                        │
│      session_id      = planner.session_id,                                  │
│      task_id         = planner.task_id,                                     │
│      phase_id        = planner.state.current_phase,                         │
│      action_id       = f"turn_{turn}",                                      │
│      purpose         = ModelPurpose.PLAN_NEXT_ACTION,                       │
│      allowed_tool_names,                                                    │
│      planner_context = planner.planner_context_message(),                   │
│      prompt_assembly,                                                       │
│      user_task       = effective_goal,                                      │
│      strict_tools    = self.strict,                                         │
│  )                                                                          │
│  → ModelTurnRequest {                                                       │
│      messages, tools, tool_choice,                                          │
│      context_metadata, trace_metadata,                                      │
│      model_config, max_tokens, temperature, …                               │
│  }                                                                          │
│                                                                             │
│  内部调用链：                                                                  │
│  ├── ContextManager.build_bundle()                                          │
│  │   ├── ContextAssembler.build_bundle()                                    │
│  │   │   ├── 分组 (group_items_by_layer)                                    │
│  │   │   ├── 评分 (score_items)                                             │
│  │   │   ├── 贪心选择 (select_items_greedy)                                  │
│  │   │   ├── 分层排序 (order_by_layer)                                        │
│  │   │   └── → ContextBundle { messages, budget, render_policy, … }        │
│  │   ├── ContextCompactionPlanner (检查是否需要压缩)                           │
│  │   ├── ContextCompactionExecutor (执行 LLM 压缩 / 纯规则压缩)                 │
│  │   └── ContextCompactionCommitter (提交压缩结果)                              │
│  ├── PromptAssemblyPipeline.build_for_model_turn()                          │
│  │   └── hierarchy → resolver → manifest → injection → compiler            │
│  └── ModelToolRenderer.render() → [ModelToolSchema]                         │
│                                                                             │
│  planner.record_instruction_prompt_observation(                             │
│      dict(prompt_assembly.summary()))                                       │
│  → 记录到 EvidenceLedger.instruction_prompts                                 │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│  步骤 ③ —— 运行模型推理                                                       │
│                                                                             │
│  result = model_runner.run_turn(request)                                    │
│  → ModelTurnResult {                                                        │
│      status: ModelTurnStatus,      # SUCCESS / ERROR / VALIDATION_ERROR …  │
│      assistant_message: …,                                                  │
│      tool_calls: [ModelToolCall, …],                                        │
│      error: ModelError | None,                                              │
│      validation: ModelValidationResult | None,                              │
│      usage: ModelUsageInfo,                                                 │
│      request_id, response_id, …                                             │
│  }                                                                          │
│                                                                             │
│  内部调用链（model/runner.py:915 行）：                                        │
│  ├── ModelProviderRegistry.select_provider()                                │
│  ├── ModelBudgetManager.check_budget()                                      │
│  ├── ModelTurnRequestBuilder.build_request() → ProviderRequest              │
│  ├── provider.complete(ProviderRequest) → ProviderResponse                  │
│  │   ├── HTTP POST → {base_url}/v1/chat/completions                         │
│  │   ├── streaming.py 处理 SSE 流 (data: [DONE])                             │
│  │   └── retry.py 指数退避重试（max_retries: 3, timeout）                      │
│  │       ├── [成功] → 返回 response                                         │
│  │       ├── [可重试错误] → 退避等待 → 重试                                    │
│  │       │   (NetworkError, RateLimitError, ServerError, Timeout)            │
│  │       └── [不可重试] → 返回 error                                         │
│  │           (AuthError, InvalidRequestError, …)                             │
│  ├── ToolCallNormalizer.normalize()                                         │
│  │   └── 标准化不同 provider 的 tool_call 格式                                │
│  └── ModelResponseValidator.validate()                                      │
│      └── 验证 schema、JSON、tool_call_id 合法性                                │
│                                                                             │
│  context.record_model_usage(result)                                         │
│  → 记录 token 消耗到 ContextUsageReporter                                     │
└──────────────────────────────────┬──────────────────────────────────────────┘
                                   │
          ╔════════════════════════╩════════════════════════╗
          ║            ★★★ 分叉点 1：模型调用结果 ★★★        ║
          ╚════════════════════════╦════════════════════════╝
                                   │
          ┌────────────────────────┼──────────────────────────┐
          │                        │                          │
          ▼                        ▼                          ▼
┌──────────────────┐  ┌──────────────────────┐  ┌──────────────────────┐
│  A. [模型失败]    │  │ B. [成功, 无工具调用]  │  │ C. [成功, 有工具调用]  │
│  result.status   │  │ result.status        │  │ result.status        │
│  != SUCCESS      │  │ == SUCCESS           │  │ == SUCCESS           │
│                  │  │ AND NOT tool_calls   │  │ AND tool_calls       │
└────────┬─────────┘  └──────────┬───────────┘  └──────────┬───────────┘
         │                       │                         │
         ▼                       ▼                         │
╔══════════════════════════════════════════╗                │
║      路径 A：模型失败处理                  ║                │
╚══════════════════════════════════════════╝                │
         │                                                  │
         ▼                                                  │
┌────────────────────────────────────────────────────────────┐
│                                                            │
│  A1. _record_model_failure(planner, result, turn)          │
│      trace.record("model_failure", {turn, status,          │
│                    error: error.to_dict(),                  │
│                    validation: validation.to_dict()})        │
│                                                            │
│  A2. outcome = _outcome_from_model_failure(result)         │
│      │                                                     │
│      ├── 提取 message = result.error.message               │
│      │          or result.validation.errors                 │
│      ├── retryable = result.error.retryable or True         │
│      ├── error_code 细化（从 message 关键词推断）：           │
│      │   ├── "invalid_json"  → INVALID_JSON                │
│      │   ├── "unknown_tool"  → UNKNOWN_TOOL               │
│      │   ├── "schema"        → SCHEMA_MISMATCH            │
│      │   └── 默认           → MODEL_RUNNER_FAILED         │
│      ├── blocked_external_dependency =                      │
│      │   !retryable AND error.kind in {NETWORK, AUTH}      │
│      └── 状态判定：                                          │
│          ├── retryable                     → RETRYABLE     │
│          ├── !retryable + NETWORK/AUTH     → BLOCKED      │
│          └── !retryable + 其他              → FATAL        │
│                                                            │
│      → ExecutionOutcome { status, source:"model",          │
│          reason, error_code, next_action,                   │
│          retry_allowed, metadata }                          │
│                                                            │
│  A3. controller.apply_outcome(outcome)                     │
│      ├── reducer.reduce_outcome(current_status, outcome)   │
│      │   → RunControlEvent { kind, from_status,            │
│      │       to_status, terminal, metadata }                │
│      │   ├── RETRYABLE → RUNNING  (非终端)                  │
│      │   ├── BLOCKED  → BLOCKED  (terminal=True)           │
│      │   └── FATAL    → FAILED   (terminal=True)           │
│      └── planner.record_execution_outcome(outcome)          │
│                                                            │
│  A4. _record_outcome_context(context, planner, outcome)    │
│      ├── trace.record("execution_outcome", outcome.to_dict())│
│      └── context.add_planner_state({current_phase,          │
│              status, execution_outcome: outcome.to_dict()}) │
│                                                            │
│  A5. terminal = _terminal_result_from_outcome(outcome,     │
│          turn)                                              │
│      │                                                     │
│      ├── [RETRYABLE] → return None                          │
│      │   │  ★ 不终止，继续下一 turn ★                        │
│      │   └── → 回到 run_loop 继续循环                        │
│      │                                                     │
│      ├── [BLOCKED] → AgentLoopResult(                      │
│      │   │   status       = BLOCKED,                       │
│      │   │   final_answer = observation_summary,            │
│      │   │   error_code   = outcome.error_code,            │
│      │   │   diagnostics  = {outcome: outcome.to_dict()}   │
│      │   │ )                                               │
│      │   ├── trace.record("final_answer", {turn, content})  │
│      │   └── ★ return terminal → 终止循环 ★                  │
│      │                                                     │
│      └── [FATAL] → AgentLoopResult(                        │
│          │   status       = FAILED,                        │
│          │   final_answer = observation_summary,            │
│          │   error_code   = outcome.error_code,            │
│          │   diagnostics  = {outcome: outcome.to_dict()}   │
│          │ )                                               │
│          ├── trace.record("final_answer", {turn, content})  │
│          └── ★ return terminal → 终止循环 ★                  │
│                                                            │
│  A6. if terminal is not None: return terminal              │
│      → 退出 run_turn，回到 run_loop → 终止 → AgentKernel    │
│                                                            │
│  A7. return None    ★ 继续下一 turn ★                       │
│                                                            │
└────────────────────────────────────────────────────────────┘
         │
         │  [RETRYABLE 时继续]    [BLOCKED/FATAL 时终止]
         │
         ▼                          ▼
   ┌──────────┐              ┌──────────────┐
   │ 回到      │              │ AgentKernel  │
   │ run_loop  │              │ .run_task()  │
   │ 继续循环   │              │ → 终止处理    │
   └──────────┘              └──────────────┘


         ┌──────────────────────────────────────────────────┐
         │                    路径 B                         │
         ▼                                                  │
╔══════════════════════════════════════════════════════════╗ │
║     路径 B：无 tool_calls → 尝试最终化                     ║ │
╚══════════════════════════════════════════════════════════╝ │
         │                                                  │
         ▼                                                  │
┌────────────────────────────────────────────────────────────┐
│                                                            │
│  B1. assistant_message = _assistant_message_from_result()  │
│      {                                                      │
│          role: "assistant",                                 │
│          content: message.text or "",                       │
│          (无 tool_calls 字段)                                │
│      }                                                      │
│                                                            │
│  B2. context.add_assistant_message(assistant_message)       │
│      → ObservationStore 写入 assistant 消息                  │
│                                                            │
│  B3. final = _attempt_finalize(                             │
│          planner, controller, context, turn,                │
│          model_answer=assistant_message["content"]          │
│      )                                                      │
│      │                                                      │
│      ▼  ★★★ 分叉：_attempt_finalize() 详图 ★★★              │
│      ╔════════════════════════════════════════════════════╗ │
│      ║     _attempt_finalize() 见下方独立详图              ║ │
│      ╚════════════════════════════════════════════════════╝ │
│      │                                                      │
│      ├── [返回 AgentLoopResult] → return final              │
│      │   │ ★ 完成或阻断，终止循环 ★                          │
│      │   └── → 回到 run_loop: result is not None → 终止     │
│      │                                                      │
│      └── [返回 None] → return None                          │
│          │ ★ 最终化被拒绝，继续下一 turn ★                    │
│          └── → 回到 run_loop: 继续下一个 turn                 │
│                                                            │
└────────────────────────────────────────────────────────────┘


         ┌──────────────────────────────────────────────────┐
         │                    路径 C                         │
         ▼                                                  │
╔══════════════════════════════════════════════════════════╗ │
║    路径 C：有 tool_calls → 工具协议 → 执行 → 结果处理      ║ │
╚══════════════════════════════════════════════════════════╝ │
         │                                                  │
         ▼                                                  │
┌────────────────────────────────────────────────────────────┐
│                                                            │
│  C1. observation_start = len(context.tool_observations)    │
│      (记录快照起点，用于后续计算本轮新增的 observation 数量)    │
│                                                            │
│  C2. protocol_result = tool_protocol.process_model_turn(   │
│          request=request,                                   │
│          result=result,                                     │
│          turn=turn,                                         │
│          context=context,                                   │
│          tool_executor=tool_executor,                       │
│          planner=planner                                    │
│      )                                                      │
│      → ToolProtocolTurnResult {                             │
│          status, next_action, batch_id,                     │
│          executed_count, failed_count, rejected_count,      │
│          pending_approval_count, appended_tool_message_count,│
│          metadata                                           │
│      }                                                      │
│      │                                                      │
│      ★★★ 工具协议引擎内部调用链见下方独立详图 ★★★              │
│                                                            │
│  C3. ★ next_action 分叉 ★                                   │
│      │                                                      │
│      ├── next_action == "finalize" ────────────────────┐   │
│      │   │  ★ 模型声明完成（无 tool_calls 时触发） ★      │   │
│      │   │                                               │   │
│      │   │  final = _attempt_finalize(planner,           │   │
│      │   │      controller, context, turn,               │   │
│      │   │      model_answer)                            │   │
│      │   │                                               │   │
│      │   ├── [非 None] → return final (终止)              │   │
│      │   └── [None]    → return None  (继续)              │   │
│      └──────────────────────────────────────────────────┘   │
│      │                                                      │
│      └── 其他 (continue / retry / fail_safe / recover)      │
│          │  继续执行后续步骤                                   │
│          ▼                                                  │
│                                                            │
│  C4. observations = context.tool_observations[               │
│          observation_start:]                                 │
│      → 本轮新增的 ToolObservation 列表                        │
│                                                            │
│  C5. controller.apply_protocol_result(                      │
│          protocol_result, observations=observations)        │
│      → reducer.reduce_protocol_result(…) → RunControlEvent  │
│      │  根据 next_action / pending_approval_count 判定：      │
│      ├── pending_approval / "pending_approval"              │
│      │   → WAITING_APPROVAL  (非终端，等待审批)               │
│      ├── "ask_user" / "request_user_input"                 │
│      │   → WAITING_USER      (非终端，等待用户输入)            │
│      ├── "continue" / "retry" / "request_model" /          │
│      │   "await_tool_result" / "execute_pending_tool" /     │
│      │   "append_tool_message"                              │
│      │   → RUNNING           (非终端，继续)                  │
│      └── "finalize"                                        │
│          → REPORTING         (非终端，即将报告)               │
│                                                            │
│  C6. reduced_outcome = controller.reduce_protocol_result(   │
│          protocol_result, observations=observations)        │
│      → ExecutionOutcome | None                              │
│      │                                                      │
│      ★★★ RunOutcomeReducer.protocol_result_to_outcome() ★★★ │
│      │  按优先级逐级判定（一旦匹配即返回，不继续 fall-through）：│
│      │                                                      │
│      ├── ① pending_count > 0 or APPROVAL_REQUIRED          │
│      │      → ExecutionOutcome(                             │
│      │            status=APPROVAL_REQUIRED,                 │
│      │            source="protocol",                        │
│      │            next_action="wait_for_approval",           │
│      │            retry_allowed=False)                       │
│      │                                                      │
│      ├── ② POLICY_ASK_USER_REQUIRED in error_codes          │
│      │      → ExecutionOutcome(                             │
│      │            status=USER_INPUT_REQUIRED,               │
│      │            source="tool",                             │
│      │            next_action="ask_user",                    │
│      │            retry_allowed=False)                       │
│      │                                                      │
│      ├── ③ any code in TOOL_BLOCKING_ERROR_CODES            │
│      │      (POLICY_BLOCKED, POLICY_DENIED,                  │
│      │       PROTECTED_PATH_DENIED, REVIEW_REQUIRED,         │
│      │       APPROVAL_DENIED, ACTION_NOT_ALLOWED,            │
│      │       RISK_ESCALATED, SANDBOX_REQUIRED,               │
│      │       SANDBOX_UNAVAILABLE, SANDBOX_VIOLATION,         │
│      │       CWD_DENIED, POLICY_ESCALATION_REQUIRED)         │
│      │      → ExecutionOutcome(BLOCKED, "tool", "blocked")   │
│      │                                                      │
│      ├── ④ any code in TOOL_REPLAN_ERROR_CODES              │
│      │      (SNAPSHOT_MISMATCH, EXTERNAL_CHANGE_DETECTED,    │
│      │       FILE_CHANGED, ROLLBACK_CONFLICT,                │
│      │       SEMANTIC_FAILURE, VERIFICATION_FAILED,          │
│      │       BLOCKED_BY_VERIFICATION, COMMAND_NOT_FOUND,     │
│      │       PROCESS_NOT_FOUND, TIMEOUT)                     │
│      │      → ExecutionOutcome(REPLAN_REQUIRED, "tool",      │
│      │            "replan", retry_allowed=True)              │
│      │                                                      │
│      ├── ⑤ any code in TOOL_RETRYABLE_ERROR_CODES           │
│      │      (BAD_ARGUMENTS_JSON, INVALID_JSON,               │
│      │       ARGUMENTS_NOT_OBJECT, VALIDATION_ERROR,         │
│      │       SCHEMA_MISMATCH, UNKNOWN_TOOL, TOOL_NOT_FOUND,  │
│      │       DISALLOWED_TOOL, PROTOCOL_VIOLATION,            │
│      │       INTERNAL_ERROR)                                 │
│      │      → ExecutionOutcome(RETRYABLE,                    │
│      │            "protocol" | "tool", "retry", True)        │
│      │                                                      │
│      ├── ⑥ next_action == "fail_safe"                       │
│      │      or status in {"failed", "invalid_assistant"}     │
│      │      → ExecutionOutcome(RETRYABLE, "protocol",        │
│      │            PROTOCOL_FAIL_SAFE, "retry", True)         │
│      │                                                      │
│      ├── ⑦ failed_count > 0 or rejected_count > 0           │
│      │      or next_action == "recover"                      │
│      │      → ExecutionOutcome(RETRYABLE, "protocol",        │
│      │            TOOL_FAILURE, "retry", True)               │
│      │                                                      │
│      └── ⑧ 无以上匹配                                       │
│          → return None                                       │
│          ★ 工具执行正常，无 reduced outcome ★                 │
│                                                            │
│  C7. ★ reduced_outcome 分叉 ★                                │
│      │                                                      │
│      ├── reduced_outcome is not None ──────────────────┐   │
│      │   │  ★ 工具执行产生异常结果 ★                      │   │
│      │   │                                               │   │
│      │   ├── controller.apply_outcome(reduced_outcome)   │   │
│      │   │   → reducer.reduce_outcome()                  │   │
│      │   │   → RunControlEvent                           │   │
│      │   │   → planner.record_execution_outcome()        │   │
│      │   │                                               │   │
│      │   ├── _record_outcome_context(context, planner,    │   │
│      │   │       reduced_outcome)                        │   │
│      │   │                                               │   │
│      │   ├── blocked = _maybe_analyze_failure(           │   │
│      │   │       planner, context,                       │   │
│      │   │       outcome=reduced_outcome,                │   │
│      │   │       failure_source="tool",                  │   │
│      │   │       turn=turn                               │   │
│      │   │   )   ★ 失败分析详图见下方 ★                    │   │
│      │   │   │                                           │   │
│      │   │   ├── blocked is not None ───────────────┐   │   │
│      │   │   │   │  ★ 失败分析返回阻断 ★              │   │   │
│      │   │   │   │                                   │   │   │
│      │   │   │   ├── controller.apply_outcome(blocked)│   │   │
│      │   │   │   ├── _record_outcome_context(…)      │   │   │
│      │   │   │   ├── terminal =                      │   │   │
│      │   │   │   │   _terminal_result_from_outcome(  │   │   │
│      │   │   │   │       blocked, turn)               │   │   │
│      │   │   │   │   ├── [非 None] → return terminal │   │   │
│      │   │   │   │   │   ★ 终止循环 ★                │   │   │
│      │   │   │   │   └── [None] → 继续               │   │   │
│      │   │   │   │                                   │   │   │
│      │   │   │   └── (继续执行下方)                    │   │   │
│      │   │   └──────────────────────────────────────┘   │   │
│      │   │                                               │   │
│      │   ├── terminal = _terminal_result_from_outcome(   │   │
│      │   │       reduced_outcome, turn)                   │   │
│      │   │   │                                           │   │
│      │   │   ├── [RETRYABLE] → None    ★ 继续 ★          │   │
│      │   │   ├── [REPLAN_REQUIRED] → None ★ 继续 ★       │   │
│      │   │   ├── [BLOCKED] → AgentLoopResult(BLOCKED)    │   │
│      │   │   │   → ★ return terminal → 终止 ★            │   │
│      │   │   ├── [USER_INPUT_REQUIRED] →                 │   │
│      │   │   │   AgentLoopResult(BLOCKED)                │   │
│      │   │   │   → ★ return terminal → 终止 ★            │   │
│      │   │   └── [FATAL] → AgentLoopResult(FAILED)       │   │
│      │   │       → ★ return terminal → 终止 ★            │   │
│      │   │                                               │   │
│      │   └── if terminal is not None: return terminal    │   │
│      └──────────────────────────────────────────────────┘   │
│      │                                                      │
│      └── reduced_outcome is None                            │
│          │ ★ 工具执行正常，无异常 outcome ★                    │
│          └── 继续执行后续步骤                                   │
│                                                            │
│  C8. blocked = _maybe_analyze_failure(                     │
│          planner, context,                                  │
│          failure_source="verification",  ← 无 outcome 参数！│
│          turn=turn                                          │
│      )                                                      │
│      │  ★ 检查 planner.evidence 的 verification 失败 ★       │
│      │  (走 _has_repairable_planner_failure 路径)            │
│      │                                                      │
│      ├── blocked is not None ──────────────────────────┐   │
│      │   │ controller.apply_outcome(blocked)            │   │
│      │   │ _record_outcome_context(…)                   │   │
│      │   │ terminal = _terminal_result_from_outcome(    │   │
│      │   │     blocked, turn)                           │   │
│      │   │ ├── [非 None] → return terminal              │   │
│      │   │ └── [None]    → 继续                         │   │
│      │   └─────────────────────────────────────────────┘   │
│      │                                                      │
│      └── blocked is None → 继续                              │
│                                                            │
│  C9. if _should_auto_finalize_after_tools(                 │
│          planner, protocol_result):                         │
│      │  条件（全部满足才触发）：                                │
│      │  ├── planner.state.status == FINALIZING              │
│      │  │   or current_phase == "finalizing"                │
│      │  ├── pending_approval_count == 0                     │
│      │  ├── failed_count == 0                               │
│      │  └── rejected_count == 0                             │
│      │                                                      │
│      ├── [True] → final = _attempt_finalize(planner,        │
│      │       controller, context, turn, model_answer)        │
│      │   ├── [非 None] → return final (终止)                 │
│      │   └── [None]    → 继续                                │
│      │                                                      │
│      └── [False] → 继续                                     │
│                                                            │
│  C10. return None    ★ 一切正常，继续下一 turn ★             │
│                                                            │
└────────────────────────────────────────────────────────────┘


╔══════════════════════════════════════════════════════════════╗
║        _attempt_finalize() — 最终化尝试 详图                  ║
║        (agent_loop.py:_attempt_finalize)                    ║
╚══════════════════════════════════════════════════════════════╝
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────┐
│                                                              │
│  F1. assessment = planner.assess_completion(mark_blocked=False)│
│      → { status: TaskStatus,                                  │
│          unmet: [...],  satisfied: [...],                     │
│          blocked_evidence: [...], … }                         │
│                                                              │
│      ★★★ 分叉点 ★★★                                          │
│                                                              │
│  ┌─ assessment["status"] != TaskStatus.COMPLETED ────────┐  │
│  │   │  ★ 完成门控未通过 ★                                 │  │
│  │   │                                                    │  │
│  │   ├── outcome = ExecutionOutcome(                      │  │
│  │   │       status        = REPLAN_REQUIRED,             │  │
│  │   │       source        = "completion",                │  │
│  │   │       reason        = "completion_rejected",       │  │
│  │   │       error_code    = COMPLETION_REJECTED,         │  │
│  │   │       missing_evidence = list(assessment["unmet"]), │  │
│  │   │       next_action   = "continue",                  │  │
│  │   │       retry_allowed = True                         │  │
│  │   │   )                                                │  │
│  │   │                                                    │  │
│  │   ├── controller.apply_outcome(outcome)                │  │
│  │   ├── _record_outcome_context(context, planner, outcome)│  │
│  │   │                                                    │  │
│  │   ├── _maybe_analyze_failure(                          │  │
│  │   │       planner, context,                            │  │
│  │   │       outcome=outcome,                             │  │
│  │   │       failure_source="completion",                 │  │
│  │   │       turn=turn                                    │  │
│  │   │   )                                                │  │
│  │   │   └── → blocked: ExecutionOutcome | None           │  │
│  │   │       ├── [非 None] → apply → _terminal → return   │  │
│  │   │       └── [None] → 继续                            │  │
│  │   │                                                    │  │
│  │   ├── repair_blocked =                                 │  │
│  │   │   _repair_phase_completion_blocked_outcome(        │  │
│  │   │       planner, assessment)                         │  │
│  │   │   │  仅在以下条件同时满足时触发：                      │  │
│  │   │   │  ├── current_phase == "repairing_failures"     │  │
│  │   │   │  └── "verification_contract_satisfaction"      │  │
│  │   │   │      in assessment["unmet"]                    │  │
│  │   │   │                                                │  │
│  │   │   ├── [非 None]                                    │  │
│  │   │   │   → ExecutionOutcome(                          │  │
│  │   │   │       status=BLOCKED,                          │  │
│  │   │   │       source="completion",                     │  │
│  │   │   │       error_code=REPAIR_BUDGET_EXCEEDED,       │  │
│  │   │   │       reason="Repair phase completion          │  │
│  │   │   │           rejected because active repair        │  │
│  │   │   │           contract is unsatisfied.")            │  │
│  │   │   │   ├── controller.apply_outcome()               │  │
│  │   │   │   ├── _record_outcome_context()                │  │
│  │   │   │   └── return _terminal_result_from_outcome()   │  │
│  │   │   │       → AgentLoopResult(BLOCKED) ★ 终止 ★      │  │
│  │   │   │                                                │  │
│  │   │   └── [None] → 继续                                │  │
│  │   │                                                    │  │
│  │   └── return None   ★ 继续下一 turn ★                   │  │
│  └────────────────────────────────────────────────────────┘  │
│                                                              │
│  ┌─ assessment["status"] == TaskStatus.COMPLETED ────────┐  │
│  │   │  ★ 完成门控通过 ★                                   │  │
│  │   │  BUT completion_criteria 未满足：                    │  │
│  │   │  ├── .required_changes_applied == False             │  │
│  │   │  └── .required_verifications_passed == False        │  │
│  │   │                                                    │  │
│  │   ├── outcome = ExecutionOutcome(                      │  │
│  │   │       status      = SUCCESS,                       │  │
│  │   │       source      = "completion",                  │  │
│  │   │       reason      = "completion_ready",            │  │
│  │   │       next_action = "finalize",                    │  │
│  │   │       retry_allowed = False                        │  │
│  │   │   )                                                │  │
│  │   │                                                    │  │
│  │   ├── controller.apply_outcome(outcome)                │  │
│  │   │   → reducer.reduce_outcome()                        │  │
│  │   │   ├── SUCCESS + next_action="finalize"             │  │
│  │   │   │   → RunLifecycleStatus.COMPLETED (terminal=True)│  │
│  │   │   └── planner.record_execution_outcome()            │  │
│  │   │                                                    │  │
│  │   ├── _record_outcome_context(…)                       │  │
│  │   ├── trace.record("final_answer", {turn, content})     │  │
│  │   │                                                    │  │
│  │   └── return AgentLoopResult(                          │  │
│  │           status       = COMPLETED,                    │  │
│  │           final_answer = model_answer,                 │  │
│  │           turn         = turn                          │  │
│  │       )   ★ 成功终止 ★                                  │  │
│  └────────────────────────────────────────────────────────┘  │
│                                                              │
│  ┌─ 最终化路径（最完整的成功路径）───────────────────────┐    │
│  │   │  report = planner.finalize()                      │    │
│  │   │  → Finalizer.build() 通过 EvidenceLedger typed     │    │
│  │   │    helper 读取 verification / sandbox / policy /  │    │
│  │   │    tool result / task outcome 关键 bucket，避免      │    │
│  │   │    从裸 dict 随意读取缺字段或 stale blocker。        │    │
│  │   │  → FinalReport {                                   │    │
│  │   │      status, files_changed, verification_summary,  │    │
│  │   │      review_summary, unresolved_issues, risks,     │    │
│  │   │      next_steps, completion_summary, …             │    │
│  │   │  }                                                 │    │
│  │   │                                                    │    │
│  │   │  report.context_usage_diagnostic =                 │    │
│  │   │      context.context_usage_diagnostic()             │    │
│  │   │                                                    │    │
│  │   ├── report.status != TaskStatus.COMPLETED ───────┐   │    │
│  │   │   │  ★ FinalReviewer 拒绝报告 ★                  │    │
│  │   │   │                                              │    │
│  │   │   ├── retry_allowed = report.status in {         │    │
│  │   │   │   INSPECTING_WORKSPACE, PLANNING_CHANGES,    │    │
│  │   │   │   APPLYING_CHANGES, RUNNING_VERIFICATION,    │    │
│  │   │   │   REPAIRING_FAILURES, FINALIZING }           │    │
│  │   │   │                                              │    │
│  │   │   ├── outcome = ExecutionOutcome(                │    │
│  │   │   │   status = REPLAN_REQUIRED if retry_allowed  │    │
│  │   │   │           else BLOCKED,                      │    │
│  │   │   │   source = "completion",                     │    │
│  │   │   │   reason = "Final report did not complete:   │    │
│  │   │   │             {report.status}",                 │    │
│  │   │   │   error_code = FINAL_REVIEW_REJECTED,        │    │
│  │   │   │   missing_evidence = next_steps or           │    │
│  │   │   │       ["final_report_completed"],            │    │
│  │   │   │   next_action = "continue" if retry_allowed  │    │
│  │   │   │                 else "blocked"                │    │
│  │   │   │ )                                            │    │
│  │   │   │                                              │    │
│  │   │   ├── controller.apply_outcome(outcome)          │    │
│  │   │   ├── _record_outcome_context(…)                 │    │
│  │   │   │                                              │    │
│  │   │   ├── _maybe_analyze_failure(                    │    │
│  │   │   │       planner, context,                      │    │
│  │   │   │       outcome=outcome,                       │    │
│  │   │   │       failure_source="completion_review",    │    │
│  │   │   │       turn=turn                              │    │
│  │   │   │   )                                          │    │
│  │   │   │   ├── [非 None] → apply → _terminal → return │    │
│  │   │   │   └── [None] → 继续                          │    │
│  │   │   │                                              │    │
│  │   │   └── return _terminal_result_from_outcome(      │    │
│  │   │           outcome, turn)                         │    │
│  │   │       ├── [REPLAN_REQUIRED] → None (继续)        │    │
│  │   │       └── [BLOCKED]        → return terminal     │    │
│  │   └──────────────────────────────────────────────────┘    │
│  │   │                                                    │    │
│  │   ├── report.status == TaskStatus.COMPLETED ───────┐   │    │
│  │   │   │  ★ 最终报告通过审核，成功完成 ★               │    │
│  │   │   │                                              │    │
│  │   │   ├── final_answer = 格式化报告摘要：               │    │
│  │   │   │   status: {report.status.value}               │    │
│  │   │   │   files_changed: {", ".join(files_changed)}   │    │
│  │   │   │   verification: {status}                      │    │
│  │   │   │   unresolved_issues: {count}                  │    │
│  │   │   │   risks: {count}                              │    │
│  │   │   │                                              │    │
│  │   │   ├── outcome = ExecutionOutcome(                │    │
│  │   │   │   status      = SUCCESS,                     │    │
│  │   │   │   source      = "completion",                │    │
│  │   │   │   reason      = "completion_ready",          │    │
│  │   │   │   next_action = "finalize"                   │    │
│  │   │   │ )                                            │    │
│  │   │   ├── controller.apply_outcome(outcome)          │    │
│  │   │   ├── _record_outcome_context(…)                 │    │
│  │   │   ├── trace.record("final_answer", …)            │    │
│  │   │   └── return AgentLoopResult(                    │    │
│  │   │           status       = COMPLETED,              │    │
│  │   │           final_answer = final_answer,           │    │
│  │   │           turn         = turn                    │    │
│  │   │       )   ★ 成功终止 ★                            │    │
│  │   └──────────────────────────────────────────────────┘    │
│  └──────────────────────────────────────────────────────────┘
│
└─────────────────────────────────────────────────────────────┘


╔══════════════════════════════════════════════════════════════╗
║     _maybe_analyze_failure() — 失败分析完整流程 详图          ║
║     (agent_loop.py:_maybe_analyze_failure)                  ║
╚══════════════════════════════════════════════════════════════╝
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────┐
│                                                              │
│  FA1. 入口条件检查（两个入口之一）                               │
│                                                              │
│  ┌─ 入口 1: outcome is not None ─────────────────────────┐  │
│  │   │  调用 _should_analyze_outcome(planner, outcome)    │  │
│  │   │                                                    │  │
│  │   ├── outcome.status != REPLAN_REQUIRED                │  │
│  │   │   → return None  (不分析非 REPLAN_REQUIRED)        │  │
│  │   │                                                    │  │
│  │   ├── outcome.error_code in                            │  │
│  │   │   FAILURE_ANALYSIS_EXCLUDED_ERROR_CODES            │  │
│  │   │   (13 个 code：APPROVAL_*, PERMISSION_DENIED,       │  │
│  │   │    POLICY_*, ACTION_NOT_ALLOWED,                   │  │
│  │   │    PROTECTED_PATH_DENIED, RISK_ESCALATED,          │  │
│  │   │    SANDBOX_*, POLICY_ESCALATION_REQUIRED)          │  │
│  │   │   → return None  (策略/权限类不触发模型分析)        │  │
│  │   │                                                    │  │
│  │   ├── outcome.error_code == COMPLETION_REJECTED        │  │
│  │   │   │  走专门的升级逻辑                                │  │
│  │   │   └── _should_escalate_completion_rejection(       │  │
│  │   │           planner, outcome)                        │  │
│  │   │       ├── 构造 key = sorted(missing_evidence)       │  │
│  │   │       ├── phase = planner.state.current_phase      │  │
│  │   │       ├── snapshot = _evidence_snapshot(planner)   │  │
│  │   │       │   { inspected_files, applied_changes,      │  │
│  │   │       │     command_results, verification_results,  │  │
│  │   │       │     tool_results, edit_results,            │  │
│  │   │       │     review_results } (7 个计数器)           │  │
│  │   │       │                                            │  │
│  │   │       ├── 首次 (key 不同)                           │  │
│  │   │       │   → 记录 {key, count:1, phase, snapshot}   │  │
│  │   │       │   → return False  (暂不分析)               │  │
│  │   │       │                                            │  │
│  │   │       └── 重复 (key 相同)                           │  │
│  │   │           ├── count++                              │  │
│  │   │           ├── phase_stalled   = phase 未变          │  │
│  │   │           ├── evidence_stalled = snapshot 无增长    │  │
│  │   │           ├── count >= 2 AND phase_stalled         │  │
│  │   │           │   AND evidence_stalled                 │  │
│  │   │           │   → return True  (触发失败分析)         │  │
│  │   │           └── 否则 → return False                   │  │
│  │   │                                                    │  │
│  │   └── 其他 error_code → return True (允许分析)          │  │
│  └────────────────────────────────────────────────────────┘  │
│                                                              │
│  ┌─ 入口 2: outcome is None (failure_source="verification")  │
│  │   │  调用 _has_repairable_planner_failure(planner)       │  │
│  │   │                                                      │  │
│  │   ├── planner.state is None → return False               │  │
│  │   │                                                      │  │
│  │   ├── latest verification assessment:                    │  │
│  │   │   ├── status in {"ready", "ready_with_warnings"}     │  │
│  │   │   │   → return False (无需分析)                       │  │
│  │   │   └── status in {"failed", "blocked", "needs_review"}│  │
│  │   │       → return True (触发分析)                        │  │
│  │   │                                                      │  │
│  │   └── 最近 5 个 unresolved_failures:                      │  │
│  │       遍历每个 failure:                                    │  │
│  │       ├── 不是 dict → return True                         │  │
│  │       └── error_code not in                              │  │
│  │           FAILURE_ANALYSIS_EXCLUDED_ERROR_CODES          │  │
│  │           → return True                                  │  │
│  │       (全都不满足 → return False)                          │  │
│  └──────────────────────────────────────────────────────────┘
│                                                              │
│  若以上任何入口返回 None → 不进行失败分析，return None           │
│                                                              │
│  FA2. request = FailureAnalysisRequest.from_planner(         │
│          planner, context, failure_source, outcome, turn)    │
│      ├── 收集当前 TaskState, EvidenceLedger, context 摘要      │
│      ├── 构造 FailureAnalysisRequest {                       │
│      │     failure_source, error_code,                        │
│      │     current_phase, task_goal,                          │
│      │     recent_evidence, context_summary, … }              │
│      └── .has_failure 判定:                                   │
│          ├── [False] → return None (无失败可分析)              │
│          └── [True]  → 继续                                   │
│                                                              │
│  FA3. 去重检查 (fingerprint-based)                             │
│      │                                                       │
│      ├── request.fingerprint in                               │
│      │   self._failure_analysis_fingerprints  ───────────┐  │
│      │   │  ★ 已分析过同 fingerprint 的失败 ★              │  │
│      │   │                                                │  │
│      │   ├── snapshot = _failure_snapshot(planner)        │  │
│      │   │   { failed_command_results,                     │  │
│      │   │     failed_verification_results,                │  │
│      │   │     failed_tool_results,                        │  │
│      │   │     failed_edit_results,                        │  │
│      │   │     failed_review_results }                    │  │
│      │   │                                                │  │
│      │   ├── _duplicate_failure_has_new_evidence(         │  │
│      │   │       fingerprint, snapshot)                    │  │
│      │   │   → any(snapshot[key] > previous[key])          │  │
│      │   │                                                │  │
│      │   ├── [无新证据]                                    │  │
│      │   │   ├── _is_stalled_completion_gate_failure?     │  │
│      │   │   │   failure_source in {"completion",         │  │
│      │   │   │       "completion_review"}                  │  │
│      │   │   │   OR error_code in {COMPLETION_REJECTED,   │  │
│      │   │   │       FINAL_REVIEW_REJECTED}               │  │
│      │   │   │                                            │  │
│      │   │   │   ├── [True]                               │  │
│      │   │   │   │   → ExecutionOutcome(                  │  │
│      │   │   │   │       USER_INPUT_REQUIRED,             │  │
│      │   │   │   │       "failure_analysis",              │  │
│      │   │   │   │       "Repeated completion/final       │  │
│      │   │   │   │        review failure without new      │  │
│      │   │   │   │        repair evidence.",              │  │
│      │   │   │   │       REPAIR_BUDGET_EXCEEDED,          │  │
│      │   │   │   │       next_action="ask_user")          │  │
│      │   │   │   │   → ★ return blocked → 终止 ★          │  │
│      │   │   │   │                                        │  │
│      │   │   │   └── [False] → return None                │  │
│      │   │   │       (非 completion gate 阻塞，静默跳过)    │  │
│      │   │   │                                            │  │
│      │   │   └── [有新证据]                                │  │
│      │   │       ├── 取已有 replan_signal                   │  │
│      │   │       │   self._failure_replan_signals          │  │
│      │   │       │   .get(fingerprint)                    │  │
│      │   │       ├── [无 signal] → return None            │  │
│      │   │       ├── 更新 snapshot                         │  │
│      │   │       ├── decision = planner.replan(signal)    │  │
│      │   │       │   → ReplanDecision { decision,         │  │
│      │   │       │       reason, metadata }                │  │
│      │   │       │                                        │  │
│      │   │       ├── decision == "ask_user"               │  │
│      │   │       │   → ExecutionOutcome(                  │  │
│      │   │       │       USER_INPUT_REQUIRED,             │  │
│      │   │       │       "failure_analysis",              │  │
│      │   │       │       REPAIR_BUDGET_EXCEEDED,          │  │
│      │   │       │       next_action="ask_user")          │  │
│      │   │       │   → ★ return blocked → 终止 ★          │  │
│      │   │       │                                        │  │
│      │   │       └── 其他 → return None (继续)            │  │
│      │   └───────────────────────────────────────────────┘  │
│      │                                                       │
│      └── request.fingerprint is new ─────────────────────┐  │
│          │  ★ 首次遇到该 failure fingerprint ★              │  │
│          │                                                │  │
│          ├── self._failure_analysis_fingerprints          │  │
│          │   .add(request.fingerprint)                     │  │
│          │                                                │  │
│          ├── analysis = self.failure_analyzer             │  │
│          │       .analyze(request)                        │  │
│          │   → FailureAnalysisResult {                    │  │
│          │       root_cause, category,                     │  │
│          │       suggestions, severity, … }                │  │
│          │   (调用模型进行失败根因分析)                       │  │
│          │                                                │  │
│          ├── repair_plan = self.repair_planner            │  │
│          │       .plan(analysis,                           │  │
│          │           repair_policy=request.repair_policy) │  │
│          │   → RepairPlan {                               │  │
│          │       needs_user_input, blocked_reason,        │  │
│          │       steps, estimated_effort, … }              │  │
│          │                                                │  │
│          ├── replan_signal = self.repair_planner          │  │
│          │       .to_replan_signal(                       │  │
│          │           request, analysis, plan)             │  │
│          │   → RepairReplanSignal {                       │  │
│          │       decision, reason, repair_contract, … }   │  │
│          │                                                │  │
│          ├── planner.record_failure_analysis(             │  │
│          │       analysis, repair_plan,                    │  │
│          │       replan_signal=signal.to_dict())          │  │
│          ├── context.add_failure({analysis,               │  │
│          │       repair_plan, replan_signal})              │  │
│          ├── self._failure_analysis_snapshots             │  │
│          │       [fingerprint] = _failure_snapshot()      │  │
│          │                                                │  │
│          ├── repair_plan.needs_user_input                 │  │
│          │   or repair_plan.blocked_reason                │  │
│          │   │                                            │  │
│          │   ├── [True]                                   │  │
│          │   │   → self.repair_planner                    │  │
│          │   │       .blocked_outcome(repair_plan)        │  │
│          │   │   → ExecutionOutcome(                      │  │
│          │   │       BLOCKED or USER_INPUT_REQUIRED,      │  │
│          │   │       "repair", …)                         │  │
│          │   │   → ★ return blocked → 终止 ★              │  │
│          │   │                                            │  │
│          │   └── [False] → 继续                            │  │
│          │                                                │  │
│          ├── self._failure_replan_signals                 │  │
│          │       [fingerprint] = signal_payload           │  │
│          │                                                │  │
│          ├── decision = planner.replan(signal_payload)    │  │
│          │   → ReplanDecision                              │  │
│          │                                                │  │
│          ├── decision == "ask_user"                       │  │
│          │   → ExecutionOutcome(                          │  │
│          │       USER_INPUT_REQUIRED,                     │  │
│          │       "failure_analysis",                      │  │
│          │       REPAIR_BUDGET_EXCEEDED,                  │  │
│          │       next_action="ask_user")                  │  │
│          │   → ★ return blocked → 终止 ★                  │  │
│          │                                                │  │
│          └── 其他 → return None (继续，repair 已注入)      │  │
│          └───────────────────────────────────────────────┘  │
│                                                              │
│  最终: return None  (失败分析不阻断，继续循环)                  │
│                                                              │
└─────────────────────────────────────────────────────────────┘


╔══════════════════════════════════════════════════════════════╗
║   ToolProtocolEngine — 工具协议引擎内部完整调用链               ║
║   (tool_protocol/engine.py:ToolProtocolEngine)              ║
╚══════════════════════════════════════════════════════════════╝
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────┐
│                                                              │
│  ⑥ 验证与批次创建 (handle_model_turn_result:80-170)            │
│                                                              │
│  _throw_if_cancelled()    ← cancellation token 检查            │
│                                                              │
│  assistant_message = _assistant_message_from_model_result()  │
│  │                                                           │
│  ├── [None / 无 assistant_message]                           │
│  │   → ToolProtocolTurnResult(                               │
│  │       status     = INVALID_ASSISTANT,                     │
│  │       next_action = "fail_safe",                          │
│  │       metadata   = {reason: "missing_assistant_message"}) │
│  │   → 回到 AgentLoop 路径 C2                                │
│  │   → RunOutcomeReducer 步骤⑥转为 RETRYABLE                  │
│  │                                                           │
│  └── [有消息] → validate_batch()                             │
│      ├── validator.validate_assistant_message(               │
│      │       run_id, session_id, task_id, phase_id,          │
│      │       model_request_id, model_response_id,            │
│      │       assistant_message, assistant_message_id,        │
│      │       allowed_tool_names, tool_choice,                │
│      │       provider_capabilities, max_tool_calls)          │
│      │   → ToolProtocolValidationResult {                    │
│      │       batch: ToolCallBatch,                           │
│      │       errors: [...],                                  │
│      │       blocked_call_ids: [...] }                       │
│      │                                                       │
│      ├── validation.batch is None                            │
│      │   → ToolProtocolTurnResult(                           │
│      │       INVALID_ASSISTANT, "fail_safe",                 │
│      │       metadata={reason: "invalid_batch"})             │
│      │                                                       │
│      └── [有效 batch]                                        │
│          ├── state_store.save_batch(batch)  → SQLite         │
│          ├── _throw_if_cancelled()                            │
│          ├── trace.emit("tool_protocol.batch_created", …)    │
│          └── context.add_assistant_message(assistant_message)│
│                                                              │
│  ★★★ 分叉：batch.tool_calls 是否为空 ★★★                      │
│                                                              │
│  ├── [空的 tool_calls]                                       │
│  │   → ToolProtocolTurnResult(                               │
│  │       NO_TOOL_CALLS,                                      │
│  │       next_action = "finalize")                           │
│  │   → 回到 AgentLoop 路径 C3: next_action=="finalize"       │
│  │   → 触发 _attempt_finalize()                              │
│  │                                                           │
│  └── [有 tool_calls]                                         │
│      ↓                                                       │
│                                                              │
│  ⑦ 构建执行计划 (build_execution_plan)                         │
│                                                              │
│  plan = scheduler.schedule(batch)                            │
│  → ToolExecutionPlan {                                       │
│      plan_id, batch_id,                                      │
│      execution_mode: ToolExecutionMode,                      │
│          # SEQUENTIAL / PARALLEL_READONLY                    │
│      ordered_calls: [ToolCallEnvelope, …],                   │
│      blocked_calls: [ToolCallEnvelope, …],                   │
│      parallel_groups: [[…], […]] or [],                      │
│      reasons: [str, …] }                                     │
│                                                              │
│  trace.emit("tool_protocol.plan_built", …)                   │
│                                                              │
│  ⑧ 执行计划 (shared lifecycle core + scheduling strategy)       │
│                                                              │
│  batch = _batch_for_plan(plan, context)                       │
│  counters = _ToolExecutionCounters()                          │
│  ★ serial 与 parallel_readonly 共用同一组 call lifecycle helper：│
│    _prepare_call()       → validation trace / record upsert / │
│                            synthetic result / replay check / │
│                            scheduled+running transition       │
│    _bind_synthetic_result() → rejected / replay-blocked 结果绑定│
│    _complete_call()      → result builder / state transition /│
│                            context append / trace emit        │
│    _turn_result()        → ToolProtocolTurnResult 汇总         │
│                                                              │
│  ★★★ 分叉：执行模式 ★★★                                      │
│                                                              │
│  ┌─ PARALLEL_READONLY (plan.execution_mode)              ──┐ │
│  │   │  AND plan.parallel_groups is not empty              │ │
│  │   └── _execute_parallel_readonly_plan(                   │ │
│  │           plan, batch, context, tool_executor, turn)    │ │
│  │       → group trace 记录调度组边界                         │ │
│  │       → 每个 call 仍先走 _prepare_call()                   │ │
│  │       → prepared read-only calls 交给 ParallelToolExecutor│ │
│  │       → 每个执行结果仍走 _complete_call()                  │ │
│  │       → 返回 ToolProtocolTurnResult                      │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                                              │
│  ┌─ SEQUENTIAL 或 PARALLEL_READONLY 无 groups ──────────┐   │
│  │                                                        │   │
│  │  executed_count = 0                                    │   │
│  │  failed_count    = 0                                    │   │
│  │  rejected_count  = 0                                    │   │
│  │  pending_approval_count = 0                             │   │
│  │  appended_tool_message_count = 0                        │   │
│  │                                                        │   │
│  │  for each call in ordered_calls:                       │   │
│  │      │                                                 │   │
│  │      ├── prepared = _prepare_call(batch, context, call)│   │
│  │      │   ├── _throw_if_cancelled()                      │   │
│  │      │   ├── state_store.upsert_record(VALIDATED)       │   │
│  │      │   └── trace.emit("tool_protocol.call_validated") │   │
│  │      │                                                 │   │
│  │      ├── [validation_errors] ──────────────────────┐   │   │
│  │      │   │  参数验证失败                                │   │   │
│  │      │   │                                            │   │   │
│  │      │   ├── rejected_count++                         │   │   │
│  │      │   ├── synthetic = _synthetic_result(          │   │   │
│  │      │   │       call, error_kind, message,          │   │   │
│  │      │   │       error_code)                         │   │   │
│  │      │   │   → ToolProtocolResultEnvelope            │   │   │
│  │      │   │       (synthetic, 非真实执行)              │   │   │
│  │      │   ├── state_store.transition(REJECTED, …)     │   │   │
│  │      │   ├── state_store.bind_result(result)          │   │   │
│  │      │   ├── _append_result(context, record,          │   │   │
│  │      │   │       synthetic, turn)                    │   │   │
│  │      │   │   → ContextManager.add_tool_result()       │   │   │
│  │      │   ├── appended_tool_message_count++            │   │   │
│  │      │   ├── trace.emit("tool_protocol.call_rejected")│  │   │
│  │      │   ├── trace.emit("tool_protocol.synthetic_result_created")│
│  │      │   └── continue → next call                    │   │   │
│  │      └──────────────────────────────────────────────┘   │   │
│  │      │                                                 │   │
│  │      ├── [无 validation 错误]                            │   │
│  │      │   │                                             │   │
│  │      │   ├── spec = registry.get(call.tool_name)       │   │
│  │      │   │                                             │   │
│  │      │   ├── replay_decision = state_store             │   │
│  │      │   │       .check_replay(                        │   │
│  │      │   │           call,                             │   │
│  │      │   │           side_effects=spec.side_effects,   │   │
│  │      │   │           idempotent=spec.idempotent)       │   │
│  │      │   │                                             │   │
│  │      │   ├── [idempotent replay → 复用缓存]             │   │
│  │      │   │   ├── state_store.transition(RECOVERED)     │   │
│  │      │   │   ├── _append_result(context, record,       │   │
│  │      │   │   │       cached_result, turn)              │   │
│  │      │   │   ├── trace.emit("tool_protocol.replay_detected")│
│  │      │   │   └── continue → next call                  │   │
│  │      │   │                                             │   │
│  │      │   └── [非缓存 / 非幂等 → 真实执行]                │   │
│  │      │       │                                         │   │
│  │      │       ├── execution_request =                   │   │
│  │      │       │   ToolExecutionRequest.from_envelope(call)│   │
│  │      │       │   → ToolExecutionRequest {              │   │
│  │      │       │       tool_name, arguments,             │   │
│  │      │       │       tool_call_id, … }                 │   │
│  │      │       │                                         │   │
│  │      │       ├── ★★★ 核心：ToolExecutor.execute_request() ★★★│
│  │      │       │   ↓                                     │   │
│  │      │       │                                         │   │
│  │      │       │  ╔══════════════════════════════════╗   │   │
│  │      │       │  ║  ToolExecutor 当前执行管线          ║   │   │
│  │      │       │  ║  (tools/executor.py:ToolExecutor) ║   │   │
│  │      │       │  ╚══════════════════════════════════╝   │   │
│  │      │       │  │                                      │   │
│  │      │       │  ├── ① 注册表查询                         │   │
│  │      │       │  │   spec = registry.get(tool_name)     │   │
│  │      │       │  │   ├── [None] → error: TOOL_NOT_FOUND │   │
│  │      │       │  │   └── [found] → 继续                  │   │
│  │      │       │  │                                      │   │
│  │      │       │  ├── ② 参数解析 + 执行侧参数验证              │   │
│  │      │       │  │   _arguments_for_execution_validation │   │
│  │      │       │  │   + spec.input_model.model_validate() │   │
│  │      │       │  │   ├── [JSON/校验失败] → ToolResult.failure│
│  │      │       │  │   └── [通过] → validated_args         │   │
│  │      │       │  │                                      │   │
│  │      │       │  ├── ③ 幂等性 replay 检查                  │   │
│  │      │       │  │   IdempotencyLedger.check(           │   │
│  │      │       │  │       tool_call_id, args_fingerprint,│   │
│  │      │       │  │       replay_allowed=...)            │   │
│  │      │       │  │   ├── [命中] → 返回历史 ToolResult      │   │
│  │      │       │  │   └── [未命中] → 继续                  │   │
│  │      │       │  │                                      │   │
│  │      │       │  ├── ④ 执行边界 / dry-run / delegated preflight│
│  │      │       │  │   _check_execution_boundary()        │   │
│  │      │       │  │   _dry_run_error()                   │   │
│  │      │       │  │   _preflight_delegated_handler()     │   │
│  │      │       │  │   ├── [失败] → ToolResult.failure    │   │
│  │      │       │  │   └── [通过] → 继续                  │   │
│  │      │       │  │                                      │   │
│  │      │       │  ├── ⑤ 策略引擎                           │   │
│  │      │       │  │   _enforce_policy()                  │   │
│  │      │       │  │   → policy_engine.enforce(PolicyRequest)│
│  │      │       │  │   → PolicyDecision {                 │   │
│  │      │       │  │       outcome, reason, risk_level,    │   │
│  │      │       │  │       risk_tags, constraints,         │   │
│  │      │       │  │       required_approval, error_code } │   │
│  │      │       │  │   ├── [allow/sandbox_required] → 继续 │   │
│  │      │       │  │   ├── [require_review]       → 审批    │   │
│  │      │       │  │   └── [deny/ask_user]        → error  │   │
│  │      │       │  │                                      │   │
│  │      │       │  ├── ⑥ 审批门控                           │   │
│  │      │       │  │   if required_approval:               │   │
│  │      │       │  │   approval_gate.authorize(           │   │
│  │      │       │  │       tool_name, arguments,          │   │
│  │      │       │  │       policy_decision)               │   │
│  │      │       │  │   → ApprovalGrant {                  │   │
│  │      │       │  │       granted, grant_id,              │   │
│  │      │       │  │       trust_boundary_verified,       │   │
│  │      │       │  │       consumption_ledger_entry }      │   │
│  │      │       │  │   ├── [granted]    → 继续             │   │
│  │      │       │  │   ├── [auto-grant] → 继续             │   │
│  │      │       │  │   └── [denied]     → error            │   │
│  │      │       │  │                                      │   │
│  │      │       │  ├── ⑦ Planner 工具授权                    │   │
│  │      │       │  │   planner.authorize_tool_call(       │   │
│  │      │       │  │       tool_name, arguments)           │   │
│  │      │       │  │   ├── [authorized] → 继续              │   │
│  │      │       │  │   └── [blocked]   → error             │   │
│  │      │       │  │                                      │   │
│  │      │       │  ├── ⑧ 结果缓存检查                         │   │
│  │      │       │  │   ToolResultCache.get(cache_key)       │   │
│  │      │       │  │   ├── [命中] → 返回缓存 ToolResult      │   │
│  │      │       │  │   └── [未命中] → 继续                   │   │
│  │      │       │  │                                      │   │
│  │      │       │  ├── ⑨ delegated backend 可用性检查         │   │
│  │      │       │  │   _delegated_backend_error()           │   │
│  │      │       │  │                                      │   │
│  │      │       │  ├── ⑩ 工具处理器调用                       │   │
│  │      │       │  │   result = _execute_handler(spec, validated)│
│  │      │       │  │   │                                  │   │
│  │      │       │  │   ├── [命令工具]                       │   │
│  │      │       │  │   │   CommandExecutor.run(           │   │
│  │      │       │  │   │       command, cwd, env, …)      │   │
│  │      │       │  │   │   ├── _policy_request()          │   │
│  │      │       │  │   │   │   + CommandPolicy.classify() │   │
│  │      │       │  │   │   │   → command PolicyRequest     │   │
│  │      │       │  │   │   ├── PolicyEngine.enforce()     │   │
│  │      │       │  │   │   │   → PolicyDecision            │   │
│  │      │       │  │   │   ├── _command_policy_result()   │   │
│  │      │       │  │   │   │   → CommandPolicyResult 投影  │   │
│  │      │       │  │   │   ├── SandboxManager.run()       │   │
│  │      │       │  │   │   │   ├── prepare(workspace)     │   │
│  │      │       │  │   │   │   ├── WindowsSandboxBackend   │   │
│  │      │       │  │   │   │   │   .execute(command)      │   │
│  │      │       │  │   │   │   │   → WindowsSandboxRunner │   │
│  │      │       │  │   │   │   │   (受限令牌+私有桌面+Job  │   │
│  │      │       │  │   │   │   │    Object+防火墙+ACL)    │   │
│  │      │       │  │   │   │   │   recheck只采纳当前账户    │   │
│  │      │       │  │   │   │   │   的network_probe；其他    │   │
│  │      │       │  │   │   │   │   enforcement blocker仍阻断│   │
│  │      │       │  │   │   │   ├── LocalProcessBackend    │   │
│  │      │       │  │   │   │   │   .execute(command)      │   │
│  │      │       │  │   │   │   │   → subprocess.run()     │   │
│  │      │       │  │   │   │   └── cleanup()              │   │
│  │      │       │  │   │   │       → 同账户Level-1删除     │   │
│  │      │       │  │   │   │         workspace projection  │   │
│  │      │       │  │   │   │       → 宿主run-root ACL/IL   │   │
│  │      │       │  │   │   │         normalization后删除   │   │
│  │      │       │  │   │   └── → CommandResult {           │   │
│  │      │       │  │   │       stdout, stderr, returncode, │   │
│  │      │       │  │   │       error_code, sandbox_result, │   │
│  │      │       │  │   │       isolation_report }          │   │
│  │      │       │  │   │                                  │   │
│  │      │       │  │   ├── [验证工具]                         │   │
│  │      │       │  │   │   VerificationToolHandlers       │   │
│  │      │       │  │   │   .run_verification()            │   │
│  │      │       │  │   │   ├── VerificationRunner.run_plan│   │
│  │      │       │  │   │   ├── CommandExecutor.run()      │   │
│  │      │       │  │   │   └── classify_failure()         │   │
│  │      │       │  │   │       Python DLL/import init     │   │
│  │      │       │  │   │       → environment_error        │   │
│  │      │       │  │   │       → blocked, no repair_hints │   │
│  │      │       │  │   │                                  │   │
│  │      │       │  │   ├── [写工具]                         │   │
│  │      │       │  │   │   WorkspaceMutationManager       │   │
│  │      │       │  │   │   .apply(mutation_request)       │   │
│  │      │       │  │   │   ├── DiffApplier.apply()        │   │
│  │      │       │  │   │   ├── FileMutationApplier        │   │
│  │      │       │  │   │   │   .create/delete/move()      │   │
│  │      │       │  │   │   ├── MutationJournal.record()   │   │
│  │      │       │  │   │   └── → MutationResult {          │   │
│  │      │       │  │   │       files_changed,              │   │
│  │      │       │  │   │       rollback_available, … }     │   │
│  │      │       │  │   │                                  │   │
│  │      │       │  │   └── [编辑工具]                       │   │
│  │      │       │  │       EditExecutor.apply(            │   │
│  │      │       │  │           file_path,                 │   │
│  │      │       │  │           edit_request)               │   │
│  │      │       │  │       → EditResult                    │   │
│  │      │       │  │                                      │   │
│  │      │       │  ├── ⑪ Planner 更新                         │   │
│  │      │       │  │   _update_planner() / _safe_update_planner()│
│  │      │       │  │                                      │   │
│  │      │       │  ├── ⑫ 缓存写入 / 写后失效                  │   │
│  │      │       │  │   ToolResultCache.set()               │   │
│  │      │       │  │   _invalidate_after_write()           │   │
│  │      │       │  │                                      │   │
│  │      │       │  ├── ⑬ replay 记录                         │   │
│  │      │       │  │   _remember_replay()                  │   │
│  │      │       │  │                                      │   │
│  │      │       │  └── ⑭ Trace 记录与 metadata 标注            │   │
│  │      │       │      _record_trace() / _emit_trace()      │   │
│  │      │       │                                         │   │
│  │      │       │  ↓ 回到 ToolProtocolEngine                │   │
│  │      │       │                                         │   │
│  │      │       ├── state_store.transition(                │   │
│  │      │       │       call.tool_call_id,                 │   │
│  │      │       │       SUCCEEDED / WAITING_APPROVAL / FAILED,│
│  │      │       │       error_kind, error_message,         │   │
│  │      │       │       tool_result_digest)                │   │
│  │      │       ├── state_store.bind_result(               │   │
│  │      │       │       record.record_id,                  │   │
│  │      │       │       result=tool_result,                │   │
│  │      │       │       raw_result_ref=…)                  │   │
│  │      │       │                                         │   │
│  │      │       ├── _append_result(context, record,        │   │
│  │      │       │       tool_result, turn)                 │   │
│  │      │       │   → ContextManager.add_tool_result()     │   │
│  │      │       │   → context.tool_observations 列表新增    │   │
│  │      │       │                                         │   │
│  │      │       ├── [ok]        → executed_count++         │   │
│  │      │       ├── [failed]    → failed_count++           │   │
│  │      │       └── [pending]   → pending_approval_count++ │   │
│  │      │                                                 │   │
│  │      │  (循环继续，下一个 call)                            │   │
│  │      │                                                 │   │
│  │  (所有 call 执行完毕)                                     │   │
│  │                                                        │   │
│  │  result = ToolProtocolTurnResult(                       │   │
│  │      status, batch_id, executed_count, failed_count,    │   │
│  │      rejected_count, pending_approval_count,            │   │
│  │      appended_tool_message_count, next_action, metadata │   │
│  │  )                                                       │   │
│  │  → ToolProtocolTurnResult {                              │   │
│  │      status,                                             │   │
│  │      next_action  ("continue" / "fail_safe" /            │   │
│  │                    "pending_approval"),                   │   │
│  │      executed_count, failed_count, … }                   │   │
│  │                                                        │   │
│  └──────────────────────────────────────────────────────────┘   │
│                                                              │
│  if workspace_state_hook:                                     │
│      _inject_workspace_state(context, batch,                  │
│          last_tool_call_id)                                    │
│      → context 注入当前工作区状态摘要                             │
│                                                              │
│  return execution → 回到 AgentLoop 路径 C2                     │
│                                                              │
└─────────────────────────────────────────────────────────────┘


╔══════════════════════════════════════════════════════════════╗
║            on_max_turns() — 循环耗尽处理                      ║
║            (agent_loop.py:on_max_turns)                      ║
╚══════════════════════════════════════════════════════════════╝
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────┐
│  触发条件：所有 max_turns 次 turn 均返回 None                     │
│                                                              │
│  1. message = "Stopped after max_turns={max_turns};           │
│       the model did not produce a final answer."              │
│                                                              │
│  2. outcome = ExecutionOutcome(                               │
│         status      = BLOCKED,                                │
│         source      = "agent_loop",                           │
│         reason      = message,                                │
│         error_code  = MAX_TURNS_EXCEEDED,                     │
│         next_action = "blocked",                              │
│         retry_allowed = False,                                │
│         metadata    = {max_turns}                             │
│     )                                                         │
│                                                              │
│  3. controller.apply_outcome(outcome)                         │
│     → reducer.reduce_outcome()                                │
│     → RunLifecycleStatus: current → BLOCKED (terminal=True)   │
│                                                              │
│  4. _record_outcome_context(context, planner, outcome)        │
│                                                              │
│  5. trace.record("error", {type:"MaxTurnsExceeded", message}) │
│     trace.record("final_answer", {turn:max_turns, content})   │
│                                                              │
│  6. return AgentLoopResult(                                   │
│         status       = MAX_TURNS_EXCEEDED,                    │
│         final_answer = message,                               │
│         turn         = max_turns,                             │
│         error_code   = MAX_TURNS_EXCEEDED                     │
│     )                                                         │
│                                                              │
│  → 回到 AgentKernel.run_task()                                │
│  → 落入 else 分支 (非 COMPLETED 非 BLOCKED)                    │
│  → lifecycle.mark_failed()                                    │
│  → shutdown(ERROR)                                            │
│  → RunResult(FAILED)                                          │
│                                                              │
└─────────────────────────────────────────────────────────────┘


═══════════════════════════════════════════════════════════════
                        附 A：状态枚举对照
═══════════════════════════════════════════════════════════════

  ExecutionOutcomeStatus (7 值, turn 层)    AgentLoopStatus (4 值, loop 层)
  ┌──────────────────────────────┐         ┌───────────────────────────────┐
  │ SUCCESS           = 继续/完成│         │ COMPLETED        → 成功终止    │
  │ RETRYABLE         → 继续    │         │ BLOCKED          → 阻断终止    │
  │ REPLAN_REQUIRED   → 继续    │         │ MAX_TURNS_EXCEEDED → 耗尽终止  │
  │ APPROVAL_REQUIRED → 等待    │         │ FAILED           → 失败终止    │
  │ USER_INPUT_REQUIRED→ 等待    │         └───────────────────────────────┘
  │ BLOCKED           → 终止    │
  │ FATAL             → 终止    │         _terminal_result_from_outcome():
  └──────────────────────────────┘         ├── RETRYABLE        → None
                                           ├── REPLAN_REQUIRED  → None
  RunLifecycleStatus (12 值, controller)   ├── APPROVAL_REQUIRED→ None
  ┌──────────────────────────────┐         ├── USER_INPUT_…     → BLOCKED
  │ RUNNING / WAITING_USER /     │         ├── BLOCKED          → BLOCKED
  │ WAITING_APPROVAL / VERIFYING │         └── FATAL            → FAILED
  │ REPAIRING / FINAL_REVIEW /   │
  │ REPORTING → 非终端，继续循环   │         RunStatus (6 值, kernel 层)
  ├──────────────────────────────┤         ┌───────────────────────────────┐
  │ COMPLETED / BLOCKED / FAILED │         │ COMPLETED / BLOCKED / FAILED  │
  │ CANCELLED → 终端，退出循环     │         │ CANCELLED / RUNNING / READY  │
  └──────────────────────────────┘         └───────────────────────────────┘
                                           _attempt_finalize() 返回:
  TaskStatus (14 值, planner 层)           ├── AgentLoopResult(COMPLETED)
  ┌──────────────────────────────┐         │   → 终止循环
  │ INSPECTING_WORKSPACE         │         ├── AgentLoopResult(BLOCKED)
  │ PLANNING_CHANGES             │         │   → 终止 (FINAL_REVIEW_REJECTED)
  │ APPLYING_CHANGES             │         └── None
  │ RUNNING_VERIFICATION         │             → 继续 (REPLAN_REQUIRED)
  │ REPAIRING_FAILURES           │
  │ FINALIZING                   │
  │ COMPLETED                    │
  │ BLOCKED / FAILED / …         │
  └──────────────────────────────┘


═══════════════════════════════════════════════════════════════
                    附 B：ErrorCode 路由分组
═══════════════════════════════════════════════════════════════

  TOOL_BLOCKING_ERROR_CODES (12 codes → BLOCKED, 终止):
    POLICY_BLOCKED, POLICY_DENIED, PROTECTED_PATH_DENIED,
    REVIEW_REQUIRED, APPROVAL_DENIED, ACTION_NOT_ALLOWED,
    RISK_ESCALATED, SANDBOX_REQUIRED, SANDBOX_UNAVAILABLE,
    SANDBOX_VIOLATION, CWD_DENIED, POLICY_ESCALATION_REQUIRED

  TOOL_REPLAN_ERROR_CODES (10 codes → REPLAN_REQUIRED, 继续):
    SNAPSHOT_MISMATCH, EXTERNAL_CHANGE_DETECTED, FILE_CHANGED,
    ROLLBACK_CONFLICT, SEMANTIC_FAILURE, VERIFICATION_FAILED,
    BLOCKED_BY_VERIFICATION, COMMAND_NOT_FOUND,
    PROCESS_NOT_FOUND, TIMEOUT

  TOOL_RETRYABLE_ERROR_CODES (10 codes → RETRYABLE, 继续):
    BAD_ARGUMENTS_JSON, INVALID_JSON, ARGUMENTS_NOT_OBJECT,
    VALIDATION_ERROR, SCHEMA_MISMATCH, UNKNOWN_TOOL,
    TOOL_NOT_FOUND, DISALLOWED_TOOL, PROTOCOL_VIOLATION,
    INTERNAL_ERROR

  FAILURE_ANALYSIS_EXCLUDED_ERROR_CODES (13 codes → 不触发分析):
    APPROVAL_REQUIRED, APPROVAL_DENIED, PERMISSION_DENIED,
    POLICY_BLOCKED, POLICY_DENIED, POLICY_ASK_USER_REQUIRED,
    ACTION_NOT_ALLOWED, PROTECTED_PATH_DENIED, RISK_ESCALATED,
    SANDBOX_REQUIRED, SANDBOX_CAPABILITY_FAILED,
    SANDBOX_VIOLATION, POLICY_ESCALATION_REQUIRED


═══════════════════════════════════════════════════════════════
                附 C：AgentLoop 全部 7 种终止路径
═══════════════════════════════════════════════════════════════

  路径 1: [模型失败 → 不可重试 (BLOCKED/FATAL)]
    run_turn → model_runner.run_turn() → status != SUCCESS
    → _outcome_from_model_failure() → BLOCKED or FATAL
    → _terminal_result_from_outcome() → AgentLoopResult(BLOCKED/FAILED)
    → ★ 终止 ★

  路径 2: [模型失败 → 可重试]
    → _outcome_from_model_failure() → RETRYABLE
    → _terminal_result_from_outcome() → None
    → ★ 继续循环 ★

  路径 3: [无 tool_calls → 最终化成功]
    run_turn → result.tool_calls 为空
    → _attempt_finalize()
    → assess_completion() == COMPLETED
    → planner.finalize() → FinalReport.status == COMPLETED
    → AgentLoopResult(COMPLETED)
    → ★ 终止 ★

  路径 4: [无 tool_calls → 最终化被拒绝 → 继续]
    → assess_completion() != COMPLETED → REPLAN_REQUIRED
    → _maybe_analyze_failure() 未触发阻塞
    → return None
    → ★ 继续循环 ★

  路径 5: [工具执行 → reduced_outcome BLOCKED/FATAL → 终止]
    run_turn → tool_protocol.process_model_turn()
    → controller.reduce_protocol_result() → BLOCKED/FATAL
    → _terminal_result_from_outcome() → AgentLoopResult(BLOCKED/FAILED)
    → ★ 终止 ★

  路径 6: [工具执行 → 正常 → 自动最终化成功]
    → reduced_outcome 为 None 或 RETRYABLE/REPLAN_REQUIRED
    → _should_auto_finalize_after_tools() == True
    → _attempt_finalize() → COMPLETED
    → AgentLoopResult(COMPLETED)
    → ★ 终止 ★

  路径 7: [max_turns 耗尽]
    run_loop → 所有 turn 返回 None
    → on_max_turns(max_turns)
    → AgentLoopResult(MAX_TURNS_EXCEEDED)
    → ★ 终止 ★


═══════════════════════════════════════════════════════════════
           附 D：_maybe_analyze_failure() 触发条件汇总
═══════════════════════════════════════════════════════════════

  AgentLoop 中 5 处调用 _maybe_analyze_failure()：

  调用点 1: 路径 C7 — tool 失败后
    outcome=reduced_outcome, failure_source="tool"
    → _should_analyze_outcome() 检查:
      1) status == REPLAN_REQUIRED
      2) error_code 不在 FAILURE_ANALYSIS_EXCLUDED 中
      3) 若为 COMPLETION_REJECTED → 需重复 stalls 才升级

  调用点 2: 路径 C8 — verification 失败后
    outcome=None, failure_source="verification"
    → _has_repairable_planner_failure() 检查:
      1) 最近 verification assessment 为 failed/blocked/needs_review
      2) 或 unresolved_failures 有非排除 error_code

  调用点 3: _attempt_finalize → 门控未通过
    outcome=REPLAN_REQUIRED, failure_source="completion"
    → 同 _should_analyze_outcome()

  调用点 4: _attempt_finalize → 最终化后 final_report rejected
    outcome, failure_source="completion_review"
    → 同 _should_analyze_outcome()

  调用点 5: _attempt_finalize → 路径 F1 中的 _maybe_analyze_failure
    outcome=REPLAN_REQUIRED, failure_source="completion"
    → 同调用点 3


═══════════════════════════════════════════════════════════════
           附 E：Phase 6.2 sandbox / verification / eval 旁路
═══════════════════════════════════════════════════════════════

  E1. sandbox doctor / setup / cleanup（CLI 能力诊断，不进入 AgentLoop）
    python -m singularity.cli sandbox doctor --json
    → WindowsSandboxBackend.doctor()
    → probe_windows_sandbox()
    → offline / online 双账户检查：
      1) account / credential / login UI / logon rights / group membership
      2) state dir ACL boundary / runner smoke / network probe
      3) Python runtime smoke:
         import _ssl, ssl, socket, hashlib, pathlib
         → _ssl.__file__, ssl.OPENSSL_VERSION,
            ssl.get_default_verify_paths()
         → OpenSSL DLL / config / provider / cert / TEMP access
    → Python runtime smoke 失败：
      diagnostics += {kind: "python_runtime_environment_blocker",
                      failure_type, module, sandbox_role,
                      module_status, runtime target hashes,
                      runner evidence, redacted/hash details}
      只扩展 diagnostics，不改变 doctor schema v2，也不改变原有
      enforcement available 计算；runtime ACL 只覆盖明确发现的
      Python/OpenSSL target，不恢复 base 根目录 RX。

    python -m singularity.cli sandbox setup --json
    → setup_windows_sandbox()
    → 授权 Python runtime targets 前：
      清理 base runtime 根目录上 sandbox 账户 stale explicit ACE
    → 只恢复 base 根目录 RX 和精确 runtime targets RX/(OI)(CI)RX
      不递归授权整个 Anaconda/base install、包缓存、用户目录或配置目录。

    python -m singularity.cli sandbox cleanup --json
    → cleanup_windows_sandbox_assets()
    → 删除 credential / firewall / login UI / attestation
    → 移除两个 current sandbox 账户在相同 runtime targets 上的全部显式 ACE
    → residual_audit 非零则 cleanup failed。

  E2. Windows sandbox command runtime recheck（AgentLoop 内命令分支）
    CommandExecutor.run()
    → SandboxManager.run()
    → WindowsSandboxBackend.run(prepared)
    → uncached enforcement probe
    → 若 blocking_requirements == ["execution:network_probe"]：
         读取 PreparedSandbox.baseline.sandbox_role
         只消费当前命令账户对应 offline/online 子状态；
         当前角色 ready 时可忽略另一账户瞬时 network_probe 失败。
      否则任一 setup、launcher、ACL、runner smoke、network filter
      或其他 enforcement blocker 仍 fail closed → BACKEND_UNAVAILABLE。
    → WindowsSandboxBackend.cleanup(prepared)
      1) 复用 PreparedSandbox.baseline.sandbox_account / credential_target
         启动同一 sandbox 账户的 Level-1 runner；
      2) runner spec operation="workspace_cleanup"，只删除当前
         runs/<sandbox_id>/workspace projection，不创建 Level-2
         low-integrity child，不扩大 runtime ACL；
      3) 宿主进程只对当前 runs/sandbox_* run root 执行 take ownership、
         ACL reset、host SID full-control、medium integrity 和属性恢复；
      4) 任一步失败 → cleanup_failed，不能把命令 success 伪装为完成。

  E3. verification failure 分类（AgentLoop 内 run_verification 工具）
    run_verification tool
    → VerificationRunner.run_plan()
    → CommandExecutor.run()
    → FailureParserRegistry.parse()
    → classify_failure()
    → Python DLL/import 初始化失败（如 _ssl/_hashlib/_socket、
      libssl/libcrypto、OpenSSL provider/config/cert、DLL search path 或
      "DLL initialization routine failed"）
      = FailureType.ENVIRONMENT_ERROR
      = VerificationResult.status BLOCKED
      = 不生成普通代码 repair_hints
    → Planner / CompletionGate 将其作为环境 blocker 证据消费，
      不进入普通业务代码 repair。

  E4. capability regression evaluation 归约（真实 provider + AgentLoop 外层）
    python -m singularity.cli eval run docs/evaluation/public-representative-task.json
    → EvaluationRunner.run()
    → 每个 task 启动真实 KernelBootstrap → AgentGraphBuilder
      → AgentKernel → AgentLoop.run
    → post-agent verification 只作为 evaluation scoring 证据，
      不回灌 AgentLoop completion。
    → _task_result() 写 EvaluationTaskResult.evaluation_metrics:
        schema_version = evaluation.metrics/v1
      resolved.value 只投影 evaluation_passed；
      FAIL_TO_PASS/PASS_TO_PASS、verification、patch、trajectory、
      tools、context/compaction、efficiency、cost、safety 均为
      诊断/回归分析 scorecard，不改变 evaluation_passed /
      tests_passed / agent_completed / status 语义。
      cost 优先读 provider usage 的 cost_estimate；否则只按
      token usage 与精确模型价格表计算，unknown 不影响 gate。
    → final report / failure repair summary 中的 latest_failure_category:
        environment_error 或 sandbox_limitation → environment_blocker
    → benchmark fixture 中 simple_patch / multi_file_reasoning /
      failure_repair 的内联 pytest.ini 只禁用 anyio 自动插件；
      completion_gate fixture 和生产项目 pytest 行为不变。

  E5. session 历史打开、继续与中断恢复（统一 session recovery path）
    sg session list
      → SessionStore.list_sessions()
      → 展示 status / updated_at / project_root / last_task_status /
        sg session show、sg continue、sg resume 可复制命令。

    sg session show <session_id> --timeline
      → SessionStore.show_session()
      → SessionHistoryReader.build_show_summary()
      → 展示 conversation 摘要、planner 状态、workspace checkpoint、
        tool protocol recovery 摘要、trace/verification 摘要、失败摘要和 timeline。

    sg continue <session_id> "<instruction>"
      → ProductionConfig(session_run_mode="continue", resume_session=session_id)
      → KernelBootstrap.prepare_launch(mode=continue)
      → Planner.resume(session_id)
      → Planner.continue_with_instruction(instruction)
      → ContextManager.seed_session_resume_context(filtered summary)
      → SessionRecoveryGate.evaluate()
      → gate 放行才进入 AgentLoop.run。

    sg resume <session_id>
      → ProductionConfig(session_run_mode="resume", resume_session=session_id)
      → KernelBootstrap.prepare_launch(mode=resume)
      → CrashRecoveryManager.inspect(session_id=session_id)
      → ToolProtocolRecoveryManager.inspect(previous run tool_protocol.sqlite3)
      → WorkspaceStateManager.recover_session(session_id)
      → SessionRecoveryGate.evaluate()
      → external user change / rollback conflict / stale lock /
        unfinished mutation journal / leftover sandbox / pending approval /
        running or pending tool 均写入 trace、session timeline、checkpoint
        或 final report；默认 fail closed，不盲目覆盖用户外部改动。
```

---

*维护规则：任何改变主链路、状态映射、错误码路由、工具协议执行、最终化、失败分析或 shutdown/finalize 行为的源码变更，都必须同步更新本文件对应段落。*
