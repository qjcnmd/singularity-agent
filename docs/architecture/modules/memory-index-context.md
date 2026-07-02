# Memory / Index / Context模块数据流

模块数据流文档 ID: memory-index-context

源码证据路径:
- src/singularity/memory/models.py
- src/singularity/memory/pipeline.py
- src/singularity/memory/retrieval.py
- src/singularity/memory/store.py
- src/singularity/memory/injector.py
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
- src/singularity/memory/store.py
- src/singularity/memory/injector.py
- src/singularity/code_index/index.py
- src/singularity/code_index/context.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

`AgentGraphBuilder._build_infra()` 创建 `ProjectIndex` 与 `MemoryLearningPipeline` -> `_prime_planner_context()` 生成 project index observation 与 `MemoryContextBlock` -> `ContextManager` 纳入模型上下文 -> session end 写入候选 memory。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例：`ProjectIndex.bootstrap()` -> `ProjectIndex.observation_for_goal()` 先生成 project index observation，`MemoryLearningPipeline.retrieve()` -> `MemoryRetriever.search()` 再生成对象 `MemorySearchResult`。`MemoryInjector.build_block()` 把候选 memory 压缩为 `MemoryContextBlock`，由 `ContextAssembler.build_bundle()` 写入模型 context 和 `context.sqlite3` 的 item 引用。任务结束时 `MemoryLearningPipeline.ingest_final_report()`、`ingest_verification_result()`、`ingest_trace_summary()` 生成候选 memory，`MemoryStore` 写入 `.singularity/memory/auto/candidates.jsonl` 或 `entries.jsonl`；过期、模板-only 或低置信记录会被 policy/maintenance 阻止进入 context。

## 真实对象完整结构

### MemoryEntry（记忆条目）

memory store 的持久化记录。**边界**：落盘对象，写入 `.singularity/memory/auto/entries.jsonl`；检索结果经 `MemoryInjector` 投影为 `MemoryContextBlock` 后才可能进入模型。

```python
@dataclass
class MemoryEntry:
    id: str
    scope: MemoryScope | str
    type: MemoryType | str
    source: MemorySource | str
    title: str
    body: str
    confidence: Confidence | str = Confidence.MEDIUM
    provenance: Provenance = field(default_factory=Provenance)
    ttl: TTL = field(default_factory=TTL)
    conflict_status: ConflictStatus | str = ConflictStatus.NONE
    status: MemoryStatus | str = MemoryStatus.ACTIVE
    author_type: MemoryAuthorType | str = MemoryAuthorType.AGENT
    created_at: str = field(default_factory=lambda: _now())
    updated_at: str = field(default_factory=lambda: _now())
    last_verified_at: str | None = None
    tags: list[str] = field(default_factory=list)
    paths: list[str] = field(default_factory=list)
    tools: list[str] = field(default_factory=list)
    error_types: list[str] = field(default_factory=list)
    modules: list[str] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)
    tombstone_reason: str | None = None
    rejection_reason: str | None = None
    schema_version: int = SCHEMA_VERSION
```

### MemoryContextBlock（记忆上下文块）

进入 ContextManager 的 memory 投影。**边界**：内部治理对象，投影写 `context.sqlite3`；其裁剪后的 items 文本可能进入模型，但完整 block 不发送。

```python
@dataclass
class MemoryContextBlock:
    items: list[dict[str, Any]]
    token_count: int
    budget: int
    component: str
    priority: float
    pollution_risk: str
    generated_at: str
```

### 关键枚举值域

```python
class MemoryScope(str, Enum):        # MemoryEntry.scope
    SESSION = "session"
    WORKSPACE = "workspace"
    PROJECT = "project"
    USER_PREFERENCE = "user_preference"
    TOOL_EXECUTOR = "tool_executor"

class MemoryType(str, Enum):         # MemoryEntry.type
    PROJECT_CONVENTION = "project_convention"
    BUILD_COMMAND = "build_command"
    TEST_COMMAND = "test_command"
    MODULE_BOUNDARY = "module_boundary"
    USER_PREFERENCE = "user_preference"
    LESSON = "lesson"
    CAUTION = "caution"
    FAILURE_LESSON = "failure_lesson"
    VERIFICATION_FACT = "verification_fact"

class MemoryStatus(str, Enum):       # MemoryEntry.status
    CANDIDATE = "candidate"
    ACTIVE = "active"
    QUARANTINED = "quarantine"
    REJECTED = "rejected"
    TOMBSTONED = "tombstoned"
    EXPIRED = "expired"

class Confidence(str, Enum):         # MemoryEntry.confidence
    LOW = "low"          # score 0.35
    MEDIUM = "medium"    # score 0.6
    HIGH = "high"        # score 0.82
    VERIFIED = "verified" # score 0.95
```

### 数据流概述

`ProjectIndex.bootstrap()` 生成 project index observation，`MemoryRetriever.search()` 生成 `MemorySearchResult`。`MemoryInjector.build_block()` 把候选 memory 压缩为 `MemoryContextBlock`，由 `ContextAssembler.build_bundle()` 写入模型 context 和 `context.sqlite3`。任务结束时 `MemoryLearningPipeline.ingest_final_report()` 等生成候选 memory，`MemoryStore` 写入 `candidates.jsonl` 或 `entries.jsonl`。完整 memory body 不写 trace；memory policy decision 属 memory 内部状态，不写 capability policy audit。

## 谁生成这些对象

`MemoryExtractor.extract()` 生成 `MemoryEvidenceRef`、`Provenance`、`TTL`、`MemoryCandidate` 和接受后的 `MemoryEntry`；`MemoryLearningPipeline.retrieve()` 生成 `MemoryQuery`，`MemoryRetriever.search()` 生成 `MemorySearchResult`，`MemoryInjector.build_block()` 从排序结果生成 `MemoryContextBlock`。

## 谁消费这些对象

MemoryStore/maintenance 消费 candidate/entry 及嵌套 provenance/TTL；retriever 消费 query/entry。`ContextManager.add_memory_context_block()` 消费 context block，只有其裁剪后的 items 文本可能进入模型；query、search result 与完整 entry metadata 不直接发送。

## 是否落盘

`MemoryStore` 将 accepted entries 写 `.singularity/memory/auto/entries.jsonl`，candidates 写 `candidates.jsonl`；evidence/provenance/TTL 嵌在父对象。Query/search result 仅内存；context block 投影写当前 run 的 `context.sqlite3`。

## 是否进入 trace / audit

MemoryLearningPipeline 只记录 ingest、accept/reject/quarantine、maintenance 与 retrieval 计数/ids；`retrieve()` 额外写 `retrieval.query.completed {duration_ms, result_count}`，不写 query、完整 memory body 或匹配正文。memory policy decision 属 memory 内部状态，不写 capability policy audit。

## 失败路径

schema version 不匹配抛 `ValueError`；expired、inactive、conflict/tombstoned 条目被检索过滤，policy 可 quarantine/reject candidate。污染风险或预算不足使 context block/item 被 ContextManager 排除，而不是绕过信任级别进入模型。

## 当前结构问题

durable memory、检索排序结果与模型可见 context block 是三层对象；修改 entry schema 时必须同步 store/retrieval/injector，不能把完整 memory JSON 当作 prompt。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
