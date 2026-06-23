# Evaluation Runtime

Singularity includes a local-first Evaluation/Benchmark Runtime for replayable benchmark suites, trace replay, A/B profile comparison, regression detection, and report generation. It is an orchestration layer: executable hooks, test checks, and workspace materialization go through the existing command, verification, mutation, trace, memory, and planner boundaries when execution is explicitly enabled.

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

Each task must use one difficulty tag: `easy`, `medium`, or `hard`. Optional workload tags include `memory-heavy` and `tool-heavy`. Task versions currently support `v1` and `v2`.

Evaluation hooks are declarative. Command hooks are intended to run through `CommandRuntime` or `VerificationRuntime` when integrated into a real execution pipeline; the evaluation CLI does not bypass those runtimes.

`archive_path` is accepted in the schema, but execution is fail-closed until Singularity has a controlled archive restore adapter that stages files and applies them through `MutationRuntime`.

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

Suite, A/B, and regression commands default to deterministic offline scoring and trace replay. Add `--execute` only when the task needs executable hooks, test commands, assertions against a prepared workspace, or inline-file snapshot materialization. The CLI execution path uses `CommandRuntime` and `VerificationRuntime`; full kernel-boot execution remains responsible for richer planner, memory, tool, and mutation wiring.

Run the optional live-provider E2E smoke benchmark:

```bash
singularity-agent eval live quicksort --json
```

This command creates a controlled workspace under `work/evaluations-live/`, boots the real CLI kernel with the configured OpenAI-compatible provider, asks the agent to create and verify `quicksort.py`, and then independently runs `python quicksort.py`. Unlike trace replay, this path can make live model calls and should be run only when `SINGULARITY_API_KEY`, `SINGULARITY_MODEL`, and `SINGULARITY_BASE_URL` are intentionally configured.

Run a manifest-driven live-provider eval:

```bash
singularity-agent eval live run docs/evaluation/live-agent-minimal-tasks.json --json
```

The live manifest schema is intentionally small: each task declares `task_id`, a `repo` or `fixture` workspace, optional `start_commit` or `prepare_commands`, `user_task`, `allowed_paths`, `verification_command`, and `success`. Each task runs in an isolated directory under `work/evaluations-live/<run_id>/<task_id>/workspace`; the runner boots the real Singularity kernel, runs the independent verification command, and writes `result.json` with completion, test, usage, cache-hit, tool-call, changed-file, duration, and error fields. Default pytest does not run this path; use the command above only when live provider environment variables are intentionally configured.

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

In trace replay mode, profile changes affect deterministic replay classification, allowed-tool policy checks, runtime overrides, and report comparison. They do not imply a fresh remote model call. Use `--execute` plus a full runtime integration when a benchmark must exercise model or prompt behavior live.

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
- report hash

Markdown reports are human-readable summaries with suite metrics, profile metrics, per-task status/score/failure rows, and a `Golden Task Evidence` section for tasks that declare `golden_contract`. Use JSON when exact evidence, replay payloads, runtime overrides, execution evidence, or report hashes are required.

Regression reports attach an opaque `trace_artifact_ref` to each regression record. These refs are stable handles for trace/report correlation and do not expose local absolute paths.
