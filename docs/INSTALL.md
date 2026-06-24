# Singularity Install

Singularity is packaged as a local Python CLI. Install it from a checkout with:

```bash
python -m pip install .
```

For isolated user-level installs, prefer:

```bash
pipx install .
```

The console script is:

```bash
singularity-agent
sg
```

It resolves to `singularity.cli:main`, so installed usage does not depend on running inside the source checkout.

## Component Home

By default Singularity stores user-level component data under the platform user data directory, then creates this layout:

```text
config/
state/
cache/
logs/
traces/
memory/
eval/
backups/
tmp/
```

Override the component root for tests, portable installs, or isolated runs:

```bash
SINGULARITY_HOME=/path/to/singularity-home singularity-agent system init
```

Portable mode keeps the component under the current project directory:

```bash
singularity-agent system init --mode portable
```

Development mode keeps the component under `.singularity/` in the current checkout:

```bash
singularity-agent system init --mode development
```

## First Run

Initialize once:

```bash
singularity-agent system init
```

The command is idempotent. Existing config and manifest files are preserved unless `--force` is passed.

Check the installation:

```bash
singularity-agent version
singularity-agent doctor
```

Machine-readable variants:

```bash
singularity-agent version --json
singularity-agent doctor --json
```

## Optional Features

Core CLI dependencies are installed by default. Optional extras are separated by feature:

```bash
python -m pip install ".[eval]"
python -m pip install ".[devtools]"
```

The `sandbox` extra currently has no additional Python dependency; sandbox capability is checked at component.
