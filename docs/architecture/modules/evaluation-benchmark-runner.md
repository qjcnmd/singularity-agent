# Evaluation Benchmark Runner模块数据流

模块数据流文档 ID: evaluation-benchmark-runner

源码证据路径:
- src/singularity/evaluation/runner.py
- src/singularity/evaluation/failure_case_replay.py
- src/singularity/evaluation/targeted_replay.py
- src/singularity/cli.py

关键符号:
- EvaluationWorkspace
- EvaluationTask
- EvaluationTaskSet
- CommandEvalResult
- EvaluationTaskResult
- EvaluationRunner
- TargetedFailureReplayResult

字段清单:
- EvaluationWorkspace: kind, path, files, start_commit
- EvaluationTask: task_id, workspace, user_task, allowed_paths, verification_command, success, task_type, description, allowed_tools, tool_policy, strategy, expected_file_changes, completion_standard, risk_tags, prepare_commands, public_verification_command, hidden_verification_command, verification_prepare_commands, verification_timeout_seconds
- EvaluationTaskSet: tasks, base_dir, schema_version
- CommandEvalResult: command, exit_code, duration_seconds, timed_out, error_summary, raw_command, resolved_argv, interpreter_strategy, failure_category
- EvaluationTaskResult: task_id, tests_passed, infrastructure_blocked, prompt_tokens, cached_tokens, request_cache_hit_rate, run_cache_hit_rate, tool_calls, files_changed, duration_seconds, error_summary, workspace, trace, verification_workspace, patch, checks, verification, agent_completed, evaluation_passed, patch_applicable, allowed_scope_passed, public_verification_passed, hidden_verification_passed, repair_attempt_count, repair_execution_count, miscompletion_count, blocked_reason, failure_category, request_cache_hit_rates, status, turn_count, verification_result, contract_satisfaction, final_report_status, policy_blocks, token_usage, cache_usage, trace_artifact_refs, reproducible_environment
- TargetedFailureReplayResult: status, agent_completed, entered_agent_loop, failure_trigger, failure_analysis_request_count, failure_analysis_result_count, repair_plan_count, repair_contract_count, repair_attempt_count, repair_execution_count, repairing_failures_seen, verification_contract_satisfaction, repair_scope, final_report_status, trace_path, phase_history, planner_status_history, repair_contract_summary, repairing_failures_evidence, trace_refs, report_paths

## 这一层解决什么问题

Evaluation runner 读取 task set manifest，在隔离 workspace 中启动真实 kernel/AgentLoop，独立验证结果并写出 result/report/failure cases。

## 当前源码位置

- src/singularity/evaluation/runner.py
- src/singularity/evaluation/failure_case_replay.py
- src/singularity/evaluation/targeted_replay.py
- src/singularity/cli.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

`python -m singularity.cli eval run <manifest>` -> `EvaluationTaskSet.from_dict()` -> `EvaluationRunner.run()` -> per task `KernelBootstrap.boot()` -> `AgentKernel.run_task()` -> `AgentLoop.run()` -> verification workspace checks -> `EvaluationTaskResult.to_dict()` -> result/report artifacts。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 的 evaluation task 为例：`load_evaluation_task_set()` -> `EvaluationRunner.run()` -> `EvaluationRunner.run_task()` 先从 manifest 生成对象 `EvaluationTaskSet` 和 `EvaluationTask`，再由 `_materialize_workspace()` 创建 task `workspace`、`baseline-workspace` 和 `verification-workspace`。`KernelBootstrap.boot()` -> `AgentKernel.run_task()` -> `AgentLoop.run()` 执行真实 agent 后，runner 读取 trace summary、final report、changed files 和 public/hidden verification，生成 `EvaluationTaskResult`。汇总阶段把结果写入 `<run_dir>/result.json`、`report.json`、`report.md`，clean verification workspace 失败时写错误并返回失败结果。

capability regression task 以 `workspace-write`、`approval_policy=never`、`network_access=denied` 进入真实 `KernelBootstrap -> AgentGraphBuilder -> AgentKernel -> AgentLoop.run` 路径。AgentLoop 内的低风险验证命令由 policy 判为 `sandbox_required` 后必须进入 `CommandExecutor -> SandboxManager -> WindowsSandboxBackend`；Windows setup 或 execution backend 缺失时，task 应记录 sandbox/permission blocker，而不是把普通本地进程当作 verification 通过。

## 真实对象完整结构

### EvaluationTask（评估任务）

manifest 输入的单个评估任务定义。**边界**：输入对象，来自 manifest JSON；不进入模型请求，只有 `user_task` 等子集写入用户 goal。

```python
@dataclass(frozen=True)
class EvaluationTask:
    task_id: str
    workspace: EvaluationWorkspace
    user_task: str
    allowed_paths: list[str]
    verification_command: str
    success: dict[str, Any]
    task_type: str = ""
    description: str = ""
    allowed_tools: list[str] = field(default_factory=list)
    tool_policy: str = "read_write"
    strategy: dict[str, Any] = field(default_factory=dict)
    expected_file_changes: list[str] = field(default_factory=list)
    completion_standard: str = ""
    risk_tags: list[str] = field(default_factory=list)
    prepare_commands: list[str] = field(default_factory=list)
    public_verification_command: str = ""
    hidden_verification_command: str = ""
    verification_prepare_commands: list[str] = field(default_factory=list)
    verification_timeout_seconds: int = 120
```

### EvaluationTaskResult（评估任务结果）

单个 task 的完整评估结果。**边界**：evaluation report 对象，落盘到 `<run_dir>/result.json`；不进入模型请求。

```python
@dataclass(frozen=True)
class EvaluationTaskResult:
    task_id: str
    tests_passed: bool
    infrastructure_blocked: bool
    prompt_tokens: int
    cached_tokens: int
    request_cache_hit_rate: float
    run_cache_hit_rate: float
    tool_calls: int
    files_changed: list[str]
    duration_seconds: float
    error_summary: str
    workspace: str
    trace: str
    verification_workspace: str = ""
    patch: dict[str, Any] = field(default_factory=dict)
    checks: dict[str, Any] = field(default_factory=dict)
    verification: CommandEvalResult | None = None
    agent_completed: bool = False
    evaluation_passed: bool = False
    patch_applicable: bool = False
    allowed_scope_passed: bool = False
    public_verification_passed: bool = False
    hidden_verification_passed: bool = False
    repair_attempt_count: int = 0
    repair_execution_count: int = 0
    miscompletion_count: int = 0
    blocked_reason: str = ""
    failure_category: str = ""
    request_cache_hit_rates: dict[str, float] = field(default_factory=dict)
    status: str = "unknown"
    turn_count: int = 0
    verification_result: dict[str, Any] = field(default_factory=dict)
    contract_satisfaction: dict[str, Any] = field(default_factory=dict)
    final_report_status: str = ""
    policy_blocks: int = 0
    token_usage: dict[str, Any] = field(default_factory=dict)
    cache_usage: dict[str, Any] = field(default_factory=dict)
    trace_artifact_refs: list[str] = field(default_factory=list)
    reproducible_environment: dict[str, Any] = field(default_factory=dict)
```

### CommandEvalResult（命令评估结果）

evaluation runner 独立执行的验证命令结果。**边界**：evaluation report 对象，嵌入 `EvaluationTaskResult.verification`/`checks`；不进入模型请求。

```python
@dataclass(frozen=True)
class CommandEvalResult:
    command: str
    exit_code: int | None
    duration_seconds: float
    timed_out: bool = False
    error_summary: str = ""
    raw_command: str = ""
    resolved_argv: list[str] = field(default_factory=list)
    interpreter_strategy: dict[str, Any] = field(default_factory=dict)
    failure_category: str = ""
```

### 关键枚举/状态值域

```python
# EvaluationTaskResult.status 由 _task_result() 归一为以下值:
INFRASTRUCTURE_BLOCKED = "infrastructure_blocked"
SUCCESS = "success"
POLICY_BLOCKED = "policy_blocked"
VERIFICATION_FAILED = "verification_failed"
BLOCKED = "blocked"
FAILED = "failed"
MAX_TURNS_EXCEEDED = "max_turns_exceeded"
FAILURE = "failure"
UNKNOWN = "unknown"

# CommandEvalResult.failure_category 区分:
COMMAND_PARSE_ERROR = "command_parse_error"
COMMAND_TIMEOUT = "command_timeout"
COMMAND_NOT_FOUND = "command_not_found"
COMMAND_EXECUTION_ERROR = "command_execution_error"
ENVIRONMENT_DEPENDENCY_MISSING = "environment_dependency_missing"
VERIFICATION_FAILED = "verification_failed"
COMMAND_FAILED = "command_failed"
```

### 数据流概述

`load_evaluation_task_set()` 从 manifest JSON 生成 `EvaluationTaskSet`/`EvaluationTask`。`EvaluationRunner.run()` 遍历 task，每个 task 调用 `KernelBootstrap.boot()` -> `AgentKernel.run_task()` -> `AgentLoop.run()` 执行真实 agent。runner 读取 trace summary、final report、changed files 和 public/hidden verification，由 `_task_result()` 聚合生成 `EvaluationTaskResult`。结果写入 `<run_dir>/result.json`、`report.json`、`report.md`。canonical 完成/判定字段为 `agent_completed`、`evaluation_passed`、`miscompletion_count`，不是 manifest 的 `success`。

## 谁生成这些对象

- `EvaluationWorkspace` 由 `EvaluationWorkspace.from_dict()` 解析 manifest；private benchmark 由 `SingularityPrivateBenchmarkAdapter._convert()` 构造。`EvaluationTask.from_dict()` 组合 workspace 与 task 配置，`EvaluationTaskSet.from_dict()` 再组合完整 task set；`load_evaluation_task_set()` 负责读取 JSON 并设置 `base_dir`。
- `CommandEvalResult` 由 `_run_shell()` 针对正常退出、命令解析失败、超时、命令不存在和 OS 执行错误分别构造；仅运行 hidden verification 时，`EvaluationRunner.run_task()` 还会构造 `mode=not_run` 的 public 占位结果。
- `EvaluationTaskResult` 只由 `EvaluationRunner._task_result()` 聚合 AgentLoop 结果、public/hidden verification、patch、scope、token/cache、trace 与 reproducible environment 生成。
- `TargetedFailureReplayResult` 由 `TargetedFailureReplayRunner.run_smoke()` 从 scripted AgentLoop、planner evidence 和 JSONL trace 聚合，`run()` 再补入 JSON/Markdown `report_paths`。它用于确定性修复链回放，不等同于真实 provider evaluation。

## 谁消费这些对象

- `EvaluationRunner._materialize_workspace()` 和 `_workspace_environment()` 消费 `EvaluationWorkspace`；该对象不直接进入模型请求，只有物化后的文件可被 AgentLoop 工具读取。
- `EvaluationRunner.run_task()` 消费 `EvaluationTask`。`_task_goal()` 只把 `user_task`、`allowed_paths`、`tool_policy`、`allowed_tools`、`expected_file_changes`、`completion_standard`、`risk_tags` 和可见 verification command 写入用户 goal；hidden command、verification prepare command、`success` 与 `description` 不进入模型请求。`EvaluationTaskSet` 本身不进模型，只由 `EvaluationRunner.run()` 遍历。
- public/hidden check、success criterion 和 contract satisfaction 消费 `CommandEvalResult`；它发生在 AgentLoop 结束后的独立 evaluator 中，不进入后续模型请求。
- `summarize_evaluation_results()`、regression compare/report、`FailureCaseReplayRunner` 和 CLI JSON 输出消费 `EvaluationTaskResult`。`TargetedFailureReplayResult.to_dict()`、Markdown renderer 与 `eval targeted-replay` 的退出码逻辑消费 targeted replay result；两种 result 都不再进入模型。
- final report 的 `sandbox_isolation_summary`、trace artifact refs 和 command evidence 是 evaluation 判断 sandbox blocker 的输入之一；其中应能看到 selected backend、`backend_unavailable_count`、`local_process_backend_count`、`network_denied_verified_count`、`job_killed_count` 和 artifact refs。

## 是否落盘

- `EvaluationTaskSet` 与 `EvaluationTask` 来自输入 manifest，runner 不复制完整对象；`EvaluationTaskSet.to_dict()` 也不写运行时 `base_dir`。每个 task 的 workspace 落在 `<output_root>/<run_id>/<task_id>/workspace`，另有 `baseline-workspace` 和 `verification-workspace`。
- `CommandEvalResult` 序列化到 `EvaluationTaskResult.verification`、`checks.public`、`checks.hidden` 和 `verification_result`。
- `EvaluationRunner.run()` 在 `<output_root>/<run_id>/` 写 `result.json`、`report.json`、`report.md`；有 baseline 时写 `regression.json`、`regression.md`，失败样本写 `failure_cases.json`。默认 `output_root` 是 `work/evaluations`。
- targeted replay 默认写 `work/evaluations-targeted/targeted_replay_result.json`、`targeted_replay_result.md`，workspace 位于同目录的 `workspace/`；协议状态写该 workspace 下 `.singularity/runs/<run_id>/tool_protocol.sqlite3`，JSONL trace 写 `.singularity/runs/<run_id>.jsonl`。

## 是否进入 trace / audit

- 六个 evaluation 对象都没有专属 trace event，也不直接写 policy audit。生产 task 的 `KernelBootstrap.boot()` 创建 `TraceRecorder`，实际 AgentLoop/model/planner/tool/verification/policy 事件写入 task workspace 的 `work/traces/runs/<runtime_run_id>/{events.jsonl,spans.jsonl,artifacts.jsonl,index.json}`。
- `EvaluationTaskResult.trace` 指向上述 trace run 目录；`trace_artifact_refs` 从 final report/trace summary 的 `key_artifacts`、`artifacts`、`artifact_ref` 提取。`FailureCaseReplayRunner` 读取 `<trace>/events.jsonl` 生成失败样本。
- targeted replay 从 JSONL 中读取 `failure_analysis_requested`、`failure_analysis_completed`、`failure_analysis_failed`、`repair_contract_validation`、`repair_signal_consumed` 及 planner phase/status；事件分别由 `FailureAnalyzer._record()`、`RepairPlanner._record_contract_validation()`、`Planner._record_repair_signal_consumed()` 和 planner recorder 产生。

## 失败路径

- workspace/task/task-set 输入错误在运行前抛 `ValueError`；缺 workspace 源抛 `FileNotFoundError`，git clone/checkout 失败抛 `RuntimeError`。task 执行阶段异常由 `EvaluationRunner.run_task()` 捕获、脱敏后写 `error_summary`。
- `CommandEvalResult.failure_category` 明确区分 `command_parse_error`、`command_timeout`、`command_not_found`、`command_execution_error`、`environment_dependency_missing`、`verification_failed` 和 `command_failed`。
- `EvaluationTaskResult.status` 由 `_task_result()` 归一为 `infrastructure_blocked`、`success`、`policy_blocked`、`verification_failed`、`blocked`、`failed`、`max_turns_exceeded`、`failure` 或 `unknown`；最终通过字段是 `evaluation_passed`，不是 manifest 的 `success`。
- 真实 AgentLoop 中 verification 被 sandbox backend unavailable 阻塞时，失败摘要应保留 policy/sandbox 分类和 trace/final report artifact；不能用 fake provider、scripted provider 或 `danger-full-access` 替代默认 `workspace-write` 验证。
- targeted replay 的 `status` 原样取 `AgentLoopResult.status.value`，`agent_completed=False` 使 CLI 退出 1；该 runner 没有总异常捕获，文件或运行异常直接传播。

## 当前结构问题

`CommandEvalResult.to_dict()` 额外输出派生字段 `passed`，`TargetedFailureReplayResult.to_dict()` 额外输出 `schema_version`，这些不是 dataclass 字段；维护字段校验时必须区分“源码字段完整性”和“序列化派生字段”。`EvaluationWorkspace.kind` 序列化为 `type`，也是明确投影而非 alias。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
