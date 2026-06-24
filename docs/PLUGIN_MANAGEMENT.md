# Local Plugin Management

Singularity plugins are local project or user extensions. They are disabled by default and are loaded only after manifest, compatibility, permission, config, status, hash, and policy checks pass.

Use:

```text
singularity-agent plugin list --json
singularity-agent plugin inspect <id> --json
singularity-agent plugin check [id] --json
singularity-agent plugin enable <id> --json
singularity-agent plugin disable <id> --json
```

Place project plugins under:

```text
.singularity/plugins/<plugin-id>/
  plugin.toml
  plugin.py
```

The manifest can also be named `singularity-plugin.toml`. See `docs/architecture/plugin-management.md` for the security model, manifest fields, and a complete minimal tool plugin.
