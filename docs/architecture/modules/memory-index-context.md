# Memory / Index / Context模块数据流

模块数据流文档 ID: memory-index-context

源码证据路径:
- src/singularity/memory/models.py
- src/singularity/memory/pipeline.py
- src/singularity/memory/retrieval.py
- src/singularity/code_index/index.py
- src/singularity/code_index/context.py

关键符号:
- MemoryEntry
- MemoryCandidate
- MemoryQuery
- MemorySearchResult
- MemoryContextBlock
- MemoryLearningPipeline
- ProjectIndex

字段清单:
- MemoryEvidenceRef: source, ref_id, summary, event_id, artifact_ref, path, captured_at, trust_level, metadata
- Provenance: evidence, created_by, source_run_id, source_session_id, source_task_id, extracted_at, notes
- TTL: expires_at, stale_after, reason
- MemoryEntry: id, scope, type, source, title, body, confidence, provenance, ttl, conflict_status, status, author_type, created_at, updated_at, last_verified_at, tags, paths, tools, error_types, modules, metadata, tombstone_reason, rejection_reason, schema_version
- MemoryCandidate: id, scope, type, source, title, body, confidence, provenance, ttl, status, author_type, created_at, updated_at, last_verified_at, tags, paths, tools, error_types, modules, metadata, decision_reason, schema_version
- MemoryQuery: goal, paths, tools, error_types, modules, limit, min_confidence
- MemorySearchResult: entry, score, matched_fields
- MemoryContextBlock: items, token_count, budget, component, priority, pollution_risk, generated_at

## 这一层解决什么问题

Memory 与 code index 层为上下文提供项目约定、失败经验、检索结果和代码结构摘要，但进入模型前仍由 ContextManager 过滤、预算和脱敏。

## 当前源码位置

- src/singularity/memory/models.py
- src/singularity/memory/pipeline.py
- src/singularity/memory/retrieval.py
- src/singularity/code_index/index.py
- src/singularity/code_index/context.py

## 关键类、函数、字段

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

`AgentGraphBuilder._build_infra()` 创建 `ProjectIndex` 与 `MemoryLearningPipeline` -> `_prime_planner_context()` 生成 project index observation 与 `MemoryContextBlock` -> `ContextManager` 纳入模型上下文 -> session end 写入候选 memory。

## 真实对象完整结构

- `MemoryEntry（记忆条目）` 完整字段列在字段清单中，落盘到 memory store。
- `MemoryContextBlock（记忆上下文块）` 完整字段列在字段清单中，是进入 ContextManager 的 memory 投影。

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
