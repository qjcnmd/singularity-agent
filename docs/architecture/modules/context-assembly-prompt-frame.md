# Context Assembly / Prompt Frame模块数据流

模块数据流文档 ID: context-assembly-prompt-frame

源码证据路径:
- src/singularity/context/models.py
- src/singularity/context/manager.py
- src/singularity/context/assembler.py
- src/singularity/instructions/prompt_assembly.py

关键符号:
- ContextItem
- ContextReference
- ContextBudgetPlan
- ContextBundle
- ContextSummaryPayload
- ContextSummaryEnvelope

字段清单:
- ContextReference: ref_id, ref_type, target, path, line_start, line_end, digest, observed_at, freshness, source_item_id, metadata, observation_id
- ContextItem: item_id, run_id, session_id, task_id, phase_id, layer, source_component, item_type, content, content_digest, created_at, updated_at, importance, relevance_score, authority, freshness, sensitivity, token_count, references, metadata, pinned, expires_at
- ContextBudgetPlan: model_context_window, output_token_reserve, reasoning_token_reserve, tool_schema_tokens, system_tokens, pinned_tokens, evidence_tokens, recent_dialogue_tokens, summary_tokens, available_tokens, used_tokens, overflow_tokens, soft_limit, hard_limit, message_tokens
- ContextRenderPolicy: include_raw_tool_outputs, include_policy_details, include_secret_content, include_full_diff, include_failed_attempts, max_tool_preview_tokens, max_evidence_items, max_recent_turns, require_references_for_claims, redact_sensitive, phase_aware
- ContextBundle: bundle_id, run_id, task_id, phase_id, model, provider, messages, included_item_ids, excluded_item_ids, budget, compression_snapshot_id, retrieval_query, render_policy, created_at, bundle_digest, metadata
- ContextUsageReport: layer_token_usage, included_item_ids, excluded_item_ids, stale_item_ids, summary_item_ids, recent_tail_item_ids, input_tokens, cached_input_tokens, cache_hit_ratio, cache_miss_reasons, cache_attribution, recommendations
- ContextSummaryPayload: goal, current_state, completed_actions, pending_actions, verified_facts, failed_attempts, policy_constraints, workspace_changes, verification_status, open_questions, reference_ids, omitted_item_ids, confidence
- ContextSummaryEnvelope: version, summary_id, summary_payload, source_item_ids, cache_attribution, previous_summary_digest, summary_digest, rendered_summary, created_at, metadata

## 这一层解决什么问题

Context 层把系统提示、用户目标、planner 状态、memory、project index、工具观察和验证证据整理为可进入模型请求的上下文 bundle。

## 当前源码位置

- src/singularity/context/models.py
- src/singularity/context/manager.py
- src/singularity/context/assembler.py
- src/singularity/instructions/prompt_assembly.py

## 关键类、函数、字段

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

`AgentGraphBuilder._build_model_context()` 创建 `ContextManager` -> `AgentLoop.run()` 每 turn 写入 planner/model/tool/verification 观察 -> `ModelRunner.build_request_from_context()` 读取 bundle 并构造 `ModelTurnRequest`。

## 真实对象完整结构

- `ContextItem（上下文条目）` 完整字段列在字段清单中，既可来自模型、工具、planner、policy、workspace state，也可来自 memory/project index。
- `ContextBundle（上下文包）` 是进入模型前的消息集合和预算诊断，消费者是 `ModelRunner`。

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
