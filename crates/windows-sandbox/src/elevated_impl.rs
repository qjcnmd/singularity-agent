use crate::absolute_path::AbsolutePathBuf;
use crate::permissions::PermissionProfile;
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
    pub protect_workspace_metadata: bool,
    #[cfg(target_os = "windows")]
    pub workspace_change_monitor: Option<&'a mut Option<crate::WorkspaceChangeMonitor>>,
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
            protect_workspace_metadata: true,
            #[cfg(target_os = "windows")]
            workspace_change_monitor: None,
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
    use crate::identity::refresh_logon_sandbox_creds;
    use crate::identity::require_logon_sandbox_creds;
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
    use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
    use crate::runner_client::retry_runner_spawn_once;
    use crate::runner_client::spawn_runner_transport;
    use crate::sandbox_utils::ensure_sandbox_home_exists;
    use crate::sandbox_utils::inject_git_safe_directory;
    use crate::setup::effective_write_roots_for_permissions;
    use crate::setup::gather_read_roots;
    use crate::token::LocalSid;
    use anyhow::Result;
    use std::collections::HashSet;
    use std::fs::File;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

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
            protect_workspace_metadata,
            workspace_change_monitor,
        } = request;
        // Resolve safe aliases once so the execution mutex, setup payload, runner registration,
        // cleanup, and every state-file operation share one long-lived sandbox-home identity.
        let canonical_sandbox_home = canonicalize_path_allow_missing(sandbox_home);
        let sandbox_home = canonical_sandbox_home.as_path();
        if cancellation
            .as_ref()
            .is_some_and(crate::WindowsSandboxCancellationToken::is_cancelled)
        {
            return Ok(cancelled_capture_result());
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
        let (deny_read_paths_override, mut protected_git_marker_handles) =
            resolve_nested_git_paths(deny_read_paths_override)?;
        let mut deny_write_inputs = deny_write_paths_override.to_vec();
        deny_write_inputs.extend(
            compute_allow_paths_for_permissions(&permissions, cwd, &env_map)
                .deny
                .into_iter()
                .filter(|path| is_git_marker_path(path))
                .map(AbsolutePathBuf::from_absolute_path_checked)
                .collect::<std::io::Result<Vec<_>>>()?,
        );
        let (deny_write_paths_override, deny_write_marker_handles) =
            resolve_nested_git_paths(&deny_write_inputs)?;
        protected_git_marker_handles.extend(deny_write_marker_handles);
        // Keep the no-follow ancestor marker handles alive through setup, runner spawn, and Job
        // Object cleanup. This prevents a host-side rename from replacing the protected object
        // between resolution and the deny-read ACL mutation.
        let _protected_git_marker_handles = protected_git_marker_handles;
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
            return Ok(cancelled_capture_result());
        }
        // The elevated identities share one authoritative read principal. Serialize setup and the
        // complete Job Object lifetime so a concurrent workspace cannot reconcile that principal
        // to a different deny-read set while this child is still alive.
        let Some(_deny_read_execution_guard) =
            acquire_deny_read_execution_guard(sandbox_home, cancellation.as_ref())?
        else {
            return Ok(cancelled_capture_result());
        };
        let sandbox_creds = require_logon_sandbox_creds(
            &permissions,
            cwd,
            &env_map,
            sandbox_home,
            read_roots_override,
            read_roots_include_platform_defaults,
            write_roots_override,
            &deny_read_paths_override,
            &deny_write_paths_override,
            proxy_enforced,
            crate::WindowsSandboxProxySettingsMode::Reconcile,
        )?;
        // Setup refresh/elevation is a synchronous external operation and cannot be interrupted
        // safely from this call. Do not continue into ACL mutation or runner creation if it
        // completes after the caller has cancelled.
        if cancellation
            .as_ref()
            .is_some_and(crate::WindowsSandboxCancellationToken::is_cancelled)
        {
            return Ok(cancelled_capture_result());
        }
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
            return Ok(cancelled_capture_result());
        }
        unsafe {
            allow_null_device(sid_for_null.as_ptr());
        }
        if cancellation
            .as_ref()
            .is_some_and(crate::WindowsSandboxCancellationToken::is_cancelled)
        {
            return Ok(cancelled_capture_result());
        }

        if let Some(slot) = workspace_change_monitor {
            let workspace = workspace_roots
                .first()
                .ok_or_else(|| anyhow::anyhow!("workspace change monitoring requires a root"))?;
            *slot = Some(crate::WorkspaceChangeMonitor::start(workspace.as_path())?);
        }

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
                        &deny_read_paths_override,
                        &deny_write_paths_override,
                        proxy_enforced,
                        crate::WindowsSandboxProxySettingsMode::Reconcile,
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
                Ok(false) => {}
                Err(error) => break Err(error),
            }
        };
        match (capture_result, lease_cleanup) {
            (Ok(capture), Ok(())) => Ok(capture),
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

    fn is_git_marker_path(path: &Path) -> bool {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(singularity_core::PROTECTED_GIT_DIR_NAME))
    }

    #[cfg(test)]
    mod tests {
        use super::ElevatedSandboxProfileCaptureRequest;
        use super::acquire_deny_read_execution_guard;
        use crate::WindowsSandboxCancellationToken;
        use crate::deny_read_state::try_lock_deny_read_execution;
        use crate::permissions::PermissionProfile;
        use std::collections::HashMap;
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
    }
}

#[cfg(target_os = "windows")]
pub use windows_impl::run_windows_sandbox_capture_for_permission_profile;

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
