# Policy / Approval Gates 模块数据流

模块数据流文档 ID: policy-approval-gates

源码证据路径:
- src/singularity/policy/permissions.py
- src/singularity/policy/models.py
- src/singularity/policy/config.py
- src/singularity/policy/rules.py
- src/singularity/policy/engine.py
- src/singularity/policy/approval.py
- src/singularity/policy/audit.py

关键符号:
- PermissionProfile
- PermissionSummary
- ProtectedPathRule
- PolicyConfig
- PolicySubject
- ResourceRef
- PolicyConstraints
- PolicyRequest
- ApprovalScope
- ApprovalRequirement
- ApprovalGrant
- PolicyDecision
- PolicyAuditEntry
- PolicyEngine
- ApprovalGate

字段清单:
- ProtectedPathRule: pattern, allow_read, allow_write, allow_execute, hard_deny, description
- PermissionSummary: profile, writable_roots, network_access, approval_policy, protected_paths_enforced
- PermissionProfile: profile, workspace_roots, additional_writable_directories, network_access, approval_policy, protected_paths
- PolicyConfig: workspace_root, audit_log_path, approval_grants_path, consumption_ledger_path, operator_key_path, permission_profile
- PolicySubject: subject_type, name
- ResourceRef: resource_type, identifier, normalized_identifier, workspace_relative, sensitive, metadata
- PolicyConstraints: filesystem_mode, network_allowed, max_duration_seconds, max_output_chars, env_redaction, sandbox_required, hard_isolation_required
- PolicyRequest: session_id, task_id, phase_id, action_id, component, operation, capability, subject, resource, reason, request_id, proposed_by_model, risk_tags, metadata, evidence_refs, reversible, requires_network, touches_workspace, touches_secrets, destructive, long_running, interactive, workspace_root
- ApprovalScope: capabilities, path_globs, command_patterns, network_hosts, max_duration_seconds, max_files, session_only, single_use
- ApprovalRequirement: message, scope, review_kind, details
- ApprovalGrant: decision_id, request_id, approved_by, scope, session_id, approved_at, grant_id, expires_at, single_use, reason, operator_signature
- PolicyDecision: request_id, outcome, reason, risk_level, risk_tags, user_message, constraints, required_approval, rule_ids, audit_severity, context_summary, decision_id, approval_grant_id
- PolicyAuditEntry: timestamp, session_id, task_id, phase_id, action_id, request_id, decision_id, component, operation, capability, resource_summary, normalized_input_hash, risk_level, risk_tags, outcome, rule_ids, reason, approval_required, approval_grant_id, approved_by_user, user_decision, constraints, execution_result_ref

## 这一层解决什么问题

Policy 层把会话级权限边界、动作级策略决策和人工 approval 串成同一条强制执行链。会话级边界由 `PermissionProfile` 描述；动作级结果仍使用仓库既有 `allow / deny / require_review / sandbox_required` 语义。完整内部对象只供 runtime、audit、trace 使用；模型只能看到裁剪后的权限摘要和安全错误信息。

## 命名来源

| 名称 | 来源 |
|---|---|
| `PermissionProfile` | Codex Permission Profiles 与通用 access-control profile 术语 |
| `read-only` / `workspace-write` / `danger-full-access` | Codex sandbox modes |
| `approval_policy` / `on-request` / `never` | Codex approvals 常见策略名 |
| `--add-dir` / additional writable directories | Codex CLI 额外可写目录语义 |
| protected paths / deny 优先 | Codex permissions、NIST least privilege 与通用 deny-overrides access control |
| `require_review` / `sandbox_required` | 沿用仓库既有动作级 `DecisionOutcome`，不是会话模式 |

## 当前源码位置

- `src/singularity/policy/permissions.py`
- `src/singularity/policy/models.py`
- `src/singularity/policy/config.py`
- `src/singularity/policy/rules.py`
- `src/singularity/policy/engine.py`
- `src/singularity/policy/approval.py`
- `src/singularity/policy/audit.py`

## 关键类、函数、字段

- `PermissionProfileName`: `READ_ONLY`, `WORKSPACE_WRITE`, `DANGER_FULL_ACCESS`
- `ApprovalPolicy`: `ON_REQUEST`, `NEVER`
- `NetworkAccess`: `DENIED`, `ALLOWED`
- `ProtectedPathRule`: `pattern`, `allow_read`, `allow_write`, `allow_execute`, `hard_deny`, `description`
- `PermissionSummary`: `profile`, `writable_roots`, `network_access`, `approval_policy`, `protected_paths_enforced`
- `PermissionProfile`: `profile`, `workspace_roots`, `additional_writable_directories`, `network_access`, `approval_policy`, `protected_paths`
- `PolicyConfig`: `workspace_root`, `audit_log_path`, `approval_grants_path`, `consumption_ledger_path`, `operator_key_path`, `permission_profile`
- `PolicySubject`: `subject_type`, `name`
- `ResourceRef`: `resource_type`, `identifier`, `normalized_identifier`, `workspace_relative`, `sensitive`, `metadata`
- `PolicyConstraints`: `filesystem_mode`, `network_allowed`, `max_duration_seconds`, `max_output_chars`, `env_redaction`, `sandbox_required`, `hard_isolation_required`
- `PolicyRequest`: `session_id`, `task_id`, `phase_id`, `action_id`, `component`, `operation`, `capability`, `subject`, `resource`, `reason`, `request_id`, `proposed_by_model`, `risk_tags`, `metadata`, `evidence_refs`, `reversible`, `requires_network`, `touches_workspace`, `touches_secrets`, `destructive`, `long_running`, `interactive`, `workspace_root`
- `ApprovalScope`: `capabilities`, `path_globs`, `command_patterns`, `network_hosts`, `max_duration_seconds`, `max_files`, `session_only`, `single_use`
- `ApprovalRequirement`: `message`, `scope`, `review_kind`, `details`
- `ApprovalGrant`: `decision_id`, `request_id`, `approved_by`, `scope`, `session_id`, `approved_at`, `grant_id`, `expires_at`, `single_use`, `reason`, `operator_signature`
- `PolicyDecision`: `request_id`, `outcome`, `reason`, `risk_level`, `risk_tags`, `user_message`, `constraints`, `required_approval`, `rule_ids`, `audit_severity`, `context_summary`, `decision_id`, `approval_grant_id`
- `PolicyAuditEntry`: `timestamp`, `session_id`, `task_id`, `phase_id`, `action_id`, `request_id`, `decision_id`, `component`, `operation`, `capability`, `resource_summary`, `normalized_input_hash`, `risk_level`, `risk_tags`, `outcome`, `rule_ids`, `reason`, `approval_required`, `approval_grant_id`, `approved_by_user`, `user_decision`, `constraints`, `execution_result_ref`

## 真实运行时调用链

`ProductionConfig.to_permission_profile()` 在 kernel 启动时生成一个不可变 `PermissionProfile`。`AgentGraphBuilder._build_policy_sandbox()` 用同一个 profile 构造 `PolicyConfig`、`PolicyEngine`、`ApprovalGate` 和 `SandboxManager`，再把同一个 `PolicyEngine` / `ApprovalGate` 注入 `CommandExecutor`、`WorkspaceMutationManager`、`VerificationRunner` 和 `ToolExecutor`。

执行时，`ToolExecutor` 只做工具准入和 hard deny；对于 delegated command/mutation，它不提前消费 approval grant。`CommandExecutor._policy_request()`、`WorkspaceMutationManager._policy_request()`、`VerificationRunner._policy_request()` 在真正执行边界生成 `PolicyRequest`，调用 `PolicyEngine.enforce()` 得到 `PolicyDecision`。`REQUIRE_REVIEW` 由该执行边界调用 `ApprovalGate.authorize()` 消费单次授权；`SANDBOX_REQUIRED` 交给 `SandboxManager.run()` 执行已经构造好的 `SandboxRequest`。`approval_policy=never` 在 rules 层把 review 转为 deny。

Windows sandbox backend 不改变 policy 语义：PolicyEngine 只决定普通本地验证命令在 `workspace-write` 下需要 sandbox，ApprovalGate 只处理 review/approval，不创建账户、不放宽到 `danger-full-access`。Sandbox 层随后验证 sandbox account、Credential Manager 凭据、ACL boundary、LocalUser firewall、private desktop、restricted low-integrity token、Job Object 和 network probe。缺任一能力时 command 结果是 sandbox/backend error，不回退到普通本地进程。

## 真实任务中的对象流

以模型请求写入 `quicksort.py` 并运行验证为例：`ToolExecutor` -> `WorkspaceMutationManager.apply_operations()` -> `PolicyEngine.enforce()` -> `ApprovalGate.authorize()` 或直接执行。Mutation manager 生成 `PolicyRequest`，`PolicyEngine.enforce()` 消费 request、生成 `PolicyDecision`，并通过 `PolicyAuditWriter.append()` 落盘到 policy audit JSONL；workspace 内普通写入在 `workspace-write` 下 allow，`.env`、`.git/config` 或 `.singularity/**` 则因 `PermissionProfile.protected_paths` hard deny。随后命令验证进入 `CommandExecutor.run()` -> `CommandExecutor._sandbox_request()` -> `SandboxManager.run()`；command executor 再次生成 command `PolicyRequest`，在 `workspace-write` 下普通本地命令得到 `sandbox_required`，并生成 `SandboxRequest` 交给 sandbox 层消费。trace 记录 `policy.requested`、`policy.decided` / `policy.blocked`、`sandbox.requested`、`sandbox.completed`；audit JSONL 记录完整 request/decision 投影；模型 context 只收到裁剪后的 outcome/reason 或 `PermissionSummary`。

`PolicyAuditWriter.append()` 写入 JSONL 对象，`PolicyEngine._emit_policy_trace()` 写入 trace 事件，`ApprovalGate.authorize()` 返回 grant 结果。

`PermissionProfile.additional_writable_directories` 仍是会话级边界来源。Windows sandbox 当前只支持 workspace projection；workspace 外 additional writable directories 和 path-specific readonly leases 由 backend fail closed，直到 sandbox 层实现独立 projection/ACL lease，而不是由 policy 层假定可执行。

## 真实对象完整结构

### PermissionProfile（会话级权限配置）

```python
@dataclass(frozen=True)
class PermissionProfile:
    profile: PermissionProfileName
    workspace_roots: tuple[Path | str, ...]
    additional_writable_directories: tuple[Path | str, ...] = ()
    network_access: NetworkAccess = NetworkAccess.DENIED
    approval_policy: ApprovalPolicy = ApprovalPolicy.ON_REQUEST
    protected_paths: tuple[ProtectedPathRule | str, ...] = ()
```

`PermissionProfile.__post_init__()` 解析和规范化 root/add-dir 路径，并把用户配置的 `protected_paths` 追加到内建保护规则后面；用户配置只能增加规则，不能移除内建规则。`summary()` 只生成模型可见摘要，不包含 matcher、内部规则、grant、decision 或 backend capability 对象。

### PermissionSummary（模型可见权限摘要）

```python
@dataclass(frozen=True)
class PermissionSummary:
    profile: PermissionProfileName
    writable_roots: tuple[str, ...]
    network_access: NetworkAccess
    approval_policy: ApprovalPolicy
    protected_paths_enforced: bool = True
```

### ProtectedPathRule（受保护路径规则）

```python
@dataclass(frozen=True)
class ProtectedPathRule:
    pattern: str
    allow_read: bool = False
    allow_write: bool = False
    allow_execute: bool = False
    hard_deny: bool = True
    description: str = ""
```

内建规则覆盖 `.git/**` 写入、`.singularity/**`、非示例 `.env*`、SSH/cloud 凭据目录、credential/token 文件和私钥/证书密钥文件。`.git` 读取允许，直接写入拒绝；VCS mutation 命令仍由 command risk 走 review。

### PolicyConfig（策略运行配置）

```python
@dataclass(frozen=True)
class PolicyConfig:
    workspace_root: Path | str = "."
    audit_log_path: Path | str | None = None
    approval_grants_path: Path | str | None = None
    consumption_ledger_path: Path | str | None = None
    operator_key_path: Path | str | None = None
    permission_profile: PermissionProfile | None = None
```

`PolicyConfig` 不再包含旧审批模式枚举或旧安全模式枚举。默认 audit、approval grants、consumption ledger 和 operator key 均落在 workspace 外的 policy home，避免模型通过 workspace 写入伪造授权。

### PolicyRequest（策略请求）

```python
@dataclass(frozen=True)
class PolicyRequest:
    session_id: str
    task_id: str
    phase_id: str
    action_id: str
    component: PolicyComponent
    operation: OperationKind
    capability: Capability
    subject: PolicySubject
    resource: ResourceRef
    reason: str
    request_id: str = field(default_factory=lambda: f"policy_req_{uuid4().hex[:12]}")
    proposed_by_model: bool = False
    risk_tags: list[RiskTag | str] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)
    evidence_refs: list[str] = field(default_factory=list)
    reversible: bool = True
    requires_network: bool = False
    touches_workspace: bool = False
    touches_secrets: bool = False
    destructive: bool = False
    long_running: bool = False
    interactive: bool = False
    workspace_root: str | None = None
```

### 枚举值域

```python
class PermissionProfileName(str, Enum):
    READ_ONLY = "read-only"
    WORKSPACE_WRITE = "workspace-write"
    DANGER_FULL_ACCESS = "danger-full-access"

class ApprovalPolicy(str, Enum):
    ON_REQUEST = "on-request"
    NEVER = "never"

class NetworkAccess(str, Enum):
    DENIED = "denied"
    ALLOWED = "allowed"

class DecisionOutcome(str, Enum):
    ALLOW = "allow"
    DENY = "deny"
    REQUIRE_REVIEW = "require_review"
    ASK_USER = "ask_user"
    ESCALATE = "escalate"
    SANDBOX_REQUIRED = "sandbox_required"
```

### PolicyDecision（策略决策）

```python
@dataclass(frozen=True)
class PolicyDecision:
    request_id: str
    outcome: DecisionOutcome
    reason: str
    risk_level: RiskLevel = RiskLevel.NONE
    risk_tags: list[RiskTag | str] = field(default_factory=list)
    user_message: str = ""
    constraints: PolicyConstraints = field(default_factory=PolicyConstraints)
    required_approval: ApprovalRequirement | None = None
    rule_ids: list[str] = field(default_factory=list)
    audit_severity: str = "info"
    context_summary: str = ""
    decision_id: str = field(default_factory=lambda: f"policy_dec_{uuid4().hex[:12]}")
    approval_grant_id: str | None = None
```

### PolicyConstraints（动作级约束）

```python
@dataclass(frozen=True)
class PolicyConstraints:
    filesystem_mode: str = "none"
    network_allowed: bool = False
    max_duration_seconds: int | None = None
    max_output_chars: int | None = None
    env_redaction: bool = True
    sandbox_required: bool = False
    hard_isolation_required: bool = False
```

`PolicyConstraints` 只保留 runtime 真实消费的动作级约束。路径、host、writable root 等边界由会话级 `PermissionProfile` 执行，不再在 constraints 上保留 no-op 字段。

### ApprovalGrant 与审计对象

```python
@dataclass
class ApprovalGrant:
    decision_id: str
    request_id: str
    approved_by: str
    scope: ApprovalScope
    session_id: str | None = None
    approved_at: str = field(default_factory=lambda: datetime.now(UTC).isoformat())
    grant_id: str = field(default_factory=lambda: f"grant_{uuid4().hex[:12]}")
    expires_at: str | None = None
    single_use: bool = True
    reason: str = ""
    operator_signature: str | None = None
```

`ApprovalGrant` 不携带 consumed 状态；消费事实由 HMAC chained `GrantConsumptionLedger` 记录。`PolicyAuditEntry` 记录 request/decision 的审计投影，`PolicyEngine._emit_policy_trace()` 记录脱敏 trace event，两者不互相替代。

## 谁生成这些对象

`ProductionConfig` 生成 `PermissionProfile`。`AgentGraphBuilder` 生成同 profile 的 `PolicyConfig`、`PolicyEngine`、`ApprovalGate` 和 `SandboxManager`。Tool、command、mutation、verification、plugin manager 生成 `PolicySubject`、`ResourceRef` 和 `PolicyRequest`。`DefaultLocalPolicyRules.decide()` 生成 `PolicyConstraints`、`ApprovalRequirement` 和 `PolicyDecision`。`ApprovalGate` 或 remote approval 生成 `ApprovalGrant`。`PolicyAuditWriter.append()` 生成 `PolicyAuditEntry`。

## 谁消费这些对象

`PolicyEngine.enforce()` 消费 request 并返回 decision。`CommandExecutor`、`WorkspaceMutationManager` 和 `VerificationRunner` 消费 decision，直接处理 allow/deny/review/sandbox_required。`ApprovalGate.authorize()` 消费 review decision 并注册/消费 single-use grant。`SandboxManager` 只消费已经完成权限判定的 `SandboxRequest`，不重新判断 session permission。`ToolProtocolResultBuilder` 不把完整 decision/request/grant/constraints 暴露给模型。

## 是否落盘

`PolicyAuditWriter` 将 `PolicyAuditEntry` 追加到 audit JSONL。`ApprovalGate` 将 grants 写入 workspace 外的 `approval_grants.jsonl`，consumption ledger 写入 workspace 外的 `grant_consumption_ledger.jsonl`。`PermissionProfile` 是内存中的启动时快照；final report 只记录安全摘要。

## 是否进入 trace / audit

Trace events.jsonl 记录 `TraceEventType.POLICY_REQUESTED`、`TraceEventType.POLICY_DECIDED`、`TraceEventType.POLICY_BLOCKED`、`TraceEventType.APPROVAL_REQUESTED`、`TraceEventType.APPROVAL_GRANTED`、`TraceEventType.APPROVAL_DENIED`，payload 含 profile 名、action decision、approval result、enforcement 状态和脱敏资源 handle。Audit JSONL 由 `PolicyAuditWriter.append()` 记录 request/decision 的审计投影。模型 context 只接收 `PermissionSummary` 和裁剪后的 policy observation；不接收可伪造审批或绕过策略的内部对象。

## 失败路径

受保护路径 hard-deny 优先。`approval_policy=never` 把 `REQUIRE_REVIEW` 转为 `DENY`。没有 `InteractionController`、grant store 不可信、grant 过期、scope 不匹配或 single-use 已消费时，`ApprovalGate` fail-closed。OS sandbox 不可用、Windows setup 未完成、account-scoped firewall 未验证或 runner smoke 失败时返回 `backend_unavailable`，不会回退为普通本地执行。

## 当前结构问题

Windows account-backed sandbox backend 已接入 runtime，但 elevated setup 资产缺失、外部 additional writable directory projection 或 path-specific readonly ACL lease 缺失时仍 fail closed。`PermissionProfile` 已接入 runtime，但新增 path/resource 入口时仍必须显式调用 protected matcher，防止直接 backend 调用绕过策略。

## 维护规则

修改权限 profile、policy request/decision、approval grant、sandbox request、CLI/config、trace/report schema、model-visible context 或执行边界时，必须同步本文件并运行 `python scripts/verify_runtime_docs.py`。文档只描述当前源码真实结构，不保留旧模式枚举、容器 sandbox backend 或未执行的兼容字段。
