# Code Intelligence / Project Index Runtime

Singularity uses `ProjectIndexRuntime` as the read-only code intelligence layer for local coding work. It builds structured project facts so PlannerRuntime, ContextManager, MutationRuntime, and VerificationRuntime do not have to rely only on `grep`, `list_files`, or raw `read_file` output.

## Boundary

`ProjectIndexRuntime` never executes workspace code. Source files, README text, comments, package metadata, and documentation are treated as untrusted workspace data. The runtime records structure, hashes, paths, symbols, imports, test mappings, document sections, freshness, confidence, evidence, trust level, and backend/source metadata.

It does not own Git, branch, commit, PR, push, shell execution, mutation writes, prompt compilation, or command verification. Those remain owned by the existing Git-absent runtime boundary, MutationRuntime, InstructionRuntime, CommandRuntime, VerificationRuntime, PolicyRuntime, and Kernel.

## Storage

The persistent index is stored in SQLite at:

```text
.singularity/index.sqlite
```

Tables include `files`, `project_roots`, `entrypoints`, `config_facts`, `symbols`, `dependencies`, `references`, `call_edges`, `test_mappings`, `doc_sections`, and `index_metadata`.

Every fact carries:

- `freshness`
- `confidence`
- `evidence`
- `trust_level`
- `backend`
- `source`

## Scanning

`WorkspaceScanner` walks only paths contained by the workspace root. It skips noisy or protected directories such as `.git`, `.singularity`, `node_modules`, `dist`, `build`, `.venv`, `venv`, `__pycache__`, `.pytest_cache`, `.mypy_cache`, `target`, `coverage`, `.next`, and `.turbo`.

The scanner records file role, language, size, hash, mtime, binary status, hidden status, and line count while enforcing `max_files`, `max_file_size`, and `max_total_bytes` budgets.

## Language Plugins

The plugin interface supports:

- `detect_project`
- `classify_file`
- `extract_config`
- `extract_entrypoints`
- `extract_symbols`
- `extract_dependencies`
- `extract_references`
- `extract_call_edges`
- `extract_tests`
- `summarize_doc`

Implemented plugins:

- Python through stdlib `ast`.
- JavaScript and TypeScript through conservative static import/export/test conventions.
- Rust through conservative static `Cargo.toml`, `mod`, `use`, `fn`, `struct`, `enum`, `trait`, and `impl` parsing.

`tree-sitter` and LSP are optional backends. They are explicit degraded-mode stubs when unavailable and are not runtime dependencies.

## Runtime Integration

`RuntimeFactory` initializes `ProjectIndexRuntime` after `LocalWorkspaceStateRuntime` and before Policy, Command, Mutation, Verification, Model, Context, and Planner wiring.

The Kernel health check includes the `project_index` component. Bootstrap records a compact index observation into:

- `ContextManager.add_project_index(...)`
- `PlannerRuntime.record_project_index_observation(...)`

The context item is tagged as runtime-authored but untrusted workspace data. Planner stores relevant files and index summary as evidence, but it does not mark those files as inspected. Actual file inspection still requires read tools.

## Mutation And Verification

Before mutation apply, `MutationRuntime` asks the index for impact. Config, entrypoint, generated/vendor, or broad reverse-dependency impact escalates risk and can require review. After mutation apply, MutationRuntime triggers incremental index refresh for changed and deleted files.

`VerificationRuntime` augments its existing path-based `ImpactAnalyzer` with `ProjectIndexRuntime` impact and test mappings. It can create targeted pytest checks when mapped tests exist and the impact is not broad. Stale or low-confidence mappings keep the broader verification path available.

## CLI

```text
singularity-agent index build
singularity-agent index refresh
singularity-agent index explain
singularity-agent index relevant "goal"
singularity-agent index impact <paths...>
singularity-agent index tests <paths...>
```

All commands support `--json`. Index CLI commands build or query the index only; they do not run project code.
