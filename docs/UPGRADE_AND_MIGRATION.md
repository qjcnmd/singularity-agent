# Upgrade And Migration

MiniHarness stores release/runtime upgrade state in:

```text
state/runtime-manifest.json
```

The manifest records:

```json
{
  "app_version": "0.1.0",
  "config_schema_version": 1,
  "memory_schema_version": 1,
  "trace_schema_version": 1,
  "eval_schema_version": 1,
  "last_migration": "001-release-runtime"
}
```

## Migration Rules

Migrations are versioned, idempotent, and run through:

```bash
miniharness system migrate
```

Before each migration, MiniHarness copies `config/` and `state/` into `backups/`. Migration writes use atomic replace. If a migration fails, MiniHarness restores the pre-migration backup.

Migrations do not delete old `traces/`, `memory/`, or `eval/` data by default.

## Doctor

`miniharness doctor` checks for pending migrations without changing files:

```bash
miniharness doctor --json
```

If a migration is pending, run:

```bash
miniharness system migrate
```

## Repair

Use repair for missing directories or missing default files:

```bash
miniharness system repair
```

Repair is conservative. It recreates missing runtime-owned structure and reports permission problems, but it does not silently overwrite user data.

## Data Protection

Default uninstall preserves:

```text
memory/
traces/
eval/
logs/
```

Preview removals first:

```bash
miniharness system uninstall --dry-run
```

Export before destructive cleanup:

```bash
miniharness system export --output miniharness-user-data.zip
```

Delete protected user data only when explicitly requested:

```bash
miniharness system uninstall --purge-user-data --yes
```
