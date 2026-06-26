# Sandbox Isolation模块数据流

模块数据流文档 ID: sandbox-isolation

源码证据路径:
- src/singularity/sandbox/models.py
- src/singularity/sandbox/manager.py
- src/singularity/sandbox/backends.py
- src/singularity/sandbox/filesystem.py

关键符号:
- SandboxRequest
- PreparedSandbox
- SandboxResult
- SandboxViolation
- SandboxManager
- default_sandbox_profile

字段清单:
- SandboxCapabilities: filesystem_isolation, copy_on_write, readonly_mount, network_isolation, env_isolation, process_tree_kill, timeout, output_limit, memory_limit, process_limit, artifact_capture, change_detection
- SandboxResourceLimits: timeout_seconds, max_output_chars, max_artifact_bytes, max_processes, max_memory_mb, memory_limit, pids_limit
- SandboxEnvPolicy: inherit_env, allowlist, denylist_patterns, redacted_patterns, extra_env, case_insensitive
- SandboxFilesystemPolicy: mode, workspace_root, sandbox_root, include_globs, exclude_globs, writable_paths, readonly_paths, artifact_paths, detect_changes
- SandboxNetworkPolicy: mode, allowed_hosts, denied_hosts, require_hard_isolation
- SandboxProfile: name, filesystem, network, env, resources, description, image_digest
- SandboxRequest: sandbox_id, session_id, task_id, action_id, command, cwd, workspace_root, profile, policy_decision_id, policy_constraints, reason, metadata
- PreparedSandbox: sandbox_id, backend_name, sandbox_root, workspace_copy_root, execution_cwd, env, request, created_at, trace_id, baseline
- SandboxArtifact: artifact_id, sandbox_id, path, relative_path, size_bytes, kind, sha256, metadata, redacted
- SandboxChangeSummary: created_files, modified_files, deleted_files, total_changed_files, diff_preview, importable
- SandboxViolation: violation_type, message, severity, evidence, detected_at
- SandboxResult: sandbox_id, backend_name, status, exit_code, stdout, stderr, started_at, ended_at, duration_ms, artifacts, filesystem_changes, violations, trace_id, cleanup_status, metadata

## 这一层解决什么问题

Sandbox 层根据 profile、filesystem/network/env/resource policy 准备隔离执行环境，捕获 artifact、文件变化、违规和清理状态。

## 当前源码位置

- src/singularity/sandbox/models.py
- src/singularity/sandbox/manager.py
- src/singularity/sandbox/backends.py
- src/singularity/sandbox/filesystem.py

## 关键类、函数、字段

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

`PolicyDecision` 要求隔离或 command policy 选择 sandbox backend -> `SandboxManager` 创建 `SandboxRequest` -> backend prepare/run/cleanup -> `SandboxResult` -> command result isolation report / trace。

## 真实对象完整结构

- `SandboxRequest（沙箱请求）` 完整字段列在字段清单中，连接 policy decision 和 backend。
- `SandboxResult（沙箱结果）` 完整字段列在字段清单中，消费者是 command result、trace 和 final report。

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
