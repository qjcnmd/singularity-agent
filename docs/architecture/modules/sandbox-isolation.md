# Sandbox Isolation 模块数据流

模块数据流文档 ID: sandbox-isolation

源码证据路径:
- src/singularity/sandbox/models.py
- src/singularity/sandbox/manager.py
- src/singularity/sandbox/backends.py
- src/singularity/sandbox/filesystem.py
- src/singularity/sandbox/windows.py
- src/singularity/sandbox/windows_common.py
- src/singularity/sandbox/windows_platform.py
- src/singularity/sandbox/windows_identity.py
- src/singularity/sandbox/windows_acl.py
- src/singularity/sandbox/windows_firewall.py
- src/singularity/sandbox/windows_runtime.py
- src/singularity/sandbox/windows_doctor.py
- src/singularity/sandbox/windows_cleanup.py
- src/singularity/sandbox/windows_models.py
- src/singularity/sandbox/trace_recorder.py
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
- WindowsUnelevatedSandboxBackend
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
- `WindowsSandboxSetup`: sandbox_accounts, login_ui_visibility, logon_rights, group_membership, state_dir_acl, acl_boundary, offline_network_filter, private_desktop, execution_backends, legacy_assets
- `WindowsSandboxExecution`: account_sids, credentials, launchers, runner_smoke, network_probe
- `WindowsSandboxDoctorReport`: implementation, platform_supported, platform_status, primitives, setup, execution, available, enforcement_status, blocking_requirements, recommended_action, diagnostics
- `WindowsSandboxSetupReport`: status, requested_operation, requires_elevation, changed, completed_steps, pending_steps, failed_steps, available_after_setup, message, diagnostics
- `WindowsSandboxCleanupReport`: status, requested_operation, requires_elevation, changed, completed_steps, failed_steps, diagnostics, residual_audit
- `WindowsRunnerSpec`: command, cwd, env, timeout_seconds, max_output_chars, network_mode, result_path, operation
- `WindowsRunnerResult`: exit_code, stdout, stderr, timed_out, started_at, ended_at, duration_ms, output_truncated, job_killed, network_denied_verified, metadata

## 这一层解决什么问题

Sandbox 层消费已经解析完成的 `SandboxRequest`，选择能够满足 filesystem、network 和 resource 要求的执行 backend，并统一返回执行结果或明确的 `backend_unavailable`。这一层不决定会话权限、不发放审批，也不把普通本地执行或 workspace copy 表述为强隔离。Windows 上的集中 selector 保留用户三档 `sandbox_mode`：`read-only`、`workspace-write`、`danger-full-access`；实际 `sandbox_backend` 为 `windows_elevated`、`windows_unelevated` 或 `local_process`。`windows_elevated` 是严格后端，`sandbox_enforcement="strict"`；`windows_unelevated` 是可用性优先的当前用户进程后端，`sandbox_enforcement="reduced"`、`enforcement_status="degraded"`，不声明 native/elevated OS sandbox。会话级 `danger-full-access` 是显式 relaxed 模式：当调用方仍显式要求 sandbox 但 native backend 不可用或能力不足时，`SandboxManager` 可执行 `local_process`，并必须把 `sandbox_enforcement="relaxed"`、`fallback_used=true`、`used_local_process_fallback=true` 写入 result、trace 和 command evidence；protected path preflight 仍先执行。

Rust native command path 不使用上述 Python oracle 的 `local_process` / `relaxed` fallback。`crates/sandbox` 暴露 `SandboxBackend`、`CommandRequest` 和 `CommandResult`，`crates/tools::WorkspaceTools::command()` 只调用已注入的 strict sandbox backend；backend 缺失、non-strict、network denied/allowlist unsupported、path admission 失败、read-only 写意图或 sensitive path 命中时都返回 blocked/unsupported 结果，不升级到 `danger-full-access`，也不把 `danger-full-access` 当成 approval bypass。Rust Windows backend 以 restricted token、read-only low-integrity token、Job Object、stdio handle allowlist、bounded output、cwd/path admission 和 controlled env 执行 `read-only`、`workspace-write`、`danger-full-access` 三种 Codex CLI 对齐 mode；显式 `danger-full-access` 仍通过同一 backend、policy、approval 和 trace/audit 链路。

Windows elevated 实现是双 principal 的 account-backed OS sandbox：父进程按`SandboxNetworkMode`选择`SingularityOffline`或`SingularityOnline`，准备 COW workspace projection，并只把本次 run root 授权给选中账户；runner 再用 restricted low-integrity token、private desktop 和 kill-on-close Job Object 启动实际命令。offline SID 被 account-scoped outbound firewall 阻断，online SID不得被该规则命中。该方向只对齐 OpenAI Codex 公开资料中的专用低权限账户、ACL、firewall、本地策略和受控 setup 原则，不声称与 Codex App 完全相同，也不复制其具体 token flags。任一账户、登录UI隐藏、logon rights、组成员、credential、state dir ACL、ACL boundary、offline firewall/online排除、private desktop、runner smoke或双向network probe缺失时，`windows_elevated` 不可用。`_ctypes/ctypes/_ssl/ssl` low-integrity Python runtime diagnostic 仍保留在 doctor diagnostics，并作为 elevated backend blocker 记录；默认 `workspace-write` 可降级到 `windows_unelevated`，不会把 elevated blocker 伪装为 elevated 成功。

公开设计参考：[Building the Codex Windows sandbox](https://openai.com/index/building-codex-windows-sandbox/) 与 [Codex on Windows](https://developers.openai.com/codex/windows/)。这些资料只用于解释原则对齐边界，不作为 Singularity 与 Codex App 实现相同的声明。

## 当前源码位置

- `src/singularity/sandbox/models.py`：请求、profile、capability、prepared/result 等对象。
- `src/singularity/sandbox/manager.py`：backend 选择、capability 校验、protected path preflight、生命周期和 trace。
- `src/singularity/sandbox/backends.py`：默认 backend 注册；Windows 上按顺序注册 `WindowsSandboxBackend` 与 `WindowsUnelevatedSandboxBackend`，非 Windows 返回空列表。
- `src/singularity/sandbox/filesystem.py`：workspace projection、protected glob 排除、变化检测和清理辅助。
- `src/singularity/sandbox/windows.py`：Windows sandbox public backend facade，定义严格 `WindowsSandboxBackend` 和 reduced `WindowsUnelevatedSandboxBackend`，并只显式导出 doctor/setup/cleanup 报告入口和少量子模块 owner 入口；私有 helper 测试直接绑定 owner module 或 `windows_common.py`，不通过 facade 兼容导出。
- `src/singularity/sandbox/windows_common.py`：Windows sandbox 共享 helper，包含 setup orchestration、doctor aggregation、diagnostic/hash/path helpers、native account/ACL/firewall/runtime 共用 helper；需要调用 owner module 时使用惰性 wrapper，避免 owner module 回导 public facade。
- `src/singularity/sandbox/windows_platform.py`：单点 Windows 平台判断 `is_windows()`，生产模块和测试通过该 wrapper 模拟平台，不 patch 全局 `os.name`。
- `src/singularity/sandbox/windows_identity.py`：sandbox account identity、credential 创建/刷新入口。
- `src/singularity/sandbox/windows_acl.py`：sandbox control dir / probe root ACL 授权入口。
- `src/singularity/sandbox/windows_firewall.py`：offline firewall rule 状态探测入口。
- `src/singularity/sandbox/windows_runtime.py`：account-backed runner smoke 入口。
- `src/singularity/sandbox/windows_doctor.py`：`probe_windows_sandbox()` doctor facade 和缓存入口。
- `src/singularity/sandbox/windows_cleanup.py`：`cleanup_windows_sandbox_assets()` cleanup 流程入口。
- `src/singularity/sandbox/windows_models.py`：Windows sandbox schema 常量、双账户 identity 映射、doctor/setup/cleanup DTO 和 `to_dict()` wire projection。
- `src/singularity/sandbox/trace_recorder.py`：默认 sandbox JSONL trace writer，使用 `TraceRedactor` 脱敏 command summary、timing、violation 和 relaxed fallback 审计投影。
- `src/singularity/sandbox/windows_runner.py`：sandbox account runner、restricted token child、private desktop、Job Object、timeout/output/network probe/result JSON。
- `src/singularity/command/executor.py`：依据会话级 `PermissionProfile` 构造完整 `SandboxRequest`，并把 `SandboxResult` 投影成 command evidence。

## 关键类、函数、字段

`SandboxManager.run()`只消费由`CommandExecutor`构造完成的请求，不重新解释`PolicyDecision`。selector 先执行 protected path preflight，再按用户 `sandbox_mode` 选择 backend：`read-only` 和 `workspace-write` 优先 `windows_elevated`，elevated 不可用或 elevated run-time recheck 返回 `BACKEND_UNAVAILABLE` 时降级 `windows_unelevated`；两者都不可用时返回 `backend_unavailable`，`sandbox_backend="unavailable"`、`enforcement_status="blocked"`。`danger-full-access` 不强制 native sandbox，可走 `local_process` relaxed fallback。所有结果 metadata 均写入 `sandbox_mode`、`sandbox_backend`、`sandbox_enforcement`、`enforcement_status`、`fallback_used`、`fallback_reason`、`elevated_available`、`elevated_blocker_summary` 和 `execution_backend`。

`WindowsSandboxBackend.prepare()`只接受`denied`和`allowed`：前者选择offline账户，后者选择online账户；`allowlist`和`unsupported`在准备文件系统前fail closed。`setup()`在elevated shell中幂等创建两个账户和独立credential，设置两个登录UI隐藏值，保留`SeInteractiveLogonRight`并移除`SeDenyInteractiveLogonRight`，清除batch/network/RDP/service allow right并添加对应deny right，强制直接本地组仅为通过SID解析的内置Users，授权machine state dir和probe/run控制目录ACL，授权当前Python runner所需runtime read/execute ACL，并只为offline SID创建outbound block。control目录授权只允许落在`%PROGRAMDATA%\Singularity\windows-sandbox`内的state/probe/run目标，授予sandbox账户Modify和宿主SID cleanup用Full Control；不递归授权用户目录、仓库、整个Python base或危险路径。runtime授权由当前`sys.executable`、`sys.prefix`、`sys.base_prefix`、`sys.exec_prefix`和`sysconfig`发现明确目标，只覆盖Python executable目录、`DLLs`、`Library/bin`、`Library/ssl`、`Library/lib/ossl-modules`、`_ssl/_hashlib/_socket.pyd`、`libssl/libcrypto`、`openssl.cnf`、provider module和base根顶层`python*.dll`；不递归授权整个base安装、包缓存、用户目录、profile或配置文件，也不授予write/modify。两个账户分别执行ACL boundary、restricted low-integrity runner身份smoke；network probe必须同时证明offline denied与online allowed。新结构验证完成后才删除固定legacy账户、credential、login UI、firewall和ACL资产；legacy残留使doctor fail closed。

`WindowsUnelevatedSandboxBackend` 复用 `SandboxFilesystemManager` 的 COW/read-only workspace staging、环境脱敏、timeout、output limit、artifact capture 和 change detection，在当前用户上下文执行命令。它的 capabilities 明确 `network_isolation=false`、`memory_limit=false`、`process_limit=false`；result metadata 写 `network_isolation="advisory"`、`filesystem_isolation="workspace_policy_enforced"`、`restricted_token=false`、`low_integrity=false`、`sandbox_account=None`。该 backend 依赖 manager 的 protected path preflight、workspace path 边界和 command/write policy，不声明 per-process network isolation、sandbox account、ACL/firewall/logon rights 或 native OS sandbox。

Windows setup按`windows_models._SANDBOX_IDENTITIES`逐个调用`_account_exists()`、`_create_sandbox_account()`、`_set_account_password()`与`_store_credential()`；账户创建和删除使用`netapi32`，超长名称校验只约束创建/改密，不阻止精确legacy删除。Firewall由PowerShell按固定`Singularity Sandbox`group重建和核验，Credential Manager、登录UI registry、LSA rights、组成员、ACL、runner smoke、Python runtime smoke与network probe均执行真实检查。Python runtime smoke分别用offline/online账户导入`_ctypes`、`ctypes`、`_ssl`、`ssl`、`socket`、`hashlib`和`pathlib`，读取`_ssl.__file__`、`ssl.OPENSSL_VERSION`、`ssl.get_default_verify_paths()`，并检查OpenSSL config/provider/cert与TEMP/TMP/profile目标可访问性；当`OPENSSL_MODULES`未设置且runtime中没有provider目录时，provider证据记为`not_configured`而不是不可读；失败时只在doctor `diagnostics`追加`kind="python_runtime_environment_blocker"`、`failure_type`、模块状态、runner evidence和hash/redacted路径，不改变doctor schema v2，也不让不依赖这些模块的任务被提前阻断。

`execution.launcher`不再只检查`CreateProcessWithLogonW` / `CreateProcessAsUserW`符号，而是真实报告account logon rights、window station/desktop、executable、working directory、domain/username form和logon flags。Level-1使用flags=0，不加载普通用户profile；runner使用显式受限环境，不依赖profile，从而减少专用账户对普通用户profile与登录体验的副作用。`launcher.status=available`要求真实interactive logon能力、无deny-interactive且代表性working directory对sandbox账户可访问；若`working_directory.account_has_access=false`，doctor必须把`execution.launchers`记为missing，不得同时报告preconditions satisfied。完整setup还要求RDP/network/service/batch deny且对应allow不存在。

`WindowsSandboxRunner.run`把自包含的`windows_runner.py`物化到ACL授权的run root，并从`PreparedSandbox.baseline`读取选中的`sandbox_account`、`credential_target`和`sandbox_role`。Level-1账户进程由`CreateProcessWithLogonW`以`.\<selected-account>`和flags=0启动（不加载profile），再创建restricted low-integrity token、private desktop、Job Object和Level-2实际命令。runner只把`account_sid_hash`和脱敏后的执行证据写入结果；doctor分别将该hash与offline/online预期SID hash比较，阻止回退到管理员当前用户。Level-1 account runner timeout 后如果未写 result file，`WindowsRunnerResult.timed_out=true` 且 metadata `error_code="account_runner_timeout"`，不得退化成普通 `runner_result_missing`。

`WindowsSandboxBackend.prepare()`先重新检查doctor可用性，再拒绝当前未实现的workspace外`writable_paths`和path-specific`readonly_paths`，之后在machine state的`runs/<sandbox_id>`短路径下创建COW projection，避免长项目路径触发Win32 process current-directory限制。run root通过control目录ACL helper关闭继承，只保留宿主principal的cleanup所需Full Control和本次选中sandbox账户的Modify；另一个sandbox账户不获授权。runner script、runner spec/result、Level-1 stdout/stderr都位于该run root控制面内，因此`CreateProcessWithLogonW`的`lpCurrentDirectory`、脚本读取和结果写入共享同一窄授权边界。仅`workspace/`projection设置低完整性标签，runner spec/result控制面保持medium integrity。protected paths仍通过projection exclude和manager preflight双重拒绝。

Windows doctor/setup使用machine-level`%PROGRAMDATA%\Singularity\windows-sandbox`保存短run root和共享probe控制面。setup负责创建目录并给两个账户授权；每个probe root以及`python-runtime-smoke/<role>`这类创建后的role子目录都会重新应用窄control ACL，避免新建目录缺少sandbox账户遍历/读写权限。doctor在账户、credential或目录缺失时只读返回missing，不会重建cleanup已删除的目录。elevated setup还在受保护HKLM键写入仅含schema、policy和两个SID hash的logon-rights证明；普通权限LSA枚举仅在返回`STATUS_ACCESS_DENIED`且该证明ACL安全、内容与当前两个principal完全匹配时使用证明，其他错误仍fail closed。cleanup删除该键。diagnostics只暴露hash，不暴露完整敏感路径；成功路径的control dir ACL审计只保留operation、hash、target和changed状态，stdout/stderr摘要等高成本展开只出现在失败或missing路径。

ACL boundary probe 分三层验证：probe root 作为 runner spec/result 控制面目录，`allowed/` 目录必须能由 sandbox account 写入，`denied/` 目录必须拒绝 sandbox account 写入。probe root和`allowed/`会显式授予目标sandbox账户Modify，且只在需要低完整性写入验证的目录设置low-integrity标签；通过条件是 allowed 写入 smoke 退出 0，denied 写入 smoke 因 `OSError` 退出 0；只创建目录不算通过。

`WindowsSandboxBackend.run()`在启动runner前重做uncached enforcement probe；除当前命令不相关的network probe单项波动外，任一双账户能力失效即返回`BACKEND_UNAVAILABLE`。如果唯一 blocking requirement 是`execution:network_probe`，backend只读取本次`PreparedSandbox.baseline.sandbox_role`对应的offline或online子状态；当前角色ready时允许继续，另一账户的瞬时probe失败不阻断当前network mode。其他setup、primitive、launcher、ACL、runner smoke或network filter blocker仍fail closed。`network_access=denied`还要求本次runner socket probe被拒绝、`setup.offline_network_filter`与当前offline probe均通过，否则返回`VIOLATION`；`network_access=allowed`只能由online账户执行。runner的restricted token、low integrity、private desktop和Job Object evidence必须来自真实结果。

## 真实运行时调用链

```text
PolicyEngine / ApprovalGate
-> CommandExecutor.run()
-> CommandExecutor._sandbox_request()
-> SandboxManager.run(SandboxRequest)
-> SandboxManager._protected_path_violation()
-> SandboxManager._select_backend()
-> backend.is_available() + SandboxManager.ensure_capabilities()
   -> windows_elevated selected when available
   -> windows_unelevated selected when elevated is unavailable/degraded
   -> local_process only for danger-full-access relaxed fallback
-> selected backend.prepare()
   -> SandboxFilesystemManager.prepare_filesystem()
   -> elevated: account ACL run root + low-integrity workspace projection
   -> unelevated: current-user workspace staging
   -> elevated: runner-spec.json / runner-result.json
-> selected backend.run()
   -> elevated: uncached doctor enforcement check
      -> WindowsSandboxRunner.run()
      -> materialize windows_runner.py into ACL'd sandbox_root (account-readable copy)
      -> CreateProcessWithLogonW(account runner)
      -> SetErrorMode + CreateRestrictedToken + low integrity + CreateDesktopW + Job Object + CreateProcessAsUserW(actual command)
      -> WindowsRunnerResult(metadata.account_sid_hash = self-identity proof)
      -> elevated BACKEND_UNAVAILABLE may trigger one windows_unelevated retry
   -> unelevated: current-user subprocess in staged workspace
-> selected backend.cleanup()
-> SandboxResult
-> CommandExecutor._result_from_sandbox()
-> CommandResult.isolation_report / trace / planner evidence / final report
```

`sandbox setup --json`（elevated）配置并验证两个专用账户；`run()`按network mode选择单一sandbox账户并授权本次run root，宿主principal只保留cleanup控制权。setup授予Python runtime targets前会先清理旧的base runtime根目录非递归显式ACE，再只恢复明确runtime target的`RX`/`(OI)(CI)RX`；不会递归授权整个base安装、包缓存、用户目录、profile或配置目录。`windows_common._runtime_env()`把发现到的Python executable、DLL和`Library/bin`等runtime目录前置到child `PATH`，用于修复DLL search path而不是扩大ACL。`sandbox cleanup --json`先删除current/legacy credential、固定firewall group、登录UI值和security attestation，并在删除账户前移除两个current账户对Python runtime targets的全部显式read/execute ACE；随后恢复并删除machine state dir，再移除账户LSA rights与账户，最后输出按accounts/credentials/firewall_rules/login_ui_entries/security_attestations/state_dirs分类的`residual_audit`。不存在的资产是completed/no-op；任一残留使cleanup失败。

## 真实任务中的对象流

以 `workspace-write` 会话运行 `python -m pytest`、`python -m compileall`、`ruff` 或 `mypy` 为例，`CommandExecutor._sandbox_request()` 从共享 `PermissionProfile` 和 policy decision 生成 `SandboxProfile` 及 `SandboxRequest`。其中 writable roots、additional writable directories、protected path patterns、network mode、timeout、output limit 和脱敏环境已经解析完成。

具体对象链路是：`CommandExecutor._sandbox_request()` -> `SandboxManager.run()` -> `SandboxManager._select_backend()` -> selected backend prepare/run/cleanup -> `CommandExecutor._result_from_sandbox()`。`CommandExecutor._sandbox_request()` 生成请求对象，`SandboxManager.run()` 消费请求并生成 lifecycle trace，`windows_elevated` 生成 runner spec/result 文件，`windows_unelevated` 直接在 staged workspace 中启动当前用户子进程，`CommandExecutor._result_from_sandbox()` 消费 `SandboxResult` 并生成 command evidence。`SandboxManager._select_backend()` 选择 backend 时取得的 `SandboxCapabilities` 会在同一次 `run()` 内复用于 capability gate 和 trace recording，避免重复 doctor/capability 探测；这只是同次调用内的只读快照，不跨命令缓存，也不替代 Windows elevated backend 在 `run()` 启动 runner 前执行的 runtime enforcement recheck。若 elevated recheck 返回 `BACKEND_UNAVAILABLE`，manager 会用该 blocker 摘要重试 `windows_unelevated`，成功结果仍标记 `fallback_used=true` 和 `sandbox_enforcement="reduced"`。

`SandboxManager.run()`不修改这些边界。Windows可用时，`prepare()`根据network mode把`baseline.sandbox_account`、`credential_target`和`sandbox_role`写成offline或online身份；run root对sandbox principal只授权选中的账户，同时保留宿主cleanup ACE。`WindowsSandboxBackend.run()`在启动 runner 前仍执行 uncached enforcement probe，除当前命令不相关的 network probe 单项波动外，任一 setup、primitive、launcher、ACL、runner smoke 或网络过滤 blocker 都 fail closed。runner读取对应credential并创建restricted low-integrity child。cleanup只接受位于machine state `runs/<sandbox_id>`下、名称为`sandbox_*`的单个run root。删除前先复用同一个sandbox账户启动Level-1 runner执行`operation="workspace_cleanup"`，仅删除当前run root下的`workspace/` projection；该cleanup不创建Level-2 low-integrity child，不访问网络，也不扩大任何Python runtime ACL。随后宿主进程对该run root执行take ownership、ACL继承/重置、宿主SID full-control恢复、medium integrity恢复和只读/系统/隐藏属性清理，再删除整个短路径root；失败时返回`cleanup_failed`，不把命令exit 0伪装为整体成功。

`danger-full-access` 模式不注册新的 native backend，也不改变 Windows backend 的 fail-closed 规则。它只在 `SandboxManager._select_backend()` 找不到可用 backend 或 `ensure_capabilities()` 报能力不足后进入私有 relaxed 本地进程分支。该分支先执行 protected path preflight，使用有界 timeout 和输出截断，stdout/stderr 经过 `TraceRedactor`，`SandboxResult.backend_name="local_process"`，`cleanup_status="not_required"`，metadata 写入 `sandbox_mode`、`sandbox_backend="local_process"`、`sandbox_enforcement="relaxed"`、`enforcement_status="relaxed"`、`execution_backend="local_process"`、`fallback_used=true`、`fallback_reason`、`used_local_process_fallback=true` 和 `local_process_fallback_reason`。

workspace 外 additional writable directories 当前不会被投影，也不会被 ACL 授权；只要 `writable_paths` 中出现 workspace 外目录，Windows backend 立即 fail closed。path-specific `readonly_paths` 也因尚无目录级 ACL lease 支持而 fail closed。workspace 内 protected paths 通过 projection exclude 和 manager preflight 生效。

`CommandExecutor._result_from_sandbox()` 消费 `SandboxResult`，把 backend、sandbox_backend、enforcement_status、execution_backend、sandbox_mode、sandbox_enforcement、fallback_used、fallback_reason、elevated_available、elevated_blocker_summary、used_local_process_fallback、local_process_fallback_reason、network_isolation、filesystem_isolation、network_denied_verified、process_tree_kill、job_killed、timeout_enforced、artifact refs、violations 和 changed files 写入 `CommandResult.isolation_report["sandbox"]` 与 metadata。只有 `windows_elevated` strict result 会投影为 `filesystem_isolation="native_os_sandbox"`；`windows_unelevated` 投影为 `workspace_policy_enforced`，relaxed `local_process` 保持 `workspace_cwd_advisory`。

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

定义位于`src/singularity/sandbox/windows_models.py`；`windows.py`继续导入并对外暴露同名对象。`to_dict()`输出`schema_version: "sandbox.windows.doctor/v2"`；复数状态聚合offline/online两个principal，每个capability item仍是`{status, checked, reason, evidence}`，证据经过`TraceRedactor`脱敏。

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
    sandbox_accounts: WindowsCapabilityState
    login_ui_visibility: WindowsCapabilityState
    logon_rights: WindowsCapabilityState
    group_membership: WindowsCapabilityState
    state_dir_acl: WindowsCapabilityState
    acl_boundary: WindowsCapabilityState
    offline_network_filter: WindowsCapabilityState
    private_desktop: WindowsCapabilityState
    execution_backends: WindowsCapabilityState
    legacy_assets: WindowsCapabilityState

@dataclass(frozen=True)
class WindowsSandboxExecution:
    account_sids: WindowsCapabilityState
    credentials: WindowsCapabilityState
    launchers: WindowsCapabilityState
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

`to_dict()`输出`schema_version: "sandbox.windows.setup/v2"`。step使用`sandbox_accounts`、`credentials`、`login_ui_visibility`、`logon_rights`、`group_membership`、`state_dir_acl`、`acl_boundary`、`offline_network_filter`、`private_desktop`、`execution_backends`、`network_probe`和`legacy_cleanup`；只有最终doctor available且无failed step时报告ready。

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

`to_dict()`输出`schema_version: "sandbox.windows.cleanup/v2"`。cleanup完成后doctor必须是`available=false/backend_unavailable`；`residual_audit`所有分类为0才允许status completed。

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
    residual_audit: dict[str, int] = field(default_factory=dict)
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
    operation: str = "command"

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

`WindowsRunnerSpec.operation` 默认为`command`，表示由Level-1账户进程创建restricted low-integrity Level-2 child执行真实命令；`workspace_cleanup`仅用于run-root cleanup，Level-1账户进程只删除当前run root下的`workspace/` projection，不执行用户命令。`WindowsRunnerResult.metadata` 在成功路径写入 `restricted_token`、`low_integrity`、`private_desktop`、`job_object`、`pid`、`account_name`、`account_sid_hash`（Level-1 账户进程自身 token user SID 的 `sha256[:16]`，供 doctor 校验非 admin 回退）；workspace cleanup metadata写`operation="workspace_cleanup"`且`restricted_token=false`、`low_integrity=false`。`run_spec` 异常路径写入 `error_type`；`WindowsSandboxRunner.run` 在 result 文件缺失时写入 `error_code="runner_result_missing"`，但如果缺失发生在 account runner timeout 后，写入 `error_code="account_runner_timeout"` 并保持 `timed_out=true`。`windows_runner.py` 必须保持 stdlib-only，以便被物化到 run root 后由 sandbox account 自包含执行；其本地 redaction regex 与 `RedactionProvider` plain marker profile 同步，覆盖 env secret、Authorization/Cookie header、URL query secret、JSON/CLI secret flag、private key 和常见 token 值，同时保留 `restricted_token` 等安全状态布尔值和 token usage 数值。

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

`PreparedSandbox.baseline`在Windows backend中包含workspace file baseline、`runner_spec`、`runner_result`、`sandbox_account`、`credential_target`、`sandbox_role`和安全 timing。timing 记录 doctor/readiness、account selection、workspace materialization 与当前 run-root ACL grant；只有doctor/setup全部通过后才生成，role只能来自network mode的固定映射。

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
- `windows_models.py` 定义 Windows sandbox schema 常量、offline/online identity 映射和 doctor/setup/cleanup DTO class。
- `windows_doctor.py` 中的 `probe_windows_sandbox()` 生成 `WindowsSandboxDoctorReport`，`windows.py` 继续导出同名入口。
- `windows_doctor.py` 中的 `setup_windows_sandbox()` 生成 `WindowsSandboxSetupReport`，`windows.py` 继续导出同名入口；该流程在elevated shell下配置两个账户、独立credential、登录UI、LSA rights、受限组成员、双账户state ACL、offline firewall、ACL/runner/network probes，并迁移固定legacy资产。
- `windows_cleanup.py` 中的 `cleanup_windows_sandbox_assets()`生成`WindowsSandboxCleanupReport`，删除current/legacy账户、credential、firewall group、登录UI值与machine state dir，执行LSA rights清理和最终residual audit；`windows.py` 继续导出同名入口。
- `WindowsSandboxBackend.prepare()` 生成 `PreparedSandbox` 与 runner spec；`WindowsSandboxBackend.run()` 生成执行型 `SandboxResult`。
- `WindowsSandboxRunner.run()` / `run_spec()` 生成 `WindowsRunnerResult`。

## 谁消费这些对象

- `SandboxManager` 和 backend 消费 `SandboxRequest`、`SandboxProfile` 及 `PreparedSandbox`。
- 内部 sandbox backend 诊断调用消费 `WindowsSandboxBackend` 的 doctor/setup/cleanup report 并原样输出 machine-readable JSON；Rust public CLI 当前只暴露 `sg config doctor` 作为运行时能力诊断入口，不暴露 sandbox 资产管理命令。
- `WindowsSandboxRunner` 消费 `runner-spec.json`，写 `runner-result.json`。
- `CommandExecutor._result_from_sandbox()` 消费 `SandboxResult` 并产生 `CommandResult`。
- `Planner.update_from_command()`、`VerificationRunner` 和 `Finalizer` 消费 command metadata / isolation report 中的 sandbox evidence。
- 完整 request、policy constraints、doctor 内部 setup 对象不直接进入模型；模型只接收经过 command/tool observation 裁剪的结果。

## 是否落盘

默认`SandboxJsonlTraceRecorder`写入`<workspace>/.singularity/sandbox/trace.jsonl`。Windows执行在machine state的`runs/<sandbox_id>/`创建短run root、workspace projection、runner spec/result和artifacts，结束前恢复medium integrity/属性并删除。doctor/setup使用同一machine state下的临时ACL/runner/network probe目录；setup创建machine state dir，asset cleanup删除它，缺失资产下的doctor不得重新创建。

Windows凭据分别写入Credential Manager target`SingularityOffline`与`SingularityOnline`，密码不落明文文件、不进入trace/report。Firewall group为`Singularity Sandbox`，唯一当前outbound block通过`LocalUser`绑定offline SID；online SID命中任何该group规则都会使doctor不可用。两个账户在`SpecialAccounts\UserList`中的DWORD均为0。受保护security attestation只保存两个SID hash和固定policy/schema，不保存SID原文。diagnostics只记录hash或redacted名称；legacy资产不存在runtime alias。

## 是否进入 trace / audit

`SandboxManager` 可发出 `sandbox.requested`、`sandbox.prepared`、`sandbox.started`、`sandbox.cleaned`、`sandbox.capability_failed`、`sandbox.violation` 和 `sandbox.completed`。`sandbox.prepared` 与 terminal payload 只增加数值 timing，不写 argv、credential 或路径正文；分段包括 doctor/readiness、account selection、ACL grant、process spawn、command runtime、output collection、artifact collection 和当前 run-root cleanup。`SandboxJsonlTraceRecorder.append()` 写相同的 result timing 安全投影，并使用 manager 选择阶段已经取得的同次 `SandboxCapabilities` 快照，避免为 trace 再次触发普通 capability probe；JSONL payload 通过 `TraceRedactor` 委托统一 `RedactionProvider` plain profile，list command 中的 secret flag 后续参数也会被替换为 `<redacted>`。JSONL 还写 `sandbox_mode`、`sandbox_backend`、`sandbox_enforcement`、`enforcement_status`、`execution_backend`、`fallback_used`、`fallback_reason`、`elevated_available`、`elevated_blocker_summary`、`used_local_process_fallback` 和 `local_process_fallback_reason`。sandbox result 不直接写 policy audit；关联的 policy decision 由 Policy 层记录。

`CommandExecutor._result_from_sandbox()` 会把 selected backend、enforcement status、execution backend、sandbox mode/enforcement、local-process fallback 审计、network denied proof、Job Object/timeout 状态、artifact refs、changed files 和 violations 放入 `CommandResult.isolation_report["sandbox"]`。Planner 和 final report 从这里聚合 `sandbox_isolation_summary`。

## 失败路径

- 平台不是 Windows：默认 backend 列表为空，返回 `backend_unavailable`。
- Windows primitive或任一offline/online账户的visibility、logon rights、受限组成员、credential、state ACL、ACL boundary、launcher、runner smoke、network probe缺失：doctor`available=false`，manager返回`backend_unavailable`。
- 任一账户缺少`SeInteractiveLogonRight`、持`SeDenyInteractiveLogonRight`、未持RDP/network/service/batch deny、仍持相应allow，或直接本地组不等于内置Users：对应复数capability报告missing并fail closed。
- offline firewall缺失/错误、online SID被Singularity firewall命中、offline probe可联网或online probe无法联网：`offline_network_filter`或`network_probe`missing，不得交换账户或回退本地执行。
- runner smoke 子进程退出 0、enforcement evidence 全部为真，但 `WindowsRunnerResult.metadata.account_sid_hash` 与 sandbox account SID hash 不匹配（疑似 admin 当前用户回退）：`runner_smoke` 报告 `missing`（`account_identity_verified=false`），fail-closed，不伪造 available。
- `CreateProcessWithLogonW`因`SeInteractiveLogonRight`、working directory/control dir ACL、runner script/spec/result/stdout/stderr所在目录权限或executable RX缺失返回error 5：launcher、runner smoke、ACL、network probe或Python runtime smoke写结构化diagnostics；如果错误发生在Level-1账户进程启动前，operation归类为`*_working_directory_access`，restricted token、low integrity、private desktop和Job Object evidence保持空/false，不误报为Level-2 child失败。setup通过`logon_rights`、`group_membership`、state/probe/run控制目录ACL和runtime RX步骤修复并复查两个账户，无法验证时保持fail closed。
- Python runtime RX授权失败：setup把`execution_backends`记入`failed_steps`，details只记录operation、target/account hash与进程退出摘要；不得用递归base-install授权或本地进程fallback绕过。setup会先移除base根目录上的stale explicit sandbox ACE，再仅恢复精确runtime target的read/execute。runner smoke仍是runtime可执行性的最终实证。
- Python runtime module smoke失败：doctor仍保持schema v2和原有`available`计算，但在`diagnostics`写`python_runtime_environment_blocker`，包含`failure_type`、失败账户角色、network mode、模块状态、OpenSSL config/provider/cert/TEMP访问信息、runtime target hash、runner exit/stdout/stderr摘要、restricted/low-integrity/private desktop/Job Object evidence及hash/redacted路径；probe root ACL、role目录ACL和导入前probe异常也必须分别写入`probe_acl_setup_failed`、`role_probe_acl_setup_failed`、`probe_execution_failed`这类机器可读`failure_type`，而不是只保留自然语言`reason`；`_ctypes/ctypes`与`_ssl/ssl`这类C扩展同时在low-integrity runner中初始化失败时归类为`python_c_extension_low_integrity_runtime_initialization_failed`，纯`_ssl.pyd`加载、`libssl/libcrypto`依赖、OpenSSL provider/config、证书路径、TEMP/profile、DLL search path和low-integrity初始化失败仍提前细分；未配置provider目录只作为`not_configured`证据，不等同于不可读失败。
- sandboxed 命令无法在 restricted low-integrity token 下初始化（如普通可执行文件的 DLL init 失败）：`SetErrorMode` 抑制 Windows hard-error 对话框，`run()` 有限默认 wait 防止无限挂起；命令以 exit non-zero 失败，调用方处理失败，不弹窗、不回退本地执行。`WorkspaceMutationManager` 的 git 快照采集使用 `collect_git_state()` 执行本地有界只读 `git` 命令，不经过 `SandboxManager`，避免普通 mutation 事务被 sandbox doctor 探测拖超时；sandbox-required 命令仍由 `CommandExecutor -> SandboxManager.run()` fail-closed。
- `sandbox setup --json` 非 elevated：返回 `requires_elevation` 和 exit code 1；不得执行 account、credential、login UI visibility、LSA rights、firewall、ACL、runner smoke 或 network probe mutation，也不得把 partial/requires_elevation 改写为 ready。
- `sandbox cleanup --json`非elevated时不执行删除；elevated cleanup删除固定current/legacy资产，先对state dir执行take ownership、ACL reset、medium integrity恢复与只读属性清理。资产不存在时completed/changed=false；residual audit非零时status failed。
- Windows machine state dir缺失时doctor只读报告`windows_state_dir_missing`；setup创建失败通过`state_dir_acl_mkdir`报告，均保持available=false。
- Windows account 探测 helper 缺失或 `net` 不可用：`_run_net()` 返回非零 `CompletedProcess`，setup 不因 `NameError` 崩溃，后续 report 仍通过 failed/partial 状态表达缺失能力。
- ACL probe directory、runner smoke、network probe 的 `OSError`、subprocess 失败或 runner result 缺失都会写入结构化 details：`operation` 区分 spec 写入失败、result 写入缺失、`CreateProcessWithLogonW`、Level-1 working directory/control dir access denied、`CreateProcessAsUserW`、restricted token、low integrity、private desktop、Job Object、child exit 非 0、host outbound baseline、firewall rule missing、runner launch 和 sandbox network not blocked。
- workspace 外 additional writable directories 或 path-specific `readonly_paths`：Windows backend 当前返回 `backend_unavailable`，直到实现独立 ACL lease/projection。
- protected path 显式访问：manager preflight 返回 `POLICY_BLOCKED`；projection 也会通过 exclude globs 排除 protected paths。
- denied network 下 host outbound baseline、runner socket probe、account-scoped firewall 或 doctor network probe 任一未验证：返回 `SandboxStatus.VIOLATION`。
- restricted token、low integrity、private desktop 或 Job Object evidence 未验证：返回 `SandboxStatus.VIOLATION`。
- timeout：runner 通过 Job Object/进程终止路径返回 `SandboxStatus.TIMEOUT`，metadata 记录 `job_killed`。
- run-root cleanup 异常：删除前会先用同一sandbox账户Level-1 runner删除当前`workspace/` projection，再修复单个run root的owner、ACL、宿主SID full-control、完整性级别和文件属性；任一步失败时`SandboxResult.cleanup_status`标记`cleanup_failed`，不得保留 success；asset cleanup 异常：`WindowsSandboxCleanupReport.status=failed`，对应 failed_steps 带 hash/redacted diagnostics。
- `read-only` 和 `workspace-write` 的 elevated 不可用路径只能降级到 `windows_unelevated`；如果 `windows_unelevated` 也不可用，返回 `backend_unavailable`，禁止回退到普通本地执行。
- `danger-full-access` 是显式 relaxed 模式：native backend 不可用或能力不足时可走本地进程，但 protected path preflight 仍先执行，结果必须审计为 `sandbox_enforcement="relaxed"` 和 `used_local_process_fallback=true`，不能声明 native OS sandbox enforcement。

## 当前结构问题

- Windows memory/process limits 当前未实现；`SandboxCapabilities.memory_limit` 和 `process_limit` 保持 false，需要请求这些能力时 fail closed。
- `windows_unelevated` 不提供 per-process network isolation、sandbox account、ACL/firewall/logon rights、restricted token、low-integrity、private desktop 或 memory/process limits；它只提供 workspace staging、policy/preflight 边界、timeout/output/artifact/change detection 和审计 metadata。
- workspace 外 additional writable directories 还没有独立 projection/ACL lease；当前正确行为是 fail closed。
- path-specific `readonly_paths` 还没有目录级 ACL lease；当前正确行为是 fail closed。
- Windows doctor会运行两个account-backed runner smoke、offline denied与online allowed probe，以避免把API存在误判成可执行backend；probe子目录在取证后清理。
- Windows probe diagnostics 只允许使用 redaction/hash 后的路径、SID、account/rule 名称和输出摘要，不得输出完整 credential、token、SID 原文或完整敏感路径；成功路径保持低成本结构化摘要，失败路径才展开 subprocess/OSError 细节。
- `execution.launcher` 的 `account_logon_rights` 只枚举账户在 LSA 中的**直接** right，不展开 group 继承的 right；group 级 deny-interactive（罕见）不会被该字段发现，empirical proof 仍由 runner_smoke 兜底。
- 登录 UI visibility 使用 Windows 常见 registry user-list 控制来避免产品化副作用；Microsoft 官方文档中可确认的是 user-rights、ACL、Credential Manager、Firewall 等控制面，不应把该 registry entry 描述为 Microsoft 官方安全边界。真实安全边界仍是 account-scoped firewall、ACL、restricted token、low integrity、private desktop、Job Object 和 fail-closed doctor。
- 账户依赖继承的 winsta/desktop DACL 访问（不授予 ACE）；在 winsta/desktop ACL 严格的宿主上 `CreateProcessWithLogonW` 可能仍 error 5。若diagnostics能识别为working directory/control dir访问失败，会报告`*_working_directory_access`；否则runner_smoke仍会以`winerror=5`和runner launch/create-process operation报告并fail-closed。
- 无法在 restricted low-integrity token 下初始化的工具（如 `git.exe`）在 sandbox 内以 exit non-zero 失败（约 40s 内）；`SetErrorMode` 抑制弹窗、有限默认 wait 防挂起，但此类命令的 sandbox 路由仍带来延迟，理想方案是 policy 把只读/VCS 命令路由到 local（属后续 policy 工作）。
- `SandboxFilesystemManager` 只负责 COW projection、exclude globs 和 change detection；不能单独作为隔离 backend。

## 维护规则

- 新 strict backend 必须以真实 OS enforcement 和 external smoke 证明 capability；workspace copy、chmod 或普通子进程不能注册为 strict/native sandbox backend。
- reduced backend 必须在 metadata 中明确 `sandbox_enforcement="reduced"` 和真实限制，不得标成 `native_os_sandbox`、`windows_elevated` 或 `execution_backend="account_restricted_token"`。
- capability 或 setup 缺失必须返回 `backend_unavailable` 或带审计的 reduced fallback，不得静默本地执行。
- Windows setup、doctor、cleanup、account/ACL/firewall、restricted token、Job Object、private desktop、runner result metadata 或 network proof 变化时同步本文件；登录 UI visibility、LSA logon right（`SeInteractiveLogonRight`、RDP/network/service/batch deny rights 及对应 allow right 清理）、`Users` 组成员、state dir ACL、Python runtime read/execute ACL生命周期、Python runtime module smoke diagnostics、成功/失败 diagnostics 展开策略、manager 同次 capabilities 快照复用、per-account network recheck语义、run-root cleanup同账户Level-1 workspace pre-cleanup、owner/ACL/host SID/integrity/attribute normalization、runner-script 物化、`SetErrorMode` 子进程弹窗抑制、有限默认 wait、`account_sid_hash` 身份证明、sandbox JSONL trace redaction 与 self-contained runner redaction 规则同属本文件维护范围。
- 修改本模块对象字段、调用链、CLI、trace 或 report schema 后运行 `python scripts/verify_runtime_docs.py`；展示对象时必须列完整字段。
