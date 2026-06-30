# Failure Analysis / Repair模块数据流

模块数据流文档 ID: failure-analysis-repair

源码证据路径:
- src/singularity/failure_analysis/request.py
- src/singularity/failure_analysis/result.py
- src/singularity/failure_analysis/analyzer.py
- src/singularity/verification/failure_analysis.py
- src/singularity/verification/runner.py
- src/singularity/repair/contract.py
- src/singularity/repair/plan.py
- src/singularity/repair/planner.py
- src/singularity/repair/signal.py

关键符号:
- FailureAnalysisRequest
- FailureAnalysisResult
- RepairContract
- RepairActionCandidate
- RepairPlan
- RepairReplanSignal
- FailureAnalyzer
- FailureAnalysisPipeline
- VerificationRunner
- RepairPlanner

字段清单:
- FailureAnalysisRequest: request_id, run_id, session_id, task_id, phase_id, workspace_root, failure_source, failure_summary, failure_sources, context_references, recent_tail, verification_log_refs, changed_files, evidence_refs, metadata, risk_points, repair_policy, verification_strategies
- FailureAnalysisResult: analysis_id, request_id, root_cause, failure_category, affected_files, evidence_refs, repair_strategy, next_actions, verification_plan, confidence, needs_user_input, blocked_reason, raw_response_ref, verification_contract
- RepairContract: contract_id, analysis_id, failure_category, target_files, evidence_refs, action_candidates, verification_plan, confidence, allowed_tool_names, needs_user_input, blocked_reason, validation_errors, verification_contract
- RepairActionCandidate: candidate_id, action_type, target_file, rationale, tool_hints, verification_ref, confidence
- RepairPlan: plan_id, analysis_id, strategy, summary, action_candidates, next_actions, verification_plan, evidence_refs, confidence, needs_user_input, blocked_reason, repair_contract, verification_contract
- RepairReplanSignal: signal_id, repair_plan_id, analysis_id, contract_id, failure_fingerprint, failure_category, target_files, action_candidates, verification_plan, confidence, needs_user_input, blocked_reason, repair_contract, error_code, verification_failed, verification_contract

## 这一层解决什么问题

失败分析与修复层把失败证据转换为根因、修复计划、修复契约和 replanner signal，避免模型在同一失败上盲目循环。

顶层 `repair/plan.py` / `repair/planner.py` 的 `RepairPlan` / `RepairPlanner` 只表示 AgentLoop failure repair。`diagnostics` 子系统的本地 doctor/repair 输出命名为 `DiagnosticRepairResult`，不是本层 repair plan，也不从 `singularity.diagnostics` 重新导出旧 `RepairPlan` 名称。

## 当前源码位置

- src/singularity/failure_analysis/request.py
- src/singularity/failure_analysis/result.py
- src/singularity/failure_analysis/analyzer.py
- src/singularity/verification/failure_analysis.py
- src/singularity/verification/runner.py
- src/singularity/repair/contract.py
- src/singularity/repair/plan.py
- src/singularity/repair/planner.py
- src/singularity/repair/signal.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

`AgentLoop._maybe_analyze_failure()` -> `FailureAnalysisRequest.from_planner()` -> `FailureAnalyzer.analyze()` -> `RepairPlanner.plan()` -> `RepairPlanner.to_replan_signal()` -> `Planner.record_failure_analysis()` -> `Planner.replan()`。验证执行路径中，`VerificationRunner` 调用 `FailureAnalysisPipeline.analyze_results()` 把 failed/blocked verification result 转成同一个顶层 `FailureAnalysisResult`，再交给顶层 `RepairPlanner.plan()`；verification 包不再定义自己的 `RepairPlanner` 或 `RepairPlan`。diagnostics doctor/repair 路径返回 `DiagnosticRepairResult`，只描述本地配置/文件系统修复动作，不进入 AgentLoop failure repair 或 replanner signal。 当 request 的结构化 failure source/evidence 表明 `sandbox_limitation` 且 sandbox/enforcement/backend status 为 `backend_unavailable` 时，`FailureAnalyzer.analyze()` 在模型调用前直接生成 blocked `FailureAnalysisResult`，不把测试文件作为 repair target。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 后验证失败为例：`AgentLoop._maybe_analyze_failure()` -> `FailureAnalysisRequest.from_planner()` 先从 planner evidence、recent tail、changed files 和 outcome 生成对象 `FailureAnalysisRequest`。`FailureAnalyzer.analyze()` 只把 `request.to_model_payload()` 的有界证据发给分析模型，返回 payload 经 `FailureAnalysisResult.from_model_payload()` 生成结果；`RepairPlanner.plan()` 再生成 `RepairContract`、`RepairActionCandidate` 和 `RepairPlan`。`RepairPlanner.to_replan_signal()` 生成 `RepairReplanSignal` 后由 `Planner.record_failure_analysis()` 写入 planner evidence、`planner_events.jsonl`、context item 和 trace；若需要用户输入，`RepairPlanner.blocked_outcome()` 返回 blocked outcome。验证 runner 内部的失败解析路径不再生成独立 repair plan：`FailureAnalysisPipeline.analyze_result()` 只根据 parsed failures、changed files、verification command 和 no-progress guard 生成顶层 `FailureAnalysisResult`，`VerificationRunner.run_plan()` / `run_existing_plan()` 再调用顶层 `RepairPlanner.plan(analyses[0])`。sandbox backend unavailable 属于基础设施 blocker，不进入普通代码修复对象流；对应 blocked result 的 `affected_files=[]`、`verification_plan=[]`、`failure_category="sandbox_limitation"`、`blocked_reason` 指向 sandbox backend unavailable。

## 真实对象完整结构

### FailureAnalysisRequest（失败分析请求）

从 planner evidence 和当前 outcome 构造的分析请求。**边界**：内部治理对象，不落盘为独立文件；`to_model_payload()` 的有界证据发送给 failure-analysis 模型。

```python
@dataclass(frozen=True)
class FailureAnalysisRequest:
    request_id: str
    run_id: str
    session_id: str
    task_id: str
    phase_id: str
    workspace_root: str
    failure_source: str
    failure_summary: str
    failure_sources: list[dict[str, Any]]
    context_references: list[str] = field(default_factory=list)
    recent_tail: list[dict[str, Any]] = field(default_factory=list)
    verification_log_refs: list[str] = field(default_factory=list)
    changed_files: list[str] = field(default_factory=list)
    evidence_refs: list[str] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)
    risk_points: list[dict[str, Any]] = field(default_factory=list)
    repair_policy: dict[str, Any] | None = None
    verification_strategies: list[dict[str, Any]] = field(default_factory=list)
```

### FailureAnalysisResult（失败分析结果）

failure-analysis 模型返回的结构化结果。**边界**：内部治理对象，写入 planner `evidence.json`；安全投影写 trace event 和 context failure item。

```python
@dataclass(frozen=True)
class FailureAnalysisResult:
    analysis_id: str
    request_id: str
    root_cause: str                     # to_dict() 投影为 {description, evidence, confidence}
    failure_category: str
    affected_files: list[str]
    evidence_refs: list[str]
    repair_strategy: str
    next_actions: list[str]
    verification_plan: list[str]
    confidence: float
    needs_user_input: bool
    blocked_reason: str | None = None
    raw_response_ref: str | None = None
    verification_contract: VerificationContract = field(default_factory=VerificationContract.empty)
```

### RepairContract（修复契约）

修复阶段的授权边界。**边界**：内部治理对象，写入 planner `evidence.json`；约束 target files、allowed tools 和 verification。

```python
@dataclass(frozen=True)
class RepairContract:
    contract_id: str
    analysis_id: str
    failure_category: str
    target_files: list[str]
    evidence_refs: list[str]
    action_candidates: list[RepairActionCandidate]
    verification_plan: list[str]
    confidence: float
    allowed_tool_names: list[str]
    needs_user_input: bool = False
    blocked_reason: str | None = None
    validation_errors: list[str] = field(default_factory=list)
    verification_contract: VerificationContract = field(default_factory=VerificationContract.empty)
```

`verification_contract` 嵌套来自 verification 模块的 `VerificationContract`（`contract_id`、`steps: list[VerificationStep]`、`status`、`validation_errors`），约束 repair 阶段允许的验证命令。

### 关键状态值域

```python
# FailureAnalysisResult.failure_category 由模型返回，常见值:
LOGIC_ERROR = "logic_error"
SYNTAX_ERROR = "syntax_error"
TYPE_ERROR = "type_error"
TEST_FAILURE = "test_failure"
BUILD_FAILURE = "build_failure"
DEPENDENCY_ISSUE = "dependency_issue"
CONFIG_ERROR = "config_error"
ENVIRONMENT_ISSUE = "environment_issue"

# RepairActionCandidate.action_type 取值:
EDIT_FILE = "edit_file"
CREATE_FILE = "create_file"
DELETE_FILE = "delete_file"
RUN_COMMAND = "run_command"
READ_FILE = "read_file"
```

### 数据流概述

`AgentLoop._maybe_analyze_failure()` 调用 `FailureAnalysisRequest.from_planner()` 从 planner evidence 生成 request。`FailureAnalyzer.analyze()` 先检查结构化 sandbox blocker；非 sandbox backend blocker 才把 `request.to_model_payload()` 发送给 failure-analysis 模型，返回 payload 经 `FailureAnalysisResult.from_model_payload()` 生成结果。`RepairPlanner.plan()` 生成 `RepairContract`、`RepairActionCandidate` 和 `RepairPlan`。`RepairPlanner.to_replan_signal()` 生成 `RepairReplanSignal`，由 `Planner.record_failure_analysis()` 写入 planner evidence、`planner_events.jsonl`、context item 和 trace。

## 谁生成这些对象

- `AgentLoop._maybe_analyze_failure()` 调用 `FailureAnalysisRequest.from_planner()`，从 planner evidence、context references、recent tail、changed files 和当前 outcome 生成 request。
- `FailureAnalyzer.analyze()` 将非 sandbox-backend-blocker 的 `request.to_model_payload()` 发送给失败分析模型，成功响应由 `FailureAnalysisResult.from_model_payload()` 生成；invalid JSON、schema 或不可用模型由 `FailureAnalysisResult.blocked()` 生成明确 blocked result。`sandbox_limitation/backend_unavailable` 由 analyzer 在模型调用前直接 blocked，避免模型把验证命令里的测试文件误判为 `affected_files`。
- `FailureAnalysisPipeline.analyze_result()` 在 verification 路径中从 `VerificationResult` 生成 rule-derived `FailureAnalysisResult`；它保留 no-progress guard 和 retrieval query metadata，但不定义 repair plan 类。
- `RepairPlanner.plan()` 先用 `_action_candidates()` 生成 `RepairActionCandidate`，再由 `RepairContract.from_analysis()` / `blocked()` 与 `RepairPlan` 构造修复边界；`RepairPlanner.to_replan_signal()` 调用 `RepairReplanSignal.from_contract()` 生成 replanner 输入。
- `singularity.diagnostics.repair.RepairEngine.run()` 生成 `DiagnosticRepairResult`；该对象只被 diagnostics CLI/render 消费，不生成 `RepairReplanSignal`，也不写入 planner `repair_plans` bucket。

## 谁消费这些对象

- `FailureAnalyzer.analyze()` 消费 `FailureAnalysisRequest`；只有 `to_model_payload()` 的有界失败证据进入 failure-analysis 模型，workspace root、raw log 和完整 metadata 不直接发送。
- `RepairPlanner.plan()`、`Planner.record_failure_analysis()` 和 `ContextManager.add_failure_item()` 消费 `FailureAnalysisResult`；`RepairContract`/candidate 限定 target files、allowed tools 和 verification。`Planner.authorize_tool_call()` 与 `VerificationRunner.run_plan()` 是生产消费者，targeted replay 只是读取证据的评估消费者。`VerificationRunner` 从 `FailureAnalysisPipeline` 接收顶层 result 后只调用顶层 `RepairPlanner`，不消费 verification-local repair plan。
- `Planner.replan()` 和 planner-decision producer 消费 `RepairReplanSignal`，因此 signal 进入的是独立 replanner 模型请求；`RepairPlan`/contract 的安全摘要通过 planner context 进入后续主模型 turn。diagnostics `DiagnosticRepairResult` 由 `render_repair_plan()` 渲染给 CLI，不进入主模型 turn。

## 是否落盘

- request 不保存完整副本；requested trace 只记录 id、failure source、evidence refs 与 changed files。`FailureAnalysisResult`、`RepairActionCandidate`、`RepairContract`、`RepairPlan` 写入 `.singularity/planner/<session_id>/evidence.json` 的对应 evidence bucket。
- ContextManager 将 failure analysis/repair plan/signal 投影为 failure item 写当前 trace run 的 `context.sqlite3`；contract/candidate/verification contract 作为父对象嵌套 JSON，不另建表。
- `RepairReplanSignal` 的安全字典写 `planner_events.jsonl` 的 `replan_signal`，并由 planner recorder 投影到 observability `events.jsonl`；没有独立 repair report 文件。

## 是否进入 trace / audit

- `FailureAnalyzer._record()` 写 `failure_analysis_requested`、`failure_analysis_completed`、`failure_analysis_failed`；requested payload 是 request 摘要，completed payload 是 result 的安全投影，raw model response 只通过 `raw_response_ref` 引用。
- `RepairPlanner._record_contract_validation()` 写 `repair_contract_validation`，只含 contract/analysis id、category、target count、allowed tools 与 validation errors；`Planner._record_repair_signal_consumed()` 写 `repair_signal_consumed`。
- 本层对象本身不写 policy audit；后续候选工具/验证执行生成各自的 `PolicyRequest`/`PolicyDecision` 后，才由 policy ledger 记录实际授权结果。

## 失败路径

- 无失败、重复 fingerprint 或没有新增证据时 AgentLoop 不重复分析；failure-analysis invalid JSON/schema、低 confidence、越权 target、缺 evidence/target/action 或不可执行 verification 产生 blocked result/contract 与 `needs_user_input`/`blocked_reason`。
- `VerificationRunner` 将 `sandbox_unavailable` 和 `backend_unavailable` command error 归类为 `FailureType.SANDBOX_LIMITATION` 并标记 check `blocked`；`FailureAnalyzer` 对结构化 `sandbox_limitation/backend_unavailable` 直接返回 blocked result，`RepairPlanner` 因 `blocked_reason` 生成 blocked contract，不生成无意义 rerun verification 循环。
- `RepairActionCandidate` 校验 action type、target、tool hint、rationale 和 confidence；`RepairContract.validation_errors` 明确记录 unsupported tool、unauthorized target 与 invalid verification contract，Planner fail-closed 拒绝越权工具/路径。
- replanner 对 ask-user、repeated-failure budget、policy/sandbox/permission 类失败返回阻塞或人工输入；verification failure 保持 `verification_failed=True` 并继续受 contract 限制，不清空旧失败证据。

## 当前结构问题

`FailureAnalysisResult.root_cause` 在 dataclass 中是字符串，但 `to_dict()` 将其投影成包含 description/evidence/confidence 的对象；这是序列化边界，不应通过新增 alias 字段解决。repair candidate、contract、plan、signal 均为不同授权阶段，文档与代码不得合并成一个宽松字典。diagnostics 的 `DiagnosticRepairResult` 与顶层 `RepairPlan` 语义不同，不能通过别名或 re-export 合并。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
