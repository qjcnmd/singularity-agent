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

关键符号见本文顶部 `关键符号:`。真实对象字段见本文顶部 `字段清单:`，字段顺序按源码声明顺序排列。

## 真实运行时调用链

`run_command` / verification tools -> `CommandExecutor.run()` -> `CommandRequest` -> command policy -> optional sandbox/backend -> `CommandResult` -> trace/context/planner evidence。

## 真实对象完整结构

- `CommandRequest（命令请求）` 完整字段列在字段清单中，生成者是 command tool、verification runner 或 evaluation runner。
- `CommandResult（命令结果）` 完整字段列在字段清单中，消费者是 planner evidence、context observation、trace 和 final report。

## 谁生成这些对象

这些对象由上文列出的源码组件在运行链路中生成。生成动作必须来自当前源码路径，不允许由文档、测试夹具或解释性包装层伪造。

## 谁消费这些对象

消费方是同一调用链后续组件、trace/audit 记录器、报告生成器或持久化 store。文档只列当前源码中真实调用的消费方。

## 是否落盘

落盘只通过当前源码中的 trace store、SQLite store、workspace state、evaluation output 或 manifest/report 写入路径发生。没有落盘代码的对象只在内存中传递。

## 是否进入 trace / audit

进入 trace / audit 的内容以 `TraceRecorder`、`JsonlTraceRecorder`、`TraceArtifactStore`、policy audit ledger 和相关 `record` / `emit` 调用为准。对象进入模型前必须经过当前工具协议、上下文组装和 redaction 逻辑。

## 失败路径

失败路径由当前源码中的异常、状态枚举、policy decision、verification result、planner outcome 和 result/report 字段表达。不得用旧 schema 或旧命名补充解释。

## 当前结构问题

当前结构仍大量使用字典 payload 连接组件，维护时最容易发生字段漂移。字段清单必须由源码校验脚本约束，不能只依赖人工描述。

## 维护规则

修改本模块相关类、字段、函数、调用链、CLI、schema、manifest、trace event、report schema 或 evaluation result 时，必须同步更新本文件并运行 `python scripts/verify_runtime_docs.py`。展示真实对象时必须列完整字段，不允许只列子集，不允许新增仅服务文档说明的运行时字段。
