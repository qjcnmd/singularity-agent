# Artifact / Long Result Handling模块数据流

模块数据流文档 ID: artifact-long-result-handling

源码证据路径:
- src/singularity/observability/artifacts.py
- src/singularity/observability/models.py
- src/singularity/tool_protocol/result.py
- src/singularity/tools/executor.py

关键符号:
- TraceArtifactStore
- TraceArtifact
- TraceArtifactKind

字段清单:
- TraceArtifact: artifact_id, run_id, session_id, task_id, kind, path, relative_path, size_bytes, sha256, content_type, redacted, sensitive, summary, metadata

## 这一层解决什么问题

Artifact 层把过长输出、trace 产物、命令日志、diff、报告和模型消息转成 artifact handle，避免大对象直接进入模型上下文或 result payload。

## 当前源码位置

- src/singularity/observability/artifacts.py
- src/singularity/observability/models.py
- src/singularity/tool_protocol/result.py
- src/singularity/tools/executor.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

组件发现长输出或文件产物 -> `TraceArtifactStore.write_text_artifact()` / `write_bytes_artifact()` / `register_file_artifact()` -> `TraceArtifact` -> trace/event/result/report 只引用 artifact id 或 relative handle。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例：`TraceArtifactStore.write_text_artifact()` / `write_bytes_artifact()` / `register_file_artifact()` -> `TraceRecorder.write_artifact()` -> `TraceStore.append_artifact()` 先生成对象 `TraceArtifact`，再把 artifact 元数据写入 `artifacts.jsonl` 和 `index.json`。`CommandResult.artifact_path`、`ToolProtocolResultEnvelope.raw_result_ref`、`TraceEvent.artifact_refs` 和 `TraceSummary.key_artifacts` 只消费 artifact 引用；`TraceRecorder.context_summary()` 与 `final_report_summary()` 读取这些引用生成摘要，evaluation report 透传 artifact ref，不把长结果原文重新写入模型 context。

## 真实对象完整结构

### TraceArtifact（追踪产物）

大输出的文件引用元数据。**边界**：trace 对象，落盘到 `artifacts.jsonl` + `artifacts/` 文件；artifact ref 进入 final report 和 evaluation result，不进入模型请求。

```python
@dataclass(frozen=True)
class TraceArtifact:
    artifact_id: str
    run_id: str
    session_id: str
    task_id: str | None
    kind: TraceArtifactKind
    path: Path
    relative_path: str
    size_bytes: int
    sha256: str
    content_type: str
    redacted: bool
    sensitive: bool
    summary: str
    metadata: dict[str, Any] = field(default_factory=dict)
```

### TraceArtifactKind（产物分类枚举）

```python
class TraceArtifactKind(str, Enum):
    STDOUT = "stdout"
    STDERR = "stderr"
    DIFF = "diff"
    REPORT = "report"
    SNAPSHOT = "snapshot"
    SANDBOX = "sandbox"
    VERIFICATION = "verification"
    EDIT_PLAN = "edit_plan"
    MODEL_MESSAGE = "model_message"
    PROMPT_MANIFEST = "prompt_manifest"
    COMMAND_LOG = "command_log"
    POLICY_AUDIT_REF = "policy_audit_ref"
    GENERIC = "generic"
```

### 数据流概述

`TraceArtifactStore.write_text_artifact()` / `write_bytes_artifact()` / `register_file_artifact()` 先写真实文件，再由 `_artifact()` 计算 relative path、size、SHA-256、content type、redaction/sensitivity 并生成 `TraceArtifact`。`TraceRecorder.write_artifact()` 调用 `TraceStore.append_artifact()` 写 `artifacts.jsonl`。`CommandResult.artifact_path`、`ToolProtocolResultEnvelope.raw_result_ref`、`TraceEvent.artifact_refs` 和 `TraceSummary.key_artifacts` 只消费 artifact 引用。

## 谁生成这些对象

`TraceArtifactStore.write_text_artifact()`、`write_bytes_artifact()` 和 `register_file_artifact()` 先写真实文件，再由 `_artifact()` 计算 relative path、size、SHA-256、content type、redaction/sensitivity 并生成 `TraceArtifact`；`TraceArtifactKind` 是调用方选择的固定分类枚举。

## 谁消费这些对象

`TraceRecorder.write_artifact()` 消费 `TraceArtifact` 并调用 `TraceStore.append_artifact()`；tool protocol、command、model、trace event、final/evaluation report 只保存 artifact id/path 引用。完整对象和文件正文不进入模型，请求中最多由 `ContentBlock.artifact_ref` 渲染出可识别引用或安全摘要。

## 是否落盘

文件默认写 `work/traces/runs/<run_id>/artifacts/<artifact_id>.<ext>`，完整元数据追加到同一 run 的 `artifacts.jsonl`；`index.json` 记录 artifact 索引文件名。其他 store 保存的是 artifact ref，不复制正文。

## 是否进入 trace / audit

`TraceEvent.artifact_refs`、`TraceSpan.artifact_refs` 和 `TraceSummary.key_artifacts` 关联 artifact id；元数据由 `TraceStore.append_artifact()` 写 `artifacts.jsonl`。Artifact 层不写 policy audit，audit 只可能保存执行结果引用。

## 失败路径

单件/总量超限、源文件不存在、敏感二进制无法脱敏、未知 artifact 分别抛 `TraceArtifactError`；`TraceRecorder.write_artifact()` 缺 text/data/path 时抛 `ValueError`，不会返回一个指向不存在文件的 handle。

## 当前结构问题

artifact 文件与 `artifacts.jsonl` 元数据必须保持同一 run 目录和 id 关联；消费者只能传播 ref/安全摘要，不能把 raw long result重新塞回 context。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
