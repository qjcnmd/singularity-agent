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

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

组件发现长输出或文件产物 -> `TraceArtifactStore.write_text_artifact()` / `write_bytes_artifact()` / `register_file_artifact()` -> `TraceArtifact` -> trace/event/result/report 只引用 artifact id 或 relative handle。

## 真实对象完整结构

- `TraceArtifact（追踪产物）` 完整字段列在字段清单中，真实文件在 trace run artifact 目录下。
- `TraceArtifactStore（追踪产物存储）` 负责大小限制、redaction、hash、content type 和 handle 生成。

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
