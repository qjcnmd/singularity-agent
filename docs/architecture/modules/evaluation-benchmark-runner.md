# Evaluation Benchmark Runner Runtime Flow

Runtime flow doc id: evaluation-benchmark-runner
Source paths:
- src/singularity/evaluation/runner.py
- src/singularity/evaluation/failure_case_replay.py
- src/singularity/evaluation/targeted_replay.py
- src/singularity/cli.py
- src/singularity/kernel/bootstrap.py
- src/singularity/kernel/agent_kernel.py
- src/singularity/agent_loop.py
- docs/evaluation/capability-fix-math-test-only.json
- docs/evaluation/capability-regression-tasks.json
- docs/evaluation/evaluation-baseline-example.json

Symbols:
- EvaluationWorkspace
- EvaluationWorkspace.from_dict
- EvaluationWorkspace.to_dict
- EvaluationTask
- EvaluationTask.from_dict
- EvaluationTask.to_dict
- EvaluationTaskSet
- EvaluationTaskSet.from_dict
- EvaluationTaskSet.to_dict
- CommandEvalResult
- CommandEvalResult.passed
- CommandEvalResult.to_dict
- EvaluationTaskResult
- EvaluationTaskResult.to_dict
- EvaluationRunner
- EvaluationRunner.run
- EvaluationRunner.run_task
- EvaluationRunner._task_result
- FailureCaseReplayRunner
- FailureCaseReplayRunner.extract
- FailureCaseReplayRunner.write
- TargetedFailureReplayRunner
- TargetedFailureReplayRunner.run
- TargetedFailureReplayRunner.run_smoke
- TargetedFailureReplayResult
- TargetedFailureReplayResult.to_dict
- load_evaluation_task_set
- summarize_evaluation_results
- compare_evaluation_results
- evaluation_report_markdown
- evaluation_regression_markdown
- _task_goal
- _reproducible_environment
- _apply_benchmark_constraints
- _contract_satisfaction
- _patch_applicable_for_task
- _run_shell
- _resolve_command_argv
- KernelBootstrap
- AgentKernel
- AgentKernel.run_task
- AgentLoop
- AgentLoop.run
- eval_run
- eval_private
- eval_provider_smoke
- eval_targeted_replay

Field checks:
- EvaluationWorkspace: kind, path, files, start_commit
- EvaluationTask: task_id, workspace, user_task, allowed_paths, verification_command, success, task_type, description, allowed_tools, tool_policy, strategy, expected_file_changes, completion_standard, risk_tags, prepare_commands, public_verification_command, hidden_verification_command, verification_prepare_commands, verification_timeout_seconds
- EvaluationTaskSet: tasks, base_dir, schema_version
- CommandEvalResult: command, exit_code, duration_seconds, timed_out, error_summary, raw_command, resolved_argv, interpreter_strategy, failure_category
- EvaluationTaskResult: task_id, success, tests_passed, infrastructure_blocked, prompt_tokens, cached_tokens, request_cache_hit_rate, run_cache_hit_rate, tool_calls, files_changed, duration_seconds, error_summary, workspace, trace, verification_workspace, patch, checks, verification, agent_completed, evaluation_passed, completed, patch_applicable, allowed_scope_passed, public_verification_passed, hidden_verification_passed, repair_attempt_count, repair_execution_count, miscompletion_count, blocked_reason, failure_category, request_cache_hit_rates, status, turn_count, verification_result, contract_satisfaction, final_report_status, policy_blocks, token_usage, cache_usage, trace_artifact_refs, reproducible_environment
- TargetedFailureReplayResult: status, completed, entered_agent_loop, failure_trigger, failure_analysis_request_count, failure_analysis_result_count, repair_plan_count, repair_contract_count, repair_attempt_count, repair_execution_count, repairing_failures_seen, verification_contract_satisfaction, repair_scope, final_report_status, trace_path, phase_history, planner_status_history, repair_contract_summary, repairing_failures_evidence, trace_refs, report_paths

## Module Boundary

This module owns manifest-driven evaluation benchmark execution, independent verification, report writing, regression comparison, failure-case extraction, and targeted repair-loop smoke evidence.

It is responsible for loading evaluation task sets, materializing isolated workspaces, booting the real Singularity kernel, calling `AgentKernel.run_task()`, evaluating public and hidden verification in a clean workspace, separating agent-completion from evaluator success, and writing result/report/regression artifacts.

It is not responsible for planner reasoning, tool execution, policy approval, sandbox enforcement, provider protocol handling, or completion review. Those remain inside the normal runtime path reached through `KernelBootstrap.boot()` and `AgentKernel.run_task()`.

## Current Source Locations

- `src/singularity/evaluation/runner.py`: canonical evaluation task-set models, runner, report schema, public/hidden verification, command interpreter strategy, and regression comparison.
- `src/singularity/evaluation/failure_case_replay.py`: post-run failure-case extraction from evaluation reports and trace summaries.
- `src/singularity/evaluation/targeted_replay.py`: deterministic targeted repair smoke that drives the real `AgentLoop.run()` repair path and writes targeted replay JSON/Markdown artifacts.
- `src/singularity/cli.py`: canonical `eval run`, `eval private`, `eval provider-smoke`, and `eval targeted-replay` entrypoints.
- `src/singularity/kernel/bootstrap.py`: `KernelBootstrap.boot()` constructs the graph and kernel.
- `src/singularity/kernel/agent_kernel.py`: `AgentKernel.run_task()` creates and runs `AgentLoop`.
- `src/singularity/agent_loop.py`: real agent loop that talks to the provider, tool protocol, planner, failure analyzer, and final reviewer.
- `docs/evaluation/capability-fix-math-test-only.json`: focused capability smoke manifest.
- `docs/evaluation/capability-regression-tasks.json`: four-task capability regression manifest.
- `docs/evaluation/evaluation-baseline-example.json`: sanitized canonical result/report shape example.

## Runtime Call Chain

1. CLI `eval_run()` calls `load_evaluation_task_set(task_set)`.
2. `EvaluationRunner.run()` creates a run directory, executes each `EvaluationTask` with `run_task()`, summarizes results, writes `result.json`, `report.json`, and `report.md`, and optionally writes regression artifacts.
3. `run_task()` materializes the fixture or repo workspace, loads `ProductionConfig.from_cli()`, and records `reproducible_environment`.
4. `run_task()` executes evaluator-owned `prepare_commands` through `_run_shell()`.
5. `run_task()` snapshots the workspace, copies a baseline workspace, builds the model-visible task goal with `_task_goal()`, boots `KernelBootstrap(...).boot(goal)`, applies benchmark constraints, and calls `kernel.run_task(goal)`.
6. `AgentKernel.run_task()` enters the real `AgentLoop.run()` path. The evaluation runner does not directly call Planner, FailureAnalyzer, VerificationRunner, ToolExecutor, or FinalReviewer internals to claim benchmark success.
7. After the agent returns, `run_task()` extracts trace summary, final report payload, model usage, turn count, tool calls, policy blocks, failure/repair summary, and final report status.
8. `run_task()` calculates changed files and patch payload, rebuilds a clean verification workspace from the baseline, and applies the changed file contents there.
9. Public verification uses `public_verification_command` when declared. If hidden setup is declared and no public command is declared, public verification is recorded as a non-model-hidden placeholder.
10. Hidden verification prepare commands run only after the agent has finished and after changed files are copied to the clean verification workspace.
11. Hidden verification uses `hidden_verification_command` when declared, otherwise `verification_command`.
12. `_contract_satisfaction()` evaluates allowed scope, independent verification, public verification, final report status, patch applicability, expected file changes, completion standard recording, risk tag recording, and expected blocked outcomes.
13. `EvaluationRunner._task_result()` emits the per-task evaluation report schema, including `agent_completed`, `evaluation_passed`, public/hidden verification booleans, patch applicability, allowed-scope pass state, contract satisfaction, miscompletion count, repair telemetry, turn/tool metadata, blocking reason, failure category, and final report status.
14. `FailureCaseReplayRunner.write()` reads the written evaluation report and emits `failure_cases.json` with one `FailureCaseRecord` for each failed task. This is post-run evaluator extraction; it does not call planner, failure analyzer, verification runner, or model internals.
15. `TargetedFailureReplayRunner.run_smoke()` creates a deterministic workspace, scripted provider, normal tool registry/executor/protocol, planner, verification runner, policy engine, and trace recorder, then calls `AgentLoop.run()` to exercise `verification_failed -> FailureAnalysisRequest -> FailureAnalysisResult -> RepairPlan/RepairContract -> repairing_failures -> VerificationContract satisfaction -> completed`.
16. `TargetedFailureReplayRunner.run()` wraps the smoke result as a first-class evaluation artifact and writes `targeted_replay_result.json` plus `targeted_replay_result.md`.

## Runtime Objects Passed

- `EvaluationTaskSet`: schema version, base directory, and ordered `EvaluationTask` entries.
- `EvaluationTask`: task id/type, workspace, model-visible user task, allowed paths/tools, tool policy, strategy, expected file changes, verification commands, completion standard, risk tags, prepare commands, hidden prepare commands, timeout, and success criteria.
- `ProductionConfig`: resolved per-task runtime config using the benchmark workspace as `project_root` and the CLI/env root for provider configuration loading.
- `CommandEvalResult`: raw command string, resolved argv, interpreter strategy, exit code, duration, timeout state, sanitized first-line error summary, pass/fail state, and command failure category.
- `EvaluationTaskResult`: per-task evaluation report object with runtime telemetry, patch/check evidence, verification result, contract satisfaction, repair summary, reproducible environment, agent completion state, evaluator success state, current schema aliases for `completed`/`success`, and failure classification.
- `FailureCaseRecord`: replayable failed-task metadata with schema version, task id, status, failure category, miscompletion count, public/hidden verification booleans, policy blocks, expected file changes, actual changed files, final report status, repair attempt/execution counts, blocked reason, report/regression paths, trace path, trace artifact refs, contract satisfaction, repair telemetry, verification payload, and bounded trace summary.
- `TargetedFailureReplayResult`: deterministic smoke evidence with AgentLoop entry flag, trigger category, FailureAnalyzer request/result counts, authoritative repair plan/contract counts, repair attempt/execution counts, repair phase observation, bounded phase/planner-status history, bounded repair-contract summary, verification-contract satisfaction, repair scope checks, final report status, trace path, trace refs, and optional report paths.
- `result.json`/`report.json`: suite payload with summary, task results, duration, optional regression comparison, and artifact paths.
- `failure_cases.json`: evaluator-owned extraction package with schema version, `runner_mode`, targeted-runner label, source report/regression paths, failure count, and serialized `FailureCaseRecord` entries.

## Model-Visible Objects

The main task model sees the string returned by `_task_goal()`. It includes:

- the manifest `user_task`;
- allowed modification scope;
- allowed tool strategy and preferred tools;
- expected file changes;
- completion standard;
- risk tags;
- the model-visible verification instruction.

When `verification_prepare_commands` are present, hidden evaluator setup remains hidden. The model sees `public_verification_command` when one is declared; otherwise it sees only an instruction to run relevant visible checks and is told hidden evaluator setup will run after it finishes. Hidden prepare commands and hidden verification command contents are not included in `_task_goal()` unless the manifest author explicitly duplicates them in public fields.

Planner benchmark constraints receive only `_model_visible_verification_command(task)`. Hidden verifier commands are not injected into planner verification requirements unless they are also public.

`FailureCaseRecord`, `failure_cases.json`, `TargetedFailureReplayResult`, and targeted replay result artifacts are never model-visible during the original evaluation run.

## Internal Trace Debug Audit Objects

Internal-only evaluation data includes:

- full task-set JSON;
- fixture file contents;
- baseline workspace and verification workspace paths;
- hidden verification prepare commands;
- hidden verification command;
- patch diff and changed-file snapshot;
- `checks.public` and `checks.hidden`;
- raw trace and final report payload after normal runtime redaction;
- `reproducible_environment.model_profile.sources`;
- command interpreter diagnostics such as `resolved_argv` and `harness_executable`;
- `FailureCaseReplayRunner._trace_summary()` bounded trace extraction, including event count, failure-analysis event count, repair event count, final-report outcome, blocked reasons, and the last phase-policy blocks;
- `TargetedFailureReplayResult` bounded evaluator evidence such as `phase_history`, `planner_status_history`, `repair_contract_summary`, `repairing_failures_evidence`, and `trace_refs`.

Provider secrets are not part of the report payload. The report records redacted provider/model/config status through the normal config/effective-config path.

## State Transitions And Failure Paths

- Task-set loading fails fast on invalid schema version, missing tasks, missing `task_id`, missing `user_task`, missing `allowed_paths`, missing `verification_command`, unsupported tool policy, unsupported approval mode, or repo task without `start_commit`/prepare command.
- `EvaluationTaskSet.from_dict()` emits the canonical `evaluation.task_set/v1` in memory and accepts the retired `evaluation.live_agent_task_set/v1` only as migration input. Result comparison emits canonical `evaluation.result/v1` artifacts and accepts retired `evaluation.live_agent_eval_result/v1` baseline artifacts only for read-only regression comparison.
- Prepare command failure returns a structured task result before kernel boot.
- Provider/network infrastructure failures are classified as `infrastructure_blocked` and do not run independent verification.
- Patch applicability is false when a patch is required and the clean verification workspace cannot reproduce the agent workspace changes, or when required `expected_file_changes` are absent.
- Public verification failure, hidden verification failure, changed files outside `allowed_paths`, failed success criteria, and patch failure all make `evaluation_passed=false`.
- `agent_completed=true` means AgentLoop/final report status claims task completion. Kernel finalized/blocked states are not agent completion by themselves.
- `evaluation_passed=true` means independent evaluator checks passed. It must not be inferred from the agent self-report.
- `miscompletion_count=1` only when `agent_completed=true` and `evaluation_passed=false`.
- `completed` remains a deprecated compatibility alias for `agent_completed`; `success` remains a deprecated compatibility alias for `evaluation_passed`.
- `failure_cases.json` is written even when no tasks failed; in that case `failure_count=0` and `records=[]`.
- `_run_shell()` parses manifest command strings with `shlex.split(posix=True)`, executes with `shell=False`, maps bare `python`/`python3`/`py` to the current harness `sys.executable`, and records command parse, timeout, command-not-found, dependency-missing, verification-failed, or command-failed categories.
- Model-assisted planner/failure/final-review paths can participate only through the real AgentLoop. They cannot bypass policy, approval, sandbox, schema validation, benchmark contract satisfaction, or independent verification.

## Current Structure Assessment

The canonical evaluation runner uses mainstream evaluation/benchmark/task/result/report naming. Retired evaluation aliases from the previous naming cleanup are not part of the current CLI, output schemas, or docs examples. The only retained migration behavior is read-only loading of retired live task-set and baseline-result schema ids.

The current report schema separates agent completion from evaluator pass state and preserves miscompletion detection. It no longer writes presentation-only provenance fields or duplicate aliases such as `tool_call_count`, `task_verification_result`, `failure_repair_count`, `result_extraction`, `repair_verification_contract`, or `agent_loop_ref`.

The V-7 focused manifest remains a one-task smoke check under canonical naming. The multi-task manifest is the primary capability regression entrypoint and covers simple patch, multi-file reasoning, failure-repair, and completion-gate task classes.

## Production-Grade Target Structure

Current code keeps evaluator-owned verification command execution local to `_run_shell()` instead of using the general `CommandExecutor`/sandbox stack. That is acceptable for independent verification, but a future production structure could expose an explicit `EvaluationCommandExecutor` with the same interpreter strategy and failure taxonomy.

Current code records task type and risk tags but does not yet enforce per-task class quotas in `EvaluationTaskSet.from_dict()`. The current regression manifest and tests enforce the four required classes.

Current code compares only tasks present in both baseline and candidate result files. A future regression object could also classify added/removed tasks and enforce per-class thresholds.

## Harness Usage Example

Run the focused capability smoke:

```bash
python -m singularity.cli eval run docs/evaluation/capability-fix-math-test-only.json --run-id capability-smoke
```

Run the multi-task capability regression:

```bash
python -m singularity.cli eval run docs/evaluation/capability-regression-tasks.json --run-id capability-regression
```

With a baseline:

```bash
python -m singularity.cli eval run docs/evaluation/capability-regression-tasks.json --baseline-result work/evaluations/previous/result.json
```

Run the targeted repair replay artifact entrypoint:

```bash
python -m singularity.cli eval targeted-replay --output-dir work/evaluations-targeted --json
```

## Maintenance Rules

Update this document when changing:

- `EvaluationWorkspace`, `EvaluationTask`, `EvaluationTaskSet`, `CommandEvalResult`, `EvaluationTaskResult`, or `TargetedFailureReplayResult` fields;
- `FailureCaseRecord`, `FailureCaseReplayRunner`, or `failure_cases.json` schema;
- evaluation task-set schema, result schema, or success criteria;
- public/hidden verification behavior;
- `_task_goal()` model-visible benchmark instructions;
- `_apply_benchmark_constraints()` and planner benchmark verification command injection;
- `_run_shell()` command parsing, interpreter mapping, or failure taxonomy;
- `_contract_satisfaction()` and miscompletion semantics;
- evaluation report, summary, regression, failure replay, reproducible environment, or completion/result alias fields.

## Verification

Relevant checks:

- `python scripts/verify_runtime_docs.py`
- `python -m pytest tests/evaluation/test_evaluation_runner.py tests/evaluation/test_targeted_failure_replay.py --basetemp work/pytest-tmp`
- real model run through `python -m singularity.cli eval run docs/evaluation/capability-regression-tasks.json --run-id <run-id>`

## Last Verified Against

- Source tree date: 2026-06-26
- Code paths: `src/singularity/evaluation/runner.py`, `src/singularity/cli.py`, `src/singularity/kernel/bootstrap.py`, `src/singularity/kernel/agent_kernel.py`, `src/singularity/agent_loop.py`
