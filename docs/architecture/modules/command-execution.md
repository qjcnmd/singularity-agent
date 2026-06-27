# Command Execution模块数据流

模块数据流文档 ID: command-execution

源码证据路径:
- src/singularity/command/models.py
- src/singularity/command/executor.py
- src/singularity/command/policy.py
- src/singularity/tools/command.py

关键符号:
- CommandRequest
- CommandPlan
- CommandResult
- ProcessSession
- ProcessOutput
- ProcessStopResult
- CommandExecutor

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

Command 层规范化 argv/shell、cwd、purpose、env、network/filesystem policy 和资源限制，再通过 policy/sandbox/backend 执行命令并生成可追踪结果。

## 当前源码位置

- src/singularity/command/models.py
- src/singularity/command/executor.py
- src/singularity/command/policy.py
- src/singularity/tools/command.py

## 关键类、函数、字段

本文顶部列出源码证据路径、关键符号和完整字段清单；下文对象流只引用这些真实源码对象。

## 真实运行时调用链

`run_command` / verification tools -> `CommandExecutor.run()` -> `CommandRequest` -> command policy -> optional sandbox/backend -> `CommandResult` -> trace/context/planner evidence。

## 真实任务中的对象流

以用户要求修复 `quicksort.py` 为例：`CommandToolHandlers.run_command()` -> `CommandExecutor.plan()` -> `CommandExecutor.run()` 先把 tool 参数生成对象 `CommandRequest`，再经 command policy 生成 `CommandPolicyResult` 和 `CommandPlan`。若策略要求隔离，`SandboxManager.run()` 返回的 sandbox payload 被 `CommandExecutor._result_from_sandbox()` 转成 `CommandResult`；否则 `_completed_result()` 从 backend exit/stdout/stderr 生成结果。`CommandExecutor._record_trace()` 写入 command trace 事件，长输出写 artifact，`CommandResult.to_observation()` 进入 `context.sqlite3` 并由 `Planner.update_from_command()` 消费为 evidence。

## 真实对象完整结构

- `CommandRequest（命令请求）` 完整字段列在字段清单中，生成者是 command tool、verification runner 或 evaluation runner。
- `CommandResult（命令结果）` 完整字段列在字段清单中，消费者是 planner evidence、context observation、trace 和 final report。

## 谁生成这些对象

- command tool、VerificationRunner 与 evaluation setup 生成 `ResourceLimits`/`CommandRequest`；`CommandPolicy.evaluate()` 或 executor 的 fail-closed 分支生成 `CommandPolicyResult`，`CommandExecutor.plan()` 组合为 `CommandPlan`。
- `CommandExecutor.run()` 的 backend、sandbox、blocked 分支生成 `CommandResult`；`start_process()`、`read_process_output()`、`stop_process()` 分别生成 `ProcessSession`、`ProcessOutput`、`ProcessStopResult`。

## 谁消费这些对象

`CommandExecutor` 消费 request/plan/policy；command tool、verification、planner 消费 result/process objects。`CommandResult.to_observation()` 和 process `to_dict()` 的安全投影进入 tool result/context，模型看不到 env request、raw secret argv 或完整内部 plan。

## 是否落盘

Command plan 和 process session 只在 executor 内存；长 stdout/stderr 由 `OutputCollector` 写 artifact，路径放入 `CommandResult.artifact_path` / `ProcessSession.logs_artifact_path`。result 的安全 observation 写 context SQLite，side effects 可写 workspace state journal。

## 是否进入 trace / audit

CommandExecutor 发出 `COMMAND_*` event 与 legacy `command` record，payload 包含 status、exit、digest、artifact ref、changed files、policy/isolation 摘要；argv/env/output 在写入前脱敏。Command policy 的 request/decision 进入 policy audit ledger。

## 失败路径

policy 返回 `REVIEW_REQUIRED`/`POLICY_DENIED`、cwd denied、sandbox setup/backend error、timeout/idle timeout、kill 或非零退出时生成非成功 `CommandResult`；process API 通过 `status`、`error_code`、`killed_reason` 表达失败，不把启动失败登记为 running session。

## 当前结构问题

同步维护 request→policy→plan→backend→result 与 long-running process 两条路径；模型可见边界是 observation，不是完整 `CommandResult.to_dict()`。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
