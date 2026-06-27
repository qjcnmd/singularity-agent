# Policy / Approval Gates模块数据流

模块数据流文档 ID: policy-approval-gates

源码证据路径:
- src/singularity/policy/models.py
- src/singularity/policy/engine.py
- src/singularity/policy/approval.py
- src/singularity/policy/audit.py

关键符号:
- PolicyRequest
- PolicyDecision
- ApprovalGrant
- PolicyAuditEntry
- PolicyEngine
- ApprovalGate

字段清单:
- PolicySubject: subject_type, name
- ResourceRef: resource_type, identifier, normalized_identifier, workspace_relative, sensitive, metadata
- PolicyConstraints: filesystem_mode, network_allowed, max_duration_seconds, max_output_chars, allowed_paths, denied_paths, allowed_hosts, env_redaction, sandbox_required, hard_isolation_required
- PolicyRequest: session_id, task_id, phase_id, action_id, component, operation, capability, subject, resource, reason, request_id, proposed_by_model, risk_tags, metadata, evidence_refs, reversible, requires_network, touches_workspace, touches_secrets, destructive, long_running, interactive, workspace_root
- ApprovalScope: capabilities, path_globs, command_patterns, network_hosts, max_duration_seconds, max_files, session_only, single_use
- ApprovalRequirement: message, scope, review_kind, details
- ApprovalGrant: decision_id, request_id, approved_by, scope, session_id, approved_at, grant_id, expires_at, single_use, reason, operator_signature
- PolicyDecision: request_id, outcome, reason, risk_level, risk_tags, user_message, constraints, required_approval, rule_ids, audit_severity, context_summary, decision_id, approval_grant_id
- PolicyAuditEntry: timestamp, session_id, task_id, phase_id, action_id, request_id, decision_id, component, operation, capability, resource_summary, normalized_input_hash, risk_level, risk_tags, outcome, rule_ids, reason, approval_required, approval_grant_id, approved_by_user, user_decision, constraints, execution_result_ref

## 这一层解决什么问题

Policy 层把组件、能力、资源、风险、约束和人工 approval 统一成可审计决策，保护文件、命令、网络、secret 和高风险操作。

## 当前源码位置

- src/singularity/policy/models.py
- src/singularity/policy/engine.py
- src/singularity/policy/approval.py
- src/singularity/policy/audit.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

`ToolExecutor` / `CommandExecutor` / `WorkspaceMutationManager` / `VerificationRunner` 创建 `PolicyRequest` -> `PolicyEngine.evaluate()` -> `ApprovalGate` 可选人工授权 -> ledger/audit -> 执行或拒绝。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 时模型请求写文件或跑命令为例：`ToolExecutor._policy_request()` / `CommandExecutor._policy_request()` / `VerificationRunner._policy_request()` -> `PolicyEngine.evaluate()` -> `ApprovalGate.resolve()` 先生成对象 `PolicyRequest`，再读取 subject、resource、risk 和 constraints 返回 `PolicyDecision`。若 decision 是 review，`ApprovalGate.resolve()` 读取或写入 `approval_grants.jsonl` 并生成 `ApprovalGrant`；随后执行器只在 grant 范围匹配时继续。`PolicyAuditWriter.append()` 写入 `audit.jsonl`，`PolicyEngine._emit_policy_trace()` 写 trace event；deny/ask_user/sandbox_required 返回失败或阻塞结果，不会让 handler 继续执行。
## 真实对象完整结构

### PolicyRequest（策略请求）

执行组件发起的能力/资源评估请求。**边界**：内部治理对象，进入 policy audit ledger；不进入模型请求。

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

### PolicyDecision（策略决策）

policy engine 的评估结果。**边界**：内部治理对象，落盘到 audit.jsonl；`outcome`/`reason`/`constraints` 投影进执行器决策，裁剪后 observation 进入 context。

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

### PolicyAuditEntry（策略审计条目）

每次 policy 评估的不可变审计记录。**边界**：audit 对象，落盘到 policy audit JSONL；不进入模型、不写 trace events.jsonl。

```python
@dataclass(frozen=True)
class PolicyAuditEntry:
    timestamp: str
    session_id: str
    task_id: str
    phase_id: str
    action_id: str
    request_id: str
    decision_id: str
    component: PolicyComponent | str
    operation: OperationKind | str
    capability: Capability | str
    resource_summary: str
    normalized_input_hash: str
    risk_level: RiskLevel | str
    risk_tags: list[RiskTag | str]
    outcome: DecisionOutcome | str
    rule_ids: list[str]
    reason: str
    approval_required: bool
    approval_grant_id: str | None = None
    approved_by_user: bool = False
    user_decision: str | None = None
    constraints: dict[str, Any] = field(default_factory=dict)
    execution_result_ref: str | None = None
```

### 关键枚举值域

```python
class DecisionOutcome(str, Enum):    # PolicyDecision.outcome
    ALLOW = "allow"
    DENY = "deny"
    REQUIRE_REVIEW = "require_review"
    ASK_USER = "ask_user"
    ESCALATE = "escalate"
    SANDBOX_REQUIRED = "sandbox_required"

class RiskLevel(str, Enum):          # PolicyDecision.risk_level
    NONE = "none"
    LOW = "low"
    MEDIUM = "medium"
    HIGH = "high"
    CRITICAL = "critical"

class PolicyComponent(str, Enum):    # PolicyRequest.component
    TOOL = "tool"
    MUTATION = "mutation"
    COMMAND = "command"
    VERIFICATION = "verification"
    PLANNER = "planner"
    WORKSPACE_STATE = "workspace_state"
    SYSTEM = "system"

class Capability(str, Enum):         # PolicyRequest.capability (20 members)
    READ_WORKSPACE = "read_workspace"
    READ_OUTSIDE_WORKSPACE = "read_outside_workspace"
    READ_SECRET = "read_secret"
    LIST_DIRECTORY = "list_directory"
    MUTATE_WORKSPACE = "mutate_workspace"
    CREATE_FILE = "create_file"
    DELETE_FILE = "delete_file"
    MOVE_FILE = "move_file"
    ROLLBACK_MUTATION = "rollback_mutation"
    EXECUTE_COMMAND = "execute_command"
    EXECUTE_PROJECT_CODE = "execute_project_code"
    EXECUTE_GENERATED_CODE = "execute_generated_code"
    NETWORK_ACCESS = "network_access"
    PACKAGE_INSTALL = "package_install"
    PACKAGE_SCRIPT = "package_script"
    START_LONG_PROCESS = "start_long_process"
    KILL_PROCESS = "kill_process"
    READ_ENV = "read_env"
    WRITE_ENV = "write_env"
    CHANGE_AGENT_CONFIG = "change_agent_config"

class RiskTag(str, Enum):            # PolicyRequest.risk_tags (19 members)
    WORKSPACE_READ = "workspace_read"
    OUTSIDE_WORKSPACE = "outside_workspace"
    SECRET_ACCESS = "secret_access"
    MUTATES_FILES = "mutates_files"
    MUTATES_CONFIG = "mutates_config"
    MUTATES_LOCKFILE = "mutates_lockfile"
    DESTRUCTIVE = "destructive"
    IRREVERSIBLE = "irreversible"
    EXECUTES_CODE = "executes_code"
    EXECUTES_PROJECT_CODE = "executes_project_code"
    EXECUTES_GENERATED_CODE = "executes_generated_code"
    SHELL_EXPANSION = "shell_expansion"
    NETWORK = "network"
    PACKAGE_MANAGER = "package_manager"
    SUPPLY_CHAIN = "supply_chain"
    LONG_RUNNING = "long_running"
    RESOURCE_HEAVY = "resource_heavy"
    PERSISTENT_SIDE_EFFECT = "persistent_side_effect"
    SECRETS_EXFILTRATION = "secrets_exfiltration"
```

### 数据流概述

`ToolExecutor._policy_request()` / `CommandExecutor._policy_request()` / `VerificationRunner._policy_request()` 生成 `PolicyRequest`。`PolicyEngine.evaluate()` 读取 rules 返回 `PolicyDecision`。若 outcome 是 `REQUIRE_REVIEW`，`ApprovalGate.resolve()` 读取或写入 `approval_grants.jsonl` 并生成 `ApprovalGrant`。`PolicyAuditWriter.append()` 将 `PolicyAuditEntry` 写入 audit JSONL。trace event 由 `PolicyEngine._emit_policy_trace()` 写 `events.jsonl`，payload 仅含 ids、operation、capability、脱敏资源、outcome/risk/rules。trace 与 audit 是两条记录，不互相替代。

## 谁生成这些对象

ToolExecutor、CommandExecutor、mutation、verification 与 plugin manager 生成 `PolicySubject`、`ResourceRef` 和 `PolicyRequest`；rules/engine 生成 `PolicyConstraints` 与 `PolicyDecision`。`PolicyDecision.review()` 生成 `ApprovalScope`/`ApprovalRequirement`，ApprovalGate 或 remote approval生成 `ApprovalGrant`，`PolicyAuditWriter.append()` 生成 `PolicyAuditEntry`。

## 谁消费这些对象

`PolicyEngine.evaluate()` 消费 request，`ApprovalGate.resolve()` 与执行器消费 decision/requirement/grant。完整 request/decision不进入模型；`ContextManager.add_policy_observation()` 最多追加裁剪后的 policy reason/outcome observation。

## 是否落盘

ApprovalGate 将 grant 写受信任 policy home 的 `approval_grants.jsonl`并记录 single-use消费；workspace内 grant store不受信任。`PolicyAuditWriter` 将 `PolicyAuditEntry` 追加到 policy audit JSONL；subject/resource/constraints作为 request/decision/audit嵌套字段。

## 是否进入 trace / audit

PolicyEngine 发出 `policy_requested`、`policy_decided`、`policy_blocked`，payload仅含 ids、operation、capability、脱敏资源、outcome/risk/rules。Audit entry另保存normalized input hash、grant/result ref；trace与audit是两条记录，不互相替代。

## 失败路径

DecisionOutcome可为 deny、require_review、ask_user、escalate、sandbox_required；非交互 review必须fail-closed转deny。ApprovalGate对拒绝、缺grant、过期/签名/范围不匹配抛对应 approval错误，single-use grant消费后不可复用。

## 当前结构问题

policy decision、approval grant、trace event和audit entry各有不同敏感度与持久化目的；新增constraint或scope字段时必须同步matching、签名/audit与执行器消费。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
