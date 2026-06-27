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

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

`python -m singularity.cli eval run <manifest>` -> `EvaluationTaskSet.from_dict()` -> `EvaluationRunner.run()` -> per task `KernelBootstrap.boot()` -> `AgentKernel.run_task()` -> `AgentLoop.run()` -> verification workspace checks -> `EvaluationTaskResult.to_dict()` -> result/report artifacts。

## 真实对象完整结构

- `EvaluationTask.success（评估通过条件）` 是 manifest 输入字段，不是 result alias；结果判定只使用 `evaluation_passed`。
- `EvaluationTaskResult（评估任务结果）` 完整字段列在字段清单中，canonical 完成/判定字段为 `agent_completed`、`evaluation_passed`、`miscompletion_count`。

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
- targeted replay 的 `status` 原样取 `AgentLoopResult.status.value`，`agent_completed=False` 使 CLI 退出 1；该 runner 没有总异常捕获，文件或运行异常直接传播。

## 当前结构问题

`CommandEvalResult.to_dict()` 额外输出派生字段 `passed`，`TargetedFailureReplayResult.to_dict()` 额外输出 `schema_version`，这些不是 dataclass 字段；维护字段校验时必须区分“源码字段完整性”和“序列化派生字段”。`EvaluationWorkspace.kind` 序列化为 `type`，也是明确投影而非 alias。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
