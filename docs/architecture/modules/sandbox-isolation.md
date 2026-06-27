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

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

`PolicyDecision` 要求隔离或 command policy 选择 sandbox backend -> `SandboxManager` 创建 `SandboxRequest` -> backend prepare/run/cleanup -> `SandboxResult` -> command result isolation report / trace。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 后运行测试为例：`SandboxManager.build_request_from_policy()` -> `SandboxManager.run()` 先把 `PolicyDecision.constraints` 和 command 参数生成对象 `SandboxRequest`。`SandboxManager._apply_policy_constraints()` 选择 profile 后，backend prepare/run/cleanup 生成 `PreparedSandbox`、`SandboxArtifact`、`SandboxChangeSummary`、`SandboxViolation` 和 `SandboxResult`；`CommandExecutor._result_from_sandbox()` 再把 sandbox stdout/stderr/exit_code 转成 `CommandResult.isolation_report`。sandbox trace 事件写入 `events.jsonl`，artifact 写入 trace artifact store；backend unavailable、policy hard isolation 不满足或 violation 产生 blocked/failed result，不回退为非隔离执行。

## 真实对象完整结构

### SandboxRequest（沙箱请求）

连接 policy decision 和 backend 的执行请求。**边界**：内部治理对象，不落盘；backend 消费后生成 PreparedSandbox。

```python
@dataclass
class SandboxRequest:
    sandbox_id: str
    session_id: str
    task_id: str
    action_id: str
    command: list[str] | str
    cwd: Path
    workspace_root: Path
    profile: SandboxProfile
    policy_decision_id: str | None = None
    policy_constraints: PolicyConstraints | None = None
    reason: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)
```

### SandboxResult（沙箱结果）

沙箱执行的完整结果。**边界**：内部治理对象；投影为 `CommandResult.isolation_report` 写 context，artifact ref 写 trace，不独立落盘。

```python
@dataclass
class SandboxResult:
    sandbox_id: str
    backend_name: str
    status: SandboxStatus
    exit_code: int | None
    stdout: str
    stderr: str
    started_at: str
    ended_at: str
    duration_ms: int
    artifacts: list[SandboxArtifact] = field(default_factory=list)
    filesystem_changes: SandboxChangeSummary = field(default_factory=SandboxChangeSummary)
    violations: list[SandboxViolation] = field(default_factory=list)
    trace_id: str | None = None
    cleanup_status: str = "not_started"
    metadata: dict[str, Any] = field(default_factory=dict)
```

### SandboxProfile（沙箱配置）

声明隔离需求的 profile。**边界**：内部治理对象，来自默认 profile 或 policy constraints 组合；不进入模型。

```python
@dataclass
class SandboxProfile:
    name: SandboxProfileName
    filesystem: SandboxFilesystemPolicy
    network: SandboxNetworkPolicy
    env: SandboxEnvPolicy
    resources: SandboxResourceLimits
    description: str = ""
    image_digest: str | None = None
```

### 关键枚举值域

```python
class SandboxStatus(str, Enum):          # SandboxResult.status
    SUCCESS = "success"
    FAILED = "failed"
    TIMEOUT = "timeout"
    POLICY_BLOCKED = "policy_blocked"
    VIOLATION = "violation"
    BACKEND_UNAVAILABLE = "backend_unavailable"
    SETUP_FAILED = "setup_failed"
    CLEANUP_FAILED = "cleanup_failed"

class SandboxProfileName(str, Enum):     # SandboxProfile.name
    READONLY_ANALYSIS = "readonly_analysis"
    ISOLATED_VERIFICATION = "isolated_verification"
    GENERATED_CODE = "generated_code"
    PACKAGE_OPERATION = "package_operation"
    LONG_RUNNING_SERVICE = "long_running_service"

class SandboxFilesystemMode(str, Enum):  # SandboxFilesystemPolicy.mode
    NONE = "none"
    READ_ONLY_WORKSPACE = "read_only_workspace"
    COPY_ON_WRITE_WORKSPACE = "copy_on_write_workspace"
    EMPTY_TEMP_WORKSPACE = "empty_temp_workspace"
    ARTIFACT_OUTPUT_ONLY = "artifact_output_only"

class SandboxNetworkMode(str, Enum):     # SandboxNetworkPolicy.mode
    DENIED = "denied"
    ALLOWED = "allowed"
    ALLOWLIST = "allowlist"
    UNSUPPORTED = "unsupported"
```

### 数据流概述

`PolicyDecision` 要求隔离时，`SandboxManager.build_request_from_policy()` 从 `PolicyDecision.constraints` 和 command 参数生成 `SandboxRequest`。`SandboxManager._apply_policy_constraints()` 选择 profile 后，backend `prepare()` 生成 `PreparedSandbox`，`run()` 生成 `SandboxResult`（含 `SandboxArtifact`、`SandboxChangeSummary`、`SandboxViolation`）。`CommandExecutor._result_from_sandbox()` 把 sandbox stdout/stderr/exit_code 转成 `CommandResult.isolation_report`。sandbox trace 事件写 `events.jsonl`，artifact 写 trace artifact store。

## 谁生成这些对象

backend `capabilities()` 生成 `SandboxCapabilities`；default profile与 policy constraints组合 resource/env/filesystem/network policy和 `SandboxProfile`。`SandboxManager.from_command_request()` 生成 `SandboxRequest`，backend `prepare()` 生成 `PreparedSandbox`，backend/artifact collector/manager生成 artifact、change summary、violation与 `SandboxResult`。

## 谁消费这些对象

`SandboxManager.run()` 和 backend `prepare()`/`run()`/`cleanup()` 消费 profile/request/prepared 对象；`CommandExecutor._result_from_sandbox()` 消费 result 并投影为 `CommandResult.isolation_report`。完整 sandbox对象不进模型，只有裁剪后的 status、changed files、violation/artifact摘要经 command tool observation可见。

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
