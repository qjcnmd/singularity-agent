# Command Execution模块数据流

模块数据流文档 ID: command-execution

源码证据路径:
- src/singularity/command/models.py
- src/singularity/command/executor.py
- src/singularity/command/backend.py
- src/singularity/command/policy.py
- src/singularity/tools/command.py
- src/singularity/error_codes.py

关键符号:
- CommandRequest
- CommandPlan
- CommandResult
- ProcessSession
- ProcessOutput
- ProcessStopResult
- CommandExecutor
- CommandPolicy

字段清单:
- ResourceLimits: timeout_seconds, idle_timeout_seconds, max_stdout_bytes, max_stderr_bytes, max_combined_output_bytes, max_memory_mb, max_processes, max_disk_write_mb
- CommandRequest: argv, shell, cwd, purpose, timeout_seconds, idle_timeout_seconds, env_request, network_mode, filesystem_mode, resource_limits, expected_outputs, risk_acceptance_reason, command_id
- CommandPolicyResult: decision, reasons, risk_tags, required_backend, required_network, required_filesystem, redaction_rules, error_code
- CommandPlan: request, policy_decision, cwd, backend, env_allowed, env_denied, isolation_report
- CommandResult: command_id, execution_status, semantic_status, exit_code, signal, duration_ms, timed_out, idle_timed_out, stdout_preview, stderr_preview, combined_output_preview, output_truncated, output_digest, artifact_path, changed_files, policy_decision, risk_tags, error_code, isolation_report, env_denied, killed_reason, backend, started_at, ended_at, stdout_bytes, stderr_bytes, secret_redactions, git_before, git_after, side_effects, metadata
- ProcessSession: process_id, command_id, pid, status, argv, shell, cwd, started_at, ports, health_check, logs_artifact_path, owner_transaction, exit_code, error_code
- ProcessOutput: process_id, stdout, stderr, combined_output, truncated, artifact_path
- ProcessStopResult: process_id, status, exit_code, killed_reason, changed_files, artifact_path, error_code

## 这一层解决什么问题

Command 层规范化 argv/shell、cwd、purpose、env、network/filesystem policy 和资源限制，再通过 `PolicyEngine`、approval、sandbox/backend 执行命令并生成可追踪结果。`CommandPolicy` 只保留 command risk 分类和 verification-runner routing helper，不再生成最终 allow/deny/review 裁决。

## 当前源码位置

- src/singularity/command/models.py
- src/singularity/command/executor.py
- src/singularity/command/backend.py
- src/singularity/command/policy.py
- src/singularity/tools/command.py
- src/singularity/error_codes.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

`run_command` / verification tools -> `CommandExecutor.run()` -> `CommandRequest` -> `CommandExecutor._policy_request()` -> `PolicyEngine.enforce()` -> `CommandExecutor._command_policy_result()` -> optional `ApprovalGate` / sandbox/backend -> `CommandResult` -> trace/context/planner evidence。`ExecutionBackend.execute()` 的当前签名必须接收 `cancellation_token` 关键字参数；`CommandExecutor` 不再静默回退到旧签名。

`LocalProcessBackend` 通过 stdout/stderr reader thread 将管道输出送入 `OutputCollector`；reader 优先使用 `read1(8192)` 读取可用缓冲，回退到 `read(8192)`，避免大输出逐字节读取，同时保持长进程的实时输出可被 `read_process_output()` 轮询。`stop_process()` 后 backend 会 drain 队列、bounded join reader thread、关闭 stdout/stderr pipe，并释放 `Popen`、reader thread 列表和队列引用；`CommandExecutor` 只保留停止后的 `ProcessSession` 与 bounded `ProcessOutput` summary，后续 `read_process_output()` / `list_processes()` / repeated `stop_process()` 不依赖活进程对象。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例：`CommandToolHandlers.run_command()` -> `CommandExecutor.plan()` -> `CommandExecutor.run()` 先把 tool 参数生成对象 `CommandRequest`，再由 `_policy_request()` 把 command risk 分类、cwd 是否越界、network/filesystem intent 和 redacted command metadata 投影为 `PolicyRequest`。`PolicyEngine.enforce()` 是唯一最终裁决者；`CommandExecutor._command_policy_result()` 只把 `PolicyDecision` 投影成嵌入 `CommandPlan` / `CommandResult` 的 `CommandPolicyResult`。若策略要求隔离，`SandboxManager.run()` 返回的 sandbox payload 被 `CommandExecutor._result_from_sandbox()` 转成 `CommandResult`；否则 `_completed_result()` 从 backend exit/stdout/stderr 生成结果。`CommandExecutor._record_trace()` 写入 command trace 事件，长输出写 artifact，`CommandResult.to_observation()` 进入 `context.sqlite3` 并由 `Planner.update_from_command()` 消费为 evidence。

在 `workspace-write` 且 `PolicyDecision.outcome=sandbox_required` 时，普通本地验证命令不得走 `local_process`。Windows backend 可用时，`CommandResult.backend` 为 `windows`，`isolation_report["sandbox"]` 和 metadata 同步记录 `enforcement_status`、`execution_backend`、`backend_is_local_process`、`network_denied_verified`、`process_tree_kill`、`job_killed`、`timeout_enforced`、`artifact_refs`、`sandbox_artifacts`、`sandbox_changed_files` 和 `sandbox_violations`。Windows backend 不可用时，结果是 `ExecutionStatus.BACKEND_ERROR` / `error_code=sandbox_unavailable`，不是普通本地执行。

## 真实对象完整结构

### CommandRequest（命令请求）

命令执行的规范化入口。**边界**：内部治理对象，不落盘；投影为 policy request 后进入 policy audit。

```python
@dataclass(frozen=True)
class CommandRequest:
    argv: list[str] | None = None
    shell: str | None = None
    cwd: str = "."
    purpose: CommandPurpose = CommandPurpose.UNKNOWN
    timeout_seconds: float | None = None
    idle_timeout_seconds: float | None = None
    env_request: dict[str, str] = field(default_factory=dict)
    network_mode: NetworkMode = NetworkMode.DISABLED
    filesystem_mode: FilesystemMode = FilesystemMode.READ_ONLY_WORKSPACE
    resource_limits: ResourceLimits = field(default_factory=ResourceLimits)
    expected_outputs: list[str] = field(default_factory=list)
    risk_acceptance_reason: str | None = None
    command_id: str = field(default_factory=lambda: uuid4().hex)
```

### CommandResult（命令结果）

命令执行的完整结果。**边界**：内部治理对象；`to_observation()` 安全投影写 `context.sqlite3`，`artifact_path` 引用 trace artifact，planner 消费 evidence。

```python
@dataclass(frozen=True)
class CommandResult:
    command_id: str
    execution_status: ExecutionStatus
    semantic_status: SemanticStatus
    exit_code: int | None
    signal: int | None
    duration_ms: int
    timed_out: bool
    idle_timed_out: bool
    stdout_preview: str
    stderr_preview: str
    combined_output_preview: str
    output_truncated: bool
    output_digest: str
    artifact_path: str | None
    changed_files: list[str]
    policy_decision: CommandPolicyResult
    risk_tags: list[CommandRisk]
    error_code: str | None
    isolation_report: dict[str, Any]
    env_denied: list[str] = field(default_factory=list)
    killed_reason: str | None = None
    backend: str = "local_process"
    started_at: str | None = None
    ended_at: str | None = None
    stdout_bytes: int = 0
    stderr_bytes: int = 0
    secret_redactions: int = 0
    git_before: dict[str, Any] = field(default_factory=dict)
    git_after: dict[str, Any] = field(default_factory=dict)
    side_effects: list[dict[str, Any]] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)
```

当结果来自 sandbox，`isolation_report["sandbox"]` 是 command 层对 `SandboxResult` 的安全投影，包含 `sandbox_id`、`backend`、`status`、`trace_id`、`enforcement_status`、`execution_backend`、`backend_is_local_process`、`network_denied_verified`、`process_tree_kill`、`job_killed`、`timeout_enforced`、`artifact_count`、`artifacts`、`artifact_refs`、`changed_files`、`changed_files_count`、`violations`、`cleanup_status`、`imported_changes_count` 和数值 `timing`。同一批字段的简化版本与 `sandbox_timing` 也进入 `CommandResult.metadata`，供 VerificationRunner、Planner、Finalizer 和 evaluation timing 聚合；timing 不包含命令正文或环境值。

### CommandPolicyResult（命令策略投影）

`PolicyDecision` 在 command 层的安全投影。**边界**：内部治理对象，嵌入 CommandPlan/CommandResult；完整 policy request/decision 由 `PolicyEngine` 进入 policy audit ledger。

```python
@dataclass(frozen=True)
class CommandPolicyResult:
    decision: CommandDecision
    reasons: list[str]
    risk_tags: list[CommandRisk]
    required_backend: str = "local_process"
    required_network: NetworkMode = NetworkMode.DISABLED
    required_filesystem: FilesystemMode = FilesystemMode.READ_ONLY_WORKSPACE
    redaction_rules: list[str] = field(default_factory=list)
    error_code: str | None = None
```

### 关键枚举值域

```python
class CommandPurpose(str, Enum):     # CommandRequest.purpose
    READ_ONLY_COMMAND = "READ_ONLY_COMMAND"
    PROJECT_VERIFICATION = "PROJECT_VERIFICATION"
    LINT = "LINT"
    TYPECHECK = "TYPECHECK"
    FORMAT_CHECK = "FORMAT_CHECK"
    FORMATTER = "FORMATTER"
    BUILD = "BUILD"
    CODE_GENERATION = "CODE_GENERATION"
    PACKAGE_MANAGER = "PACKAGE_MANAGER"
    NETWORK = "NETWORK"
    WRITE_WORKSPACE = "WRITE_WORKSPACE"
    DESTRUCTIVE = "DESTRUCTIVE"
    LONG_RUNNING = "LONG_RUNNING"
    SECRET_RISK = "SECRET_RISK"
    VCS_READ = "VCS_READ"
    VCS_MUTATION = "VCS_MUTATION"
    SYSTEM_MUTATION = "SYSTEM_MUTATION"
    EXECUTES_PROJECT_CODE = "EXECUTES_PROJECT_CODE"
    UNKNOWN = "UNKNOWN"

class ExecutionStatus(str, Enum):    # CommandResult.execution_status
    COMPLETED = "completed"
    POLICY_DENIED = "policy_denied"
    REVIEW_REQUIRED = "review_required"
    SPAWN_FAILED = "spawn_failed"
    TIMED_OUT = "timed_out"
    IDLE_TIMED_OUT = "idle_timed_out"
    PROCESS_KILLED = "process_killed"
    BACKEND_ERROR = "backend_error"

class SemanticStatus(str, Enum):     # CommandResult.semantic_status
    SUCCEEDED = "succeeded"
    EXIT_NONZERO = "exit_nonzero"
    TESTS_FAILED = "tests_failed"
    BUILD_FAILED = "build_failed"
    LINT_FAILED = "lint_failed"
    TYPECHECK_FAILED = "typecheck_failed"
    EXECUTION_FAILED = "execution_failed"
    POLICY_BLOCKED = "policy_blocked"

class CommandDecision(str, Enum):    # CommandPolicyResult.decision
    ALLOW = "allow"
    REQUIRE_REVIEW = "require_review"
    DENY = "deny"
```

### 数据流概述

`CommandToolHandlers.run_command()` 生成 `CommandRequest`，`CommandExecutor._policy_request()` 生成 `PolicyRequest`，`PolicyEngine.enforce()` 生成 `PolicyDecision`，`CommandExecutor._command_policy_result()` 生成 command-local `CommandPolicyResult`，`CommandExecutor.plan()` 组合为 `CommandPlan`。若策略要求隔离，`SandboxManager.run()` 返回 sandbox payload 被 `_result_from_sandbox()` 转成 `CommandResult`；否则 `_completed_result()` 从 backend 生成结果。`CommandResult.to_observation()` 写 `context.sqlite3`，长输出写 trace artifact，`CommandExecutor._record_trace()` 写 trace event。

命令执行层的核心 `error_code` 值来自 `singularity.error_codes.ErrorCode`：policy、sandbox、timeout、idle timeout、semantic failure、exit nonzero、output limit、process not found、verification runner required 等分支仍输出原字符串值，但不再在 `CommandExecutor` 内维护独立字面量映射。

## 谁生成这些对象

- command tool、VerificationRunner 与 evaluation setup 生成 `ResourceLimits`/`CommandRequest`；`CommandExecutor._policy_request()` 生成 `PolicyRequest`，`PolicyEngine.enforce()` 生成最终 `PolicyDecision`，`CommandExecutor._command_policy_result()` 或 executor 的 fail-closed 分支生成 command-local `CommandPolicyResult`，`CommandExecutor.plan()` 组合为 `CommandPlan`。
- `CommandExecutor.run()` 的 backend、sandbox、blocked 分支生成 `CommandResult`；`start_process()`、`read_process_output()`、`stop_process()` 分别生成 `ProcessSession`、`ProcessOutput`、`ProcessStopResult`。长运行进程停止后，executor 内部 session record 生成停止态 `ProcessSession` 和输出摘要，并清空 `RunningProcess` 强引用。

## 谁消费这些对象

`CommandExecutor` 消费 request、`PolicyDecision` 投影和 backend/sandbox result；command tool、verification、planner 消费 result/process objects。`CommandResult.to_observation()` 和 process `to_dict()` 的安全投影进入 tool result/context，模型看不到 env request、raw secret argv 或完整内部 plan。

## 是否落盘

Command plan 和 process session 只在 executor 内存；长 stdout/stderr 由 `OutputCollector` 写 artifact，路径放入 `CommandResult.artifact_path` / `ProcessSession.logs_artifact_path`。停止后的 process session 仍保留 bounded output summary 与 artifact path，但不保留 live process、pipe、reader thread 或 queue。result 的安全 observation 写 context SQLite，side effects 可写 workspace state journal。

## 是否进入 trace / audit

CommandExecutor 发出 `COMMAND_*` event 与 legacy `command` record，payload 包含 status、exit、digest、artifact ref、changed files、policy/isolation 摘要；argv/env/output 在写入前脱敏。`PolicyEngine` 的 request/decision 进入 policy audit ledger。

## 失败路径

`PolicyEngine` 返回 `REQUIRE_REVIEW`/`DENY`、cwd denied、sandbox setup/backend error、timeout/idle timeout、kill 或非零退出时生成非成功 `CommandResult`；process API 通过 `status`、`error_code`、`killed_reason` 表达失败，不把启动失败登记为 running session。`workspace-write` 下 sandbox backend unavailable 时 `backend` 不得是 `local_process`，`sandbox_availability` 必须说明 backend 状态，输出仍按 `SecretRedactor` 和 output limit 处理。

## 当前结构问题

同步维护 request→policy→plan→backend→result 与 long-running process 两条路径；模型可见边界是 observation，不是完整 `CommandResult.to_dict()`。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
