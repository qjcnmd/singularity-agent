# Sandbox Isolation 模块数据流

模块数据流文档 ID: sandbox-isolation

源码证据路径:
- src/singularity/sandbox/models.py
- src/singularity/sandbox/manager.py
- src/singularity/sandbox/backends.py
- src/singularity/sandbox/filesystem.py
- src/singularity/command/executor.py

关键符号:
- SandboxRequest
- PreparedSandbox
- SandboxResult
- SandboxViolation
- SandboxManager
- WindowsSandboxBackend
- WindowsSandboxDoctorReport
- probe_windows_sandbox
- default_sandbox_profile
- default_sandbox_backends

字段清单:
- `SandboxCapabilities`: filesystem_isolation, copy_on_write, readonly_mount, network_isolation, env_isolation, process_tree_kill, timeout, output_limit, memory_limit, process_limit, artifact_capture, change_detection
- `SandboxResourceLimits`: timeout_seconds, max_output_chars, max_artifact_bytes, max_processes, max_memory_mb, memory_limit, pids_limit
- `SandboxEnvPolicy`: inherit_env, allowlist, denylist_patterns, redacted_patterns, extra_env, case_insensitive
- `SandboxFilesystemPolicy`: mode, workspace_root, sandbox_root, include_globs, exclude_globs, writable_paths, readonly_paths, artifact_paths, detect_changes
- `SandboxNetworkPolicy`: mode, allowed_hosts, denied_hosts, require_hard_isolation
- `SandboxProfile`: name, filesystem, network, env, resources, description
- `SandboxRequest`: sandbox_id, session_id, task_id, action_id, command, cwd, workspace_root, profile, policy_decision_id, policy_constraints, reason, metadata
- `PreparedSandbox`: sandbox_id, backend_name, sandbox_root, workspace_copy_root, execution_cwd, env, request, created_at, trace_id, baseline
- `SandboxArtifact`: artifact_id, sandbox_id, path, relative_path, size_bytes, kind, sha256, metadata, redacted
- `SandboxChangeSummary`: created_files, modified_files, deleted_files, total_changed_files, diff_preview, importable
- `SandboxViolation`: violation_type, message, severity, evidence, detected_at
- `SandboxResult`: sandbox_id, backend_name, status, exit_code, stdout, stderr, started_at, ended_at, duration_ms, artifacts, filesystem_changes, violations, trace_id, cleanup_status, metadata
- `WindowsSandboxPrimitives`: restricted_token, job_object, low_integrity, acl, firewall, private_desktop
- `WindowsSandboxSetup`: sandbox_account, acl_boundary, network_filter, private_desktop, execution_backend
- `WindowsSandboxDoctorReport`: implementation, platform_supported, primitives, setup, available, missing_requirements

## 这一层解决什么问题

Sandbox 层消费已经解析完成的 `SandboxRequest`，选择能够满足 filesystem、network 和 resource 要求的 OS-native backend，并统一返回执行结果或明确的 `backend_unavailable`。这一层不决定会话权限、不发放审批，也不把普通本地执行或 workspace copy 表述为强隔离。

## 当前源码位置

- `src/singularity/sandbox/models.py`：请求、profile、capability、prepared/result 等对象。
- `src/singularity/sandbox/manager.py`：backend 选择、capability 校验、生命周期和 trace。
- `src/singularity/sandbox/backends.py`：Windows capability/setup 边界及默认 backend 注册。
- `src/singularity/sandbox/filesystem.py`：workspace projection、变化检测和清理辅助；当前没有可用 backend 调用它执行命令。
- `src/singularity/command/executor.py`：依据会话级 `PermissionProfile` 构造完整 `SandboxRequest`。

## 关键类、函数、字段

`SandboxManager.run()`只消费由CommandExecutor构造完成的请求，不重新解释`PolicyDecision`。`ensure_capabilities()`校验请求声明的 denied network、read-only workspace、memory limit 和 process limit。`WindowsSandboxBackend.doctor()`返回真实探测报告；当前 `setup()`、`prepare()`和`run()`均 fail-closed，不执行普通本地进程。

本文顶部字段清单是当前源码对象的完整字段。`SandboxProfile`已无容器 image 字段；backend 也没有容器、镜像、container user 或 daemon availability 配置。

## 真实运行时调用链

```text
PolicyEngine / ApprovalGate
-> CommandExecutor.run()
-> CommandExecutor._sandbox_request()
-> SandboxManager.run(SandboxRequest)
-> SandboxManager._select_backend()
-> backend.is_available() + SandboxManager.ensure_capabilities()
-> backend.prepare() -> backend.run() -> backend.cleanup()
-> SandboxResult
-> CommandExecutor._result_from_sandbox()
-> CommandResult.isolation_report / trace / planner evidence
```

当前 Windows 实际路径在 `backend.is_available()`处结束：doctor 的 setup 状态未完成，manager 返回 `SandboxStatus.BACKEND_UNAVAILABLE`，不调用 `prepare()`，也不启动进程。

## 真实任务中的对象流

以 workspace-write 会话运行本地验证为例，`CommandExecutor._sandbox_request()`从共享 `PermissionProfile`和已经产生的 policy decision 生成 `SandboxProfile`及`SandboxRequest`。其中 writable roots、additional writable directories、protected path patterns、network mode、timeout、output limit 和脱敏环境已经解析完成。

`SandboxManager.run()`不修改这些边界，只寻找可用 backend 并核验 capability。当前 `WindowsSandboxBackend`探测 restricted token、Job Object、low integrity、ACL、Windows Firewall 和 private desktop primitives，但 `sandbox_account`、`acl_boundary`、`network_filter`、`private_desktop`及`execution_backend` setup 均为 false，因此结果为 `backend_unavailable`。该结果被转换为 command backend error；不存在未隔离本地执行 fallback。

`CommandExecutor._sandbox_request()`生成请求对象 -> `SandboxManager.run()`消费请求并生成结果 -> `CommandExecutor._result_from_sandbox()`消费结果 -> `SandboxJsonlTraceRecorder.append()`把安全字段落盘到`.singularity/sandbox/trace.jsonl`。

## 真实对象完整结构

### SandboxRequest（沙箱请求）

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

### SandboxProfile（沙箱执行需求）

```python
@dataclass
class SandboxProfile:
    name: SandboxProfileName
    filesystem: SandboxFilesystemPolicy
    network: SandboxNetworkPolicy
    env: SandboxEnvPolicy
    resources: SandboxResourceLimits
    description: str = ""
```

### WindowsSandboxDoctorReport（Windows capability/setup 报告）

```python
@dataclass(frozen=True)
class WindowsSandboxPrimitives:
    restricted_token: bool
    job_object: bool
    low_integrity: bool
    acl: bool
    firewall: bool
    private_desktop: bool

@dataclass(frozen=True)
class WindowsSandboxSetup:
    sandbox_account: bool
    acl_boundary: bool
    network_filter: bool
    private_desktop: bool
    execution_backend: bool

@dataclass(frozen=True)
class WindowsSandboxDoctorReport:
    implementation: str
    platform_supported: bool
    primitives: WindowsSandboxPrimitives
    setup: WindowsSandboxSetup
    available: bool
    missing_requirements: tuple[str, ...]
```

### PreparedSandbox（已准备环境）

```python
@dataclass
class PreparedSandbox:
    sandbox_id: str
    backend_name: str
    sandbox_root: Path
    workspace_copy_root: Path
    execution_cwd: Path
    env: dict[str, str]
    request: SandboxRequest
    created_at: str
    trace_id: str
    baseline: dict[str, Any] = field(default_factory=dict)
```

`PreparedSandbox`和`SandboxFilesystemManager`保留 projection/capture 数据结构，但当前默认 backend 不会生成它。字段存在不等于当前具备 filesystem isolation。

### SandboxResult（沙箱结果）

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

### 关键枚举值域

```python
class SandboxStatus(str, Enum):
    SUCCESS = "success"
    FAILED = "failed"
    TIMEOUT = "timeout"
    POLICY_BLOCKED = "policy_blocked"
    VIOLATION = "violation"
    BACKEND_UNAVAILABLE = "backend_unavailable"
    SETUP_FAILED = "setup_failed"
    CLEANUP_FAILED = "cleanup_failed"

class SandboxProfileName(str, Enum):
    READONLY_ANALYSIS = "readonly_analysis"
    ISOLATED_VERIFICATION = "isolated_verification"
    GENERATED_CODE = "generated_code"
    PACKAGE_OPERATION = "package_operation"
    LONG_RUNNING_SERVICE = "long_running_service"

class SandboxFilesystemMode(str, Enum):
    NONE = "none"
    READ_ONLY_WORKSPACE = "read_only_workspace"
    COPY_ON_WRITE_WORKSPACE = "copy_on_write_workspace"
    EMPTY_TEMP_WORKSPACE = "empty_temp_workspace"
    ARTIFACT_OUTPUT_ONLY = "artifact_output_only"

class SandboxNetworkMode(str, Enum):
    DENIED = "denied"
    ALLOWED = "allowed"
    ALLOWLIST = "allowlist"
    UNSUPPORTED = "unsupported"
```

`COPY_ON_WRITE_WORKSPACE`是请求中的 projection 模式名称，不是 backend capability 证明。只有可用 OS backend 的实际 enforcement 才能形成成功隔离结果。

## 谁生成这些对象

- `CommandExecutor._sandbox_request()`生成`SandboxProfile`和`SandboxRequest`。
- `default_sandbox_profile()`生成基础 profile，CommandExecutor 再写入会话权限边界。
- `probe_windows_sandbox()`生成`WindowsSandboxDoctorReport`。
- 可用 backend 才能生成`PreparedSandbox`和执行型`SandboxResult`；当前默认 Windows backend只由manager生成 unavailable result。

## 谁消费这些对象

- `SandboxManager`和 backend 消费 `SandboxRequest`、`SandboxProfile`及`PreparedSandbox`。
- `CommandExecutor._result_from_sandbox()`消费`SandboxResult`并产生`CommandResult`。
- trace recorder消费请求、result和安全的 capability 摘要。
- 完整 request、policy constraints、doctor内部setup对象不进入模型；模型只接收经过 command/tool observation 裁剪的结果。

## 是否落盘

默认 `SandboxJsonlTraceRecorder`写入`<workspace>/.singularity/sandbox/trace.jsonl`。当前 Windows backend unavailable 时不会创建 sandbox root、workspace projection或artifact。`SandboxFilesystemManager`只有在未来可用 OS backend 显式调用时才创建临时 projection；projection本身不得被报告为隔离能力。

## 是否进入 trace / audit

`SandboxManager`可发出`sandbox.requested`、`sandbox.prepared`、`sandbox.started`、`sandbox.cleaned`、`sandbox.capability_failed`、`sandbox.violation`和`sandbox.completed`。当前 backend unavailable 主路径记录 requested/completed及 JSONL unavailable result。sandbox result不直接写policy audit；关联的 policy decision 由Policy层记录。

## 失败路径

- 平台不是 Windows：默认 backend 列表为空，返回`backend_unavailable`。
- Windows primitive或setup缺失：backend不可用，返回`backend_unavailable`且不启动进程。
- 已注册可用backend无法满足denied network、read-only mount、memory/process limit：`SandboxCapabilityError`转为`backend_unavailable`。
- prepare/run异常：返回`setup_failed`或backend自身失败状态。
- cleanup异常：标记`cleanup_failed`，不得保留success。
- 所有不可用路径均禁止回退到普通本地执行。

## 当前结构问题

- Windows elevated setup和native execution尚未实现，所以当前没有可成功执行的强隔离backend。
- primitive探测只能证明API存在，不能证明sandbox account、ACL、network filter、private desktop或launcher已配置。
- `SandboxFilesystemManager`仍提供projection辅助，但必须等OS身份、ACL和network enforcement完成后才能由backend使用。
- `SandboxRequest`仍携带policy关联字段；SandboxManager不消费或修改这些字段，后续应继续保持内部治理对象与模型摘要分离。

## 维护规则

- 新backend必须以真实OS enforcement和external smoke证明capability；workspace copy、chmod或普通子进程不能注册为sandbox backend。
- capability或setup缺失必须返回`backend_unavailable`，不得静默本地执行。
- Windows setup、doctor、account/ACL/firewall、restricted token、Job Object或private desktop变化时同步本文件。
- 修改本模块对象字段、调用链、CLI、trace或report schema后运行`python scripts/verify_runtime_docs.py`；展示对象时必须列完整字段。
