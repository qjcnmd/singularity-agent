//! Windows command runner used by the **elevated** sandbox path.
//!
//! The parent launches this binary under a sandbox user. It reads one framed
//! `SpawnRequest`, derives a restricted token, spawns a non-interactive child with
//! anonymous pipes, streams captured output, accepts termination, and emits exit status.
//! The unelevated restricted-token fallback spawns the child directly.

#![allow(unsafe_op_in_unsafe_fn)]

mod cwd_junction;

use anyhow::Context;
use anyhow::Result;
use singularity_windows_sandbox::ErrorPayload;
use singularity_windows_sandbox::ErrorStage;
use singularity_windows_sandbox::ExitPayload;
use singularity_windows_sandbox::FramedMessage;
use singularity_windows_sandbox::IPC_PROTOCOL_VERSION;
use singularity_windows_sandbox::JobObject;
use singularity_windows_sandbox::LaunchDesktop;
use singularity_windows_sandbox::LocalSid;
use singularity_windows_sandbox::Message;
use singularity_windows_sandbox::OutputPayload;
use singularity_windows_sandbox::OutputStream;
use singularity_windows_sandbox::PipeSpawnHandles;
use singularity_windows_sandbox::SpawnReady;
use singularity_windows_sandbox::SpawnRequest;
use singularity_windows_sandbox::StderrMode;
use singularity_windows_sandbox::StdinMode;
use singularity_windows_sandbox::WindowsSandboxTokenMode;
use singularity_windows_sandbox::allow_null_device;
use singularity_windows_sandbox::create_readonly_token_with_caps_and_user_from;
use singularity_windows_sandbox::create_workspace_write_token_with_caps_and_user_from;
use singularity_windows_sandbox::encode_bytes;
use singularity_windows_sandbox::get_current_token_for_restriction;
use singularity_windows_sandbox::hide_current_user_profile_dir;
use singularity_windows_sandbox::log_note;
use singularity_windows_sandbox::product_identity::READ_ACL_MUTEX_NAME;
use singularity_windows_sandbox::read_frame;
use singularity_windows_sandbox::read_handle_loop;
use singularity_windows_sandbox::spawn_process_with_pipes;
use singularity_windows_sandbox::to_wide;
use singularity_windows_sandbox::token_mode_for_permission_profile;
use singularity_windows_sandbox::write_frame;
use std::ffi::OsStr;
use std::fs::File;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
use windows_sys::Win32::System::Threading::GetExitCodeProcess;
use windows_sys::Win32::System::Threading::GetProcessId;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::Threading::MUTEX_ALL_ACCESS;
use windows_sys::Win32::System::Threading::OpenMutexW;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x0000_0102;
const WAIT_FAILED: u32 = u32::MAX;

struct IpcSpawnedProcess {
    log_dir: PathBuf,
    pi: PROCESS_INFORMATION,
    stdout_handle: HANDLE,
    stderr_handle: HANDLE,
    job: JobObject,
    _desktop: LaunchDesktop,
}

impl IpcSpawnedProcess {
    fn take_capture_handles(&mut self) -> (PROCESS_INFORMATION, HANDLE, HANDLE) {
        let pi = std::mem::replace(&mut self.pi, unsafe { std::mem::zeroed() });
        let stdout_handle = std::mem::replace(&mut self.stdout_handle, 0);
        let stderr_handle = std::mem::replace(&mut self.stderr_handle, 0);
        (pi, stdout_handle, stderr_handle)
    }
}

impl Drop for IpcSpawnedProcess {
    fn drop(&mut self) {
        let _ = self.job.close();
        unsafe {
            if self.pi.hThread != 0 {
                CloseHandle(self.pi.hThread);
            }
            if self.pi.hProcess != 0 {
                CloseHandle(self.pi.hProcess);
            }
            if self.stdout_handle != 0 {
                CloseHandle(self.stdout_handle);
            }
            if self.stderr_handle != 0
                && self.stderr_handle != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE
            {
                CloseHandle(self.stderr_handle);
            }
        }
    }
}

/// Small RAII wrapper for raw Win32 handles.
///
/// The elevated runner has a few early-return paths where we acquire a token, job, or pipe
/// handle and then may fail while preparing the child. Keeping those handles in a guard makes
/// the error paths read more directly and closes the gaps that were previously leaking them.
struct OwnedWinHandle(HANDLE);

impl OwnedWinHandle {
    fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    fn raw(&self) -> HANDLE {
        self.0
    }

    fn into_raw(mut self) -> HANDLE {
        // Transfer ownership to the caller. After this point the caller is responsible for
        // eventually closing the returned HANDLE.
        let handle = self.0;
        self.0 = 0;
        handle
    }
}

impl Drop for OwnedWinHandle {
    fn drop(&mut self) {
        if self.0 != 0 && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Open a named pipe created by the parent process.
fn open_pipe(name: &str, access: u32) -> Result<HANDLE> {
    let path = to_wide(name);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            0,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            0,
        )
    };
    if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        return Err(anyhow::anyhow!("CreateFileW failed for pipe {name}: {err}"));
    }
    Ok(handle)
}

/// Send an error frame back to the parent process.
fn send_error(
    writer: &Arc<StdMutex<File>>,
    stage: ErrorStage,
    windows_error_code: Option<u32>,
    message: String,
) -> Result<()> {
    let msg = FramedMessage {
        version: IPC_PROTOCOL_VERSION,
        message: Message::Error {
            payload: ErrorPayload {
                message,
                stage,
                windows_error_code,
            },
        },
    };
    if let Ok(mut guard) = writer.lock() {
        write_frame(&mut *guard, &msg)?;
    }
    Ok(())
}

fn windows_error_code(err: &anyhow::Error) -> Option<u32> {
    err.chain().find_map(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error)
            .and_then(|code| u32::try_from(code).ok())
    })
}

/// Read and validate the initial spawn request frame.
fn read_spawn_request(reader: &mut File) -> Result<SpawnRequest> {
    let Some(msg) = read_frame(reader)? else {
        anyhow::bail!("runner: pipe closed before spawn_request");
    };
    if msg.version != IPC_PROTOCOL_VERSION {
        anyhow::bail!("runner: unsupported protocol version {}", msg.version);
    }
    match msg.message {
        Message::SpawnRequest { payload } => Ok(*payload),
        other => anyhow::bail!("runner: expected spawn_request, got {other:?}"),
    }
}

fn read_acl_mutex_exists() -> Result<bool> {
    let name = to_wide(OsStr::new(READ_ACL_MUTEX_NAME));
    let handle = unsafe { OpenMutexW(MUTEX_ALL_ACCESS, 0, name.as_ptr()) };
    if handle == 0 {
        let err = unsafe { GetLastError() };
        if err == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        return Err(anyhow::anyhow!("OpenMutexW failed: {err}"));
    }
    unsafe {
        CloseHandle(handle);
    }
    Ok(true)
}

/// Pick an effective CWD, using a junction if the ACL helper is active.
fn effective_cwd(req_cwd: &Path, log_dir: Option<&Path>) -> PathBuf {
    let use_junction = match read_acl_mutex_exists() {
        Ok(exists) => exists,
        Err(err) => {
            log_note(
                &format!(
                    "junction: failed to probe ACL mutex state: {err}; defaulting to junction cwd"
                ),
                log_dir,
            );
            true
        }
    };
    if use_junction {
        cwd_junction::create_cwd_junction(req_cwd, log_dir).unwrap_or_else(|| req_cwd.to_path_buf())
    } else {
        req_cwd.to_path_buf()
    }
}

fn spawn_ipc_process(req: &SpawnRequest) -> Result<IpcSpawnedProcess> {
    let log_dir = req.sandbox_home.clone();
    hide_current_user_profile_dir(req.sandbox_home.as_path());
    let token_mode = token_mode_for_permission_profile(
        &req.permission_profile,
        &req.workspace_roots,
        &req.cwd,
        &req.env,
    )
    .context("resolve permission profile token mode")?;
    let mut cap_psids: Vec<LocalSid> = Vec::new();
    for sid in &req.cap_sids {
        cap_psids.push(
            LocalSid::from_string(sid)
                .context("ConvertStringSidToSidW failed for capability SID")?,
        );
    }
    if cap_psids.is_empty() {
        anyhow::bail!("runner: empty capability SID list");
    }

    // The token helpers still take raw SID pointers, but we keep ownership in `LocalSid`
    // wrappers for as long as possible. That way any failure after SID parsing but before the
    // child is fully spawned still releases the backing LocalAlloc memory automatically.
    let cap_psid_ptrs: Vec<*mut _> = cap_psids.iter().map(LocalSid::as_ptr).collect();
    let base = OwnedWinHandle::new(unsafe { get_current_token_for_restriction()? });
    let h_token = OwnedWinHandle::new(unsafe {
        match token_mode {
            WindowsSandboxTokenMode::ReadOnlyCapability => {
                create_readonly_token_with_caps_and_user_from(base.raw(), &cap_psid_ptrs)
            }
            WindowsSandboxTokenMode::WritableRootsCapability => {
                create_workspace_write_token_with_caps_and_user_from(base.raw(), &cap_psid_ptrs)
            }
        }
    }?);
    unsafe {
        // These ACL adjustments need the raw SID values, but ownership stays with `cap_psids`.
        // We do not manually `LocalFree` anything here; the wrappers handle every return path.
        allow_null_device(cap_psid_ptrs[0])?;
        for psid in &cap_psid_ptrs {
            allow_null_device(*psid)?;
        }
    }

    let effective_cwd = effective_cwd(&req.cwd, Some(log_dir.as_path()));

    let spawned_pipes: PipeSpawnHandles = spawn_process_with_pipes(
        h_token.raw(),
        &req.command,
        &effective_cwd,
        &req.env,
        StdinMode::Closed,
        StderrMode::Separate,
        req.use_private_desktop,
        Some(log_dir.as_path()),
    )?;
    let (pi, job, stdin_write, stdout_handle, stderr_read, desktop) = spawned_pipes.into_parts();
    if let Some(stdin_write) = stdin_write {
        unsafe {
            CloseHandle(stdin_write);
        }
    }
    let stderr_handle = stderr_read.unwrap_or(windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE);
    Ok(IpcSpawnedProcess {
        log_dir,
        pi,
        stdout_handle,
        stderr_handle,
        job,
        _desktop: desktop,
    })
}

/// Stream stdout/stderr from the child into Output frames.
fn spawn_output_reader(
    writer: Arc<StdMutex<File>>,
    handle: HANDLE,
    stream: OutputStream,
    log_dir: Option<PathBuf>,
) -> std::thread::JoinHandle<()> {
    read_handle_loop(handle, move |chunk| {
        let msg = FramedMessage {
            version: IPC_PROTOCOL_VERSION,
            message: Message::Output {
                payload: OutputPayload {
                    data_b64: encode_bytes(chunk),
                    stream,
                },
            },
        };
        if let Ok(mut guard) = writer.lock()
            && let Err(err) = write_frame(&mut *guard, &msg)
        {
            log_note(
                &format!("runner output write failed: {err}"),
                log_dir.as_deref(),
            );
        }
    })
}

fn record_termination_error(slot: &StdMutex<Option<String>>, message: String) {
    if let Ok(mut guard) = slot.lock()
        && guard.is_none()
    {
        *guard = Some(message);
    }
}

/// Read capture-control frames and terminate the child when requested.
fn spawn_control_loop(
    mut reader: File,
    job: JobObject,
    cancel_requested: Arc<AtomicBool>,
    shutdown_requested: Arc<AtomicBool>,
    termination_error: Arc<StdMutex<Option<String>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        loop {
            let message = match read_frame(&mut reader) {
                Ok(Some(message)) => message,
                Ok(None) => {
                    if shutdown_requested.load(Ordering::SeqCst) {
                        break;
                    }
                    cancel_requested.store(true, Ordering::SeqCst);
                    let message = match job.terminate(1) {
                        Ok(()) => "runner control pipe closed before child completion".to_string(),
                        Err(error) => format!(
                            "runner control pipe closed before child completion; Job Object termination failed: {error:#}"
                        ),
                    };
                    record_termination_error(&termination_error, message);
                    break;
                }
                Err(error) => {
                    if shutdown_requested.load(Ordering::SeqCst) {
                        break;
                    }
                    cancel_requested.store(true, Ordering::SeqCst);
                    let message = match job.terminate(1) {
                        Ok(()) => format!("runner control pipe read failed: {error:#}"),
                        Err(termination_error) => format!(
                            "runner control pipe read failed: {error:#}; Job Object termination failed: {termination_error:#}"
                        ),
                    };
                    record_termination_error(&termination_error, message);
                    break;
                }
            };
            if matches!(message.message, Message::Terminate { .. }) {
                cancel_requested.store(true, Ordering::SeqCst);
                if let Err(error) = job.terminate(1) {
                    record_termination_error(&termination_error, error.to_string());
                }
                break;
            }
        }
    })
}

/// Entry point for the Windows command runner process.
pub fn main() -> Result<()> {
    let mut pipe_in = None;
    let mut pipe_out = None;
    for arg in std::env::args().skip(1) {
        if let Some(rest) = arg.strip_prefix("--pipe-in=") {
            pipe_in = Some(rest.to_string());
        } else if let Some(rest) = arg.strip_prefix("--pipe-out=") {
            pipe_out = Some(rest.to_string());
        }
    }

    let Some(pipe_in) = pipe_in else {
        anyhow::bail!("runner: no pipe-in provided");
    };
    let Some(pipe_out) = pipe_out else {
        anyhow::bail!("runner: no pipe-out provided");
    };

    // Open both pipe ends under guards first so a failure on the second open cannot leak the
    // first HANDLE. Only after both opens succeed do we transfer ownership into `File`, which
    // then becomes responsible for closing them.
    let h_pipe_in = OwnedWinHandle::new(open_pipe(&pipe_in, FILE_GENERIC_READ)?);
    let h_pipe_out = OwnedWinHandle::new(open_pipe(&pipe_out, FILE_GENERIC_WRITE)?);
    let mut pipe_read = unsafe { File::from_raw_handle(h_pipe_in.into_raw() as _) };
    let pipe_write = Arc::new(StdMutex::new(unsafe {
        File::from_raw_handle(h_pipe_out.into_raw() as _)
    }));

    let req = match read_spawn_request(&mut pipe_read) {
        Ok(v) => v,
        Err(err) => {
            let _ = send_error(
                &pipe_write,
                ErrorStage::ReadSpawnRequest,
                /*windows_error_code*/ None,
                err.to_string(),
            );
            return Err(err);
        }
    };

    let mut ipc_spawn = match spawn_ipc_process(&req) {
        Ok(value) => value,
        Err(err) => {
            let _ = send_error(
                &pipe_write,
                ErrorStage::SpawnChild,
                windows_error_code(&err),
                err.to_string(),
            );
            return Err(err);
        }
    };
    let log_dir_path = ipc_spawn.log_dir.clone();
    let log_dir = Some(log_dir_path.as_path());

    let msg = FramedMessage {
        version: IPC_PROTOCOL_VERSION,
        message: Message::SpawnReady {
            payload: SpawnReady {
                process_id: unsafe { GetProcessId(ipc_spawn.pi.hProcess) },
            },
        },
    };
    if let Err(err) = if let Ok(mut guard) = pipe_write.lock() {
        write_frame(&mut *guard, &msg)
    } else {
        anyhow::bail!("runner spawn_ready write failed: pipe_write lock poisoned");
    } {
        let _ = send_error(
            &pipe_write,
            ErrorStage::WriteSpawnReady,
            /*windows_error_code*/ None,
            err.to_string(),
        );
        return Err(err);
    }
    let job = ipc_spawn.job.clone();
    let (pi, stdout_handle, stderr_handle) = ipc_spawn.take_capture_handles();
    let log_dir_owned = log_dir.map(Path::to_path_buf);
    let out_thread = spawn_output_reader(
        Arc::clone(&pipe_write),
        stdout_handle,
        OutputStream::Stdout,
        log_dir_owned.clone(),
    );
    let err_thread = if stderr_handle != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        Some(spawn_output_reader(
            Arc::clone(&pipe_write),
            stderr_handle,
            OutputStream::Stderr,
            log_dir_owned.clone(),
        ))
    } else {
        None
    };

    let cancel_requested = Arc::new(AtomicBool::new(false));
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let termination_error = Arc::new(StdMutex::new(None));
    let control_thread = spawn_control_loop(
        pipe_read,
        job.clone(),
        Arc::clone(&cancel_requested),
        Arc::clone(&shutdown_requested),
        Arc::clone(&termination_error),
    );

    let timeout = req.timeout_ms.map(|ms| ms as u32).unwrap_or(INFINITE);
    let wait_res = unsafe { WaitForSingleObject(pi.hProcess, timeout) };
    let timed_out = wait_res == WAIT_TIMEOUT;
    let wait_error = if wait_res != WAIT_OBJECT_0 && wait_res != WAIT_TIMEOUT {
        Some(if wait_res == WAIT_FAILED {
            let error = unsafe { GetLastError() };
            format!("WaitForSingleObject failed while waiting for child: {error}")
        } else {
            format!("WaitForSingleObject returned unexpected status {wait_res}")
        })
    } else {
        None
    };
    let cancelled = !timed_out && cancel_requested.load(Ordering::SeqCst);
    let cleanup_error = if timed_out || cancelled || wait_error.is_some() {
        job.terminate_and_wait(pi.hProcess, 1).err()
    } else {
        job.close().err()
    };
    if let Some(error) = wait_error {
        record_termination_error(&termination_error, error);
    }
    if let Some(error) = cleanup_error {
        record_termination_error(&termination_error, error.to_string());
    }

    let exit_code: i32;

    unsafe {
        if timed_out {
            exit_code = 128 + 64;
        } else {
            let mut raw_exit: u32 = 1;
            GetExitCodeProcess(pi.hProcess, &mut raw_exit);
            exit_code = raw_exit as i32;
        }
        if pi.hThread != 0 {
            CloseHandle(pi.hThread);
        }
        if pi.hProcess != 0 {
            CloseHandle(pi.hProcess);
        }
    }

    let _ = out_thread.join();
    if let Some(thread) = err_thread {
        let _ = thread.join();
    }

    let termination_error = termination_error
        .lock()
        .ok()
        .and_then(|mut guard| guard.take());
    if let Some(message) = termination_error {
        let _ = send_error(
            &pipe_write,
            ErrorStage::SpawnChild,
            /*windows_error_code*/ None,
            message.clone(),
        );
        shutdown_requested.store(true, Ordering::SeqCst);
        drop(pipe_write);
        let _ = control_thread.join();
        drop(ipc_spawn);
        anyhow::bail!(message);
    }

    let exit_msg = FramedMessage {
        version: IPC_PROTOCOL_VERSION,
        message: Message::Exit {
            payload: ExitPayload {
                exit_code,
                timed_out,
                cancelled,
            },
        },
    };
    if let Ok(mut guard) = pipe_write.lock()
        && let Err(err) = write_frame(&mut *guard, &exit_msg)
    {
        log_note(&format!("runner exit write failed: {err}"), log_dir);
    }

    shutdown_requested.store(true, Ordering::SeqCst);
    drop(pipe_write);
    let _ = control_thread.join();
    drop(ipc_spawn);
    std::process::exit(exit_code);
}
