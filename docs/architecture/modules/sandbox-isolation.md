# Sandbox Isolation 模块数据流

模块数据流文档 ID: sandbox-isolation

源码证据路径:
- src/singularity/sandbox/models.py
- src/singularity/sandbox/manager.py
- src/singularity/sandbox/backends.py
- src/singularity/sandbox/filesystem.py
- src/singularity/sandbox/windows.py
- src/singularity/sandbox/windows_runner.py
- src/singularity/command/executor.py

关键符号:
- SandboxRequest
- PreparedSandbox
- SandboxResult
- SandboxViolation
- SandboxManager
- SandboxFilesystemManager
- WindowsSandboxBackend
- WindowsSandboxDoctorReport
- WindowsSandboxSetupReport
- WindowsRunnerSpec
- WindowsRunnerResult
- WindowsSandboxRunner
- probe_windows_sandbox
- setup_windows_sandbox
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
- `WindowsCapabilityState`: status, checked, reason, evidence
- `WindowsSandboxPrimitives`: restricted_token, job_object, low_integrity, acl, firewall, private_desktop
- `WindowsSandboxSetup`: sandbox_account, login_ui_visibility, logon_rights, group_membership, state_dir_acl, acl_boundary, network_filter, private_desktop, execution_backend
- `WindowsSandboxExecution`: account_sid, credential, launcher, runner_smoke, network_probe
- `WindowsSandboxDoctorReport`: implementation, platform_supported, platform_status, primitives, setup, execution, available, enforcement_status, blocking_requirements, recommended_action, diagnostics
- `WindowsSandboxSetupReport`: status, requested_operation, requires_elevation, changed, completed_steps, pending_steps, failed_steps, available_after_setup, message, diagnostics
- `WindowsSandboxCleanupReport`: status, requested_operation, requires_elevation, changed, completed_steps, failed_steps, diagnostics
- `WindowsRunnerSpec`: command, cwd, env, timeout_seconds, max_output_chars, network_mode, result_path
- `WindowsRunnerResult`: exit_code, stdout, stderr, timed_out, started_at, ended_at, duration_ms, output_truncated, job_killed, network_denied_verified, metadata

## 这一层解决什么问题

Sandbox 层消费已经解析完成的 `SandboxRequest`，选择能够满足 filesystem、network 和 resource 要求的 OS-native backend，并统一返回执行结果或明确的 `backend_unavailable`。这一层不决定会话权限、不发放审批，也不把普通本地执行或 workspace copy 表述为强隔离。

Windows 当前实现是 account-backed OS sandbox：父进程准备 COW workspace projection 和 run root ACL，子进程以 `SingularitySandbox` 本地账户启动 `windows_runner.py`，runner 再用 restricted low-integrity token、private desktop 和 kill-on-close Job Object 启动实际验证命令。该方向只对齐 OpenAI Codex 公开资料中的专用低权限账户、ACL、firewall、本地策略和受控 setup 原则，不声称与 Codex App 完全相同。缺少 sandbox account、登录 UI 隐藏、logon rights hardening、Credential Manager 凭据、state dir ACL、ACL boundary、account-scoped firewall、private desktop、runner smoke 或 network probe 任意一项时，backend 不可用。

## 当前源码位置

- `src/singularity/sandbox/models.py`：请求、profile、capability、prepared/result 等对象。
- `src/singularity/sandbox/manager.py`：backend 选择、capability 校验、protected path preflight、生命周期和 trace。
- `src/singularity/sandbox/backends.py`：默认 backend 注册；Windows 上注册 `WindowsSandboxBackend`，非 Windows 返回空列表。
- `src/singularity/sandbox/filesystem.py`：workspace projection、protected glob 排除、变化检测和清理辅助。
- `src/singularity/sandbox/windows.py`：Windows doctor/setup/cleanup、account/firewall/ACL/login UI/logon rights probe、backend prepare/run/cleanup。
- `src/singularity/sandbox/windows_runner.py`：sandbox account runner、restricted token child、private desktop、Job Object、timeout/output/network probe/result JSON。
- `src/singularity/command/executor.py`：依据会话级 `PermissionProfile` 构造完整 `SandboxRequest`，并把 `SandboxResult` 投影成 command evidence。

## 关键类、函数、字段

`SandboxManager.run()` 只消费由 `CommandExecutor` 构造完成的请求，不重新解释 `PolicyDecision`。`ensure_capabilities()` 校验 denied network、read-only workspace、memory limit 和 process limit。`WindowsSandboxBackend.doctor()` 返回稳定 JSON schema 的真实探测报告，`setup()` 在 Windows elevated shell 下创建/验证本地账户、Credential Manager 凭据、登录 UI 隐藏项、`SeInteractiveLogonRight`（经 LSA `LsaAddAccountRights` 授予，并移除会阻断 `CreateProcessWithLogonW` 的 `SeDenyInteractiveLogonRight`）、logon hardening（移除 `SeBatchLogonRight` / `SeNetworkLogonRight` / `SeRemoteInteractiveLogonRight` / `SeServiceLogonRight`，并添加 `SeDenyBatchLogonRight` / `SeDenyNetworkLogonRight` / `SeDenyRemoteInteractiveLogonRight` / `SeDenyServiceLogonRight`）、`Users` 本地组成员（为 `python.exe` 与系统目录提供 RX 和 traverse）、machine state dir ACL、account-scoped firewall、ACL boundary、runner smoke 和 network probe。`SeInteractiveLogonRight` 被保留给 `CreateProcessWithLogonW`，普通用户登录体验通过隐藏标准登录 UI 用户列表和 deny RDP/network/service/batch 登录面收紧；不把登录 UI 隐藏当成唯一安全边界。`WindowsSandboxSetupReport.completed_steps` / `pending_steps` / `failed_steps` 显式包含 `login_ui_visibility`、`logon_right`、`logon_hardening`、`account_group`、`state_dir_acl` 与 `network_probe`。非 elevated setup 在任何 system mutation 前返回 `requires_elevation`，不得假成功。

Windows setup 的 sandbox account 存在性探测由 `_account_exists()` 调用 `_run_net(["user", SANDBOX_ACCOUNT])`，`_run_net()` 只通过 `shutil.which("net")` 定位 Windows `net` 命令并复用 `_run_command()` 返回 `CompletedProcess`。账户创建、密码更新和删除由 `_create_sandbox_account()` / `_set_account_password()` / `_delete_sandbox_account()` 的 `netapi32` helper 执行；firewall 仍由 `_run_powershell()` 执行 `Remove-NetFirewallRule` / `New-NetFirewallRule`，Credential Manager、登录 UI visibility registry entry、LSA rights、ACL、runner smoke、network probe 由各自 helper 真实验证。

`execution.launcher` 不再只检查 `CreateProcessWithLogonW` / `CreateProcessAsUserW` 符号是否存在，而是真实报告该 launcher 的文档化前置条件：`account_logon_rights`（经 LSA `LsaEnumerateAccountRights` 枚举账户**直接** right，区分 interactive、batch、network、remote interactive、service 以及对应 deny right；group 继承的 right 不枚举，empirical proof 由 runner_smoke 兜底）、`window_station` / `desktop`（`lpDesktop=NULL` 继承父进程，账户依赖继承的 winsta/desktop DACL，无 just-in-time ACE 授予）、`executable`（`sys.executable` 的 path hash 与 `icacls` 摘要）、`working_directory`（代表性 path hash 与 `account_has_access`）、`domain_username_form`（`.\\<redacted>`）和 `logon_flags`（`LOGON_WITH_PROFILE (0x1)`）。`launcher.status=available` 当且仅当符号存在、账户持 `SeInteractiveLogonRight`（或非提权枚举返回 `STATUS_ACCESS_DENIED 0xC0000022` 无法判定时，defer 到 runner_smoke）且无 `SeDenyInteractiveLogonRight`。`setup.logon_rights.status=available` 还要求 RDP/network/service/batch 登录面被 deny，并且不保留对应 allow right。

`WindowsSandboxRunner.run` 把 `windows_runner.py`（自包含、仅 stdlib）物化到 ACL 授权的 `sandbox_root`（`windows_runner_<pid>_<ns>.py`），使 sandbox account 能读取 runner 脚本而不在宿主 repo 留持久 ACL；账户进程（Level-1）由 `CreateProcessWithLogonW` 以 `.\SingularitySandbox` + `LOGON_WITH_PROFILE` 启动。Level-1 账户进程在 `main()` 调用 `SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX)`，使 Level-2 sandboxed child（restricted low-integrity token）启动失败时不弹 Windows hard-error 对话框；`run()` 还在 profile 未设 timeout 时强制有限默认 wait（40s），避免无法在 restricted token 下初始化的工具（如 `git.exe`）无限挂起。Level-1 账户进程在 `run_spec` 内通过 `OpenProcessToken` + `GetTokenInformation(TokenUser)` + `ConvertSidToStringSidW` 读取自身 token user SID，只把 `account_sid_hash`（`sha256[:16]`）与 `account_name` 写入 `WindowsRunnerResult.metadata`；doctor 的 `runner_smoke` 据此校验 `account_sid_hash == _hash_sid(sandbox_account_sid)`，不匹配即 `missing`（fail-closed，证明非 admin 当前用户回退）。

`WindowsSandboxBackend.prepare()` 先重新检查 doctor 可用性，再拒绝当前未实现的 workspace 外 `writable_paths` 和 path-specific `readonly_paths`，之后调用 `SandboxFilesystemManager.prepare_filesystem()` 创建 COW projection。protected paths 通过 `CommandExecutor` 合入 `exclude_globs`，所以 `.env`、`.git`、`.singularity` 等不会进入 projection；`SandboxManager._protected_path_violation()` 还会在命令参数、cwd 和显式 resource 上做前置 hard deny。`prepare()` 对 run root 授予 sandbox account 修改权限，但只对 `workspace/` projection 设置低完整性标签，避免低完整性命令子进程写入 `runner-spec.json`、`runner-result.json` 等控制面文件。

Windows doctor/setup 的 probe state 不再使用当前登录用户的 user-data 目录，而是使用 machine-level state 目录 `%PROGRAMDATA%\Singularity\windows-sandbox`（非 Windows 测试路径仍使用 `resolve_user_data_paths().state_dir/windows-sandbox/`）。该目录用于管理员 setup shell 和 `SingularitySandbox` 本地账户之间共享 probe 控制面文件，diagnostics 只暴露 `state_dir_hash`、`probe_root_hash` 或 `path_hash`，不暴露完整路径。

ACL boundary probe 分三层验证：probe root 作为 runner spec/result 控制面目录，`allowed/` 目录必须能由 sandbox account 写入，`denied/` 目录必须拒绝 sandbox account 写入。通过条件是 allowed 写入 smoke 退出 0，denied 写入 smoke 因 `OSError` 退出 0；只创建目录不算通过。

`WindowsSandboxBackend.run()` 在启动 runner 前再做一次 uncached enforcement probe；如果防火墙、凭据、runner smoke 等状态已失效，返回 `BACKEND_UNAVAILABLE` 且不启动进程。执行成功与否以 `WindowsRunnerResult` 的真实元数据为准，不能硬编码 restricted token、low integrity、private desktop 或 Job Object evidence。`network_access=denied` 需要同时满足宿主机 outbound baseline 可连通、本次 runner socket probe 被拒绝、doctor 的 `setup.network_filter` 和 `execution.network_probe`，否则返回 `SandboxStatus.VIOLATION`。

## 真实运行时调用链

```text
PolicyEngine / ApprovalGate
-> CommandExecutor.run()
-> CommandExecutor._sandbox_request()
-> SandboxManager.run(SandboxRequest)
-> SandboxManager._protected_path_violation()
-> SandboxManager._select_backend()
-> backend.is_available() + SandboxManager.ensure_capabilities()
-> WindowsSandboxBackend.prepare()
   -> SandboxFilesystemManager.prepare_filesystem()
   -> account ACL run root + low-integrity workspace projection
   -> runner-spec.json / runner-result.json
-> WindowsSandboxBackend.run()
   -> uncached doctor enforcement check
   -> WindowsSandboxRunner.run()
   -> materialize windows_runner.py into ACL'd sandbox_root (account-readable copy)
   -> CreateProcessWithLogonW(account runner)  # needs SeInteractiveLogonRight + executable RX
   -> SetErrorMode (suppress child hard-error dialogs) + CreateRestrictedToken + low integrity + CreateDesktopW + Job Object + CreateProcessAsUserW(actual command)
   -> WindowsRunnerResult(metadata.account_sid_hash = self-identity proof)
-> WindowsSandboxBackend.cleanup()
-> SandboxResult
-> CommandExecutor._result_from_sandbox()
-> CommandResult.isolation_report / trace / planner evidence / final report
```

`sandbox setup --json`（elevated）会预先授予 `SeInteractiveLogonRight`、移除 `SeDenyInteractiveLogonRight`、隐藏标准 Windows 登录 UI 用户列表中的 sandbox account、加固 RDP/network/service/batch 登录面、加入 `Users` 组并设置 state dir ACL；`run()` 时 runner 脚本物化到 sandbox_root，账户依赖继承的 winsta/desktop DACL（无 ACE 授予）。当前本机如果尚未从 elevated shell 运行 `sandbox setup --json`，真实路径仍会在 `backend.is_available()` 或 `run()` 前的 enforcement probe 处返回 `backend_unavailable`；这不是 fallback，也不会启动普通本地进程。`sandbox cleanup --json`（elevated）只删除固定命名的 current/legacy Singularity sandbox account、credential target、firewall rule、login UI visibility entry 和固定 machine state dir；不得通配删除非 Singularity 资产。

## 真实任务中的对象流

以 `workspace-write` 会话运行 `python -m pytest`、`python -m compileall`、`ruff` 或 `mypy` 为例，`CommandExecutor._sandbox_request()` 从共享 `PermissionProfile` 和 policy decision 生成 `SandboxProfile` 及 `SandboxRequest`。其中 writable roots、additional writable directories、protected path patterns、network mode、timeout、output limit 和脱敏环境已经解析完成。

具体对象链路是：`CommandExecutor._sandbox_request()` -> `SandboxManager.run()` -> `WindowsSandboxBackend.prepare()` -> `WindowsSandboxRunner.run()` -> `WindowsSandboxBackend.run()` -> `CommandExecutor._result_from_sandbox()`。`CommandExecutor._sandbox_request()` 生成请求对象，`SandboxManager.run()` 消费请求并生成 lifecycle trace，Windows backend 生成 runner spec/result 文件，`CommandExecutor._result_from_sandbox()` 消费 `SandboxResult` 并生成 command evidence。

`SandboxManager.run()` 不修改这些边界，只寻找可用 backend 并核验 capability。Windows 可用时，`WindowsSandboxBackend.prepare()` 复制 workspace 到 `work/sandboxes/<sandbox_id>/workspace`，按 exclude globs 排除 protected paths，写入 runner spec/result 路径，并只给 sandbox account 访问本次 run root。runner 作为 sandbox account 读取 spec，再创建 restricted low-integrity child 执行实际命令；stdout/stderr、timeout、network denied proof、Job Object 状态和 artifacts 通过 result JSON 返回父进程。runner 写 result JSON 前会先做本地脱敏，并在读取 account runner 与 child stdout/stderr 后删除临时输出文件，降低 cleanup 失败时的磁盘残留风险。

workspace 外 additional writable directories 当前不会被投影，也不会被 ACL 授权；只要 `writable_paths` 中出现 workspace 外目录，Windows backend 立即 fail closed。path-specific `readonly_paths` 也因尚无目录级 ACL lease 支持而 fail closed。workspace 内 protected paths 通过 projection exclude 和 manager preflight 生效。

`CommandExecutor._result_from_sandbox()` 消费 `SandboxResult`，把 backend、enforcement_status、execution_backend、network_denied_verified、process_tree_kill、job_killed、timeout_enforced、artifact refs、violations 和 changed files 写入 `CommandResult.isolation_report["sandbox"]` 与 metadata。

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

`to_dict()` 输出 `schema_version: "sandbox.windows.doctor/v1"`，每个 capability item 都是 `{status, checked, reason, evidence}`，证据经过 `TraceRedactor` 脱敏。

```python
@dataclass(frozen=True)
class WindowsCapabilityState:
    status: str
    checked: bool
    reason: str
    evidence: dict[str, Any] = field(default_factory=dict)

@dataclass(frozen=True)
class WindowsSandboxPrimitives:
    restricted_token: WindowsCapabilityState
    job_object: WindowsCapabilityState
    low_integrity: WindowsCapabilityState
    acl: WindowsCapabilityState
    firewall: WindowsCapabilityState
    private_desktop: WindowsCapabilityState

@dataclass(frozen=True)
class WindowsSandboxSetup:
    sandbox_account: WindowsCapabilityState
    login_ui_visibility: WindowsCapabilityState
    logon_rights: WindowsCapabilityState
    group_membership: WindowsCapabilityState
    state_dir_acl: WindowsCapabilityState
    acl_boundary: WindowsCapabilityState
    network_filter: WindowsCapabilityState
    private_desktop: WindowsCapabilityState
    execution_backend: WindowsCapabilityState

@dataclass(frozen=True)
class WindowsSandboxExecution:
    account_sid: WindowsCapabilityState
    credential: WindowsCapabilityState
    launcher: WindowsCapabilityState
    runner_smoke: WindowsCapabilityState
    network_probe: WindowsCapabilityState

@dataclass(frozen=True)
class WindowsSandboxDoctorReport:
    implementation: str
    platform_supported: bool
    platform_status: str
    primitives: WindowsSandboxPrimitives
    setup: WindowsSandboxSetup
    execution: WindowsSandboxExecution
    available: bool
    enforcement_status: str
    blocking_requirements: tuple[str, ...]
    recommended_action: str
    diagnostics: tuple[dict[str, Any], ...] = ()
```

### WindowsSandboxSetupReport（setup 报告）

`to_dict()` 输出 `schema_version: "sandbox.windows.setup/v1"`。`status` 只能是 `not_supported`、`requires_elevation`、`partial`、`ready` 或 `failed`；只有 doctor 最终 `available=True` 时才报告 `ready`。

```python
@dataclass(frozen=True)
class WindowsSandboxSetupReport:
    status: str
    requested_operation: str
    requires_elevation: bool
    changed: bool
    completed_steps: tuple[str, ...]
    pending_steps: tuple[str, ...]
    failed_steps: tuple[dict[str, Any], ...]
    available_after_setup: bool
    message: str
    diagnostics: tuple[dict[str, Any], ...] = ()
```

### WindowsSandboxCleanupReport（cleanup 报告）

`to_dict()` 输出 `schema_version: "sandbox.windows.cleanup/v1"`。`status` 只能表达当前动作结果，不代表 backend 可用性；cleanup 完成后通常会让 doctor 变成 `available=false`，这是预期回滚状态。

```python
@dataclass(frozen=True)
class WindowsSandboxCleanupReport:
    status: str
    requested_operation: str
    requires_elevation: bool
    changed: bool
    completed_steps: tuple[str, ...]
    failed_steps: tuple[dict[str, Any], ...]
    diagnostics: tuple[dict[str, Any], ...] = ()
```

### WindowsRunnerSpec / WindowsRunnerResult（执行 backend I/O）

```python
@dataclass(frozen=True)
class WindowsRunnerSpec:
    command: list[str] | str
    cwd: str
    env: dict[str, str]
    timeout_seconds: float | None = None
    max_output_chars: int | None = None
    network_mode: str = "denied"
    result_path: str = ""

@dataclass(frozen=True)
class WindowsRunnerResult:
    exit_code: int | None
    stdout: str
    stderr: str
    timed_out: bool
    started_at: str
    ended_at: str
    duration_ms: int
    output_truncated: bool = False
    job_killed: bool = False
    network_denied_verified: bool = False
    metadata: dict[str, Any] = field(default_factory=dict)
```

`WindowsRunnerResult.metadata` 在成功路径写入 `restricted_token`、`low_integrity`、`private_desktop`、`job_object`、`pid`、`account_name`、`account_sid_hash`（Level-1 账户进程自身 token user SID 的 `sha256[:16]`，供 doctor 校验非 admin 回退）；`run_spec` 异常路径写入 `error_type`，`WindowsSandboxRunner.run` 在 result 文件缺失时写入 `error_code="runner_result_missing"`。

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

`PreparedSandbox.baseline` 在 Windows backend 中包含 workspace file baseline、`runner_spec`、`runner_result` 和 sandbox account 名称。字段存在不等于 backend 可用；只有 doctor/setup 全部通过后才会生成该对象。

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

`COPY_ON_WRITE_WORKSPACE` 是请求中的 projection 模式名称，不是 backend capability 证明。只有可用 OS backend 的实际 enforcement 才能形成成功隔离结果。

## 谁生成这些对象

- `CommandExecutor._sandbox_request()` 生成 `SandboxProfile` 和 `SandboxRequest`。
- `default_sandbox_profile()` 生成基础 profile，`CommandExecutor` 再写入会话权限边界、protected globs、network mode 和 resource limits。
- `probe_windows_sandbox()` 生成 `WindowsSandboxDoctorReport`。
- `setup_windows_sandbox()` 生成 `WindowsSandboxSetupReport`，并在 elevated Windows shell 下创建或验证账户、凭据、登录 UI visibility、LSA logon rights、Users group、state dir ACL、account-scoped firewall、ACL boundary、private desktop、runner smoke 和 network probe assets。
- `cleanup_windows_sandbox_assets()` 生成 `WindowsSandboxCleanupReport`，并在 elevated Windows shell 下删除固定命名的 current/legacy Singularity sandbox account、credential、firewall rule、login UI visibility entry 和 machine state dir。
- `WindowsSandboxBackend.prepare()` 生成 `PreparedSandbox` 与 runner spec；`WindowsSandboxBackend.run()` 生成执行型 `SandboxResult`。
- `WindowsSandboxRunner.run()` / `run_spec()` 生成 `WindowsRunnerResult`。

## 谁消费这些对象

- `SandboxManager` 和 backend 消费 `SandboxRequest`、`SandboxProfile` 及 `PreparedSandbox`。
- `singularity-agent sandbox doctor/setup/cleanup --json` 消费 `WindowsSandboxBackend` 的 doctor/setup/cleanup report 并原样输出 machine-readable JSON。
- `WindowsSandboxRunner` 消费 `runner-spec.json`，写 `runner-result.json`。
- `CommandExecutor._result_from_sandbox()` 消费 `SandboxResult` 并产生 `CommandResult`。
- `Planner.update_from_command()`、`VerificationRunner` 和 `Finalizer` 消费 command metadata / isolation report 中的 sandbox evidence。
- 完整 request、policy constraints、doctor 内部 setup 对象不直接进入模型；模型只接收经过 command/tool observation 裁剪的结果。

## 是否落盘

默认 `SandboxJsonlTraceRecorder` 写入 `<workspace>/.singularity/sandbox/trace.jsonl`。Windows 可用执行会在 workspace 下 `work/sandboxes/<sandbox_id>/` 创建 run root、workspace projection、`runner-spec.json`、`runner-result.json` 和 artifacts；cleanup 成功后删除 run root。doctor/setup 的 smoke 目录位于 `%PROGRAMDATA%\Singularity\windows-sandbox\acl-probe`、`runner-smoke` 和 `network-smoke`；该 machine-level state dir 是为了让 elevated setup、普通 doctor 和 sandbox account runner 共享可 ACL 管理的 probe 控制面。

Windows 凭据只写入 Windows Credential Manager target `SingularitySandbox`，不写 plaintext 文件，不进入 trace/report。Firewall rule group 为 `Singularity Sandbox`，当前规则 display name 为 `Singularity Sandbox Outbound Block`，规则以 `LocalUser` 绑定 sandbox account SID。登录 UI visibility 使用固定 registry key `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\SpecialAccounts\UserList` 下的固定 account name DWORD=0；这是产品化登录体验控制，不是 Microsoft 官方文档中的核心安全边界。doctor/setup/cleanup evidence 只记录 SID hash、state/probe/path hash、account/rule/credential target hash 或 redacted 名称、operation、errno、winerror、strerror、returncode、stdout/stderr 摘要和 elevated 状态等脱敏信息。旧失败状态中的 `SingularitySandboxRunner` account、同名 credential target 或 `Singularity Sandbox Runner Outbound Block` firewall rule 不参与新执行路径；doctor/setup 在 `diagnostics` 中以 hash/redacted 形式报告这些 legacy artifacts，cleanup 会安全删除这些固定 legacy artifacts。

## 是否进入 trace / audit

`SandboxManager` 可发出 `sandbox.requested`、`sandbox.prepared`、`sandbox.started`、`sandbox.cleaned`、`sandbox.capability_failed`、`sandbox.violation` 和 `sandbox.completed`。`SandboxJsonlTraceRecorder.append()` 写 result、capability、request 安全投影。sandbox result 不直接写 policy audit；关联的 policy decision 由 Policy 层记录。

`CommandExecutor._result_from_sandbox()` 会把 selected backend、enforcement status、execution backend、network denied proof、Job Object/timeout 状态、artifact refs、changed files 和 violations 放入 `CommandResult.isolation_report["sandbox"]`。Planner 和 final report 从这里聚合 `sandbox_isolation_summary`。

## 失败路径

- 平台不是 Windows：默认 backend 列表为空，返回 `backend_unavailable`。
- Windows primitive、sandbox account、login UI visibility、logon rights hardening、credential、state dir ACL、ACL boundary、account-scoped firewall、private desktop、runner smoke 或 network probe 缺失：doctor `available=false`，manager 返回 `backend_unavailable`，不启动进程。
- sandbox account 缺少 `SeInteractiveLogonRight` 或持 `SeDenyInteractiveLogonRight`（SE_DENY 覆盖同名 allow right）：`execution.launcher` 报告 `missing` 并带 `account_logon_rights` 证据，doctor `available=false`，返回 `backend_unavailable`；setup 的 `logon_right` step 负责授予并复查，doctor 据此证明非靠 admin 当前用户回退。
- sandbox account 出现在普通 Windows 登录 UI 用户列表、未添加 RDP/network/service/batch deny right、仍保留对应 allow right、或 state dir ACL 不含 sandbox account：doctor `setup.login_ui_visibility` / `setup.logon_rights` / `setup.state_dir_acl` 报告 `missing`，backend fail-closed；setup 的 `login_ui_visibility`、`logon_hardening`、`state_dir_acl` steps 负责修复。
- runner smoke 子进程退出 0、enforcement evidence 全部为真，但 `WindowsRunnerResult.metadata.account_sid_hash` 与 sandbox account SID hash 不匹配（疑似 admin 当前用户回退）：`runner_smoke` 报告 `missing`（`account_identity_verified=false`），fail-closed，不伪造 available。
- `CreateProcessWithLogonW` 因 `SeInteractiveLogonRight` 缺失或 executable RX 缺失返回 `ERROR_ACCESS_DENIED (5)`：runner_smoke / network_probe / acl_boundary 捕获 `OSError`，写入 `operation=*_create_process_with_logon`、`winerror=5`、`errno=5` 结构化 details；setup 通过 `logon_right`（LSA `SeInteractiveLogonRight`）与 `account_group`（`Users` 成员，提供 `python.exe`/系统目录 RX）补齐；账户依赖继承的 winsta/desktop DACL（无 ACE 授予）。
- sandboxed 命令无法在 restricted low-integrity token 下初始化（如普通可执行文件的 DLL init 失败）：`SetErrorMode` 抑制 Windows hard-error 对话框，`run()` 有限默认 wait 防止无限挂起；命令以 exit non-zero 失败，调用方处理失败，不弹窗、不回退本地执行。`WorkspaceMutationManager` 的 git 快照采集使用 `collect_git_state()` 执行本地有界只读 `git` 命令，不经过 `SandboxManager`，避免普通 mutation 事务被 sandbox doctor 探测拖超时；sandbox-required 命令仍由 `CommandExecutor -> SandboxManager.run()` fail-closed。
- `sandbox setup --json` 非 elevated：返回 `requires_elevation` 和 exit code 1；不得执行 account、credential、login UI visibility、LSA rights、firewall、ACL、runner smoke 或 network probe mutation，也不得把 partial/requires_elevation 改写为 ready。
- `sandbox cleanup --json` 非 elevated：返回 `requires_elevation` 和 exit code 1；不得删除 account、credential、firewall、registry entry 或 state dir。elevated cleanup 只删除固定 current/legacy Singularity 资产；如果固定资产不存在，仍报告 `completed` 且 `changed=false`。
- Windows machine state dir 不可创建或不可写：doctor/setup 通过 `windows_sandbox_state_dir` diagnostic 或对应 probe failure details 返回 `operation=windows_state_dir_mkdir` / `acl_probe_root_mkdir` 等结构化信息，仍保持 `available=false`。
- Windows account 探测 helper 缺失或 `net` 不可用：`_run_net()` 返回非零 `CompletedProcess`，setup 不因 `NameError` 崩溃，后续 report 仍通过 failed/partial 状态表达缺失能力。
- ACL probe directory、runner smoke、network probe 的 `OSError`、subprocess 失败或 runner result 缺失都会写入结构化 details：`operation` 区分 spec 写入失败、result 写入缺失、`CreateProcessWithLogonW`、`CreateProcessAsUserW`、restricted token、low integrity、private desktop、Job Object、child exit 非 0、host outbound baseline、firewall rule missing、runner launch 和 sandbox network not blocked。
- workspace 外 additional writable directories 或 path-specific `readonly_paths`：Windows backend 当前返回 `backend_unavailable`，直到实现独立 ACL lease/projection。
- protected path 显式访问：manager preflight 返回 `POLICY_BLOCKED`；projection 也会通过 exclude globs 排除 protected paths。
- denied network 下 host outbound baseline、runner socket probe、account-scoped firewall 或 doctor network probe 任一未验证：返回 `SandboxStatus.VIOLATION`。
- restricted token、low integrity、private desktop 或 Job Object evidence 未验证：返回 `SandboxStatus.VIOLATION`。
- timeout：runner 通过 Job Object/进程终止路径返回 `SandboxStatus.TIMEOUT`，metadata 记录 `job_killed`。
- run-root cleanup 异常：`SandboxResult.cleanup_status` 标记 `cleanup_failed`，不得保留 success；asset cleanup 异常：`WindowsSandboxCleanupReport.status=failed`，对应 failed_steps 带 hash/redacted diagnostics。
- 所有不可用路径均禁止回退到普通本地执行。

## 当前结构问题

- Windows memory/process limits 当前未实现；`SandboxCapabilities.memory_limit` 和 `process_limit` 保持 false，需要请求这些能力时 fail closed。
- workspace 外 additional writable directories 还没有独立 projection/ACL lease；当前正确行为是 fail closed。
- path-specific `readonly_paths` 还没有目录级 ACL lease；当前正确行为是 fail closed。
- Windows doctor 会运行 account-backed runner/network smoke，可能在 state dir 下创建 probe 目录；这是为了避免把 API primitive 存在误判成可执行 backend。
- Windows probe diagnostics 只允许使用 redaction/hash 后的路径、SID、account/rule 名称和输出摘要，不得输出完整 credential、token、SID 原文或完整敏感路径。
- `execution.launcher` 的 `account_logon_rights` 只枚举账户在 LSA 中的**直接** right，不展开 group 继承的 right；group 级 deny-interactive（罕见）不会被该字段发现，empirical proof 仍由 runner_smoke 兜底。
- 登录 UI visibility 使用 Windows 常见 registry user-list 控制来避免产品化副作用；Microsoft 官方文档中可确认的是 user-rights、ACL、Credential Manager、Firewall 等控制面，不应把该 registry entry 描述为 Microsoft 官方安全边界。真实安全边界仍是 account-scoped firewall、ACL、restricted token、low integrity、private desktop、Job Object 和 fail-closed doctor。
- 账户依赖继承的 winsta/desktop DACL 访问（不授予 ACE）；在 winsta/desktop ACL 严格的宿主上 `CreateProcessWithLogonW` 可能仍 error 5，届时 runner_smoke 会以 `winerror=5` 报告并 fail-closed。
- 无法在 restricted low-integrity token 下初始化的工具（如 `git.exe`）在 sandbox 内以 exit non-zero 失败（约 40s 内）；`SetErrorMode` 抑制弹窗、有限默认 wait 防挂起，但此类命令的 sandbox 路由仍带来延迟，理想方案是 policy 把只读/VCS 命令路由到 local（属后续 policy 工作）。
- `SandboxFilesystemManager` 只负责 COW projection、exclude globs 和 change detection；不能单独作为隔离 backend。

## 维护规则

- 新 backend 必须以真实 OS enforcement 和 external smoke 证明 capability；workspace copy、chmod 或普通子进程不能注册为 sandbox backend。
- capability 或 setup 缺失必须返回 `backend_unavailable`，不得静默本地执行。
- Windows setup、doctor、cleanup、account/ACL/firewall、restricted token、Job Object、private desktop、runner result metadata 或 network proof 变化时同步本文件；登录 UI visibility、LSA logon right（`SeInteractiveLogonRight`、RDP/network/service/batch deny rights 及对应 allow right 清理）、`Users` 组成员、state dir ACL、runner-script 物化、`SetErrorMode` 子进程弹窗抑制、有限默认 wait、`account_sid_hash` 身份证明同属本文件维护范围。
- 修改本模块对象字段、调用链、CLI、trace 或 report schema 后运行 `python scripts/verify_runtime_docs.py`；展示对象时必须列完整字段。
