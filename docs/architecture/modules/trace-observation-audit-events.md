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

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

各组件调用 `trace.record()` / `trace.emit()` -> `TraceRecorder` redaction -> `TraceStore` 写 events/spans/artifacts -> timeline/summary/final report/evaluation result 引用。

## 真实对象完整结构

- `TraceEvent（追踪事件）` 完整字段列在字段清单中，payload 需要 redaction。
- `TraceArtifact（追踪产物）` 完整字段列在字段清单中，消费者是 final report、evaluation report 和 failure case replay。

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
