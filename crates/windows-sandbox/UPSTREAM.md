# Upstream provenance

## Source

- Repository: `https://github.com/openai/codex`
- Upstream subtree: `codex-rs/windows-sandbox-rs`
- Extraction commit: `1f0566d3f59298d1bb88820a0d35294f1eeb07ea`
- Commit date: `2026-07-09`
- Extraction date: `2026-07-10`
- Official verification commit: [`56395bddaf26eb2829387ca6a417bf9128e5b239`](https://github.com/openai/codex/commit/56395bddaf26eb2829387ca6a417bf9128e5b239), verified `2026-07-18`
- Upstream license: Apache License 2.0

The upstream `LICENSE` and `NOTICE` files are preserved verbatim in this directory.

## Upstream files included and adapted

- `build.rs`
- `codex-windows-sandbox-setup.manifest` (renamed to `singularity-windows-sandbox-setup.manifest`)
- `src/acl.rs`
- `src/allow.rs`
- `src/audit.rs`
- `src/cap.rs`
- `src/deny_read_acl.rs`
- `src/deny_read_resolver.rs`
- `src/deny_read_state.rs`
- `src/desktop.rs`
- `src/dpapi.rs`
- `src/elevated/ipc_framed.rs`
- `src/elevated/mod.rs`
- `src/elevated/runner_client.rs`
- `src/elevated/runner_pipe.rs`
- `src/elevated_impl.rs`
- `src/env.rs`
- `src/helper_materialization.rs`
- `src/hide_users.rs`
- `src/identity.rs`
- `src/lib.rs`
- `src/logging.rs`
- `src/path_normalization.rs`
- `src/proc_thread_attr.rs`
- `src/process.rs`
- `src/resolved_permissions.rs`
- `src/sandbox_utils.rs`
- `src/setup.rs`
- `src/setup_error.rs`
- `src/spawn_prep.rs`
- `src/ssh_config_dependencies.rs`
- `src/token.rs`
- `src/wfp.rs`
- `src/wfp/filter_specs.rs`
- `src/wfp_setup.rs`
- `src/winutil.rs`
- `src/workspace_acl.rs`
- `src/bin/command_runner/main.rs`
- `src/bin/command_runner/win.rs`
- `src/bin/command_runner/win/cwd_junction.rs`
- `src/bin/setup_main/main.rs`
- `src/bin/setup_main/win.rs`
- `src/bin/setup_main/win/firewall.rs`
- `src/bin/setup_main/win/read_acl_mutex.rs`
- `src/bin/setup_main/win/sandbox_users.rs`
- `src/bin/setup_main/win/setup_runtime_bin.rs`
- `src/bin/setup_main/win/setup_runtime_bin_tests.rs`

## Upstream surfaces intentionally omitted

- `BUILD.bazel`
- `sandbox_smoketests.py` (targets the upstream product CLI and wrapper surface)
- `src/conpty/**`
- `src/stdio_bridge.rs`
- `src/stdio_bridge_tests.rs`
- `src/unified_exec/**`
- `src/wrapper.rs`
- `src/wrapper_tests.rs`

The port does not depend on `codex-utils-pty`, `codex-otel`, the upstream protocol crate, or the upstream absolute-path/string utility crates.

The omitted product surface also includes Codex PTY/ConPTY sessions, managed proxy and unified-exec policy, GUI/desktop application integration, multi-session services, Codex configuration/protocol compatibility, and telemetry. Singularity retains only the private Windows desktop used as a process-isolation primitive; it does not expose Codex GUI or desktop integration.

## Local files

- `src/absolute_path.rs`: minimal absolute-path boundary type required by the extracted code.
- `src/permissions.rs`: canonical filesystem/network permission types required by the Windows sandbox; this is not a copy of the complete upstream protocol crate.
- `src/product_identity.rs`: the single source of product-owned account, directory, helper, desktop, pipe, mutex, firewall, WFP name, and WFP GUID values.
- `src/string_utils.rs`: the two small string helpers required by logging and setup errors.
- `tests/product_identity.rs`
- `tests/security_contract.rs`
- `tests/source_boundaries.rs`

## Functional modifications

1. Renamed the package/library to `singularity_windows_sandbox` and the release binaries to `singularity-windows-sandbox-setup` and `singularity-command-runner`.
2. Replaced all runtime account, group, directory, helper, desktop, named-pipe, mutex, firewall, WFP names, and persistent WFP GUIDs with Singularity-owned identifiers centralized in `product_identity.rs`.
3. Removed OpenAI Codex application/runtime path discovery. Runtime ACL setup now checks only Singularity-owned runtime roots.
4. Reduced elevated IPC and the command runner to non-PTY capture: spawn, output, terminate, exit, and error frames only.
5. Removed upstream telemetry integration. WFP setup failures now fail closed instead of being logged and ignored.
6. Preserved dual offline/online accounts. Restricted network policy selects the elevated offline identity; the unelevated restricted-token fallback rejects restricted network as unsupported.
7. Kept private desktop support and made it the default in `ElevatedSandboxProfileCaptureRequest::new`.
8. Changed the strict world-writable audit path to propagate incomplete ACL scans and deny-ACE failures instead of treating them as success.
9. Replaced upstream protocol and absolute-path dependencies with the bounded local types listed above.
10. In Cargo test layouts, helper discovery also checks the profile directory above `deps`; installed and release layouts still prefer direct siblings or packaged resources.
11. Extended the elevated capture request with additive read roots so the product adapter can grant host toolchain directories read/execute access without replacing Codex's default readable roots or widening writable roots.
12. Made child-process containment fail closed: both elevated and unelevated launch paths create and configure a kill-on-close Job Object before spawning, create the child suspended, assign it to the Job Object before resuming its primary thread, and terminate the whole Job Object on cancellation or timeout.
13. Capture cleanup closes or terminates the kill-on-close Job Object before joining stdout/stderr readers, including normal parent exit; the elevated runner also terminates the Job Object when its control transport reaches EOF or returns a read error, and its process, pipe, and Job handles have explicit early-return cleanup.
14. Preserved Codex's dedicated Sandbox Users read principal, asynchronous read-grant helper, and best-effort `NUL` compatibility grant. Deny-read remains synchronous on that same principal. Its persistent current-set reconciliation is strengthened with a global cross-process state mutex, atomic replacement, versioned runtime-ACE ownership, and a second native mutex held across setup plus the complete Job Object lifetime so concurrent workspaces cannot revoke a live child's protection. Legacy paths without provable ownership are retained as unmanaged and never revoked. The `WRITE_RESTRICTED` fallback explicitly rejects deny-read overrides.
15. Added the upstream optional `missing_path_behavior=skip` filesystem-entry field for generated workspace metadata protections. Windows writable-root projections remove only these default skip entries, protect metadata objects that already exist, and retain explicit missing deny entries for fail-closed materialization. The elevated write-root ACL refresh now checks both principals from one target DACL snapshot, ignores inherited stale delete-child grants, and starts command runners without loading a user profile while polling startup readiness at 5 ms.
16. The current upstream `cwd_junction.rs` still derives its junction root from `USERPROFILE`, which is unavailable as a usable sandbox path when the no-profile runner is started with `CreateProcessWithLogonW` flags `0`. The local runner instead uses the existing explicit `SpawnRequest.sandbox_home` (already the canonical `.sandbox` directory) and creates junctions below `sandbox_home\cwd`; this is a necessary local difference, not an upstream fix.
17. The local workspace observer uses the Win32 `ReadDirectoryChangesW` contract with typed `FILE_ACTION_*` records; `ERROR_NOTIFY_ENUM_DIR`, zero-byte completion, malformed records, cancellation gaps, and buffer overflow remain `Unknown`. The product adapter performs the only path-only projection, with bounded `Added`/`Modified` and ancestor coalescing; a complete same-path `Added` then `Removed` lifecycle is folded back to no change, while replacement, rename, security-only, or unpaired events remain `Changed`/`Unknown`. This is a local integration rule, not an upstream Codex change; no USN journal, generic watcher service, or background observer was adopted.

## Selective upstream delta verification

The following upstream commits were resolved to complete SHAs and reviewed from their official diffs against the verification commit above. They were not cherry-picked as a directory or as blind commits.

The previous verification point `82b294c73c902a4c51f789ba68bb599f0065616f` is an ancestor of the current verification commit, and the path-limited diff for `codex-rs/windows-sandbox-rs` between them is empty. No additional Windows sandbox change therefore required migration in that interval.

- [`87f3e39fdf7e676d0ba25b0587f78e5b85e695e2`](https://github.com/openai/codex/commit/87f3e39fdf7e676d0ba25b0587f78e5b85e695e2) — not applicable. The upstream `ConsoleMode::NoWindow` path hides the upstream `--codex-run-as-fs-helper` filesystem helper and also changes the omitted upstream `unified_exec` surface. This crate has no filesystem-helper argument contract or `src/unified_exec`; its ordinary `SpawnRequest.command` is a product command. The local setup helper already uses `CREATE_NO_WINDOW`/`SW_HIDE` in `src/setup.rs`, so no parallel console-mode API was added.
- [`abbb8c569cbda65fd75f1a51fcc8dd99ced199fa`](https://github.com/openai/codex/commit/abbb8c569cbda65fd75f1a51fcc8dd99ced199fa) — excluded as not applicable. The Windows-sandbox part is a proxy-enforcement validation in the upstream `src/unified_exec/mod.rs` and depends on upstream config and cross-crate proxy contracts absent from this crate. No partial proxy interface or fallback behavior was added.
- [`3370181ec6e3227a922257497bdd28f3e8e76144`](https://github.com/openai/codex/commit/3370181ec6e3227a922257497bdd28f3e8e76144) — applied in local form. `src/setup.rs` now coalesces identical serialized setup payloads through `SETUP_FLIGHTS`, shares structured `SetupFailure` results, removes completed flights, and covers one in-flight helper execution with `identical_setup_requests_share_one_in_flight_run`.
- [`4bc2c723efc6445ea833e2ffd1b11e48298fb8f4`](https://github.com/openai/codex/commit/4bc2c723efc6445ea833e2ffd1b11e48298fb8f4) — excluded as not applicable. The upstream revert is also in the absent `src/unified_exec/mod.rs`; `crates/sandbox/src/lib.rs` already attempts elevated capture first for every Windows command, but no managed-proxy field or proxy product contract reaches this crate, so that elevated-first behavior is supporting evidence rather than a proxy-specific port.
- [`87f71e35b86cc4d2da4d81728004adac45a9dd3a`](https://github.com/openai/codex/commit/87f71e35b86cc4d2da4d81728004adac45a9dd3a) — applied in local form. The permission and Windows sandbox types preserve the stable path variants while carrying optional skip-missing metadata entries; projection removes only generated defaults and keeps explicit deny materialization.
- [`bd92b056ddd91bd7c2ecfea3d8773f7eb5a879a6`](https://github.com/openai/codex/commit/bd92b056ddd91bd7c2ecfea3d8773f7eb5a879a6) — applied in local form through the shared pinned-handle ACL target path. Required effective write rights remain checked, while stale `FILE_DELETE_CHILD` refresh detection considers explicit allow ACEs only so inherited grants do not trigger non-converging repairs.
- [`a26f219f6788c951dcb3bf435fab4c6d0f4d2f40`](https://github.com/openai/codex/commit/a26f219f6788c951dcb3bf435fab4c6d0f4d2f40) — applied in local form for the applicable Windows helper behavior: one DACL snapshot checks the sandbox-group and root-capability SIDs, `CreateProcessWithLogonW` omits profile loading, and runner readiness polling uses 5 ms. The upstream Bazel and unified-exec test surfaces are absent here.
- [`f0c30e528a54bdf0fa9a4d52ff74b34383434811`](https://github.com/openai/codex/commit/f0c30e528a54bdf0fa9a4d52ff74b34383434811) — its CI and secure-devcontainer configuration place Cargo artifacts in a dedicated build root. Singularity adopts only that external-build-root invariant in the product adapter's `isolated` environment, partitioned by canonical workspace; it is not copied into this Windows platform crate and adds no Codex CI, container, configuration, or service surface.

`singularity_sandbox::WindowsSandboxBackend` is the product adapter for this crate. It maps the runtime filesystem and network policy into the permission profile above, tries the elevated path first, permits the restricted-token path only when it can enforce the requested profile, and otherwise fails closed.
