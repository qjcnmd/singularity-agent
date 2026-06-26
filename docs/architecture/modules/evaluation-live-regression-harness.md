# Evaluation Live Regression Harness Runtime Flow

Runtime flow doc id: evaluation-live-regression-harness
Source paths:
- src/singularity/evaluation/live.py
- src/singularity/evaluation/targeted_replay.py
- src/singularity/cli.py
- src/singularity/kernel/bootstrap.py
- src/singularity/kernel/agent_kernel.py
- src/singularity/agent_loop.py
- docs/evaluation/live-fix-math-test-only.json
- docs/evaluation/live-agent-regression-tasks.json
- docs/evaluation/live-agent-baseline-example.json

Symbols:
- LiveEvalWorkspace
- LiveEvalTask
- LiveEvalTask.from_dict
- LiveEvalTask.to_dict
- LiveEvalManifest
- LiveEvalManifest.from_dict
- CommandEvalResult
- CommandEvalResult.to_dict
- LiveEvalTaskResult
- LiveEvalTaskResult.to_dict
- LiveAgentEvalRunner
- LiveAgentEvalRunner.run
- LiveAgentEvalRunner.run_task
- TargetedFailureReplayRunner
- TargetedFailureReplayRunner.run_smoke
- TargetedFailureReplayResult
- TargetedFailureReplayResult.to_dict
- load_live_eval_manifest
- summarize_live_results
- compare_live_eval_results
- live_eval_report_markdown
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
- eval_live_run

Field checks:
- LiveEvalTask: task_id, workspace, user_task, allowed_paths, verification_command, success, task_type, expected_file_changes, completion_standard, risk_tags, prepare_commands, public_verification_command, hidden_verification_command, verification_prepare_commands, verification_timeout_seconds
- CommandEvalResult: command, exit_code, duration_seconds, timed_out, error_summary, raw_command, resolved_argv, interpreter_strategy, failure_category
- LiveEvalTaskResult: task_id, success, tests_passed, infrastructure_blocked, tool_calls, files_changed, verification_workspace, patch, checks, verification, completed, patch_applicable, public_verification_passed, hidden_verification_passed, repair_attempt_count, repair_execution_count, miscompletion_count, blocked_reason, failure_category, tool_call_count, status, turn_count, verification_result, contract_satisfaction, final_report_status, failure_repair_count, policy_blocks, result_extraction, task_verification_result, repair_verification_contract, agent_loop_ref

## Module Boundary

This module owns manifest-driven live-provider benchmark execution and reporting.

It is responsible for loading live eval manifests, materializing isolated task workspaces, booting the real Singularity kernel, calling `AgentKernel.run_task()`, independently applying the resulting workspace changes to a clean verification workspace, running public and hidden verification commands, and writing result/report/regression artifacts.

It is not responsible for planner reasoning, tool execution, policy approval, sandbox enforcement, provider protocol handling, or completion review. Those remain inside the normal runtime path reached through `KernelBootstrap.boot()` and `AgentKernel.run_task()`.

## Current Source Locations

- `src/singularity/evaluation/live.py`: manifest models, live runner, report schema, public/hidden verification, command interpreter strategy, regression comparison.
- `src/singularity/evaluation/models.py`: shared evaluation dataclasses, including `FailureCaseRecord`.
- `src/singularity/evaluation/failure_case_replay.py`: `FailureCaseReplayRunner` extraction from live report and trace summaries.
- `src/singularity/evaluation/targeted_replay.py`: `TargetedFailureReplayRunner` deterministic smoke that drives the real `AgentLoop.run()` repair path and emits explicit repair-activation evidence.
- `src/singularity/cli.py`: `eval live run` and `eval live private` CLI entrypoints.
- `src/singularity/kernel/bootstrap.py`: `KernelBootstrap.boot()` constructs the graph and kernel.
- `src/singularity/kernel/agent_kernel.py`: `AgentKernel.run_task()` creates and runs `AgentLoop`.
- `src/singularity/agent_loop.py`: real agent loop that talks to the provider, tool protocol, planner, failure analyzer, and final reviewer.
- `docs/evaluation/live-fix-math-test-only.json`: retained V-7 focused smoke manifest.
- `docs/evaluation/live-agent-regression-tasks.json`: multi-task capability regression manifest.
- `docs/evaluation/live-agent-baseline-example.json`: sanitized result/report shape example.

## Runtime Call Chain

1. CLI `eval_live_run()` calls `load_live_eval_manifest(task_set)`.
2. `LiveAgentEvalRunner.run()` creates a run directory, executes each `LiveEvalTask` with `run_task()`, summarizes results, writes `result.json`, `report.json`, and `report.md`, and optionally writes regression artifacts.
3. `run_task()` materializes the fixture or repo workspace, loads `ProductionConfig.from_cli()`, and records `reproducible_environment`.
4. `run_task()` executes manifest `prepare_commands` through `_run_shell()`.
5. `run_task()` snapshots the workspace, copies a baseline workspace, builds the model-visible task goal with `_task_goal()`, boots `KernelBootstrap(...).boot(goal)`, applies benchmark constraints, and calls `kernel.run_task(goal)`.
6. `AgentKernel.run_task()` enters the real `AgentLoop.run()` path. The live runner does not directly call Planner, FailureAnalyzer, VerificationRunner, ToolExecutor, or FinalReviewer internals to claim benchmark success.
7. After the agent returns, `run_task()` extracts trace summary, final report payload, model usage, turn count, tool call count, policy blocks, failure/repair summary, and final report status.
8. `run_task()` calculates changed files and patch payload, rebuilds a clean verification workspace from the baseline, and applies the changed file contents there.
9. Public verification uses `public_verification_command` when declared. If hidden setup is declared and no public command is declared, public verification is recorded as a non-model-hidden placeholder.
10. Hidden verification prepare commands run only after the agent has finished and after changed files are copied to the clean verification workspace.
11. Hidden verification uses `hidden_verification_command` when declared, otherwise `verification_command`.
12. `_contract_satisfaction()` evaluates allowed scope, independent verification, public verification, final report status, patch applicability, expected file changes, completion standard recording, risk tag recording, and expected blocked outcomes.
13. `_task_result()` emits the per-task Eval Report schema fields, including `success`, `completed`, `patch_applicable`, `public_verification_passed`, `hidden_verification_passed`, `contract_satisfaction`, `miscompletion_count`, repair telemetry, turn/tool counts, blocking reason, failure category, and final report status.
14. `FailureCaseReplayRunner.write()` reads the written live report and emits `failure_cases.json` with one `FailureCaseRecord` for each failed task. This is post-run evaluator extraction; it does not call planner, failure analyzer, verification runner, or model internals. The payload declares `runner_mode="post_run_failure_extraction"` and points targeted execution replay to `TargetedFailureReplayRunner`.
15. `TargetedFailureReplayRunner.run_smoke()` is a separate targeted smoke API. It creates a deterministic workspace, scripted provider, normal tool registry/executor/protocol, planner, verification runner, policy engine, and trace recorder, then calls `AgentLoop.run()` to exercise `verification_failed -> FailureAnalysisRequest -> FailureAnalysisResult -> RepairPlan/RepairContract -> repairing_failures -> VerificationContract satisfaction -> completed`.

## Runtime Objects Passed

- `LiveEvalManifest`: schema version, base directory, and ordered `LiveEvalTask` entries.
- `LiveEvalTask`: task id/type, workspace, model-visible user task, allowed paths/tools, tool policy, strategy, expected file changes, verification commands, completion standard, risk tags, prepare commands, hidden prepare commands, timeout, and success criteria.
- `ProductionConfig`: resolved per-task runtime config using the benchmark workspace as `project_root` and the CLI/env root for provider configuration loading.
- `CommandEvalResult`: raw command string, resolved argv, interpreter strategy, exit code, duration, timeout state, sanitized first-line error summary, pass/fail state, and command failure category.
- `LiveEvalTaskResult`: per-task Eval Report object with runtime telemetry, patch/check evidence, verification result, contract satisfaction, repair contract summary, reproducible environment, and AgentLoop reference.
- `FailureCaseRecord`: replayable failed-task metadata with schema version, task id, status, failure category, miscompletion count, public/hidden verification booleans, policy blocks, expected file changes, actual changed files, final report status, repair attempt/execution counts, blocked reason, report/regression paths, trace path, trace artifact refs, contract satisfaction, repair telemetry, verification payload, and bounded trace summary.
- `TargetedFailureReplayResult`: deterministic smoke evidence with schema version, AgentLoop entry flag/ref, trigger category, FailureAnalyzer request/result counts, authoritative repair plan/contract counts, repair attempt/execution counts, repair phase observation, verification-contract satisfaction, repair scope checks, final report status, trace path, and model-visible/internal object boundary labels.
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

`FailureCaseRecord` and `failure_cases.json` are never model-visible during the original live run. They are evaluator-internal extraction records for later diagnostics and targeted regression. `TargetedFailureReplayRunner` can run a deterministic smoke later, but that smoke uses normal `AgentLoop.run()` model context and does not make prior `FailureCaseRecord` objects visible to the task model.

## Internal Trace Debug Audit Objects

Internal-only evaluation data includes:

- full manifest JSON;
- fixture file contents;
- baseline workspace and verification workspace paths;
- hidden verification prepare commands;
- hidden verification command;
- patch diff and changed-file snapshot;
- `checks.public` and `checks.hidden`;
- raw trace and final report payload after normal runtime redaction;
- `reproducible_environment.model_profile.sources`;
- command interpreter diagnostics such as `resolved_argv` and `harness_executable`.
- `FailureCaseReplayRunner._trace_summary()` bounded trace extraction, including event count, failure-analysis event count, repair event count, final-report outcome, blocked reasons, and the last phase-policy blocks.
- `TargetedFailureReplayResult.evaluator_internal_objects`, which labels evaluation-only objects such as `FailureCaseRecord`, `FailureCaseReplayRunner.extract`, and `failure_cases.json`.

Provider secrets are not part of the report payload. The report records redacted provider/model/config status through the normal config/effective-config path.

## State Transitions And Failure Paths

- Manifest loading fails fast on invalid schema version, missing tasks, missing `task_id`, missing `user_task`, missing `allowed_paths`, missing `verification_command`, unsupported tool policy, unsupported approval mode, or repo task without `start_commit`/prepare command.
- Prepare command failure returns a structured task result before kernel boot.
- Provider/network infrastructure failures are classified as `infrastructure_blocked` and do not run independent verification.
- Patch applicability is false when a patch is required and the clean verification workspace cannot reproduce the agent workspace changes, or when required `expected_file_changes` are absent.
- Public verification failure, hidden verification failure, changed files outside `allowed_paths`, failed success criteria, and patch failure all make `success=false`.
- If the final report or agent status claims completed/success but the live task contract fails, `_task_result()` records `miscompletion_count=1`; suite summary uses the same completed/success-only predicate.
- `_completed()` counts only explicit completed/success agent statuses as completed. A kernel-finalized blocked run is not counted as task completion.
- `summarize_live_results()` counts `completed_count` only from `LiveEvalTaskResult.completed` or `final_report_status in {"completed", "success"}`. A `final_report_status="finalized"` value by itself is kernel finalization, not task completion, and is not counted as completed or miscompleted unless the task result separately records completed status.
- `failure_cases.json` is written even when no tasks failed; in that case `failure_count=0` and `records=[]`.
- `_run_shell()` parses manifest command strings with `shlex.split(posix=True)`, executes with `shell=False`, maps bare `python`/`python3`/`py` to the current harness `sys.executable`, and records command parse, timeout, command-not-found, dependency-missing, verification-failed, or command-failed categories.
- Model-assisted planner/failure/final-review paths can participate only through the real AgentLoop. They cannot bypass policy, approval, sandbox, schema validation, benchmark contract satisfaction, or independent verification.

## Current Structure Assessment

The live runner already uses the real runtime path and independent verification workspace, which makes it a suitable capability regression harness rather than a synthetic evaluator. The current report schema now exposes stable per-task telemetry fields needed for regression analysis: completion state, patch applicability, public/hidden verification pass state, contract satisfaction, repair attempts/executions, turns, tool calls, blocked reason, failure category, and final report status.

The V-7 focused manifest remains a one-task smoke check. The multi-task manifest is the primary capability regression entrypoint and covers simple patch, multi-file reasoning, failure-repair, and completion-gate task classes.

The `FailureCaseReplayRunner` name is retained for compatibility, but its payload and docstring now identify it as post-run failure extraction. Actual targeted execution replay is the separate `TargetedFailureReplayRunner` API, so extraction artifacts cannot be mistaken for proof that the repair loop was activated.

## Production-Grade Target Structure

Current code keeps live eval command execution local to `_run_shell()` instead of using the general `CommandExecutor`/sandbox stack. That is acceptable for evaluator-owned independent verification, but a future production structure could expose an explicit `EvaluationCommandExecutor` object with the same interpreter strategy and failure taxonomy.

Current code records task type and risk tags but does not yet enforce per-task class quotas in `LiveEvalManifest.from_dict()`. The current regression manifest and tests enforce the four required classes.

Current code compares only tasks present in both baseline and candidate result files. A future regression object could also classify added/removed tasks and enforce per-class thresholds.

## Harness Usage Example

Run the retained V-7 smoke:

```bash
python -m singularity eval live run docs/evaluation/live-fix-math-test-only.json --run-id v7-smoke
```

Run the multi-task capability regression:

```bash
python -m singularity eval live run docs/evaluation/live-agent-regression-tasks.json --run-id capability-regression
```

With a baseline:

```bash
python -m singularity eval live run docs/evaluation/live-agent-regression-tasks.json --baseline-result work/evaluations-live/previous/result.json
```

## Maintenance Rules

Update this document when changing:

- `LiveEvalTask`, `CommandEvalResult`, or `LiveEvalTaskResult` fields;
- `FailureCaseRecord`, `FailureCaseReplayRunner`, or `failure_cases.json` schema;
- live eval manifest schema or success criteria;
- public/hidden verification behavior;
- `_task_goal()` model-visible benchmark instructions;
- `_apply_benchmark_constraints()` and planner benchmark verification command injection;
- `_run_shell()` command parsing, interpreter mapping, or failure taxonomy;
- `_contract_satisfaction()` and miscompletion semantics;
- live eval report, summary, regression, failure replay, or reproducible environment fields.
- `TargetedFailureReplayRunner`, `TargetedFailureReplayResult`, or targeted repair-activation smoke evidence fields.

## Verification

Relevant checks:

- `python -m pytest tests/evaluation/test_live_eval.py --basetemp work/pytest-tmp`
- `python -m pytest tests/evaluation --basetemp work/pytest-tmp`
- `python scripts/verify_runtime_docs.py`
- real model run through `python -m singularity eval live run docs/evaluation/live-agent-regression-tasks.json --run-id <run-id>`

## Last Verified Against

- Source tree date: 2026-06-26
- Code paths: `src/singularity/evaluation/live.py`, `src/singularity/cli.py`, `src/singularity/kernel/bootstrap.py`, `src/singularity/kernel/agent_kernel.py`, `src/singularity/agent_loop.py`
