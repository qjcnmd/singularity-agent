use crate::desktop::LaunchDesktop;
use crate::logging;
use crate::proc_thread_attr::ProcThreadAttributeList;
use crate::winutil::argv_to_command_line;
use crate::winutil::format_last_error;
use crate::winutil::to_wide;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::ptr;
use std::sync::Arc;
use std::sync::Mutex;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::SetHandleInformation;
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::Console::GetStdHandle;
use windows_sys::Win32::System::Console::STD_ERROR_HANDLE;
use windows_sys::Win32::System::Console::STD_INPUT_HANDLE;
use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;
use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
use windows_sys::Win32::System::JobObjects::CreateJobObjectW;
use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
use windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
use windows_sys::Win32::System::JobObjects::JobObjectExtendedLimitInformation;
use windows_sys::Win32::System::JobObjects::SetInformationJobObject;
use windows_sys::Win32::System::JobObjects::TerminateJobObject;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
use windows_sys::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT;
use windows_sys::Win32::System::Threading::CreateProcessAsUserW;
use windows_sys::Win32::System::Threading::EXTENDED_STARTUPINFO_PRESENT;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::ResumeThread;
use windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
use windows_sys::Win32::System::Threading::STARTUPINFOEXW;
use windows_sys::Win32::System::Threading::STARTUPINFOW;
use windows_sys::Win32::System::Threading::TerminateProcess;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const TERMINATION_WAIT_MS: u32 = 10_000;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x0000_0102;
const WAIT_FAILED: u32 = u32::MAX;

#[derive(Clone)]
pub struct JobObject {
    inner: Arc<JobObjectInner>,
}

struct JobObjectInner {
    handle: Mutex<Option<HANDLE>>,
}

impl JobObject {
    pub fn create_kill_on_close() -> Result<Self> {
        let handle = unsafe { CreateJobObjectW(ptr::null_mut(), ptr::null()) };
        if handle == 0 {
            return Err(last_error("CreateJobObjectW failed"));
        }

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &mut limits as *mut _ as *mut c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            let error = last_error("SetInformationJobObject failed");
            unsafe {
                CloseHandle(handle);
            }
            return Err(error);
        }

        Ok(Self {
            inner: Arc::new(JobObjectInner {
                handle: Mutex::new(Some(handle)),
            }),
        })
    }

    pub fn assign_process(&self, process: HANDLE) -> Result<()> {
        let guard = self
            .inner
            .handle
            .lock()
            .map_err(|_| anyhow!("Job Object handle lock poisoned"))?;
        let handle = guard
            .as_ref()
            .copied()
            .ok_or_else(|| anyhow!("Job Object is already closed"))?;
        if unsafe { AssignProcessToJobObject(handle, process) } == 0 {
            return Err(last_error("AssignProcessToJobObject failed"));
        }
        Ok(())
    }

    /// Closes the kill-on-close handle and therefore terminates any remaining processes in the
    /// Job Object. This is idempotent so independent cleanup paths can converge safely.
    pub fn close(&self) -> Result<()> {
        let mut guard = self
            .inner
            .handle
            .lock()
            .map_err(|_| anyhow!("Job Object handle lock poisoned"))?;
        let Some(handle) = guard.take() else {
            return Ok(());
        };
        if unsafe { CloseHandle(handle) } == 0 {
            let close_error = last_error("CloseHandle failed for kill-on-close Job Object");
            *guard = Some(handle);
            if unsafe { TerminateJobObject(handle, 1) } != 0 {
                return Err(close_error.context(
                    "TerminateJobObject fallback succeeded after Job Object close failed",
                ));
            }
            let terminate_error = last_error("TerminateJobObject fallback also failed");
            return Err(anyhow!("{close_error:#}; {terminate_error:#}"));
        }
        Ok(())
    }

    /// Terminates every process in the Job Object. The handle remains available for a separate
    /// cleanup step; callers that need to release capture pipes must use
    /// [`JobObject::terminate_and_wait`].
    pub fn terminate(&self, exit_code: u32) -> Result<()> {
        let guard = self
            .inner
            .handle
            .lock()
            .map_err(|_| anyhow!("Job Object handle lock poisoned"))?;
        let Some(handle) = guard.as_ref().copied() else {
            return Ok(());
        };
        if unsafe { TerminateJobObject(handle, exit_code) } != 0 {
            return Ok(());
        }

        Err(last_error("TerminateJobObject failed"))
    }

    pub fn terminate_and_wait(&self, process: HANDLE, exit_code: u32) -> Result<()> {
        let termination_error = self.terminate(exit_code).err();
        let close_error = self.close().err();
        let wait_error = wait_for_process_termination(process).err();
        let errors = [termination_error, close_error, wait_error]
            .into_iter()
            .flatten()
            .map(|error| format!("{error:#}"))
            .collect::<Vec<_>>();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(errors.join("; ")))
        }
    }
}

impl Drop for JobObjectInner {
    fn drop(&mut self) {
        let handle = self
            .handle
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(handle) = handle.take() {
            unsafe {
                CloseHandle(handle);
            }
        }
    }
}

fn last_error(context: &'static str) -> anyhow::Error {
    let error = unsafe { GetLastError() } as i32;
    anyhow::Error::new(std::io::Error::from_raw_os_error(error)).context(context)
}

unsafe fn close_process_info(process_info: &PROCESS_INFORMATION) {
    // SAFETY: callers pass the process-information structure returned by CreateProcessAsUserW;
    // each nonzero handle is owned by that structure and is closed at most once here.
    unsafe {
        if process_info.hThread != 0 {
            CloseHandle(process_info.hThread);
        }
        if process_info.hProcess != 0 {
            CloseHandle(process_info.hProcess);
        }
    }
}

unsafe fn terminate_unassigned_process(process_info: &PROCESS_INFORMATION) -> Result<()> {
    let terminated = unsafe { TerminateProcess(process_info.hProcess, 1) };
    let result = if terminated == 0 {
        Err(last_error("TerminateProcess failed for suspended child"))
    } else {
        wait_for_process_termination(process_info.hProcess)
    };
    unsafe {
        close_process_info(process_info);
    }
    result
}

unsafe fn terminate_assigned_process(
    process_info: &PROCESS_INFORMATION,
    job: &JobObject,
) -> Result<()> {
    let result = job.terminate_and_wait(process_info.hProcess, 1);
    unsafe {
        close_process_info(process_info);
    }
    result
}

fn wait_for_process_termination(process: HANDLE) -> Result<()> {
    match unsafe { WaitForSingleObject(process, TERMINATION_WAIT_MS) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => Err(anyhow!(
            "process did not terminate within {TERMINATION_WAIT_MS} ms"
        )),
        WAIT_FAILED => Err(last_error(
            "WaitForSingleObject failed while terminating process",
        )),
        status => Err(anyhow!(
            "WaitForSingleObject returned unexpected status {status} while terminating process"
        )),
    }
}

unsafe fn assign_job_and_resume(process_info: &PROCESS_INFORMATION, job: &JobObject) -> Result<()> {
    if let Err(assign_error) = job.assign_process(process_info.hProcess) {
        return match unsafe { terminate_unassigned_process(process_info) } {
            Ok(()) => Err(assign_error),
            Err(cleanup_error) => Err(anyhow!(
                "{assign_error:#}; suspended child cleanup also failed: {cleanup_error:#}"
            )),
        };
    }

    if unsafe { ResumeThread(process_info.hThread) } == u32::MAX {
        let resume_error = last_error("ResumeThread failed");
        return match unsafe { terminate_assigned_process(process_info, job) } {
            Ok(()) => Err(resume_error),
            Err(cleanup_error) => Err(anyhow!(
                "{resume_error:#}; assigned child cleanup also failed: {cleanup_error:#}"
            )),
        };
    }
    Ok(())
}

pub struct CreatedProcess {
    pub process_info: PROCESS_INFORMATION,
    pub startup_info: STARTUPINFOW,
    pub job: JobObject,
    _desktop: LaunchDesktop,
}

impl CreatedProcess {
    /// Transfers the process, Job Object, and launch desktop to the caller.
    pub fn into_parts(self) -> (PROCESS_INFORMATION, JobObject, LaunchDesktop) {
        let Self {
            process_info,
            job,
            _desktop: desktop,
            ..
        } = self;
        (process_info, job, desktop)
    }
}

pub fn make_env_block(env: &HashMap<String, String>) -> Vec<u16> {
    let mut items: Vec<(String, String)> =
        env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    items.sort_by(|a, b| {
        a.0.to_uppercase()
            .cmp(&b.0.to_uppercase())
            .then(a.0.cmp(&b.0))
    });
    let mut w: Vec<u16> = Vec::new();
    for (k, v) in items {
        let mut s = to_wide(format!("{k}={v}"));
        s.pop();
        w.extend_from_slice(&s);
        w.push(0);
    }
    w.push(0);
    w
}

unsafe fn ensure_inheritable_stdio(si: &mut STARTUPINFOW) -> Result<()> {
    // SAFETY: `si` points to caller-owned startup storage; each standard handle is obtained
    // from the current process and is made inheritable before being copied into that storage.
    unsafe {
        for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let h = GetStdHandle(kind);
            if h == 0 || h == INVALID_HANDLE_VALUE {
                return Err(anyhow!("GetStdHandle failed: {}", GetLastError()));
            }
            if SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
                return Err(anyhow!("SetHandleInformation failed: {}", GetLastError()));
            }
        }
        si.dwFlags |= STARTF_USESTDHANDLES;
        si.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
        si.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
        si.hStdError = GetStdHandle(STD_ERROR_HANDLE);
    }
    Ok(())
}

/// # Safety
/// Caller must provide a valid primary token handle (`h_token`) with appropriate access,
/// and the `argv`, `cwd`, and `env_map` must remain valid for the duration of the call.
pub unsafe fn create_process_as_user(
    h_token: HANDLE,
    argv: &[String],
    cwd: &Path,
    env_map: &HashMap<String, String>,
    logs_base_dir: Option<&Path>,
    stdio: Option<(HANDLE, HANDLE, HANDLE)>,
    use_private_desktop: bool,
) -> Result<CreatedProcess> {
    let cmdline_str = argv_to_command_line(argv);
    let mut cmdline: Vec<u16> = to_wide(&cmdline_str);
    let env_block = make_env_block(env_map);
    let desktop = LaunchDesktop::prepare(use_private_desktop, logs_base_dir)?;
    let job = JobObject::create_kill_on_close()?;
    // SAFETY: PROCESS_INFORMATION and STARTUPINFO(EX) are plain Win32 output/input structs;
    // all raw pointers passed below are derived from the still-live local buffers or handles
    // covered by this function's caller safety contract.
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let cwd_wide = to_wide(cwd);
    let env_block_len = env_block.len();
    match stdio {
        Some((stdin_h, stdout_h, stderr_h)) => {
            let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
            si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            // Some processes (e.g., PowerShell) can fail with STATUS_DLL_INIT_FAILED
            // if lpDesktop is not set when launching with a restricted token.
            // Point explicitly at the interactive desktop or a private desktop.
            si.StartupInfo.lpDesktop = desktop.startup_info_desktop();
            si.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
            si.StartupInfo.hStdInput = stdin_h;
            si.StartupInfo.hStdOutput = stdout_h;
            si.StartupInfo.hStdError = stderr_h;
            let mut inherited_handles = vec![stdin_h, stdout_h];
            if !inherited_handles.contains(&stderr_h) {
                inherited_handles.push(stderr_h);
            }
            for &handle in &inherited_handles {
                if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                    == 0
                {
                    return Err(anyhow!(
                        "SetHandleInformation failed for stdio handle: {}",
                        unsafe { GetLastError() }
                    ));
                }
            }
            let mut attrs = ProcThreadAttributeList::new(/*attr_count*/ 1)?;
            attrs.set_handle_list(inherited_handles)?;
            si.lpAttributeList = attrs.as_mut_ptr();

            let creation_flags =
                CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED;
            let ok = unsafe {
                CreateProcessAsUserW(
                    h_token,
                    std::ptr::null(),
                    cmdline.as_mut_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    1,
                    creation_flags,
                    env_block.as_ptr() as *mut c_void,
                    cwd_wide.as_ptr(),
                    &si.StartupInfo,
                    &mut pi,
                )
            };
            if ok == 0 {
                let err = unsafe { GetLastError() } as i32;
                let msg = format!(
                    "CreateProcessAsUserW failed: {} ({}) | cwd={} | cmd={} | env_u16_len={} | si_flags={} | creation_flags={}",
                    err,
                    format_last_error(err),
                    cwd.display(),
                    cmdline_str,
                    env_block_len,
                    si.StartupInfo.dwFlags,
                    creation_flags,
                );
                logging::debug_log(&msg, logs_base_dir);
                return Err(std::io::Error::from_raw_os_error(err)).context(msg);
            }
            unsafe { assign_job_and_resume(&pi, &job) }?;
            Ok(CreatedProcess {
                process_info: pi,
                startup_info: si.StartupInfo,
                job,
                _desktop: desktop,
            })
        }
        None => {
            let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            si.lpDesktop = desktop.startup_info_desktop();
            unsafe { ensure_inheritable_stdio(&mut si)? };

            let creation_flags = CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED;
            let ok = unsafe {
                CreateProcessAsUserW(
                    h_token,
                    std::ptr::null(),
                    cmdline.as_mut_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    1,
                    creation_flags,
                    env_block.as_ptr() as *mut c_void,
                    cwd_wide.as_ptr(),
                    &si,
                    &mut pi,
                )
            };
            if ok == 0 {
                let err = unsafe { GetLastError() } as i32;
                let msg = format!(
                    "CreateProcessAsUserW failed: {} ({}) | cwd={} | cmd={} | env_u16_len={} | si_flags={} | creation_flags={}",
                    err,
                    format_last_error(err),
                    cwd.display(),
                    cmdline_str,
                    env_block_len,
                    si.dwFlags,
                    creation_flags,
                );
                logging::debug_log(&msg, logs_base_dir);
                return Err(std::io::Error::from_raw_os_error(err)).context(msg);
            }
            unsafe { assign_job_and_resume(&pi, &job) }?;
            Ok(CreatedProcess {
                process_info: pi,
                startup_info: si,
                job,
                _desktop: desktop,
            })
        }
    }
}

/// Controls whether the child's stdin handle is kept open for writing.
#[allow(dead_code)]
pub enum StdinMode {
    Closed,
    Open,
}

/// Controls how stderr is wired for a pipe-spawned process.
#[allow(dead_code)]
pub enum StderrMode {
    MergeStdout,
    Separate,
}

/// Handles returned by `spawn_process_with_pipes`.
#[allow(dead_code)]
pub struct PipeSpawnHandles {
    pub process: PROCESS_INFORMATION,
    pub job: JobObject,
    pub stdin_write: Option<HANDLE>,
    pub stdout_read: HANDLE,
    pub stderr_read: Option<HANDLE>,
    pub(crate) desktop: LaunchDesktop,
}

impl PipeSpawnHandles {
    /// Transfers every handle and the launch desktop to the caller.
    pub fn into_parts(
        self,
    ) -> (
        PROCESS_INFORMATION,
        JobObject,
        Option<HANDLE>,
        HANDLE,
        Option<HANDLE>,
        LaunchDesktop,
    ) {
        let Self {
            process,
            job,
            stdin_write,
            stdout_read,
            stderr_read,
            desktop,
        } = self;
        (process, job, stdin_write, stdout_read, stderr_read, desktop)
    }
}

/// Spawns a process with anonymous pipes and returns the relevant handles.
#[allow(clippy::too_many_arguments)]
pub fn spawn_process_with_pipes(
    h_token: HANDLE,
    argv: &[String],
    cwd: &Path,
    env_map: &HashMap<String, String>,
    stdin_mode: StdinMode,
    stderr_mode: StderrMode,
    use_private_desktop: bool,
    logs_base_dir: Option<&Path>,
) -> Result<PipeSpawnHandles> {
    let mut in_r: HANDLE = 0;
    let mut in_w: HANDLE = 0;
    let mut out_r: HANDLE = 0;
    let mut out_w: HANDLE = 0;
    let mut err_r: HANDLE = 0;
    let mut err_w: HANDLE = 0;
    unsafe {
        if CreatePipe(&mut in_r, &mut in_w, ptr::null_mut(), 0) == 0 {
            return Err(anyhow!("CreatePipe stdin failed: {}", GetLastError()));
        }
        if CreatePipe(&mut out_r, &mut out_w, ptr::null_mut(), 0) == 0 {
            CloseHandle(in_r);
            CloseHandle(in_w);
            return Err(anyhow!("CreatePipe stdout failed: {}", GetLastError()));
        }
        if matches!(stderr_mode, StderrMode::Separate)
            && CreatePipe(&mut err_r, &mut err_w, ptr::null_mut(), 0) == 0
        {
            CloseHandle(in_r);
            CloseHandle(in_w);
            CloseHandle(out_r);
            CloseHandle(out_w);
            return Err(anyhow!("CreatePipe stderr failed: {}", GetLastError()));
        }
    }

    let stderr_handle = match stderr_mode {
        StderrMode::MergeStdout => out_w,
        StderrMode::Separate => err_w,
    };

    let stdio = Some((in_r, out_w, stderr_handle));
    let spawn_result = unsafe {
        create_process_as_user(
            h_token,
            argv,
            cwd,
            env_map,
            logs_base_dir,
            stdio,
            use_private_desktop,
        )
    };
    let created = match spawn_result {
        Ok(v) => v,
        Err(err) => {
            unsafe {
                CloseHandle(in_r);
                CloseHandle(in_w);
                CloseHandle(out_r);
                CloseHandle(out_w);
                if matches!(stderr_mode, StderrMode::Separate) {
                    CloseHandle(err_r);
                    CloseHandle(err_w);
                }
            }
            return Err(err);
        }
    };
    let CreatedProcess {
        process_info: pi,
        job,
        _desktop: desktop,
        ..
    } = created;

    unsafe {
        CloseHandle(in_r);
        CloseHandle(out_w);
        if matches!(stderr_mode, StderrMode::Separate) {
            CloseHandle(err_w);
        }
        if matches!(stdin_mode, StdinMode::Closed) {
            CloseHandle(in_w);
        }
    }

    Ok(PipeSpawnHandles {
        process: pi,
        job,
        stdin_write: match stdin_mode {
            StdinMode::Open => Some(in_w),
            StdinMode::Closed => None,
        },
        stdout_read: out_r,
        stderr_read: match stderr_mode {
            StderrMode::Separate => Some(err_r),
            StderrMode::MergeStdout => None,
        },
        desktop,
    })
}

/// Reads a HANDLE until EOF and invokes `on_chunk` for each read.
pub fn read_handle_loop<F>(handle: HANDLE, mut on_chunk: F) -> std::thread::JoinHandle<()>
where
    F: FnMut(&[u8]) + Send + 'static,
{
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            let mut read_bytes: u32 = 0;
            let ok = unsafe {
                ReadFile(
                    handle,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut read_bytes,
                    ptr::null_mut(),
                )
            };
            if ok == 0 || read_bytes == 0 {
                break;
            }
            on_chunk(&buf[..read_bytes as usize]);
        }
        unsafe {
            CloseHandle(handle);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::JobObject;
    use std::io::Read;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::process::Command;
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::sync::mpsc::Receiver;
    use std::time::Duration;
    use std::time::Instant;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Threading::OpenProcess;
    use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    const WAIT_OBJECT_0: u32 = 0;

    type ReaderResult = Result<Vec<u8>, String>;

    fn spawn_reader<R>(mut reader: R) -> Receiver<ReaderResult>
    where
        R: Read + Send + 'static,
    {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut output = Vec::new();
            let result = reader
                .read_to_end(&mut output)
                .map(|_| output)
                .map_err(|error| error.to_string());
            let _ = sender.send(result);
        });
        receiver
    }

    fn escaped_ps_literal(path: &Path) -> String {
        path.display().to_string().replace('\'', "''")
    }

    fn wait_for_descendant_pid(path: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "root process did not report descendant PID"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_reader_still_blocked(receiver: &Receiver<ReaderResult>, stream: &str) {
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            other => panic!("{stream} reader completed before Job cleanup: {other:?}"),
        }
    }

    fn assert_reader_finished(receiver: &Receiver<ReaderResult>, stream: &str) -> Vec<u8> {
        receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("{stream} reader did not finish after Job cleanup"))
            .unwrap_or_else(|error| panic!("{stream} reader failed: {error}"))
    }

    fn spawn_capture_descendant(
        parent_exits: bool,
    ) -> (
        tempfile::TempDir,
        JobObject,
        std::process::Child,
        Receiver<ReaderResult>,
        Receiver<ReaderResult>,
        HANDLE,
    ) {
        let temp = tempfile::tempdir().expect("create temp directory");
        let start_marker = temp.path().join("start");
        let descendant_pid_file = temp.path().join("descendant.pid");
        let start_marker_literal = escaped_ps_literal(&start_marker);
        let descendant_pid_literal = escaped_ps_literal(&descendant_pid_file);
        let parent_tail = if parent_exits {
            "exit 0"
        } else {
            "Start-Sleep -Seconds 60"
        };
        let script = format!(
            "$ErrorActionPreference='Stop'; \
             while(-not (Test-Path -LiteralPath '{start_marker_literal}')) {{ \
                 Start-Sleep -Milliseconds 10 \
             }}; \
             $child=Start-Process -FilePath (Join-Path $PSHOME 'pwsh.exe') \
                 -ArgumentList @('-NoLogo','-NoProfile','-Command','Start-Sleep -Seconds 60') \
                 -NoNewWindow -PassThru; \
             Set-Content -LiteralPath '{descendant_pid_literal}' -Value $child.Id; \
             Write-Output 'parent stdout'; [Console]::Error.WriteLine('parent stderr'); \
             {parent_tail}"
        );

        let mut root = Command::new("pwsh.exe")
            .args(["-NoLogo", "-NoProfile", "-Command", &script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn capture root process");
        let stdout = spawn_reader(root.stdout.take().expect("capture stdout"));
        let stderr = spawn_reader(root.stderr.take().expect("capture stderr"));
        let job = JobObject::create_kill_on_close().expect("create capture Job Object");
        job.assign_process(root.as_raw_handle() as HANDLE)
            .expect("assign capture root before descendant spawn");
        std::fs::write(&start_marker, b"go").expect("release capture root process");

        let descendant_pid = wait_for_descendant_pid(&descendant_pid_file);
        let descendant = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, descendant_pid) };
        assert_ne!(descendant, 0, "open descendant process for synchronization");
        (temp, job, root, stdout, stderr, descendant)
    }

    #[test]
    fn normal_parent_exit_closes_inherited_capture_handles_before_join() {
        let (_temp, job, mut root, stdout, stderr, descendant) =
            spawn_capture_descendant(/*parent_exits*/ true);
        root.wait().expect("reap normal parent process");
        assert_reader_still_blocked(&stdout, "stdout");
        assert_reader_still_blocked(&stderr, "stderr");

        job.close().expect("close capture Job Object");
        let _ = assert_reader_finished(&stdout, "stdout");
        let _ = assert_reader_finished(&stderr, "stderr");
        assert_eq!(
            unsafe { WaitForSingleObject(descendant, 5_000) },
            WAIT_OBJECT_0,
            "normal parent cleanup must terminate the inherited-handle descendant"
        );
        unsafe {
            CloseHandle(descendant);
        }
    }

    #[test]
    fn timeout_termination_closes_inherited_capture_handles_before_join() {
        let (_temp, job, mut root, stdout, stderr, descendant) =
            spawn_capture_descendant(/*parent_exits*/ false);
        assert_reader_still_blocked(&stdout, "stdout");
        assert_reader_still_blocked(&stderr, "stderr");

        job.terminate_and_wait(root.as_raw_handle() as HANDLE, 1)
            .expect("terminate timed-out capture process tree");
        root.wait().expect("reap timed-out parent process");
        let _ = assert_reader_finished(&stdout, "stdout");
        let _ = assert_reader_finished(&stderr, "stderr");
        assert_eq!(
            unsafe { WaitForSingleObject(descendant, 5_000) },
            WAIT_OBJECT_0,
            "timeout cleanup must terminate the inherited-handle descendant"
        );
        unsafe {
            CloseHandle(descendant);
        }
    }

    #[test]
    fn terminating_job_terminates_descendant_processes() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let start_marker = temp.path().join("start");
        let descendant_pid_file = temp.path().join("descendant.pid");
        let start_marker_literal = start_marker.display().to_string().replace('\'', "''");
        let descendant_pid_literal = descendant_pid_file
            .display()
            .to_string()
            .replace('\'', "''");
        let script = format!(
            "$deadline=[DateTime]::UtcNow.AddSeconds(10); \
             while(-not (Test-Path -LiteralPath '{start_marker_literal}')) {{ \
                 if([DateTime]::UtcNow -gt $deadline) {{ exit 2 }}; \
                 Start-Sleep -Milliseconds 10 \
             }}; \
             $child=Start-Process -FilePath (Join-Path $PSHOME 'pwsh.exe') \
                 -ArgumentList @('-NoLogo','-NoProfile','-Command','Start-Sleep -Seconds 60') \
                 -PassThru; \
             Set-Content -LiteralPath '{descendant_pid_literal}' -Value $child.Id; \
             Wait-Process -Id $child.Id"
        );

        let job = JobObject::create_kill_on_close().expect("create kill-on-close Job Object");
        let mut root = Command::new("pwsh.exe")
            .args(["-NoLogo", "-NoProfile", "-Command", &script])
            .spawn()
            .expect("spawn synchronized root process");
        job.assign_process(root.as_raw_handle() as HANDLE)
            .expect("assign root process before allowing descendant spawn");
        std::fs::write(&start_marker, b"go").expect("release root process");

        let deadline = Instant::now() + Duration::from_secs(10);
        let descendant_pid = loop {
            if let Ok(text) = std::fs::read_to_string(&descendant_pid_file)
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "root process did not report descendant PID"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let descendant = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, descendant_pid) };
        assert_ne!(descendant, 0, "open descendant process for synchronization");

        job.terminate_and_wait(root.as_raw_handle() as HANDLE, 1)
            .expect("terminate complete process tree");
        root.wait().expect("reap root process");
        let descendant_wait = unsafe { WaitForSingleObject(descendant, 5_000) };
        unsafe {
            CloseHandle(descendant);
        }
        assert_eq!(
            descendant_wait, WAIT_OBJECT_0,
            "descendant must exit when its Job Object is terminated"
        );
    }
}
