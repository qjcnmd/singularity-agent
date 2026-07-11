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
        }
    }
}

mod windows_impl {
    use super::ElevatedSandboxProfileCaptureRequest;
    use crate::absolute_path::AbsolutePathBuf;
    use crate::acl::allow_null_device;
    use crate::cap::load_or_create_cap_sids;
    use crate::cap::workspace_write_cap_sid_for_root;
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
    use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
    use crate::runner_client::retry_runner_spawn_once;
    use crate::runner_client::spawn_runner_transport;
    use crate::sandbox_utils::ensure_sandbox_home_exists;
    use crate::sandbox_utils::inject_git_safe_directory;
    use crate::setup::effective_write_roots_for_permissions;
    use crate::setup::gather_read_roots;
    use crate::token::LocalSid;
    use anyhow::Result;
    use std::fs::File;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    pub use crate::windows_impl::CaptureResult;

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
        } = request;
        let permissions =
            ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
                permission_profile,
                workspace_roots,
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
        let deny_read_paths_override = deny_read_paths_override
            .iter()
            .map(AbsolutePathBuf::to_path_buf)
            .collect::<Vec<_>>();
        let deny_write_paths_override = deny_write_paths_override
            .iter()
            .map(AbsolutePathBuf::to_path_buf)
            .collect::<Vec<_>>();
        normalize_null_device_env(&mut env_map);
        ensure_non_interactive_pager(&mut env_map);
        inherit_path_env(&mut env_map);
        inject_git_safe_directory(&mut env_map, cwd);
        // Use a temp-based log dir that the sandbox user can write.
        let sandbox_base = sandbox_home.join(".sandbox");
        ensure_sandbox_home_exists(&sandbox_base)?;

        let logs_base_dir: Option<&Path> = Some(sandbox_base.as_path());
        log_start(&command, logs_base_dir);
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
        // Build capability SID for ACL grants.
        let caps = load_or_create_cap_sids(sandbox_home)?;
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
            let sid = LocalSid::from_string(&caps.readonly)?;
            (sid, vec![caps.readonly])
        };

        unsafe {
            allow_null_device(sid_for_null.as_ptr());
        }

        (|| -> Result<CaptureResult> {
            let spawn_request = SpawnRequest {
                command: command.clone(),
                cwd: cwd.to_path_buf(),
                env: env_map.clone(),
                permission_profile: permission_profile.clone(),
                workspace_roots: workspace_roots.to_vec(),
                sandbox_home: sandbox_base.clone(),
                real_sandbox_home: sandbox_home.to_path_buf(),
                cap_sids,
                timeout_ms,
                use_private_desktop,
            };
            let transport = retry_runner_spawn_once(
                sandbox_creds,
                &spawn_request.command,
                |sandbox_creds| {
                    spawn_runner_transport(
                        sandbox_home,
                        cwd,
                        &sandbox_creds,
                        logs_base_dir,
                        spawn_request.clone(),
                    )
                },
                || {
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
            )?;
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
        })()
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
