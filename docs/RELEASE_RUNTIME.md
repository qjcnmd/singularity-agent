# Release Runtime

The Release Runtime makes Singularity usable as an installed local CLI instead of a source-directory-only tool.

It owns package metadata, user runtime directories, initialization, version output, health checks, repair, export, and safe uninstall. It does not implement a Git Runtime and has no branch, commit, PR, or push commands.

## Modules

```text
src/singularity/release/metadata.py
src/singularity/release/paths.py
src/singularity/release/init.py
src/singularity/release/migrations.py
src/singularity/release/doctor.py
src/singularity/release/repair.py
src/singularity/release/models.py
```

## Directory Layout

`RuntimePaths` resolves the runtime root from, in order:

1. `--home` when a release command provides it.
2. `SINGULARITY_HOME`.
3. `--mode development` as `<project>/.singularity`.
4. `--mode portable` as `<current-directory>/.singularity`.
5. Platform user data directory via `platformdirs` with a stdlib fallback.

The runtime root contains:

```text
config/singularity.json
state/runtime-manifest.json
cache/
logs/
traces/
memory/
eval/
backups/
tmp/
```

## CLI

```bash
singularity version [--json]
singularity-agent doctor [--json]
singularity-agent system init [--force]
singularity-agent system migrate
singularity-agent system repair
singularity-agent system export --output user-data.zip
singularity-agent system uninstall --dry-run
singularity-agent system uninstall --purge-user-data --yes
```

`doctor` is read-only. It reports Python compatibility, source-vs-installed execution, directory access, config schema, critical runtime config, optional dependencies, and pending migrations.

`repair` creates missing runtime directories and missing default files. It does not overwrite an existing config.

`uninstall` protects `memory/`, `traces/`, `eval/`, and `logs/` by default. Those paths are only deleted when `--purge-user-data` is explicit and destructive execution is confirmed with `--yes` or the interactive prompt.

`export` writes a zip with relative paths and a manifest, avoiding embedded absolute runtime-root paths.
