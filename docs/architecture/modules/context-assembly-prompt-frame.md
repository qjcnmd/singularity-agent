# Context Assembly / Prompt Frame模块数据流

模块数据流文档 ID: context-assembly-prompt-frame

源码证据路径:
- src/singularity/context/models.py
- src/singularity/context/manager.py
- src/singularity/context/assembler.py
- src/singularity/context/compaction.py
- src/singularity/context/compression.py
- src/singularity/instructions/prompt_assembly.py
- src/singularity/instructions/models.py

关键符号:
- ContextItem
- ContextReference
- ContextBudgetPlan
- ContextBundle
- ContextSummaryPayload
- ContextSummaryEnvelope
- PromptManifest
- PromptBundle

字段清单:
- ContextReference: ref_id, ref_type, target, path, line_start, line_end, digest, observed_at, freshness, source_item_id, metadata, observation_id
- ContextItem: item_id, run_id, session_id, task_id, phase_id, layer, source_component, item_type, content, content_digest, created_at, updated_at, importance, relevance_score, authority, freshness, sensitivity, token_count, references, metadata, pinned, expires_at
- ContextBudgetPlan: model_context_window, output_token_reserve, reasoning_token_reserve, tool_schema_tokens, system_tokens, pinned_tokens, evidence_tokens, recent_dialogue_tokens, summary_tokens, available_tokens, used_tokens, overflow_tokens, soft_limit, hard_limit, message_tokens
- ContextRenderPolicy: include_raw_tool_outputs, include_policy_details, include_secret_content, include_full_diff, include_failed_attempts, max_tool_preview_tokens, max_evidence_items, max_recent_turns, require_references_for_claims, redact_sensitive, phase_aware
- ContextBundle: bundle_id, run_id, task_id, phase_id, model, provider, messages, included_item_ids, excluded_item_ids, budget, compression_snapshot_id, retrieval_query, render_policy, created_at, bundle_digest, metadata
- ContextUsageReport: layer_token_usage, included_item_ids, excluded_item_ids, stale_item_ids, summary_item_ids, recent_tail_item_ids, input_tokens, cached_input_tokens, cache_hit_ratio, cache_miss_reasons, cache_attribution, recommendations
- ContextSummaryPayload: goal, current_state, completed_actions, pending_actions, verified_facts, failed_attempts, policy_constraints, workspace_changes, verification_status, open_questions, reference_ids, omitted_item_ids, confidence
- ContextSummaryEnvelope: version, summary_id, summary_payload, source_item_ids, cache_attribution, previous_summary_digest, summary_digest, rendered_summary, created_at, metadata
- PromptManifest: manifest_id, bundle_id, purpose, source_count, section_count, trust_summary, priority_summary, conflict_count, injection_warning_count, redaction_applied, prompt_hash, token_estimate, folded_developer_into_system, metadata
- PromptBundle: bundle_id, purpose, messages, sections, manifest, token_estimate, prompt_hash, created_at, metadata

## 这一层解决什么问题

Context 层把系统提示、用户目标、planner 状态、memory、project index、工具观察和验证证据整理为可进入模型请求的上下文 bundle。

## 当前源码位置

- src/singularity/context/models.py
- src/singularity/context/manager.py
- src/singularity/context/assembler.py
- src/singularity/context/compaction.py
- src/singularity/context/compression.py
- src/singularity/instructions/prompt_assembly.py
- src/singularity/instructions/models.py

## 关键类、函数、字段

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

`AgentGraphBuilder._build_model_context()` 创建 `ContextManager` -> `AgentLoop.run()` 每 turn 写入 planner/model/tool/verification 观察 -> `ModelRunner.build_request_from_context()` 读取 bundle 并构造 `ModelTurnRequest`。

## 真实对象完整结构

- `ContextItem（上下文条目）` 完整字段列在字段清单中，既可来自模型、工具、planner、policy、workspace state，也可来自 memory/project index。
- `ContextBundle（上下文包）` 是进入模型前的消息集合和预算诊断，消费者是 `ModelRunner`。

## 谁生成这些对象

- `ContextManager._make_item()` 与各 `add_*` 入口生成 `ContextItem` 和 `ContextReference`；tool result、planner state、memory、project index、policy、verification 与 assistant message 都先转换成这两个内部对象。
- `ContextAssembler.build_bundle()` 根据 token counter、phase、visibility、freshness、sensitivity 和 `ContextRenderPolicy` 生成 `ContextBudgetPlan`、`ContextBundle` 与初始 `ContextUsageReport`；usage reporter 再用实际 provider usage 更新报告。
- compaction executor 生成并校验 `ContextSummaryPayload`，`summary_envelope_for_plan()` 生成 `ContextSummaryEnvelope`。`PromptAssemblyPipeline.build_for_model_turn()` 收集/解析 instruction sources，`PromptCompiler.compile()` 生成 `PromptManifest` 与 `PromptBundle`。

## 谁消费这些对象

- ObservationStore、assembler、compaction 和 failure request 消费 `ContextReference`/`ContextItem`。只有通过 visibility、预算与 redaction 的 item 内容进入 `ContextBundle.messages`；完整 item/reference 元数据不发送给 provider。
- `ModelRunner.build_request_from_context()` 消费 `ContextBundle`；其中 `messages` 直接组成 `ModelTurnRequest.messages`，`ContextBudgetPlan`、`ContextRenderPolicy` 与 `ContextUsageReport` 只用于内部诊断，不作为消息正文。
- compaction committer/recovery 消费 `ContextSummaryEnvelope`，其 `rendered_summary` 通过 summary item 进入后续模型请求。`ModelTurnRequestBuilder.build_request()` 消费 `PromptBundle.messages` 并与 context messages 合并；`PromptManifest` 不进模型，只用于 hash、预算、trace 与诊断。

## 是否落盘

- `ObservationStore` 在当前 trace run 目录的 `context.sqlite3` 写 `context_items`、`context_references`、tool observations/messages、`context_bundles` 和 snapshot 数据。`ContextBudgetPlan`、`ContextRenderPolicy`、usage metadata 嵌在 bundle 行内。
- `ContextSummaryPayload`/`ContextSummaryEnvelope` 嵌入 summary `ContextItem` 与 snapshot metadata，不另建独立表。`PromptBundle` 不写 context DB，也不保存完整 prompt 正文副本。
- 配置 `store_prompt_manifest` 时，`PromptAssemblyPipeline._emit_bundle_events()` 通过 `TraceRecorder.write_artifact()` 写 redacted prompt manifest artifact；默认 trace artifact 索引为 `work/traces/runs/<run_id>/artifacts.jsonl`，文件在同目录 `artifacts/`。

## 是否进入 trace / audit

- context 增量写 `context.item_added` 等摘要事件；bundle 构造写 `context.bundle_built`、`context.rendered_for_model`，payload 只含 bundle id、included/excluded ids 与 token 统计，不写完整敏感正文。实际 cache usage 写 `context.cache_usage_recorded`。
- prompt assembly 写 `instruction_sources_collected`、`instruction_conflict_detected`、`instruction_injection_detected`、`prompt_compiled` 与 `prompt_manifest_created`。injection excerpt 在事件前替换成 hash/`<redacted>`；manifest artifact 仅在配置开启时产生。
- 本层不写 policy audit；若 context 来源是 policy observation，保存的是已经由 PolicyEngine/audit 产生并经 ContextManager 投影的摘要。

## 失败路径

- bundle 超过 hard limit 且无法通过选择/压缩收敛时 `ContextAssembler` 抛 `ContextOverflowError`；敏感、过期、低相关或超预算 item 进入 `excluded_item_ids`，不通过“截断后仍发送原文”的方式绕过。
- summary 的 invalid JSON、缺 reference、内容漂移、previous/summary digest 或版本不匹配会使 compaction validation/commit 失败，旧 snapshot 仍保持有效。
- prompt 检测到 critical injection 且 `fail_on_critical_injection` 开启时抛 `PromptInjectionWarning`；token estimate 超 `max_prompt_tokens` 时抛 `PromptBudgetExceeded`。`build_for_model_turn()` 将 instruction span 标记 failed 后继续抛出，不降级为未审查 prompt。

## 当前结构问题

`ContextBundle.messages` 与 `PromptBundle.messages` 最终在 request builder 合并，但两套 bundle 的预算/hash/trace 责任不同；维护时必须分别说明“上下文选择”和“指令编译”，不能把所有 ContextItem 描述成模型可见，也不能把 prompt manifest 当作 provider payload。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
