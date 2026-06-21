# Local Plugin Runtime

MiniHarness plugins are local project or user extensions. They are disabled by default and are loaded only after manifest, compatibility, permission, config, status, hash, and policy checks pass.

Use:

```text
miniharness plugin list --json
miniharness plugin inspect <id> --json
miniharness plugin check [id] --json
miniharness plugin enable <id> --json
miniharness plugin disable <id> --json
```

Place project plugins under:

```text
.miniharness/plugins/<plugin-id>/
  plugin.toml
  plugin.py
```

The manifest can also be named `miniharness-plugin.toml`. See `docs/architecture/plugin-runtime.md` for the security model, manifest fields, and a complete minimal tool plugin.
