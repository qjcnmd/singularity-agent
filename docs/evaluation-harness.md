# Evaluation Harness

Singularity includes a local-first Evaluation Harness for replayable benchmark suites, trace replay, A/B profile comparison, regression detection, and report generation. It is an orchestration layer: executable hooks, test checks, and workspace materialization go through the existing command, verification, mutation, trace, memory, and planner boundaries when execution is explicitly enabled.

## Benchmark Task Schema

Golden tasks are stored as JSON or YAML documents:

```json
{
  "schema_version": "evaluation.golden_task_set/v1",
  "task_schema_version": "evaluation.benchmark_task/v1",
  "tasks": [
    {
      "task_id": "parser.empty-section",
      "version": "v1",
      "title": "Fix parser empty sections",
      "task_type": "repo_issue_repair",
      "visibility": "private",
      "adapter": "singularity_private",
      "input": {
        "prompt": "Fix parsing for empty sections and run tests."
      },
      "workspace_snapshot": {
        "kind": "git_ref",
        "git_ref": "HEAD"
      },
      "expected_outcomes": [
        {
          "kind": "test",
          "weight": 0.7,
          "command": "python -m pytest tests/test_parser.py"
        },
        {
          "kind": "heuristic",
          "weight": 0.3,
          "heuristic": "patch_quality"
        }
      ],
      "evaluation_hooks": [
        {
          "name": "compile",
          "stage": "before_run",
          "command": "python -m compileall src"
        }
      ],
      "golden_contract": {
        "scenario": "create_file_smoke_verify",
        "expected_files": ["quicksort.py", "tests/test_quicksort.py"],
        "expected_commands": ["python -m pytest tests/test_quicksort.py"],
        "expected_evidence": ["file_created", "smoke_verified", "final_report_written"],
        "expected_report_sections": ["Goal", "Changes", "Verification", "Risks"],
        "required_trace_artifacts": ["diff", "verification", "report"]
      },
      "tags": ["easy", "tool-heavy"],
      "profiles": {
        "model": "baseline",
        "tool_policy": "read_write"
      }
    }
  ]
}
```

Supported `workspace_snapshot.kind` values:

- `git_ref`
- `archive_path`
- `inline_files`
- `baseline_trace_run_id`

Supported `expected_outcomes.kind` values:

- `test`
- `assertion`
- `diff`
- `heuristic`

Supported `task_type` values:

- `repo_issue_repair`
- `terminal_task`
- `singularity_internal`

Supported `visibility` values are `public` and `private`. Public benchmarks are intended for externally reproducible tasks; private benchmarks can carry hidden setup/checks without exposing them to the agent prompt.

Supported `adapter` values are:

- `singularity_private` (implemented)
- `swe_bench` (reserved adapter boundary)
- `terminal_bench` (reserved adapter boundary)

Each task must use one difficulty tag: `easy`, `medium`, or `hard`. Optional workload tags include `memory-heavy` and `tool-heavy`. Task versions currently support `v1` and `v2`.

Evaluation hooks are declarative. Command hooks are intended to run through `CommandExecutor` or `VerificationRunner` when integrated into a real execution pipeline; the evaluation CLI does not bypass those executors or runners.

`archive_path` is accepted in the schema, but execution is fail-closed until Singularity has a controlled archive restore adapter that stages files and applies them through `WorkspaceMutationManager`.

`golden_contract` is optional for legacy benchmark tasks and required for the Phase 1J built-in Golden Task Set. When present, it records the scenario, expected files, expected commands, expected evidence names, expected markdown report sections, and required trace artifact kinds. Evaluation reports carry this contract through `execution_evidence.golden_contract` and render it in a `Golden Task Evidence` markdown section.

The checked-in Phase 1J Golden Task Set lives at:

```text
docs/evaluation/phase1j-golden-tasks.json
```

It covers:

- create file + smoke verify
- modify bug + test pass
- verification failure + repair
- completion rejected + continue
- final review rejected + repair
- full markdown report
- approval required + resume
- sandbox required / unavailable fail closed
- dynamic retrieval after failure
- memory write only after verified completion

## CLI

Validate and list a Golden Task Set:

```bash
singularity-agent eval task validate golden.json --json
singularity-agent eval task list golden.json --version v1 --tag tool-heavy
singularity-agent eval task validate docs/evaluation/phase1j-golden-tasks.json --json
```

Replay a trace with a fixed profile:

```bash
singularity-agent eval trace replay work/traces/runs/run_123 \
  --profile-json "{\"name\":\"baseline\",\"model\":\"gpt-test\",\"prompt_profile\":\"default\",\"memory_enabled\":true,\"allowed_tools\":[\"read_file\"],\"tool_policy\":\"read_only\"}"
```

Run a suite:

```bash
singularity-agent eval suite run golden.json \
  --trace-run-dir work/traces/runs/run_123 \
  --profile-json "{\"name\":\"baseline\",\"model\":\"gpt-a\",\"memory_enabled\":true,\"allowed_tools\":[\"read_file\",\"run_verification\"],\"tool_policy\":\"read_write\"}"
```

Suite, A/B, and regression commands default to deterministic offline scoring and trace replay. Add `--execute` only when the task needs executable hooks, test commands, assertions against a prepared workspace, or inline-file snapshot materialization. The CLI execution path uses `CommandExecutor` and `VerificationRunner`; full kernel-boot execution remains responsible for richer planner, memory, tool, and mutation wiring.

Run the optional real-provider E2E smoke benchmark:

```bash
singularity-agent eval provider-smoke --json
```

This command creates a controlled workspace under `work/evaluations/`, boots the real CLI kernel with the configured OpenAI-compatible provider, asks the agent to create and verify `quicksort.py`, and then independently runs `python quicksort.py`. Unlike trace replay, this path can make real model calls and should be run only when `SINGULARITY_API_KEY`, `SINGULARITY_MODEL`, and `SINGULARITY_BASE_URL` are intentionally configured.

Run the focused capability smoke manifest:

```bash
singularity-agent eval run docs/evaluation/capability-fix-math-test-only.json --json
```

Run the multi-task capability regression:

```bash
singularity-agent eval run docs/evaluation/capability-regression-tasks.json --json
singularity-agent eval run docs/evaluation/capability-regression-tasks.json --baseline-result work/evaluations/previous/result.json
```

Run a private BenchmarkTask set through the private adapter:

```bash
singularity-agent eval private private-benchmark.json --json
```

The evaluation task-set schema is the production benchmark layer for coding-agent behavior. Each task declares `task_id`, optional `task_type`, `description`, a `repo` or `fixture` workspace, optional `start_commit` or `prepare_commands`, `user_task`, `allowed_paths`, `allowed_tools`, `tool_policy` or `strategy`, `expected_file_changes`, `verification_command`, optional `public_verification_command` / `hidden_verification_command`, `completion_standard`, `risk_tags`, and `success`. Use `verification_prepare_commands` only for evaluator-owned hidden setup that must run after the agent finishes and before independent verification, such as applying a benchmark test patch without exposing it to the model. When hidden setup is present, only `public_verification_command` is model-visible; hidden setup and hidden verification remain evaluator-internal.

Each task runs in an isolated directory under `work/evaluations/<run_id>/<task_id>/workspace`. The runner boots the real Singularity kernel through `KernelBootstrap(...).boot(goal)` and then calls `AgentKernel.run_task(goal)`, which drives the real `AgentLoop`. The benchmark layer does not call Planner, ToolExecutor, or VerificationRunner directly to fake outcomes.

Evaluation also writes a clean verification workspace per task. The runner records the agent workspace patch as `patch.diff`, reapplies changed files onto the clean verification workspace, then records public and hidden check results under `checks.public` and `checks.hidden`. A task's evaluator success is `evaluation_passed=true` only when allowed path scope, expected file changes, patch applicability where a patch is expected, independent public/hidden verification, and success criteria all pass. Rejection and policy-block tasks can express evaluator success with `agent_status` or `policy_blocks_min` criteria instead of pretending they are normal edit tasks.

The evaluation runner uses the normal `ProductionConfig.from_cli()` path. For manifest tasks, `--project-root` identifies the project/config root used to load project-local `.env` values while the actual agent run still uses each benchmark workspace as `project_root`. The loader only sets variables that are not already present in the process environment and keeps `SINGULARITY_API_KEY` out of effective config, trace, result, and report payloads. Per-task `reproducible_environment` records the fixture or repo source, prepare commands, verification commands, timeout, model/profile source, approval/security mode, sandbox strategy, baseline artifact reference, and interpreter strategy. Manifest command strings are parsed to argv with `shlex.split(posix=True)` and executed with `shell=False`; bare `python`, `python3`, or `py` resolve to the harness `sys.executable` so benchmark verification does not depend on a manually installed shell `python`.

The evaluation runner writes stable `result.json`, `report.json`, `report.md`, and `failure_cases.json` files. Per-task results include `success`, `agent_completed`, `evaluation_passed`, `completed`, `patch_applicable`, `allowed_scope_passed`, `public_verification_passed`, `hidden_verification_passed`, `contract_satisfaction`, `miscompletion_count`, `repair_attempt_count`, `repair_execution_count`, `turn_count`, `blocked_reason`, `failure_category`, `final_report_status`, `status`, `tool_calls`, `files_changed`, `verification_result`, `policy_blocks`, `token_usage`, `cache_usage`, `trace_artifact_refs`, and `reproducible_environment`. `completed` is a deprecated compatibility alias for `agent_completed`; `success` is a deprecated compatibility alias for `evaluation_passed`. `failure_cases.json` contains replayable `FailureCaseRecord` entries for failed tasks, including expected/actual changed files, verification booleans, repair telemetry, policy blocks, trace refs, and a bounded trace summary; it is evaluator-internal and does not become model-visible during the original run. Suite summaries include evaluator success rate, verification pass rate, average turns, average tool calls, agent-completed count, failure reasons, repair attempt/execution count, policy blocks, and miscompletion count. With `--baseline-result`, or when a previous run result is available under the same output root, the runner also writes `regression.json` and `regression.md`.

`docs/evaluation/capability-regression-tasks.json` is the main multi-task capability regression manifest. It currently covers four task classes: `simple_patch`, `multi_file_reasoning`, `failure_repair`, and `completion_gate`. `docs/evaluation/capability-fix-math-test-only.json` is the focused smoke manifest.

`docs/evaluation/evaluation-baseline-example.json` is a sanitized result example. It is safe to reference in docs and regression-shape tests because it does not include provider credentials, raw prompts, raw traces, or private artifacts. Real evaluation runs should continue writing under `work/evaluations/`.

Legacy `eval live run`, `eval live private`, `eval live quicksort`, `evaluation.live_agent_task_set/v1`, `evaluation.live_agent_eval_result/v1`, and `docs/evaluation/live-*` files remain readable compatibility aliases for historical reports and manifests. New code, docs, manifests, tests, and CLI examples must use the canonical evaluation/benchmark names above.

Run A/B or regression checks:

```bash
singularity-agent eval ab run golden.json \
  --baseline-profile-json "{\"name\":\"baseline\",\"model\":\"gpt-a\",\"memory_enabled\":true,\"allowed_tools\":[\"read_file\",\"run_verification\"],\"tool_policy\":\"read_write\"}" \
  --candidate-profile-json "{\"name\":\"candidate\",\"model\":\"gpt-b\",\"prompt_profile\":\"compact\",\"memory_enabled\":false,\"allowed_tools\":[\"read_file\"],\"tool_policy\":\"read_only\"}"

singularity-agent eval regression run golden.json \
  --baseline-profile-json "{\"name\":\"baseline\",\"model\":\"gpt-a\",\"memory_enabled\":true,\"allowed_tools\":[\"read_file\",\"run_verification\"],\"tool_policy\":\"read_write\"}" \
  --candidate-profile-json "{\"name\":\"candidate\",\"model\":\"gpt-b\",\"prompt_profile\":\"compact\",\"memory_enabled\":false,\"allowed_tools\":[\"read_file\"],\"tool_policy\":\"read_only\"}" \
  --threshold 0.05 --block-on-regression
```

Reports are written to `work/evaluations/<run_id>/report.json` and `work/evaluations/<run_id>/report.md` unless `--output-dir` is provided. Regression runs also write `regression.json` and `regression.md` in the same run directory.

## Trace Replay

Trace replay reads `events.jsonl`, `spans.jsonl`, and `artifacts.jsonl` from an existing trace run directory. It produces deterministic replay classification, metrics, score inputs, trace input digest, and a result hash for the same trace input, fixed profile, and fixed replay policy.

Replay does not promise deterministic remote model output. Read-only and idempotent events are classified as replayable. Command, mutation, edit, and patch side-effect events are simulated by default rather than executed again.

## Scoring

Task results include:

- `status`
- `score`
- `subscores`
- `evidence`
- `failure_reasons`

The scoring engine combines test, assertion, diff, and heuristic outcomes with task-defined weights. Test evidence comes from verification summaries or trace replay. Heuristics can include trace metrics, intervention counts, policy denials, planner completion evidence, and patch quality.

Patch quality evaluates:

- diff size
- changed file count
- complexity
- minimal modification
- redundant code markers
- test pass state

## A/B And Regression Reports

An `EvaluationProfile` fixes the model, prompt profile, memory setting, allowed tools, and tool policy:

```json
{
  "name": "candidate",
  "model": "gpt-b",
  "prompt_profile": "compact",
  "memory_enabled": false,
  "allowed_tools": ["read_file"],
  "tool_policy": "read_only"
}
```

Regression detection compares baseline and candidate profile reports over the same Golden Task Set. It reports per-metric and per-task regressions for success rate, average score, latency, cost, tool calls, and intervention rate. `--block-on-regression` exits non-zero when regressions exceed the threshold.

In trace replay mode, profile changes affect deterministic replay classification, allowed-tool policy checks, component overrides, and report comparison. They do not imply a fresh remote model call. Use `--execute` plus a full component integration when a benchmark must exercise real model or prompt behavior.

## Report Fields

JSON reports include the complete machine-readable evaluation record:

- success rate
- average score
- cost
- latency
- tool calls
- intervention count and rate
- per-profile metrics
- per-task status, score, evidence, and failures
- per-task Golden Task contract evidence when declared
- failure taxonomy counts
- previous-run metric comparison when an earlier report exists in the same output root
- report hash

Markdown reports are human-readable summaries with suite metrics, profile metrics, per-task status/score/failure rows, and a `Golden Task Evidence` section for tasks that declare `golden_contract`. Use JSON when exact evidence, replay payloads, component overrides, execution evidence, or report hashes are required.

Regression reports attach an opaque `trace_artifact_ref` to each regression record. These refs are stable handles for trace/report correlation and do not expose local absolute paths.
