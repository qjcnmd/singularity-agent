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
