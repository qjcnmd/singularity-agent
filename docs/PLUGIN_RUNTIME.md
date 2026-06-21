# Local Plugin Runtime

Singularity plugins are local project or user extensions. They are disabled by default and are loaded only after manifest, compatibility, permission, config, status, hash, and policy checks pass.

Use:

```text
singularity plugin list --json
singularity plugin inspect <id> --json
singularity plugin check [id] --json
singularity plugin enable <id> --json
singularity plugin disable <id> --json
```

Place project plugins under:

```text
.singularity/plugins/<plugin-id>/
  plugin.toml
  plugin.py
```

The manifest can also be named `singularity-plugin.toml`. See `docs/architecture/plugin-runtime.md` for the security model, manifest fields, and a complete minimal tool plugin.
