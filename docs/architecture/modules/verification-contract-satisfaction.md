# Verification / Contract Satisfaction模块数据流

模块数据流文档 ID: verification-contract-satisfaction

源码证据路径:
- src/singularity/verification/models.py
- src/singularity/verification/contract.py
- src/singularity/verification/runner.py
- src/singularity/verification/satisfaction.py
- src/singularity/verification/assessor.py
- src/singularity/tools/verification.py
- src/singularity/planner/engine.py

关键符号:
- VerificationCheck
- VerificationResult
- VerificationEvidence
- CompletionAssessment
- VerificationStep
- VerificationContract
- StepEvidence
- ContractSatisfaction
- CompletionAssessor
- VerificationRunner

字段清单:
- VerificationCheck: kind, command, scope, required, timeout, risk_tags, failure_policy, id, policy_decision, policy_reasons, skip_reason, source, contract_step_id
- VerificationResult: check_id, kind, status, failure_type, evidence, repair_hints, confidence_impact, duration_ms, attempts, policy_decision
- VerificationEvidence: command_id, command, exit_code, output_excerpt, artifact_path, parsed_failures, duration_ms, timestamp, stdout_excerpt, stderr_excerpt, sandbox_id, sandbox_backend, sandbox_status, sandbox_artifacts, sandbox_changed_files, sandbox_violations, sandbox_enforcement, enforcement_status, execution_backend, fallback_used, fallback_reason, elevated_available, elevated_blocker_summary, network_denied_verified, process_tree_kill, job_killed, timeout_enforced, capability_summary
- CompletionAssessment: status, confidence, passed_checks, failed_checks, skipped_checks, warnings, remaining_risks
- VerificationStep: step_id, command, kind, required
- VerificationContract: contract_id, steps, status, validation_errors
- StepEvidence: step_id, check_id, command_id, status, artifact_ref
- ContractSatisfaction: contract_id, satisfied, completed_steps, failed_steps, skipped_steps, reason, step_evidence

## 这一层解决什么问题

Verification 层发现、计划并执行验证命令，把命令结果转换为 `VerificationResult（验证结果）` 和 completion assessment，同时约束 repair contract 的允许命令。

## 当前源码位置

- src/singularity/verification/models.py
- src/singularity/verification/contract.py
- src/singularity/verification/runner.py
- src/singularity/verification/satisfaction.py
- src/singularity/verification/assessor.py
- src/singularity/tools/verification.py
- src/singularity/planner/engine.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

`run_verification` tool -> `VerificationToolHandlers.run_verification()` -> `VerificationRunner.plan_verification()` / `run_plan()` -> `CommandExecutor.run()` -> parsers -> `CompletionAssessor.assess()` -> planner evidence -> `Planner.assess_verification_contract_satisfaction()` -> `AgentLoop._attempt_finalize()`。

`VerificationRunner.plan_verification()` 内的 `VerificationPolicy.evaluate()` 只处理 required check 没有 command 这类 verification 结构 blocker，并用 `CommandPolicy.classify()` 给可执行 check 补充风险标签；它不再调用 `CommandPolicy.evaluate()`，也不产生最终 command allow/deny/review 裁决。可执行验证命令的最终策略结果来自 `VerificationRunner._run_check()` 和 `CommandExecutor.run()` 中的 `PolicyEngine.enforce()`。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 后执行验证为例：`VerificationToolHandlers.run_verification()` -> `VerificationRunner.plan_verification()` 先生成对象 `VerificationCheck` 和 verification plan，plan-time preflight 只把无 executable command 的 required check 放入 blocked_checks，并把 command 分类标签写回 `VerificationCheck.risk_tags`。`VerificationRunner.run_plan()` 对可执行 check 先调用 `_policy_request()` / `PolicyEngine.enforce()` 记录 verification-level policy，再调用 `CommandExecutor.run()` 让同一个 `PolicyEngine` 对实际 command 执行边界裁决，随后生成 `VerificationResult`。`CompletionAssessor.assess()` 消费 result 列表返回 `CompletionAssessment`，同时 tool observation 写入 `context.sqlite3`，planner evidence 写入 `.singularity/planner/<session_id>/evidence.json`。repair contract 场景中，`Planner.assess_verification_contract_satisfaction()` 读取 `VerificationContract` 和 evidence，生成 `StepEvidence` / `ContractSatisfaction`；failed、blocked、skipped 或 command 不在 contract 时返回 `satisfied=False` 并阻止 `AgentLoop._attempt_finalize()` 完成。

## 真实对象完整结构

### VerificationCheck（验证检查）

描述要执行或跳过的验证动作。**边界**：内部治理对象，不落盘为独立文件；每个 check 产生 `PolicyRequest`/`PolicyDecision` 进入 policy audit。

```python
@dataclass
class VerificationCheck:
    kind: CheckKind
    command: CommandRequest | None
    scope: str
    required: bool
    timeout: float
    risk_tags: list[str]
    failure_policy: str
    id: str = field(default_factory=lambda: f"check_{uuid4().hex[:12]}")
    policy_decision: VerificationDecision | None = None
    policy_reasons: list[str] = field(default_factory=list)
    skip_reason: str | None = None
    source: str | None = None
    contract_step_id: str | None = None
```

### VerificationResult（验证结果）

单个 check 的执行结果。**边界**：内部治理对象，写入 planner `evidence.json` 和 `context.sqlite3`；安全投影写 trace event。

```python
@dataclass(frozen=True)
class VerificationResult:
    check_id: str
    kind: CheckKind
    status: CheckStatus
    failure_type: FailureType | None
    evidence: VerificationEvidence
    repair_hints: list[RepairHint]
    confidence_impact: float
    duration_ms: int
    attempts: list[VerificationEvidence] = field(default_factory=list)
    policy_decision: CommandPolicyResult | None = None
```

### VerificationEvidence（验证证据）

单次验证命令的安全证据投影。**边界**：由 `VerificationRunner._result_from_command()` 从 `CommandResult` 生成，写入 verification result、planner evidence 和 trace 安全摘要；长输出只通过 artifact ref 关联。sandbox 字段来自 command metadata，用于区分 `windows_elevated`、`windows_unelevated` 与 `local_process`，并保留降级原因摘要。

```python
@dataclass(frozen=True)
class VerificationEvidence:
    command_id: str | None
    command: str | None
    exit_code: int | None
    output_excerpt: str
    artifact_path: str | None
    parsed_failures: list[ParsedFailure]
    duration_ms: int
    timestamp: str
    stdout_excerpt: str = ""
    stderr_excerpt: str = ""
    sandbox_id: str | None = None
    sandbox_backend: str | None = None
    sandbox_status: str | None = None
    sandbox_artifacts: list[dict[str, Any]] = field(default_factory=list)
    sandbox_changed_files: dict[str, Any] = field(default_factory=dict)
    sandbox_violations: list[dict[str, Any]] = field(default_factory=list)
    sandbox_enforcement: str | None = None
    enforcement_status: str | None = None
    execution_backend: str | None = None
    fallback_used: bool | None = None
    fallback_reason: str | None = None
    elevated_available: bool | None = None
    elevated_blocker_summary: str | None = None
    network_denied_verified: bool | None = None
    process_tree_kill: bool | None = None
    job_killed: bool | None = None
    timeout_enforced: bool | None = None
    capability_summary: dict[str, Any] = field(default_factory=dict)
```

### ContractSatisfaction（契约满足度）

repair contract 的兑现评估。**边界**：内部治理对象，嵌入 planner evidence/final report；不独立落盘。

```python
@dataclass(frozen=True)
class ContractSatisfaction:
    contract_id: str
    satisfied: bool
    completed_steps: list[str]
    failed_steps: list[str]
    skipped_steps: list[str]
    reason: str | None = None
    step_evidence: list[StepEvidence] = field(default_factory=list)
```

### 关键枚举值域

```python
class CheckKind(str, Enum):          # VerificationCheck.kind
    SYNTAX = "syntax"
    FORMAT = "format"
    LINT = "lint"
    TYPECHECK = "typecheck"
    UNIT_TEST = "unit_test"
    INTEGRATION_TEST = "integration_test"
    BUILD = "build"
    VERIFICATION_SMOKE = "verification_smoke"
    SECURITY = "security"
    CUSTOM = "custom"
    MANUAL_REVIEW = "manual_review"

class CheckStatus(str, Enum):        # VerificationResult.status
    PASSED = "passed"
    FAILED = "failed"
    SKIPPED = "skipped"
    BLOCKED = "blocked"
    FLAKY = "flaky"
    TIMEOUT = "timeout"
    INCONCLUSIVE = "inconclusive"

class FailureType(str, Enum):        # VerificationResult.failure_type (28 members)
    PROJECT_PROFILE_UNKNOWN = "project_profile_unknown"
    COMMAND_DISCOVERY_FAILED = "command_discovery_failed"
    VERIFICATION_PLAN_FAILED = "verification_plan_failed"
    CHECK_POLICY_DENIED = "check_policy_denied"
    CHECK_REVIEW_REQUIRED = "check_review_required"
    CHECK_BLOCKED = "check_blocked"
    COMMAND_EXECUTION_FAILED = "command_execution_failed"
    OUTPUT_PARSE_FAILED = "output_parse_failed"
    SYNTAX_ERROR = "syntax_error"
    TYPE_ERROR = "type_error"
    LINT_ERROR = "lint_error"
    FORMAT_ERROR = "format_error"
    UNIT_TEST_FAILURE = "unit_test_failure"
    INTEGRATION_TEST_FAILURE = "integration_test_failure"
    BUILD_FAILURE = "build_failure"
    MISSING_DEPENDENCY = "missing_dependency"
    MISSING_COMMAND = "missing_command"
    ENVIRONMENT_ERROR = "environment_error"
    CONFIGURATION_ERROR = "configuration_error"
    TIMEOUT = "timeout"
    FLAKY_FAILURE = "flaky_failure"
    EXTERNAL_SERVICE_UNAVAILABLE = "external_service_unavailable"
    PERMISSION_DENIED = "permission_denied"
    SANDBOX_LIMITATION = "sandbox_limitation"
    SANDBOX_VIOLATION = "sandbox_violation"
    INCONCLUSIVE_RESULT = "inconclusive_result"
    REPAIR_BUDGET_EXCEEDED = "repair_budget_exceeded"
    UNKNOWN_FAILURE = "unknown_failure"

class CompletionStatus(str, Enum):   # CompletionAssessment.status
    READY = "ready"
    READY_WITH_WARNINGS = "ready_with_warnings"
    BLOCKED = "blocked"
    FAILED = "failed"
    NEEDS_REVIEW = "needs_review"
```

### 数据流概述

`VerificationRunner.plan_verification()` 生成 `VerificationCheck` 列表，`VerificationPolicy.evaluate()` 只做 plan-time 结构检查和风险标签补充。`run_plan()` 调用 `PolicyEngine.enforce()` 做 verification-level preflight，再调用 `CommandExecutor.run()` 执行命令生成 `VerificationResult`。`CompletionAssessor.assess()` 消费 result 列表返回 `CompletionAssessment`。`VerificationContract.from_plan_strings()` 生成 `VerificationContract`。`Planner.assess_verification_contract_satisfaction()` 读取 contract 和 evidence 生成 `StepEvidence` 和 `ContractSatisfaction`。可执行 check 的 `PolicyRequest`/`PolicyDecision` 在执行阶段进入 policy audit ledger。

`VerificationRunner._result_from_command()` 对命令输出先走 `FailureParserRegistry.parse()`，再由 `classify_failure()` 将 sandbox backend unavailable归为`sandbox_limitation`、sandbox violation归为`sandbox_violation`、timeout归为`timeout`、missing command归为`missing_command`。它同时把 `CommandResult.metadata` 中的 `sandbox_enforcement`、`enforcement_status`、`execution_backend`、`fallback_used`、`fallback_reason`、`elevated_available` 和 `elevated_blocker_summary` 写入 `VerificationEvidence`，因此 verification evidence 能区分严格 elevated sandbox、降级 unelevated sandbox 和 relaxed local process。Python DLL/import初始化类环境问题（例如 `ImportError: DLL load failed while importing _ssl`、`_hashlib`、`_socket`、`libssl/libcrypto`缺失、OpenSSL provider/config不可读、证书路径不可读、DLL search path失败或 DLL initialization routine failed）优先归为`environment_error`，状态为`blocked`，不会按pytest普通失败进入代码修复。`environment_error`、`sandbox_limitation`和`sandbox_violation`不生成普通`repair_hints`；调用方必须把它们作为环境/沙箱 blocker 处理，而不是让模型修改业务代码。

benchmark/public task 的 AgentLoop 内 smoke command 来自 Planner `contract_smoke_commands()`；Planner 已把 manifest 可见的 `verification_command` 收敛为唯一 required requirement。benchmark constraint 存在时，这组 contract smoke 优先于模型传给 `run_verification` 的 `smoke_commands`，因此模型不能在 tool 参数中追加 broad pytest；普通非 benchmark task 仍保留显式 smoke 的原有优先级。语法检查仍可作为独立 syntax check 生成；仅当 manifest smoke 与自动 syntax 都是 Python-like executable、精确 `-m py_compile` 且规范化文件参数完全一致时，runner 保留 manifest smoke 并跳过自动 syntax。文件子集、pytest、`-c`、不同命令或不同参数不去重，也不把 evaluator-owned public/hidden verification 注入 AgentLoop。

## 谁生成这些对象

- `VerificationRunner._check()` 生成 `VerificationCheck`；plan-time 结构 blocker、执行、skipped、budget 与 policy 分支生成 `VerificationResult`。`CompletionAssessor.assess()` 从 verification plan 和 result 列表生成 `CompletionAssessment`。
- `VerificationContract.from_plan_strings()` 或 Planner 的 benchmark/repair contract 路径生成 `VerificationStep` 与 `VerificationContract`；contract validation errors 在构造时确定。
- `Planner.assess_verification_contract_satisfaction()` 将 contract step 与 evidence 对齐，逐步生成 `StepEvidence`，再生成 `ContractSatisfaction`；这不是 `VerificationRunner` 的输出。

## 谁消费这些对象

- verification planner、`PolicyEngine`、`CommandExecutor` 消费 `VerificationCheck`；assessor、failure analysis 和 Planner 消费 `VerificationResult`。`run_verification` tool 将安全 result/assessment 投影写入 context，因此下一轮模型可见该投影，而不是内部 command/policy 对象。
- `AgentLoop._attempt_finalize()` 通过 Planner completion state 消费 `CompletionAssessment` 的结论；assessment 本体也进入 verification tool result 与 planner `final_assessment`。
- Planner tool authorization、`VerificationRunner.plan_verification()` 和 contract satisfaction 消费 `VerificationContract`/`VerificationStep`。repair planner context 可把 contract 摘要送入模型；`StepEvidence`/`ContractSatisfaction` 用于 completion/report，不直接作为 provider message。

## 是否落盘

- verification check/result/assessment 作为 tool result 与 planner evidence 写 `.singularity/planner/<session_id>/evidence.json`；相关 tool observation/message 另写当前 trace run 的 `context.sqlite3`。
- `VerificationContract`、`VerificationStep`、`StepEvidence` 和 `ContractSatisfaction` 嵌入 failure analysis、repair plan、planner evidence/final report 的 JSON，不设独立 store。命令长输出使用 `CommandResult.artifact_path` 指向 trace artifact。
- evaluation runner 的独立 public/hidden verification 使用自己的 `CommandEvalResult` 和 evaluation report，不应与本层 `VerificationResult` store 混称。

## 是否进入 trace / audit

- verification runner 记录 plan、check/result、`verification.evidence_recorded` 与 completion assessment 摘要；legacy `verification` record 的 payload 来自 result/assessment 的安全 `to_dict()` 投影，并关联 verification/command ids 与 artifact refs。
- 每个可执行检查在 `_run_check()` 产生 verification-level `PolicyRequest`/`PolicyDecision`，decision 进入 policy audit ledger；随后实际 command 再由 `CommandExecutor.run()` 产生 command-level `PolicyRequest`/`PolicyDecision`。blocked/denied check 的 `policy_decision` 和 reasons 同时进入 `VerificationCheck`/`VerificationResult`。
- Planner 对 contract satisfaction 的结果进入 planner event/final report；不存在单独的 `VerificationRunner.contract_satisfaction` recorder。

## 失败路径

- `VerificationResult.status` 区分 passed、failed、timeout、blocked、skipped、flaky；plan-time missing command、execution-time policy、budget 与 parser failure分别由 helper 生成对应 result，而不是抛弃该 check。
- Python runtime DLL/import初始化失败在`classify_failure()`中归为`FailureType.ENVIRONMENT_ERROR`，`VerificationRunner._status_from_command()`把它归为`blocked`，并且`_result_from_command()`不给这类环境/沙箱失败生成普通代码 repair hints。该路径覆盖 `_ssl.pyd`、`libssl/libcrypto`、OpenSSL provider/config、证书路径、DLL search path与low-integrity初始化失败，避免 runtime blocker 被误认为 `unit_test_failure` 后进入 repair loop。
- `CompletionAssessor` 对 required failure 返回 `failed`，required blocked 返回 `blocked`，缺结果/高风险人工复核返回 `needs_review`，warning/flaky 返回 `ready_with_warnings`，全部满足才是 `ready`。
- contract 的 invalid/pending、missing step、command 不在 contract 或 step evidence failed/skipped 会使 `ContractSatisfaction.satisfied=False` 并给出 reason；Planner completion gate据此拒绝完成或进入 repair。

## 当前结构问题

执行验证与契约满足度是两个边界：`VerificationRunner.run_plan()` 产生命令证据，`Planner.assess_verification_contract_satisfaction()` 判断 repair contract 是否兑现；维护时必须同步这两条链的字段和失败语义。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
