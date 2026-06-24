# Priority P0 Resolution Report: Live Provider Benchmark / Project Root / Final Report

## Scope

Resolved the P0 blockers from the remaining roadmap:

- Add an optional live-provider end-to-end benchmark.
- Allow the CLI run command to target an explicit workspace root.
- Expand planner final report Markdown into a fuller experiment-style report.

## Changes

- `src/singularity/cli.py`
  - Added `run --project-root`.
  - Added `eval live quicksort`, which creates a controlled benchmark workspace, boots the real kernel with the configured OpenAI-compatible provider, asks the agent to create and verify `quicksort.py`, and independently runs `python quicksort.py`.
- `src/singularity/planner/finalizer.py`
  - Expanded `final_report.md` sections to include objective, outcome, implementation, verification evidence, results, final review, risks, and evidence appendix.
- `README.md`
  - Documented `--project-root` and the optional live quicksort benchmark.
- `docs/evaluation-harness.md`
  - Documented that `eval live quicksort` can make live model calls, unlike deterministic trace replay.
- `tests/test_cli.py`
  - Added coverage for explicit project roots and the live benchmark wrapper with a fake kernel plus independent smoke check.
- `tests/test_planner.py`
  - Added report-section assertions for the expanded Markdown.

## Verification

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_cli.py tests\test_planner.py tests\evaluation\test_models_store.py tests\evaluation\test_scoring_replay_harness.py --basetemp work\pytest-tmp-p0
```

Result:

```text
61 passed, 1 skipped
```

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_docs_consistency.py --basetemp work\pytest-tmp-p0-docs
```

Result:

```text
7 passed
```

```powershell
.\.venv\Scripts\python.exe -m singularity.cli eval live quicksort --help
```

Result: command help rendered successfully.

```powershell
git diff --check
```

Result: no whitespace errors.

## Remaining

The live benchmark is intentionally optional and was not executed against a paid/remote provider during this local verification. It requires intentional `SINGULARITY_API_KEY`, `SINGULARITY_MODEL`, and `SINGULARITY_BASE_URL` configuration.
