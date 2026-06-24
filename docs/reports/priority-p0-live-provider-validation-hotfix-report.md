# P0 Live Provider Validation Hotfix Report

Date: 2026-06-22

## Scope

After the user provided local provider settings in `C:\Users\Lenovo\Desktop\key.txt`, the live-provider benchmark was run against a real configured model provider. The key file was used only as a temporary process environment source and was not printed, copied, committed, or written into reports.

## Findings From Real Live Run

The first live run proved that the model/provider path was real: the agent created `quicksort.py` and verification executed. It also exposed two component defects that offline tests did not catch:

- model usage metrics such as `output_tokens` were redacted as if they were secrets, causing final report usage aggregation to raise a numeric conversion error;
- finalizing phase policy denied read-only evidence tools, so a model that tried to inspect the generated file before finalizing could make the task fail even after verification passed.

## Fixes

- Preserved safe numeric model usage metrics during trace redaction while still redacting actual secret-bearing keys.
- Made model usage aggregation tolerant of historical traces where token counts may already be redacted.
- Reclassified `get_verification_result` as a read-only analysis action for planner authorization.
- Allowed finalizing phase to use read-only evidence tools needed to inspect generated files and verification state, without allowing mutation or command execution.

## Verification

Commands run:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_planner.py tests\test_trace_redaction.py tests\test_trace_timeline_summary.py tests\test_observability_integration.py tests\test_cli.py::test_eval_live_quicksort_uses_kernel_and_independent_smoke --basetemp work\pytest-tmp-live-hotfix-2
git diff --check
.\.venv\Scripts\python.exe -m singularity.cli eval live quicksort --run-id live_keyfile_p0_pass --max-turns 12 --json
```

Results:

- `41 passed`;
- `git diff --check` reported no whitespace errors, only Windows CRLF conversion warnings;
- live provider benchmark result: `ok=true`;
- live agent status: `completed`;
- component verification status: `ready`;
- independent smoke command: `python quicksort.py`;
- independent smoke exit code: `0`.

## Residual Risk

The benchmark used the provider settings supplied in the local key file. Those settings are intentionally environment-local and are not stored in repository files.
