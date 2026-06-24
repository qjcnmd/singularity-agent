# ADR 0006: Singularity Project Identity

Status: Accepted

## Context

The project is a local coding agent component with a Python CLI baseline and a future desktop architecture. The public identity, package namespace, commands, environment variables, user data directories, and documentation should be consistent.

## Decision

Use Singularity as the only active project identity.

Names:

- package: `singularity`
- primary CLI: `singularity-agent`
- short CLI alias: `sg`
- environment prefix: `SINGULARITY_`
- project component directory: `.singularity/`
- user configuration: `~/.config/singularity/`
- user data: `~/.local/share/singularity/`
- user cache: `~/.cache/singularity/`

Do not create a bare `singularity` executable because it conflicts with existing Singularity container tooling.

## Consequences

- Source imports use `singularity.*`.
- Component config files use `singularity.json`.
- Schema ids use `https://singularity.local/...`.
- Documentation and tests use Singularity-only naming.
