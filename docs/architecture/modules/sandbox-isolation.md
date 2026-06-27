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

backend `capabilities()` 生成 `SandboxCapabilities`；default profile与 policy constraints组合 resource/env/filesystem/network policy和 `SandboxProfile`。`SandboxManager.from_command_request()` 生成 `SandboxRequest`，backend `prepare()` 生成 `PreparedSandbox`，backend/artifact collector/manager生成 artifact、change summary、violation与 `SandboxResult`。

## 谁消费这些对象

manager/backend消费profile/request/prepared对象；CommandExecutor消费 result并投影为 `CommandResult.isolation_report`。完整 sandbox对象不进模型，只有裁剪后的 status、changed files、violation/artifact摘要经 command tool observation可见。

## 是否落盘

prepare阶段创建sandbox root/workspace copy，artifact collector写sandbox artifact文件；cleanup按backend删除临时环境。没有 `SandboxResult` 专属durable store，持久引用是trace artifact ref与CommandResult/context投影。

## 是否进入 trace / audit

SandboxManager发出 `sandbox_started`、`sandbox_cleaned`、`sandbox_capability_failed`、`sandbox_violation`、`sandbox_completed`，payload含backend/status/exit/duration/artifact ids/changed files/violations。是否要求sandbox来自PolicyDecision，request/decision由policy audit保存；sandbox result本体不写audit。

## 失败路径

能力不满足、backend unavailable、setup failed、violation、timeout与cleanup failed均产生明确status/violation或异常；cleanup异常会改写result metadata/status，不能在隔离未清理时报告success。

## 当前结构问题

profile表达需求、capabilities表达backend能力、result表达实际执行；三者必须分别记录，不能用profile声明替代真实隔离证明。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
