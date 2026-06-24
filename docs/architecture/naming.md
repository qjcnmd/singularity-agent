# Singularity Naming

Singularity is the only active project identity.

## Public Surfaces

| Surface | Name |
| --- | --- |
| Product | Singularity |
| Python package | `singularity` |
| Primary CLI | `singularity-agent` |
| Short CLI | `sg` |
| Environment prefix | `SINGULARITY_` |
| Project component directory | `.singularity/` |
| User config | `~/.config/singularity/` |
| User data | `~/.local/share/singularity/` |
| User cache | `~/.cache/singularity/` |

Do not expose a bare `singularity` executable. It conflicts with existing Singularity container tooling and some HPC environments.

## Package And CLI

The source package is `src/singularity`. All project imports use `singularity.*`.

Console scripts:

```text
singularity-agent = singularity.cli:main
sg = singularity.cli:main
```

## Environment

Precedence:

```text
explicit CLI flag > SINGULARITY_* > config file > defaults
```

Supported names:

- `SINGULARITY_BASE_URL`
- `SINGULARITY_API_KEY`
- `SINGULARITY_MODEL`
- `SINGULARITY_HOME`
- `SINGULARITY_MODE`
- `SINGULARITY_PLUGIN_PATH`

Secret values must not be logged or copied into generated config files.

## State Paths

Project-local state uses `.singularity/`. Component and release commands use `singularity.json` for generated configuration. JSON schema ids use `https://singularity.local/...`.
