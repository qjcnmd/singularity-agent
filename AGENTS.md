# Singularity Agent Instructions

## Repo Map First

Before handling a task in this repository, read `.codex/repo-map.json` first and use it to choose the smallest relevant file set. Do not default to scanning the whole repository.

Use the repo map to identify:

- likely entrypoints for the task
- modules that define relevant classes, functions, imports, and exports
- nearby tests that match the target subsystem

Only after narrowing the scope should you read source files. Prefer targeted reads over broad `rg --files` or full-directory scans.

## Repo Map Maintenance

The repo map is maintained with the `repo-mapping` skill as a local cache and stored at:

```text
.codex/repo-map.json
```

If the map is missing, stale, or clearly inconsistent with the current task, refresh it before relying on it. Use `ast-grep` through the `repo-mapping` skill. Do not install or update mapping dependencies without explicit user approval.

When source files are added, removed, or renamed, refresh the repo map locally before relying on it. Do not commit `.codex/repo-map.json`; it is intentionally ignored to avoid large generated diffs.

## Default Skip Paths

Do not read these paths unless the task specifically concerns runtime state, caches, artifacts, or environment setup:

- `.git/`
- `.singularity/`
- `.venv/`
- `.pytest_cache/`
- `.ruff_cache/`
- `outputs/`
- `work/`
- `__pycache__/`

Do not read `.env` unless the user explicitly asks for environment diagnosis and confirms that sensitive values may be inspected.

## Scope Discipline

For code tasks, start from the mapped subsystem and its tests. Keep edits inside the task boundary, preserve existing runtime layering, and avoid unrelated cleanup.

## Naming Discipline

Use mainstream domain terms for production runtime objects, files, schemas, and CLI commands. Evaluation code must use `evaluation`, `eval`, `evaluator`, `evaluation harness`, `benchmark`, `task`, `task set`, `result`, `report`, `runner`, or `experiment` as appropriate.

Do not introduce prompt-polluted names, explanatory static fields, duplicate alias fields, or bespoke names when a mainstream term already exists. Retired evaluation aliases from the previous naming cleanup have been removed from current code, schemas, manifests, docs, tests, and CLI examples; do not reintroduce them as compatibility surfaces.

## Documentation Scope

Repository documentation describes only the current runtime structure, complete fields, call chains, schemas, CLI entrypoints, and data flow present in the source tree. Do not keep historical phase reports, roadmap reports, production review reports, old manifest copies, old runtime docs, or compatibility notes in the worktree. Historical facts belong in git history, not in current docs.

## Runtime Flow Docs

Module-level Runtime Flow Docs live under:

```text
docs/architecture/modules/
```

Any change that modifies core runtime objects, object fields, call chains, model-visible schemas, tool result envelopes, policy or approval behavior, trace or audit payloads, context assembly, prompt framing, model request construction, compaction, observation storage, planner or replanner behavior, failure recovery, or artifact/long-result handling must update the corresponding Runtime Flow Doc in the same change.

Runtime Flow Docs must stay source-backed. Do not document fields, objects, functions, or call chains that cannot be located in the current source tree. Current implementation details and proposed production-grade target structure must be separated clearly.

Any change to code structure, dataclass fields, public CLI commands, schema versions, trace event payloads, evaluation manifests, or result/report schemas must update the corresponding Runtime Flow Doc in the same change. Runtime Flow Docs must treat source code as the only authority, list complete fields for documented runtime dataclasses, and avoid historical aliases or retired compatibility layers.

Before finishing a runtime-sensitive change, run:

```text
python scripts/verify_runtime_docs.py
```

Final responses for code changes in this repository must state:

- which source files changed;
- which `docs/architecture/modules/*.md` files were updated;
- if no Runtime Flow Doc changed, why the change does not affect documented runtime flow.
## Mandatory Real Model Validation

Singularity is a production-grade local CLI coding agent harness. Any change that affects agent capability, execution behavior, model interaction, prompt assembly, context management, tool exposure, tool execution, planner behavior, repair flow, verification flow, evaluation harness, CLI task execution, tracing, reporting, policy/approval, or benchmark behavior must be validated with at least one real model call.

Fake providers, mock providers, unit tests, and synthetic harness tests are allowed as supporting tests, but they do not satisfy final validation for agent-capability changes.

Required rule:

1. Before finalizing the task, run the relevant unit/static checks and at least one real-model Singularity agent validation through the real execution chain.
2. The real validation must enter the actual AgentLoop path, such as:
   `KernelBootstrap -> AgentGraphBuilder -> AgentKernel -> AgentLoop.run`.
3. Do not bypass AgentLoop by directly invoking Planner, ToolExecutor, VerificationRunner, FailureAnalyzer, RepairPlanner, or EvaluationHarness internals to claim real agent validation.
4. Use the project’s existing `.env` / configuration loading path for provider credentials. Never print, copy, commit, expose, or include API keys or secrets in logs, reports, traces, markdown, screenshots, or final output.
5. When checking environment readiness, only report redacted status, for example:
   `SINGULARITY_API_KEY=present(redacted)`, `SINGULARITY_BASE_URL=present`, `SINGULARITY_MODEL=present`.
6. For benchmark/evaluation work, run a real evaluation benchmark whenever possible, preferably:
   `python -m singularity.cli eval run docs/evaluation/capability-regression-tasks.json --run-id <meaningful-run-id>`
   or the closest valid project CLI command if the CLI entrypoint differs.
7. For non-evaluation agent changes, run the smallest real task that exercises the changed path, but it must still use the real model provider and real AgentLoop.
8. Final output must include:

   * the exact real-model command run;
   * the redacted provider/model/config status;
   * whether the call entered AgentLoop;
   * result/report/trace artifact paths when produced;
   * status, turn count, tool calls, verification result, and failure summary when available.
9. If the real model validation cannot run, the final output must explicitly classify the blocker:

   * `.env` not found or not loaded;
   * required env var missing;
   * authentication/provider error;
   * base_url/network error;
   * model name/config error;
   * sandbox/permission error;
   * AgentLoop/runtime error;
   * verification failure;
   * user explicitly prohibited real model calls.
10. Do not silently replace real validation with fake-provider tests. If real validation is blocked, say it is blocked and explain the exact blocker. The task is not fully validated until a real model call succeeds or the blocker is fixed.
