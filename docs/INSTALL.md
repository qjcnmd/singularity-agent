# MiniHarness Install

MiniHarness is packaged as a local Python CLI. Install it from a checkout with:

```bash
python -m pip install .
```

For isolated user-level installs, prefer:

```bash
pipx install .
```

The console script is:

```bash
miniharness
```

It resolves to `miniharness.cli:main`, so installed usage does not depend on running inside the source checkout.

## Runtime Home

By default MiniHarness stores user-level runtime data under the platform user data directory, then creates this layout:

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

Override the runtime root for tests, portable installs, or isolated runs:

```bash
MINIHARNESS_HOME=/path/to/miniharness-home miniharness system init
```

Portable mode keeps the runtime under the current project directory:

```bash
miniharness system init --mode portable
```

Development mode keeps the runtime under `.miniharness/` in the current checkout:

```bash
miniharness system init --mode development
```

## First Run

Initialize once:

```bash
miniharness system init
```

The command is idempotent. Existing config and manifest files are preserved unless `--force` is passed.

Check the installation:

```bash
miniharness version
miniharness doctor
```

Machine-readable variants:

```bash
miniharness version --json
miniharness doctor --json
```

## Optional Features

Core CLI dependencies are installed by default. Optional extras are separated by feature:

```bash
python -m pip install ".[eval]"
python -m pip install ".[devtools]"
```

The `sandbox` extra currently has no additional Python dependency; sandbox capability is checked at runtime.
