use crate::absolute_path::AbsolutePathBuf;
#[cfg(target_os = "windows")]
use crate::path_safety::WorkspaceRootLease;
use crate::permissions::PermissionProfile;
#[cfg(target_os = "windows")]
use crate::trusted_workspace::TrustedWorkspaceLease;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

pub struct ElevatedSandboxProfileCaptureRequest<'a> {
    pub permission_profile: &'a PermissionProfile,
    pub workspace_roots: &'a [AbsolutePathBuf],
    pub sandbox_home: &'a Path,
    pub command: Vec<String>,
    pub cwd: &'a Path,
    pub env_map: HashMap<String, String>,
    pub timeout_ms: Option<u64>,
    pub cancellation: Option<crate::WindowsSandboxCancellationToken>,
    pub use_private_desktop: bool,
    pub proxy_enforced: bool,
    pub read_roots_override: Option<&'a [PathBuf]>,
    pub additional_read_roots: &'a [PathBuf],
    pub read_roots_include_platform_defaults: bool,
    pub write_roots_override: Option<&'a [PathBuf]>,
    pub deny_read_paths_override: &'a [AbsolutePathBuf],
    pub deny_write_paths_override: &'a [AbsolutePathBuf],
    /// Whether the controller deferred an ACL-private subtree to the sandbox identity.
    pub protected_path_scan_incomplete: bool,
    /// Existing capability-scoped deny-write paths to revoke before trusted preparation.
    ///
    /// This is separate from `deny_write_paths_override`: the latter is the policy to install
    /// for the child, while this collection is only the current, already-existing stale set.
    pub trusted_deny_write_paths_override: &'a [AbsolutePathBuf],
    pub protect_workspace_metadata: bool,
    #[cfg(target_os = "windows")]
    pub workspace_change_monitor: Option<&'a mut Option<crate::WorkspaceChangeMonitor>>,
    #[cfg(target_os = "windows")]
    pub trusted_workspace: Option<&'a TrustedWorkspaceLease>,
    /// Controller-held no-delete workspace root handle kept alive through Job cleanup.
    #[cfg(target_os = "windows")]
    pub workspace_root_lease: Option<&'a WorkspaceRootLease>,
}

impl<'a> ElevatedSandboxProfileCaptureRequest<'a> {
    pub fn new(
        permission_profile: &'a PermissionProfile,
        workspace_roots: &'a [AbsolutePathBuf],
        sandbox_home: &'a Path,
        command: Vec<String>,
        cwd: &'a Path,
        env_map: HashMap<String, String>,
    ) -> Self {
        Self {
            permission_profile,
            workspace_roots,
            sandbox_home,
            command,
            cwd,
            env_map,
            timeout_ms: None,
            cancellation: None,
            use_private_desktop: crate::product_identity::DEFAULT_USE_PRIVATE_DESKTOP,
            proxy_enforced: false,
            read_roots_override: None,
            additional_read_roots: &[],
            read_roots_include_platform_defaults: true,
            write_roots_override: None,
            deny_read_paths_override: &[],
            deny_write_paths_override: &[],
            protected_path_scan_incomplete: false,
            trusted_deny_write_paths_override: &[],
            protect_workspace_metadata: true,
            #[cfg(target_os = "windows")]
            workspace_change_monitor: None,
            #[cfg(target_os = "windows")]
            trusted_workspace: None,
            #[cfg(target_os = "windows")]
            workspace_root_lease: None,
        }
    }
}

mod windows_impl {
    use super::ElevatedSandboxProfileCaptureRequest;
    use crate::absolute_path::AbsolutePathBuf;
    use crate::acl::allow_null_device;
    use crate::allow::compute_allow_paths_for_permissions;
    use crate::cap::workspace_cap_sid_for_cwd;
    use crate::cap::workspace_write_cap_sid_for_root;
    use crate::deny_read_acl::open_existing_git_ancestor;
    use crate::deny_read_state::StateMutex;
    use crate::deny_read_state::reconcile_runner_leases;
    use crate::deny_read_state::register_runner_lease;
    use crate::deny_read_state::try_lock_deny_read_execution;
    use crate::env::ensure_non_interactive_pager;
    use crate::env::inherit_path_env;
    use crate::env::normalize_null_device_env;

    type WorkspaceObservation<T> = Option<(T, T, crate::WorkspaceChangeObservation)>;
    use crate::identity::SandboxCreds;
    use crate::identity::existing_sandbox_creds;
    use crate::identity::observe_as_sandbox_user;
    use crate::identity::refresh_logon_sandbox_creds;
    use crate::ipc_framed::EmptyPayload;
    use crate::ipc_framed::FramedMessage;
    use crate::ipc_framed::Message;
    use crate::ipc_framed::OutputStream;
    use crate::ipc_framed::SpawnRequest;
    use crate::ipc_framed::decode_bytes;
    use crate::ipc_framed::read_frame;
    use crate::ipc_framed::write_frame;
    use crate::logging::log_failure;
    use crate::logging::log_start;
    use crate::logging::log_success;
    use crate::path_normalization::canonicalize_path_allow_missing;
    use crate::path_safety::open_pinned_workspace_path;
    use crate::path_safety::pin_existing_workspace_paths;
    use crate::path_safety::revalidate_existing_pinned_workspace_paths;
    use crate::path_safety::revalidate_pinned_workspace_paths;
    use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
    use crate::runner_client::retry_runner_spawn_once;
    use crate::runner_client::spawn_runner_transport;
    use crate::sandbox_utils::ensure_sandbox_home_exists;
    use crate::sandbox_utils::inject_git_safe_directory;
    use crate::setup::effective_write_roots_for_permissions;
    use crate::setup::gather_read_roots;
    use crate::token::LocalSid;
    use anyhow::{Context, Result};
    use std::collections::HashSet;
    use std::fs::File;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;

    pub use crate::windows_impl::CaptureResult;

    fn cancelled_capture_result() -> CaptureResult {
        CaptureResult {
            exit_code: 1,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
            cancelled: true,
            output_truncated: false,
        }
    }

    /// One resolver result bound to no-follow handles for a single ACL reconciliation pass.
    struct ProtectedSetupPlan {
        deny_read: Vec<PathBuf>,
        deny_write: Vec<PathBuf>,
        pins: Vec<crate::path_safety::PinnedWorkspacePath>,
        _handles: Vec<File>,
    }

    fn resolved_path_keys(paths: &[AbsolutePathBuf]) -> Vec<String> {
        let mut keys = paths
            .iter()
            .map(|path| path.as_path().to_string_lossy().to_ascii_lowercase())
            .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        keys
    }

    fn resolved_path_sets_equal(
        left: &(Vec<AbsolutePathBuf>, Vec<AbsolutePathBuf>),
        right: &(Vec<AbsolutePathBuf>, Vec<AbsolutePathBuf>),
    ) -> bool {
        resolved_path_keys(&left.0) == resolved_path_keys(&right.0)
            && resolved_path_keys(&left.1) == resolved_path_keys(&right.1)
    }

    fn merge_required_protected_paths(
        mut resolved: (Vec<AbsolutePathBuf>, Vec<AbsolutePathBuf>),
        required_read: &[AbsolutePathBuf],
        required_write: &[AbsolutePathBuf],
    ) -> (Vec<AbsolutePathBuf>, Vec<AbsolutePathBuf>) {
        resolved.0.extend_from_slice(required_read);
        resolved.1.extend_from_slice(required_write);
        for paths in [&mut resolved.0, &mut resolved.1] {
            let mut seen = HashSet::new();
            paths.retain(|path| seen.insert(path.as_path().to_string_lossy().to_ascii_lowercase()));
        }
        resolved
    }

    fn prepare_protected_setup_plan(
        mut resolved: (Vec<AbsolutePathBuf>, Vec<AbsolutePathBuf>),
        permissions: &ResolvedWindowsSandboxPermissions,
        cwd: &Path,
        env_map: &std::collections::HashMap<String, String>,
        workspace: &AbsolutePathBuf,
        observer_creds: Option<&SandboxCreds>,
    ) -> Result<ProtectedSetupPlan> {
        let mut targets = resolved.0.clone();
        targets.extend(resolved.1.iter().cloned());
        let mut pins = Vec::new();
        let mut seen_targets = HashSet::new();
        for target in targets {
            let key = target.as_path().to_string_lossy().to_ascii_lowercase();
            if !seen_targets.insert(key) {
                continue;
            }
            match pin_existing_workspace_paths(workspace.as_path(), std::slice::from_ref(&target)) {
                Ok(mut target_pins) => pins.append(&mut target_pins),
                Err(controller_error) => {
                    let creds = observer_creds.ok_or_else(|| {
                        anyhow::anyhow!("sandbox observer credentials are unavailable")
                    })?;
                    let workspace = workspace.clone();
                    let target_path = target.as_path().to_path_buf();
                    let mut target_pins = observe_as_sandbox_user(creds, move || {
                        pin_existing_workspace_paths(
                            workspace.as_path(),
                            std::slice::from_ref(&target),
                        )
                        .map_err(|sandbox_error| {
                            format!(
                                "protected workspace path pinning failed for both trusted and sandbox identities at {}: trusted={controller_error:#}; sandbox={sandbox_error:#}",
                                target_path.display()
                            )
                        })
                    })?;
                    pins.append(&mut target_pins);
                }
            }
        }
        resolved.1.extend(
            compute_allow_paths_for_permissions(permissions, cwd, env_map)
                .deny
                .into_iter()
                .filter(|path| is_git_marker_path(path))
                .map(AbsolutePathBuf::from_absolute_path_checked)
                .collect::<std::io::Result<Vec<_>>>()?,
        );
        let (deny_read, mut handles) = resolve_nested_git_paths(&resolved.0)?;
        let (deny_write, deny_write_handles) = resolve_nested_git_paths(&resolved.1)?;
        handles.extend(deny_write_handles);
        Ok(ProtectedSetupPlan {
            deny_read,
            deny_write,
            pins,
            _handles: handles,
        })
    }

    fn acquire_deny_read_execution_guard(
        sandbox_home: &Path,
        cancellation: Option<&crate::WindowsSandboxCancellationToken>,
    ) -> Result<Option<StateMutex>> {
        loop {
            if cancellation.is_some_and(crate::WindowsSandboxCancellationToken::is_cancelled) {
                return Ok(None);
            }
            if let Some(guard) = try_lock_deny_read_execution(sandbox_home, 50)? {
                loop {
                    if cancellation
                        .is_some_and(crate::WindowsSandboxCancellationToken::is_cancelled)
                    {
                        return Ok(None);
                    }
                    if reconcile_runner_leases(sandbox_home, 50)? {
                        return Ok(Some(guard));
                    }
                    // A live runner lease may outlast this bounded reconciliation attempt. Yield
                    // before retrying so a slow child cannot turn lease coordination into a spin.
                    std::thread::park_timeout(Duration::from_millis(50));
                }
            }
        }
    }

    /// Polls for cancellation and sends the runner's terminate IPC frame when requested.
    ///
    /// The 50 ms park bounds cancellation latency without busy-waiting.
    fn spawn_cancel_writer(
        pipe_write: &File,
        cancellation: Option<crate::WindowsSandboxCancellationToken>,
    ) -> Result<Option<(std::thread::JoinHandle<()>, Arc<AtomicBool>)>> {
        let Some(cancellation) = cancellation else {
            return Ok(None);
        };
        let mut pipe_write = pipe_write.try_clone()?;
        let done = Arc::new(AtomicBool::new(false));
        let done_for_thread = Arc::clone(&done);
        let handle = std::thread::spawn(move || {
            while !done_for_thread.load(Ordering::SeqCst) {
                if cancellation.is_cancelled() {
                    let _ = write_frame(
                        &mut pipe_write,
                        &FramedMessage {
                            version: crate::ipc_framed::IPC_PROTOCOL_VERSION,
                            message: Message::Terminate {
                                payload: EmptyPayload::default(),
                            },
                        },
                    );
                    break;
                }
                std::thread::park_timeout(Duration::from_millis(50));
            }
        });
        Ok(Some((handle, done)))
    }

    /// Launches the command runner under the sandbox user and captures its output.
    #[allow(clippy::too_many_arguments)]
    pub fn run_windows_sandbox_capture_for_permission_profile(
        request: ElevatedSandboxProfileCaptureRequest<'_>,
    ) -> Result<CaptureResult> {
        run_windows_sandbox_capture_for_permission_profile_inner::<
            (),
            fn() -> std::result::Result<(Vec<AbsolutePathBuf>, Vec<AbsolutePathBuf>), String>,
            fn() -> std::result::Result<(), String>,
            fn(crate::WorkspaceChangeObservation) -> std::result::Result<(), String>,
        >(request, None)
        .map(|(capture, _)| capture)
    }

    /// Captures a command between two workspace observations made as its sandbox account.
    pub fn run_windows_sandbox_capture_for_permission_profile_with_observations<T, R, F, G>(
        request: ElevatedSandboxProfileCaptureRequest<'_>,
        protected_path_resolver: R,
        before_observer: F,
        after_observer: G,
    ) -> Result<(CaptureResult, T, T, crate::WorkspaceChangeObservation)>
    where
        R: FnMut() -> std::result::Result<(Vec<AbsolutePathBuf>, Vec<AbsolutePathBuf>), String>
            + Send,
        F: FnOnce() -> std::result::Result<T, String> + Send,
        G: FnMut(crate::WorkspaceChangeObservation) -> std::result::Result<T, String> + Send,
        T: Send,
    {
        let (capture, observations) = run_windows_sandbox_capture_for_permission_profile_inner(
            request,
            Some((protected_path_resolver, before_observer, after_observer)),
        )?;
        let (before, after, change) = observations
            .ok_or_else(|| anyhow::anyhow!("sandbox workspace observer credentials unavailable"))?;
        Ok((capture, before, after, change))
    }

    #[allow(clippy::too_many_arguments)]
    fn run_windows_sandbox_capture_for_permission_profile_inner<T, R, F, G>(
        request: ElevatedSandboxProfileCaptureRequest<'_>,
        observers: Option<(R, F, G)>,
    ) -> Result<(CaptureResult, WorkspaceObservation<T>)>
    where
        R: FnMut() -> std::result::Result<(Vec<AbsolutePathBuf>, Vec<AbsolutePathBuf>), String>
            + Send,
        F: FnOnce() -> std::result::Result<T, String> + Send,
        G: FnMut(crate::WorkspaceChangeObservation) -> std::result::Result<T, String> + Send,
        T: Send,
    {
        let ElevatedSandboxProfileCaptureRequest {
            permission_profile,
            workspace_roots,
            sandbox_home,
            command,
            cwd,
            mut env_map,
            timeout_ms,
            cancellation,
            use_private_desktop,
            proxy_enforced,
            read_roots_override,
            additional_read_roots,
            read_roots_include_platform_defaults,
            write_roots_override,
            deny_read_paths_override,
            deny_write_paths_override,
            protected_path_scan_incomplete,
            trusted_deny_write_paths_override,
            protect_workspace_metadata,
            mut workspace_change_monitor,
            trusted_workspace,
            workspace_root_lease,
        } = request;
        if let Some(root_lease) = workspace_root_lease {
            // The controller owns this handle and keeps it alive until this function returns,
            // which is after runner IPC and Job Object cleanup. Verify it before any ACL side
            // effect; pathname replacement therefore cannot redirect the child to a new root.
            root_lease
                .verify()
                .context("workspace root lease verification failed")?;
        }
        if let Some(trusted_workspace) = trusted_workspace {
            trusted_workspace
                .verify()
                .map_err(|error| anyhow::anyhow!(error.code()))
                .context("trusted workspace lease verification failed")?;
        }
        // Resolve safe aliases once so the execution mutex, setup payload, runner registration,
        // cleanup, and every state-file operation share one long-lived sandbox-home identity.
        let canonical_sandbox_home = canonicalize_path_allow_missing(sandbox_home);
        let sandbox_home = canonical_sandbox_home.as_path();
        if cancellation
            .as_ref()
            .is_some_and(crate::WindowsSandboxCancellationToken::is_cancelled)
        {
            return Ok((cancelled_capture_result(), None));
        }
        let permissions = ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots_with_protected_metadata(
            permission_profile,
            workspace_roots,
            protect_workspace_metadata,
        )?;
        let merged_read_roots = if additional_read_roots.is_empty() {
            None
        } else {
            let mut roots = read_roots_override.map_or_else(
                || gather_read_roots(cwd, &permissions, &env_map, sandbox_home),
                <[PathBuf]>::to_vec,
            );
            roots.extend(additional_read_roots.iter().cloned());
            roots.sort_by_key(|path| path.to_string_lossy().to_ascii_lowercase());
            roots.dedup_by(|left, right| {
                left.to_string_lossy()
                    .eq_ignore_ascii_case(&right.to_string_lossy())
            });
            Some(roots)
        };
        let read_roots_override = merged_read_roots.as_deref().or(read_roots_override);
        let read_roots_include_platform_defaults =
            read_roots_include_platform_defaults && merged_read_roots.is_none();
        normalize_null_device_env(&mut env_map);
        ensure_non_interactive_pager(&mut env_map);
        inherit_path_env(&mut env_map);
        inject_git_safe_directory(&mut env_map, cwd);
        // Use a temp-based log dir that the sandbox user can write.
        let sandbox_base = sandbox_home.join(".sandbox");
        ensure_sandbox_home_exists(&sandbox_base)?;

        let logs_base_dir: Option<&Path> = Some(sandbox_base.as_path());
        log_start(&command, logs_base_dir);
        if cancellation
            .as_ref()
            .is_some_and(crate::WindowsSandboxCancellationToken::is_cancelled)
        {
            return Ok((cancelled_capture_result(), None));
        }
        // The elevated identities share one authoritative read principal. Serialize setup and the
        // complete Job Object lifetime so a concurrent workspace cannot reconcile that principal
        // to a different deny-read set while this child is still alive.
        let Some(_deny_read_execution_guard) =
            acquire_deny_read_execution_guard(sandbox_home, cancellation.as_ref())?
        else {
            return Ok((cancelled_capture_result(), None));
        };
        let mut observer_creds = None;
        let (resolved_by_sandbox, mut observers) = match observers {
            Some((mut resolver, before, after)) => {
                let scan_permissions =
                    ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots_with_protected_metadata(
                        permission_profile,
                        workspace_roots,
                        false,
                    )?;
                let scan_creds = match existing_sandbox_creds(
                    &scan_permissions,
                    proxy_enforced,
                    sandbox_home,
                )? {
                    Some(creds) => creds,
                    None => {
                        if protected_path_scan_incomplete {
                            anyhow::bail!(
                                "protected path scan requires an existing sandbox identity"
                            );
                        }
                        // A first-run setup may establish the account, but it must preserve
                        // the controller's conservative protected-path set instead of
                        // reconciling persisted denies against an empty placeholder.
                        let mut scan_inputs = deny_read_paths_override.to_vec();
                        scan_inputs.extend_from_slice(deny_write_paths_override);
                        let scan_setup_pins = pin_existing_workspace_paths(
                            workspace_roots
                                .first()
                                .ok_or_else(|| {
                                    anyhow::anyhow!("sandbox observation requires a workspace root")
                                })?
                                .as_path(),
                            &scan_inputs,
                        )?;
                        let scan_deny_read_paths = deny_read_paths_override
                            .iter()
                            .map(AbsolutePathBuf::to_path_buf)
                            .collect::<Vec<_>>();
                        let scan_deny_write_paths = deny_write_paths_override
                            .iter()
                            .map(AbsolutePathBuf::to_path_buf)
                            .collect::<Vec<_>>();
                        let mut creds = crate::identity::require_logon_sandbox_creds(
                            &scan_permissions,
                            cwd,
                            &env_map,
                            sandbox_home,
                            read_roots_override,
                            read_roots_include_platform_defaults,
                            write_roots_override,
                            &scan_deny_read_paths,
                            &scan_deny_write_paths,
                            &[],
                            proxy_enforced,
                            crate::WindowsSandboxProxySettingsMode::Reconcile,
                            None,
                        )?;
                        if revalidate_existing_pinned_workspace_paths(&scan_setup_pins)? {
                            let materialized_pins = pin_existing_workspace_paths(
                                workspace_roots
                                    .first()
                                    .expect("workspace root was required above")
                                    .as_path(),
                                &scan_inputs,
                            )?;
                            creds = crate::identity::require_logon_sandbox_creds(
                                &scan_permissions,
                                cwd,
                                &env_map,
                                sandbox_home,
                                read_roots_override,
                                read_roots_include_platform_defaults,
                                write_roots_override,
                                &scan_deny_read_paths,
                                &scan_deny_write_paths,
                                &[],
                                proxy_enforced,
                                crate::WindowsSandboxProxySettingsMode::Reconcile,
                                None,
                            )?;
                            if revalidate_existing_pinned_workspace_paths(&materialized_pins)? {
                                anyhow::bail!(
                                    "protected path remained missing after bounded observer setup"
                                );
                            }
                        }
                        creds
                    }
                };
                let resolved = merge_required_protected_paths(
                    observe_as_sandbox_user(&scan_creds, &mut resolver)?,
                    deny_read_paths_override,
                    deny_write_paths_override,
                );
                observer_creds = Some(scan_creds);
                (Some(resolved), Some((resolver, Some(before), after)))
            }
            None => (None, None),
        };
        // Capture the command baseline before any protected setup can mutate ACLs. The setup
        // checkpoint below may report security-descriptor notifications caused by that setup;
        // the callback reconciles them against this authoritative snapshot rather than treating
        // every notification as a workspace content change.
        let mut before_observation = None;
        if let Some((_, before_observer, _)) = observers.as_mut() {
            let before_observer = before_observer.take().ok_or_else(|| {
                anyhow::anyhow!("sandbox workspace observer baseline already consumed")
            })?;
            before_observation = Some(observe_as_sandbox_user(
                observer_creds.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("sandbox observer credentials are unavailable")
                })?,
                before_observer,
            )?);
        }
        let mut current_resolved = resolved_by_sandbox.unwrap_or_else(|| {
            (
                deny_read_paths_override.to_vec(),
                deny_write_paths_override.to_vec(),
            )
        });
        let workspace = workspace_roots
            .first()
            .ok_or_else(|| anyhow::anyhow!("sandbox setup requires a workspace root"))?;
        let mut protected_setup = prepare_protected_setup_plan(
            current_resolved.clone(),
            &permissions,
            cwd,
            &env_map,
            workspace,
            observer_creds.as_ref(),
        )?;
        let (trusted_deny_write_paths_override, trusted_deny_write_target_handles) =
            if trusted_workspace.is_some() {
                resolve_trusted_deny_write_paths(
                    trusted_deny_write_paths_override,
                    workspace_roots
                        .first()
                        .map(AbsolutePathBuf::as_path)
                        .ok_or_else(|| {
                            anyhow::anyhow!("trusted deny-write cleanup requires a workspace root")
                        })?,
                    trusted_workspace.ok_or_else(|| {
                        anyhow::anyhow!("trusted deny-write cleanup requires a workspace lease")
                    })?,
                    &permissions,
                    cwd,
                    &env_map,
                )?
            } else {
                (Vec::new(), Vec::new())
            };
        // Keep no-follow target handles alive through setup, runner spawn, and Job Object cleanup.
        let _trusted_deny_write_target_handles = trusted_deny_write_target_handles;
        let mut sandbox_creds = crate::identity::require_logon_sandbox_creds(
            &permissions,
            cwd,
            &env_map,
            sandbox_home,
            read_roots_override,
            read_roots_include_platform_defaults,
            write_roots_override,
            &protected_setup.deny_read,
            &protected_setup.deny_write,
            &trusted_deny_write_paths_override,
            proxy_enforced,
            crate::WindowsSandboxProxySettingsMode::Reconcile,
            trusted_workspace,
        )?;
        let included_missing = revalidate_existing_pinned_workspace_paths(&protected_setup.pins)?;
        let next_resolved = match observers.as_mut() {
            Some((resolver, _, _)) => Some(merge_required_protected_paths(
                observe_as_sandbox_user(
                    observer_creds.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("sandbox observer credentials are unavailable")
                    })?,
                    resolver,
                )?,
                deny_read_paths_override,
                deny_write_paths_override,
            )),
            None => None,
        };
        if included_missing
            || next_resolved
                .as_ref()
                .is_some_and(|next| !resolved_path_sets_equal(&current_resolved, next))
        {
            if let Some(next) = next_resolved {
                current_resolved = next;
            }
            protected_setup = prepare_protected_setup_plan(
                current_resolved.clone(),
                &permissions,
                cwd,
                &env_map,
                workspace,
                observer_creds.as_ref(),
            )?;
            sandbox_creds = crate::identity::require_logon_sandbox_creds(
                &permissions,
                cwd,
                &env_map,
                sandbox_home,
                read_roots_override,
                read_roots_include_platform_defaults,
                write_roots_override,
                &protected_setup.deny_read,
                &protected_setup.deny_write,
                &trusted_deny_write_paths_override,
                proxy_enforced,
                crate::WindowsSandboxProxySettingsMode::Reconcile,
                trusted_workspace,
            )?;
            if revalidate_existing_pinned_workspace_paths(&protected_setup.pins)? {
                anyhow::bail!("protected path remained unstable after bounded ACL setup");
            }
        }
        // Setup refresh/elevation is a synchronous external operation and cannot be interrupted
        // safely from this call. Do not continue into ACL mutation or runner creation if it
        // completes after the caller has cancelled.
        if cancellation
            .as_ref()
            .is_some_and(crate::WindowsSandboxCancellationToken::is_cancelled)
        {
            return Ok((cancelled_capture_result(), None));
        }
        // Preserve the initial observer identity for cancellation before runner spawn. A
        // successful credential-refresh retry replaces it with the identity that actually ran.
        let mut used_sandbox_creds = Some(sandbox_creds.clone());
        // Build per-workspace capability SIDs for ACL grants.
        let (sid_for_null, cap_sids) = if permissions.uses_write_capabilities_for_cwd(cwd, &env_map)
        {
            let write_roots = effective_write_roots_for_permissions(
                &permissions,
                cwd,
                &env_map,
                sandbox_home,
                write_roots_override,
            );
            let cap_sids = write_roots
                .iter()
                .map(|root| workspace_write_cap_sid_for_root(sandbox_home, cwd, root))
                .collect::<Result<Vec<_>>>()?;
            if cap_sids.is_empty() {
                anyhow::bail!("workspace-write sandbox has no writable root capability SIDs");
            }
            (LocalSid::from_string(&cap_sids[0])?, cap_sids)
        } else {
            let sid_str = workspace_cap_sid_for_cwd(sandbox_home, cwd)?;
            let sid = LocalSid::from_string(&sid_str)?;
            (sid, vec![sid_str])
        };

        if cancellation
            .as_ref()
            .is_some_and(crate::WindowsSandboxCancellationToken::is_cancelled)
        {
            return Ok((cancelled_capture_result(), None));
        }
        unsafe {
            allow_null_device(sid_for_null.as_ptr());
        }
        if cancellation
            .as_ref()
            .is_some_and(crate::WindowsSandboxCancellationToken::is_cancelled)
        {
            return Ok((cancelled_capture_result(), None));
        }

        if let Some(slot) = workspace_change_monitor.as_deref_mut() {
            // Rollover the cache checkpoint guard before the child starts. Start the command
            // monitor first so there is no finish-to-spawn observation gap. Security-only setup
            // notifications are reconciled by the authoritative before/after snapshot callback;
            // incomplete observations still fail closed.
            let next = crate::WorkspaceChangeMonitor::start(workspace.as_path())?;
            let setup_observation = match slot.take() {
                Some(setup_guard) => setup_guard.finish()?,
                None => crate::WorkspaceChangeObservation::Unchanged,
            };
            if setup_observation == crate::WorkspaceChangeObservation::Unknown {
                anyhow::bail!("workspace observation was incomplete during protected setup");
            }
            if let Some((_, _, after_observer)) = observers.as_mut() {
                let setup_observation = setup_observation.clone();
                let creds = observer_creds.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "sandbox observer credentials are unavailable during protected setup"
                    )
                })?;
                observe_as_sandbox_user(creds, || after_observer(setup_observation))?;
            }
            *slot = Some(next);
        }
        if let Some((resolver, _, _)) = observers.as_mut() {
            let final_resolved = merge_required_protected_paths(
                observe_as_sandbox_user(
                    observer_creds.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("sandbox observer credentials are unavailable")
                    })?,
                    resolver,
                )?,
                deny_read_paths_override,
                deny_write_paths_override,
            );
            if !resolved_path_sets_equal(&current_resolved, &final_resolved) {
                anyhow::bail!("protected path set changed after bounded ACL setup");
            }
        }
        revalidate_pinned_workspace_paths(&protected_setup.pins)?;
        // Keep the no-follow deny-set pins alive through runner spawn, IPC, and Job cleanup.
        // Public certificate-only PEM targets are intentionally skipped by deny-read ACL
        // mutation, but remain pinned so their admitted identity cannot be replaced meanwhile.
        let after_observer = match observers {
            Some((_resolver, before_observer, after_observer)) => {
                if before_observer.is_some() {
                    anyhow::bail!("sandbox workspace observer baseline was not captured");
                }
                Some(after_observer)
            }
            None => None,
        };

        let capture_result = (|| -> Result<CaptureResult> {
            let spawn_request = SpawnRequest {
                command: command.clone(),
                cwd: cwd.to_path_buf(),
                env: env_map.clone(),
                permission_profile: permission_profile.clone(),
                workspace_roots: workspace_roots.to_vec(),
                sandbox_home: sandbox_base.clone(),
                deny_read_runner_lease_name: String::new(),
                cap_sids,
                timeout_ms,
                use_private_desktop,
            };
            let transport = match retry_runner_spawn_once(
                sandbox_creds,
                &spawn_request.command,
                |sandbox_creds| {
                    if cancellation
                        .as_ref()
                        .is_some_and(crate::WindowsSandboxCancellationToken::is_cancelled)
                    {
                        anyhow::bail!("sandbox capture cancelled before runner spawn");
                    }
                    // Keep the credentials that actually established the successful runner
                    // transport; the retry path may replace the initially selected identity.
                    used_sandbox_creds = Some(sandbox_creds.clone());
                    let registration =
                        register_runner_lease(sandbox_home, &sandbox_creds.username)?;
                    let mut request = spawn_request.clone();
                    request.deny_read_runner_lease_name = registration.name().to_string();
                    let transport = spawn_runner_transport(
                        sandbox_home,
                        cwd,
                        &sandbox_creds,
                        logs_base_dir,
                        request,
                    )?;
                    drop(registration);
                    Ok(transport)
                },
                || {
                    if cancellation
                        .as_ref()
                        .is_some_and(crate::WindowsSandboxCancellationToken::is_cancelled)
                    {
                        anyhow::bail!("sandbox capture cancelled before credential refresh");
                    }
                    refresh_logon_sandbox_creds(
                        &permissions,
                        cwd,
                        &env_map,
                        sandbox_home,
                        read_roots_override,
                        read_roots_include_platform_defaults,
                        write_roots_override,
                        &protected_setup.deny_read,
                        &protected_setup.deny_write,
                        &trusted_deny_write_paths_override,
                        proxy_enforced,
                        crate::WindowsSandboxProxySettingsMode::Reconcile,
                        trusted_workspace,
                    )
                },
            ) {
                Ok(transport) => transport,
                Err(_error)
                    if cancellation
                        .as_ref()
                        .is_some_and(crate::WindowsSandboxCancellationToken::is_cancelled) =>
                {
                    return Ok(cancelled_capture_result());
                }
                Err(error) => return Err(error),
            };
            let (pipe_write, mut pipe_read) = transport.into_files();
            let cancel_writer = spawn_cancel_writer(&pipe_write, cancellation)?;

            let mut stdout =
                crate::BoundedCapture::new(crate::DEFAULT_MAX_CAPTURE_BYTES_PER_STREAM);
            let mut stderr =
                crate::BoundedCapture::new(crate::DEFAULT_MAX_CAPTURE_BYTES_PER_STREAM);
            let result = loop {
                let msg = match read_frame(&mut pipe_read) {
                    Ok(Some(msg)) => msg,
                    Ok(None) => break Err(anyhow::anyhow!("runner pipe closed before exit")),
                    Err(err) => break Err(err),
                };
                match msg.message {
                    Message::SpawnReady { .. } => {}
                    Message::Output { payload } => match decode_bytes(&payload.data_b64) {
                        Ok(bytes) => match payload.stream {
                            OutputStream::Stdout => stdout.extend(&bytes),
                            OutputStream::Stderr => stderr.extend(&bytes),
                        },
                        Err(err) => {
                            break Err(err);
                        }
                    },
                    Message::Exit { payload } => {
                        break Ok((payload.exit_code, payload.timed_out, payload.cancelled));
                    }
                    Message::Error { payload } => {
                        break Err(anyhow::anyhow!("runner error: {}", payload.message));
                    }
                    other => {
                        break Err(anyhow::anyhow!(
                            "unexpected runner message during capture: {other:?}"
                        ));
                    }
                }
            };
            if let Some((cancel_handle, done)) = cancel_writer {
                done.store(true, Ordering::SeqCst);
                cancel_handle.thread().unpark();
                let _ = cancel_handle.join();
            }
            drop(pipe_write);
            let (exit_code, timed_out, cancelled) = result?;
            let (stdout, stdout_truncated) = stdout.into_parts();
            let (stderr, stderr_truncated) = stderr.into_parts();

            if exit_code == 0 {
                log_success(&command, logs_base_dir);
            } else {
                log_failure(&command, &format!("exit code {exit_code}"), logs_base_dir);
            }

            Ok(CaptureResult {
                exit_code,
                stdout,
                stderr,
                timed_out,
                cancelled,
                output_truncated: stdout_truncated || stderr_truncated,
            })
        })();
        // The runner releases its lease only after Job Object cleanup. Keep the execution mutex
        // while consuming every released or abandoned registration, including startup and IPC
        // failures, so this live parent never opens the crash-only reconciliation gap.
        let lease_cleanup = loop {
            match reconcile_runner_leases(sandbox_home, 50) {
                Ok(true) => break Ok(()),
                Ok(false) => {
                    // Keep cleanup fail-closed while yielding between bounded waits for a live
                    // runner lease to release.
                    std::thread::park_timeout(Duration::from_millis(50));
                }
                Err(error) => break Err(error),
            }
        };
        match (capture_result, lease_cleanup) {
            (Ok(capture), Ok(())) => {
                let observation = match (before_observation, after_observer) {
                    (Some(before), Some(mut after_observer)) => {
                        let workspace = workspace_roots.first().ok_or_else(|| {
                            anyhow::anyhow!("workspace observation requires a root")
                        })?;
                        // Overlap a second monitor with the end of the command monitor. This
                        // closes the finish-to-snapshot TOCTOU gap: any out-of-band write while
                        // the final snapshot is read invalidates the observation.
                        let after_guard =
                            crate::WorkspaceChangeMonitor::start(workspace.as_path())?;
                        let monitor_slot = workspace_change_monitor.ok_or_else(|| {
                            anyhow::anyhow!(
                                "sandbox workspace change monitor unavailable after capture"
                            )
                        })?;
                        let change = monitor_slot
                            .take()
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "sandbox workspace change monitor unavailable after capture"
                                )
                            })?
                            .finish()?;
                        let creds = used_sandbox_creds.ok_or_else(|| {
                            anyhow::anyhow!(
                                "sandbox workspace observer credentials unavailable after capture"
                            )
                        })?;
                        let change_for_after = change.clone();
                        let after =
                            observe_as_sandbox_user(&creds, || after_observer(change_for_after))?;
                        let retry_guard =
                            crate::WorkspaceChangeMonitor::start(workspace.as_path())?;
                        let trailing_change = after_guard.finish()?;
                        if trailing_change == crate::WorkspaceChangeObservation::Unchanged {
                            *monitor_slot = Some(retry_guard);
                            Some((before, after, change))
                        } else {
                            let trailing_change_for_after = trailing_change.clone();
                            let after = observe_as_sandbox_user(&creds, || {
                                after_observer(trailing_change_for_after)
                            })?;
                            let continuation_guard =
                                crate::WorkspaceChangeMonitor::start(workspace.as_path())?;
                            if retry_guard.finish()? != crate::WorkspaceChangeObservation::Unchanged
                            {
                                anyhow::bail!(
                                    "workspace did not stabilize during its final observation"
                                );
                            }
                            *monitor_slot = Some(continuation_guard);
                            Some((
                                before,
                                after,
                                crate::workspace_change::merge_workspace_change_observations(
                                    change,
                                    trailing_change,
                                ),
                            ))
                        }
                    }
                    (None, None) => None,
                    _ => anyhow::bail!("sandbox workspace observer state is inconsistent"),
                };
                Ok((capture, observation))
            }
            (Err(error), Ok(())) | (_, Err(error)) => Err(error),
        }
    }

    fn resolve_nested_git_paths(paths: &[AbsolutePathBuf]) -> Result<(Vec<PathBuf>, Vec<File>)> {
        let mut resolved = Vec::with_capacity(paths.len());
        let mut handles = Vec::new();
        let mut seen = HashSet::new();
        for path in paths {
            let path = path.as_path();
            let (effective, handle) = match open_existing_git_ancestor(path)? {
                Some((ancestor, handle)) => (ancestor, Some(handle)),
                None => (path.to_path_buf(), None),
            };
            let key = effective.to_string_lossy().to_ascii_lowercase();
            if seen.insert(key) {
                resolved.push(effective);
                if let Some(handle) = handle {
                    handles.push(handle);
                }
            }
        }
        Ok((resolved, handles))
    }

    fn resolve_trusted_deny_write_paths(
        paths: &[AbsolutePathBuf],
        workspace_root: &Path,
        trusted_workspace: &crate::trusted_workspace::TrustedWorkspaceLease,
        permissions: &ResolvedWindowsSandboxPermissions,
        cwd: &Path,
        env_map: &std::collections::HashMap<String, String>,
    ) -> Result<(Vec<PathBuf>, Vec<File>)> {
        let mut cleanup_inputs = paths.to_vec();
        if permissions.uses_write_capabilities_for_cwd(cwd, env_map) {
            cleanup_inputs.extend(
                compute_allow_paths_for_permissions(permissions, cwd, env_map)
                    .deny
                    .into_iter()
                    .map(AbsolutePathBuf::from_absolute_path_checked)
                    .collect::<std::io::Result<Vec<_>>>()?,
            );
        }
        let mut resolved = Vec::new();
        let mut handles = Vec::new();
        let mut seen = HashSet::new();
        let root_handle = trusted_workspace
            .duplicate_root_handle()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        for path in cleanup_inputs {
            let path = path.as_path();
            match std::fs::symlink_metadata(path) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            }
            let key = path.to_string_lossy().to_ascii_lowercase();
            if seen.insert(key) {
                // Open the final object relative to the trusted no-follow root without delete
                // sharing. Holding this handle prevents pathname replacement after setup has
                // inspected the same existing object.
                let handle =
                    open_pinned_workspace_path(&root_handle, workspace_root, path, READ_CONTROL)?
                        .ok_or_else(|| {
                        anyhow::anyhow!(
                            "trusted deny-write path is outside the pinned workspace: {}",
                            path.display()
                        )
                    })?;
                handles.push(handle);
                resolved.push(path.to_path_buf());
            }
        }
        Ok((resolved, handles))
    }

    fn is_git_marker_path(path: &Path) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(singularity_core::PROTECTED_GIT_DIR_NAME))
    }

    #[cfg(test)]
    mod tests {
        use super::ElevatedSandboxProfileCaptureRequest;
        use super::acquire_deny_read_execution_guard;
        use super::merge_required_protected_paths;
        use super::prepare_protected_setup_plan;
        use super::resolved_path_sets_equal;
        use crate::AbsolutePathBuf;
        use crate::WindowsSandboxCancellationToken;
        use crate::deny_read_acl::TEST_CERTIFICATE_DER_BASE64;
        use crate::deny_read_acl::existing_public_certificate_only_pem;
        use crate::deny_read_state::try_lock_deny_read_execution;
        use crate::permissions::PermissionProfile;
        use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
        use std::collections::HashMap;
        use std::fs;
        use std::path::Path;
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        #[test]
        fn cancellation_before_elevated_setup_returns_without_spawning() {
            let profile = PermissionProfile::workspace_write();
            let mut request = ElevatedSandboxProfileCaptureRequest::new(
                &profile,
                &[],
                Path::new("."),
                vec!["cmd.exe".to_string()],
                Path::new("."),
                HashMap::new(),
            );
            request.cancellation = Some(WindowsSandboxCancellationToken::new(|| true));

            let result = super::run_windows_sandbox_capture_for_permission_profile(request)
                .expect("cancelled capture result");
            assert!(result.cancelled);
            assert!(result.stdout.is_empty());
            assert!(result.stderr.is_empty());
        }

        #[test]
        fn cancellation_interrupts_execution_mutex_wait() {
            let temp = tempfile::tempdir().expect("tempdir");
            let sandbox_home = temp.path().join("singularity-home");
            std::fs::create_dir_all(&sandbox_home).expect("create sandbox home");
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let holder_home = sandbox_home.clone();
            let holder = std::thread::spawn(move || {
                let _held = try_lock_deny_read_execution(&holder_home, u32::MAX)
                    .expect("lock execution mutex")
                    .expect("execution mutex acquired");
                ready_tx.send(()).expect("signal held execution mutex");
                release_rx.recv().expect("release execution mutex");
            });
            ready_rx.recv().expect("wait for held execution mutex");
            let cancelled = Arc::new(AtomicBool::new(false));
            let token = WindowsSandboxCancellationToken::new({
                let cancelled = Arc::clone(&cancelled);
                move || cancelled.load(Ordering::SeqCst)
            });
            let cancel_thread = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                cancelled.store(true, Ordering::SeqCst);
            });

            let guard = acquire_deny_read_execution_guard(&sandbox_home, Some(&token))
                .expect("cancelled execution mutex wait");
            cancel_thread.join().expect("join cancellation thread");
            assert!(guard.is_none());
            release_tx.send(()).expect("release held execution mutex");
            holder.join().expect("join execution mutex holder");
        }

        #[test]
        fn protected_path_stabilization_compares_windows_sets_case_insensitively() {
            let temp = tempfile::tempdir().expect("tempdir");
            let lower = temp.path().join("secret.env");
            let upper = temp.path().join("SECRET.ENV");
            let lower =
                AbsolutePathBuf::from_absolute_path_checked(&lower).expect("absolute lower path");
            let upper =
                AbsolutePathBuf::from_absolute_path_checked(&upper).expect("absolute upper path");

            assert!(resolved_path_sets_equal(
                &(vec![lower.clone(), lower.clone()], vec![]),
                &(vec![upper], vec![])
            ));
            assert!(!resolved_path_sets_equal(
                &(vec![lower.clone()], vec![]),
                &(vec![], vec![lower])
            ));
        }

        #[test]
        fn protected_path_stabilization_retains_explicit_missing_targets() {
            let temp = tempfile::tempdir().expect("tempdir");
            let required =
                AbsolutePathBuf::from_absolute_path_checked(temp.path().join("future-secret.env"))
                    .expect("absolute required path");

            let merged = merge_required_protected_paths(
                (vec![], vec![]),
                std::slice::from_ref(&required),
                std::slice::from_ref(&required),
            );

            assert_eq!(merged.0.len(), 1);
            assert_eq!(merged.1.len(), 1);
        }

        #[test]
        fn public_certificate_setup_keeps_identity_pin_through_replacement_boundary() {
            let temp = tempfile::tempdir().expect("workspace parent");
            let workspace = temp.path().join("workspace");
            fs::create_dir(&workspace).expect("workspace");
            let certificate = workspace.join("certificate.pem");
            let certificate_contents = format!(
                "# Issuer: CN=Public Root\n# Subject: CN=Public Root\n-----BEGIN CERTIFICATE-----\n{TEST_CERTIFICATE_DER_BASE64}\n-----END CERTIFICATE-----\n"
            );
            fs::write(&certificate, &certificate_contents).expect("public certificate");
            assert!(
                existing_public_certificate_only_pem(&certificate)
                    .expect("classify public certificate")
            );

            let workspace =
                AbsolutePathBuf::from_absolute_path_checked(&workspace).expect("workspace path");
            let target =
                AbsolutePathBuf::from_absolute_path_checked(&certificate).expect("target path");
            let permissions =
                ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
                    &PermissionProfile::workspace_write(),
                    std::slice::from_ref(&workspace),
                )
                .expect("resolved workspace permissions");
            let plan = prepare_protected_setup_plan(
                (vec![target], vec![]),
                &permissions,
                workspace.as_path(),
                &HashMap::new(),
                &workspace,
                None,
            )
            .expect("protected setup plan");
            assert_eq!(plan.pins.len(), 1, "public certificate must remain pinned");

            let displaced = temp.path().join("certificate-displaced.pem");
            let replaced = fs::rename(&certificate, &displaced).is_ok();
            if replaced {
                fs::write(&certificate, certificate_contents).expect("replacement certificate");
                assert!(
                    crate::path_safety::revalidate_pinned_workspace_paths(&plan.pins).is_err(),
                    "public certificate replacement must fail the setup boundary"
                );
            } else {
                crate::path_safety::revalidate_pinned_workspace_paths(&plan.pins)
                    .expect("unchanged public certificate pin");
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub use windows_impl::run_windows_sandbox_capture_for_permission_profile;

#[cfg(target_os = "windows")]
pub use windows_impl::run_windows_sandbox_capture_for_permission_profile_with_observations;

#[cfg(not(target_os = "windows"))]
mod stub {
    use super::ElevatedSandboxProfileCaptureRequest;
    use anyhow::Result;
    use anyhow::bail;

    #[derive(Debug, Default)]
    pub struct CaptureResult {
        pub exit_code: i32,
        pub stdout: Vec<u8>,
        pub stderr: Vec<u8>,
        pub timed_out: bool,
        pub cancelled: bool,
        pub output_truncated: bool,
    }

    /// Stub implementation for non-Windows targets; sandboxing only works on Windows.
    #[allow(clippy::too_many_arguments)]
    pub fn run_windows_sandbox_capture_for_permission_profile(
        _request: ElevatedSandboxProfileCaptureRequest<'_>,
    ) -> Result<CaptureResult> {
        bail!("Windows sandbox is only available on Windows")
    }
}

#[cfg(not(target_os = "windows"))]
pub use stub::run_windows_sandbox_capture_for_permission_profile;
