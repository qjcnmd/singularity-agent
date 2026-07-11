# Upstream provenance

## Source

- Repository: `https://github.com/openai/codex`
- Upstream subtree: `codex-rs/windows-sandbox-rs`
- Commit: `1f0566d3f59298d1bb88820a0d35294f1eeb07ea`
- Commit date: `2026-07-09`
- Extraction date: `2026-07-10`
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

`singularity_sandbox::WindowsSandboxBackend` is the product adapter for this crate. It maps the runtime filesystem and network policy into the permission profile above, tries the elevated path first, permits the restricted-token path only when it can enforce the requested profile, and otherwise fails closed.
