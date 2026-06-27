# Trace / Observation / Audit Events模块数据流

模块数据流文档 ID: trace-observation-audit-events

源码证据路径:
- src/singularity/observability/models.py
- src/singularity/observability/recorder.py
- src/singularity/observability/store.py
- src/singularity/policy/audit.py

关键符号:
- TraceEvent
- TraceSpan
- TraceArtifact
- TraceTimelineItem
- TraceSummary
- TraceRecorder

字段清单:
- TraceEvent: event_id, event_type, run_id, session_id, task_id, phase_id, action_id, parent_event_id, timestamp, monotonic_ms, component, severity, summary, payload, artifact_refs, policy_decision_id, approval_grant_id, sandbox_id, command_id, transaction_id, verification_id, span_id, redaction_applied, payload_hash
- TraceSpan: span_id, parent_span_id, run_id, session_id, task_id, phase_id, action_id, name, component, started_at, ended_at, duration_ms, status, error_type, error_message, attributes, artifact_refs
- TraceArtifact: artifact_id, run_id, session_id, task_id, kind, path, relative_path, size_bytes, sha256, content_type, redacted, sensitive, summary, metadata
- TraceTimelineItem: timestamp, event_id, event_type, component, summary, severity, related_ids, artifact_refs
- TraceSummary: run_id, session_id, task_id, total_events, total_spans, total_artifacts, action_count, failed_action_count, command_count, sandboxed_command_count, mutation_count, verification_count, policy_denial_count, approval_count, replan_count, error_count, critical_events, key_artifacts, model_usage_summary

## 这一层解决什么问题

Trace 层记录运行事件、span、artifact、timeline 和 summary；audit 相关数据由 policy 与 approval 链路写入，用于复现和最终报告。

## 当前源码位置

- src/singularity/observability/models.py
- src/singularity/observability/recorder.py
- src/singularity/observability/store.py
- src/singularity/policy/audit.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

各组件调用 `trace.record()` / `trace.emit()` -> `TraceRecorder` redaction -> `TraceStore` 写 events/spans/artifacts -> timeline/summary/final report/evaluation result 引用。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例：`TraceRecorder.emit()` / `record()` -> `TraceStore.append_event()` 先生成对象 `TraceEvent` 并写入 run 目录 `events.jsonl`；`TraceRecorder.start_span()` / `end_span()` 生成 `TraceSpan` 写入 `spans.jsonl`，`TraceRecorder.write_artifact()` 生成 `TraceArtifact` 写入 `artifacts.jsonl` 和 `index.json`。`TraceStore.get_timeline()` 读取 events/spans 生成 `TraceTimelineItem`，`TraceStore.summarize()` 生成 `TraceSummary`；final report 和 evaluation result 只消费 summary/artifact refs。policy audit 仍由 `PolicyAuditWriter.append()` 写 `audit.jsonl`，trace event 不能替代 audit entry。

## 真实对象完整结构

- `TraceEvent（追踪事件）` 完整字段列在字段清单中，payload 需要 redaction。
- `TraceArtifact（追踪产物）` 完整字段列在字段清单中，消费者是 final report、evaluation report 和 failure case replay。

## 谁生成这些对象

`TraceRecorder.emit()`经redactor生成`TraceEvent`；SpanManager的start/end生成追加式`TraceSpan`；TraceArtifactStore写文件后生成`TraceArtifact`。`TraceTimelineBuilder`从events派生`TraceTimelineItem`，`TraceSummaryBuilder`从events/spans/artifacts聚合`TraceSummary`。

## 谁消费这些对象

TraceStore消费event/span/artifact；CLI、final report、evaluation/replay消费timeline/summary/artifact refs。完整trace对象不自动进入模型；只有`ContextManager.add_trace_summary()`生成的安全文本摘要进入context。

## 是否落盘

默认run目录`work/traces/runs/<run_id>/`包含`events.jsonl`、`spans.jsonl`、`artifacts.jsonl`、`index.json`和`artifacts/`文件。Timeline/Summary按需派生不独立落盘；其context投影写`context.sqlite3`。

## 是否进入 trace / audit

TraceEvent在append前执行payload redaction并计算payload_hash；span/artifact通过refs关联。Policy audit是独立JSONL，由PolicyAuditWriter保存request/decision摘要，不能用events.jsonl替代审计账本，也不能把audit entry称为TraceEvent。

## 失败路径

非法run id抛`ValueError`，未知span抛`TraceStoreError`，artifact错误抛`TraceArtifactError`。`TraceRecorder.emit()`写失败降级返回`trace_write_failed` warning dict并输出脱敏stderr警告；业务执行继续，但final diagnostics应暴露trace不完整。

## 当前结构问题

events、spans、artifact index、timeline/summary与policy audit是不同层；新增event时必须定义payload来源、redaction、相关id和artifact refs，不能只在报告端猜测。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
