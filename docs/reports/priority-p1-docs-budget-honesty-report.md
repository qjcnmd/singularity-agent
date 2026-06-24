# P1 Resolution Report: CLI Docs, Turn Budget, And Capability Honesty

Date: 2026-06-22

## Scope

P1 covered three issues from the prioritized checklist:

- installation and usage docs still showed a bare `singularity` command even though the package registers `singularity-agent` and `sg`;
- the main CLI path used a fixed fallback `max_turns=8`, which was too small for long task loops;
- README, CLI help, ADR, and package metadata still used an over-strong production claim while several capabilities remain planned.

Items 11 and 12 remain intentionally out of scope for this goal: web search and multi-agent orchestration.

## Verification Of Existing Issues

- `pyproject.toml` registers only `singularity-agent` and `sg` console scripts.
- `docs/INSTALL.md`, `docs/evaluation-harness.md`, `docs/PLUGIN_RUNTIME.md`, `docs/RELEASE_RUNTIME.md`, and architecture component docs still contained bare `singularity ...` command examples.
- `src/singularity/config.py` used `8` as the default `max_turns` fallback.
- `README.md`, `src/singularity/cli.py`, `docs/adr/0001-local-first-agent.md`, and `pyproject.toml` used over-strong production wording.

## Implementation

- Added `adaptive_default_max_turns(goal)` with conservative tiers:
  - short/default tasks: `8`;
  - medium tasks: `12`;
  - long roadmap, integration, report, commit, or verification-heavy tasks: `16`.
- Wired the CLI `run` command to pass the adaptive default into `ProductionConfig.from_cli(...)`.
- Preserved precedence: explicit CLI flag, `SINGULARITY_MAX_TURNS`, and `.singularity/config.toml` still override the adaptive default.
- Marked the config source as `default:adaptive` when adaptive fallback is used.
- Updated CLI help to describe `--max-turns` as an override for the adaptive default.
- Replaced the over-strong public production claim with `production-oriented`.
- Updated public command examples from bare `singularity ...` to `singularity-agent ...`.

## Verification

Commands run:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_production_baseline_alignment.py tests\test_cli.py tests\test_docs_consistency.py --basetemp work\pytest-tmp-p1
git diff --check
rg -n "(^|[ `])singularity (version|eval|plugin|index|trace)" docs README.md -g '!docs/reports/codebase-fact-report.md'
```

Results:

- `36 passed`;
- `git diff --check` reported no whitespace errors, only Windows CRLF conversion warnings;
- no remaining over-strong production wording in tracked working files, excluding the unrelated untracked audit report;
- no remaining bare `singularity version/eval/plugin/index/trace` command examples in public docs checked by the search.

## Residual Risk

The adaptive budget is intentionally simple and local to CLI goal shape. It does not implement a full planner-driven dynamic budget manager, but it removes the fixed-short default for long task loops without changing explicit configuration behavior.
