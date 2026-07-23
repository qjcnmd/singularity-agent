#![cfg(target_os = "linux")]

//! Linux sandbox adapter.
//!
//! The adapter keeps Linux kernel objects behind this module.  The portable
//! `SandboxBackend` contract receives only policy, capability and result
//! values; namespace, Landlock and seccomp file descriptors never cross that
//! boundary.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use cap_fs_ext::DirExt as _;
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use singularity_core::{CancellationToken, is_protected_path};

use super::{
    CommandEnvironmentPolicy, CommandExecutionStatus, CommandRequest, CommandResult,
    CommandScriptRequest, CommandSemanticStatus, DEFAULT_MAX_OUTPUT_CHARS, SandboxBackend,
    SandboxBackendEnforcement, SandboxCapabilities, SandboxFilesystemMode, SandboxNetworkMode,
    WorkspaceChangeSummary, WorkspaceMutation, WorkspaceSnapshot, snapshot_workspace,
};

const BACKEND_NAME: &str = "linux";
const SANDBOX_UNAVAILABLE: &str = "linux sandbox unavailable";
const SANDBOX_POLICY_DENIED: &str = "linux sandbox policy denied";
const SANDBOX_PROTECTED_PATH_DENIED: &str = "linux sandbox protected path denied";
const SANDBOX_CWD_DENIED: &str = "linux sandbox cwd is outside workspace";
const SANDBOX_EXECUTABLE_UNAVAILABLE: &str = "linux sandbox executable unavailable";
const SANDBOX_HARDLINK_DENIED: &str = "linux sandbox workspace hardlink safety check failed";
const WORKSPACE_CHANGE_SUMMARY_UNAVAILABLE: &str =
    "capability_not_supported:workspace_change_summary";
const SANDBOX_CHILD_CANCELLED: &str = "linux sandbox command cancelled";
const SANDBOX_CHILD_TIMED_OUT: &str = "linux sandbox command timed out";
const SANDBOX_HOME: &str = "/run/singularity-home";

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_TYPE_PATH_BENEATH: u32 = 1;
const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13;
const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14;
const LANDLOCK_ACCESS_FS_ALL: u64 = (1 << 15) - 1;
const LANDLOCK_ACCESS_FS_READ: u64 = LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR;
const LANDLOCK_ACCESS_FS_WRITE: u64 = LANDLOCK_ACCESS_FS_WRITE_FILE
    | LANDLOCK_ACCESS_FS_REMOVE_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_FILE
    | LANDLOCK_ACCESS_FS_MAKE_CHAR
    | LANDLOCK_ACCESS_FS_MAKE_DIR
    | LANDLOCK_ACCESS_FS_MAKE_REG
    | LANDLOCK_ACCESS_FS_MAKE_SOCK
    | LANDLOCK_ACCESS_FS_MAKE_FIFO
    | LANDLOCK_ACCESS_FS_MAKE_BLOCK
    | LANDLOCK_ACCESS_FS_MAKE_SYM
    | LANDLOCK_ACCESS_FS_REFER
    | LANDLOCK_ACCESS_FS_TRUNCATE;

const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;
const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
const PR_SET_SECCOMP: libc::c_int = 22;
const SECCOMP_MODE_FILTER: libc::c_ulong = 2;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x15;
const BPF_RET_K: u16 = 0x06;
const SECCOMP_DATA_NR_OFFSET: u32 = 0;
const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;
const AUDIT_ARCH_ARM: u32 = 0x4000_0028;

const CHILD_SETUP_UNAVAILABLE: u8 = 1;
const CHILD_SETUP_CAPABILITY: u8 = 2;
const CHILD_SETUP_OVERLAY_FILESYSTEM: u8 = 3;
const WORKSPACE_TRANSACTION_DENIED: &str = "linux sandbox workspace transaction denied";

static TRANSACTION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[repr(C)]
#[derive(Clone, Copy)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

#[repr(C)]
struct LandlockRulesetAttr {
    handled_access_fs: u64,
}

#[repr(C)]
struct LandlockPathBeneathAttr {
    allowed_access: u64,
    parent_fd: RawFd,
}

#[repr(C)]
struct CapabilityHeader {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapabilityData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// A Linux kernel capability that is relevant to strict command execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxCapability {
    UserNamespace,
    PidNamespace,
    MountNamespace,
    NetworkNamespace,
    NoNewPrivs,
    Seccomp,
    Landlock,
    ProcessTreeCleanup,
    OverlayFilesystem,
    WorkspaceTransaction,
}

impl LinuxCapability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserNamespace => "user_namespace",
            Self::PidNamespace => "pid_namespace",
            Self::MountNamespace => "mount_namespace",
            Self::NetworkNamespace => "network_namespace",
            Self::NoNewPrivs => "no_new_privs",
            Self::Seccomp => "seccomp",
            Self::Landlock => "landlock",
            Self::ProcessTreeCleanup => "process_tree_cleanup",
            Self::OverlayFilesystem => "overlay_filesystem",
            Self::WorkspaceTransaction => "workspace_transaction",
        }
    }
}

/// Read-only capability facts collected without exposing kernel handles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxSandboxProbe {
    pub user_namespace: bool,
    pub pid_namespace: bool,
    pub mount_namespace: bool,
    pub network_namespace: bool,
    pub no_new_privs: bool,
    pub seccomp: bool,
    pub landlock_abi: Option<u32>,
    pub process_tree_cleanup: bool,
    /// Whether cgroup v2 is mounted; observed for diagnostics, not enforcement.
    pub cgroup_v2: bool,
    /// Whether the caller can delegate a cgroup; observed for diagnostics, not enforcement.
    pub cgroup_delegated: bool,
}

impl LinuxSandboxProbe {
    pub fn strict_ready(&self) -> bool {
        self.user_namespace
            && self.pid_namespace
            && self.mount_namespace
            && self.network_namespace
            && self.no_new_privs
            && self.seccomp
            && self.landlock_abi.is_some_and(|abi| abi >= 3)
            && self.process_tree_cleanup
    }

    pub fn missing_capability(&self) -> Option<LinuxCapability> {
        [
            (self.user_namespace, LinuxCapability::UserNamespace),
            (self.pid_namespace, LinuxCapability::PidNamespace),
            (self.mount_namespace, LinuxCapability::MountNamespace),
            (self.network_namespace, LinuxCapability::NetworkNamespace),
            (self.no_new_privs, LinuxCapability::NoNewPrivs),
            (self.seccomp, LinuxCapability::Seccomp),
            (
                self.landlock_abi.is_some_and(|abi| abi >= 3),
                LinuxCapability::Landlock,
            ),
            (
                self.process_tree_cleanup,
                LinuxCapability::ProcessTreeCleanup,
            ),
        ]
        .into_iter()
        .find_map(|(available, capability)| (!available).then_some(capability))
    }
}

static PROBE: OnceLock<LinuxSandboxProbe> = OnceLock::new();

/// Probe the Linux controls needed by the strict adapter.
pub fn probe_linux_capabilities() -> LinuxSandboxProbe {
    PROBE
        .get_or_init(|| LinuxSandboxProbe {
            user_namespace: probe_child(unshare_user_namespace),
            pid_namespace: probe_child(unshare_pid_namespace),
            mount_namespace: probe_child(unshare_mount_namespace),
            network_namespace: probe_child(unshare_network_namespace),
            no_new_privs: probe_child(probe_no_new_privs),
            seccomp: probe_child(probe_seccomp),
            landlock_abi: probe_landlock_abi(),
            process_tree_cleanup: probe_child(probe_process_group),
            cgroup_v2: Path::new("/sys/fs/cgroup/cgroup.controllers").is_file(),
            cgroup_delegated: cgroup_is_writable(),
        })
        .clone()
}

fn probe_child(probe: fn() -> bool) -> bool {
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let success = probe();
        unsafe { libc::_exit(i32::from(!success)) };
    }
    if pid < 0 {
        return false;
    }
    let mut status = 0;
    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
    waited == pid && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
}

fn unshare_user_namespace() -> bool {
    unsafe { libc::unshare(libc::CLONE_NEWUSER) == 0 }
}

fn unshare_pid_namespace() -> bool {
    unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWPID) == 0 }
}

fn unshare_mount_namespace() -> bool {
    unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) == 0 }
}

fn unshare_network_namespace() -> bool {
    unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) == 0 }
}

fn probe_no_new_privs() -> bool {
    unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0 }
}

fn probe_seccomp() -> bool {
    if !probe_no_new_privs() {
        return false;
    }
    install_seccomp_filter(false).is_ok()
}

fn probe_process_group() -> bool {
    unsafe { libc::setpgid(0, 0) == 0 }
}

fn probe_landlock_abi() -> Option<u32> {
    let abi = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            ptr::null::<LandlockRulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
            0usize,
        )
    };
    (abi >= 0).then_some(abi as u32)
}

fn cgroup_is_writable() -> bool {
    let Ok(path) = CString::new("/sys/fs/cgroup") else {
        return false;
    };
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxSandboxError {
    Unavailable,
    WorkspaceObservationUnavailable,
    CapabilityNotSupported(LinuxCapability),
    PolicyDenied(&'static str),
    ExecutableUnavailable,
}

impl LinuxSandboxError {
    fn into_result(self, command_id: &str) -> CommandResult {
        match self {
            Self::Unavailable => CommandResult::backend_error(command_id, SANDBOX_UNAVAILABLE),
            Self::WorkspaceObservationUnavailable => {
                CommandResult::unsupported(command_id, WORKSPACE_CHANGE_SUMMARY_UNAVAILABLE)
            }
            Self::CapabilityNotSupported(capability) => CommandResult::unsupported(
                command_id,
                format!("capability_not_supported:{}", capability.as_str()),
            ),
            Self::PolicyDenied(reason) => CommandResult::policy_denied(command_id, reason),
            Self::ExecutableUnavailable => {
                CommandResult::executable_unavailable(command_id, SANDBOX_EXECUTABLE_UNAVAILABLE)
            }
        }
        .with_workspace_mutation(WorkspaceMutation::Unknown)
        .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Unavailable)
    }
}

/// Linux implementation of the portable command sandbox contract.
#[derive(Debug, Clone)]
pub struct LinuxSandboxBackend {
    probe: LinuxSandboxProbe,
}

impl Default for LinuxSandboxBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxSandboxBackend {
    pub fn new() -> Self {
        Self {
            probe: probe_linux_capabilities(),
        }
    }

    pub fn probe(&self) -> &LinuxSandboxProbe {
        &self.probe
    }

    fn strict_capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            filesystem_isolation: true,
            copy_on_write: true,
            readonly_mount: true,
            network_isolation: true,
            env_isolation: true,
            restricted_token: false,
            job_object: false,
            path_admission: true,
            process_tree_kill: true,
            timeout: true,
            output_limit: true,
            memory_limit: false,
            process_limit: false,
            artifact_capture: false,
            change_detection: true,
        }
    }

    fn execute_inner(
        &self,
        input: LinuxExecutionInput,
        cancellation: &CancellationToken,
    ) -> CommandResult {
        if cancellation.is_cancelled() {
            return CommandResult::cancelled(&input.command_id, 0)
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Unavailable);
        }
        let Some(capability) = self.probe.missing_capability() else {
            return match PreparedCommand::from_parts(
                &input.command_id,
                input.argv,
                input.cwd,
                input.timeout_seconds,
                input.network,
                input.filesystem,
                input.environment,
            ) {
                Ok(prepared) => run_prepared_command(&input.command_id, prepared, cancellation),
                Err(error) => error.into_result(&input.command_id),
            };
        };
        LinuxSandboxError::CapabilityNotSupported(capability).into_result(&input.command_id)
    }
}

struct LinuxExecutionInput {
    command_id: String,
    argv: Vec<String>,
    cwd: String,
    timeout_seconds: u64,
    network: SandboxNetworkMode,
    filesystem: super::SandboxFilesystemPolicy,
    environment: CommandEnvironmentPolicy,
}

impl SandboxBackend for LinuxSandboxBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn capabilities(&self) -> SandboxCapabilities {
        if self.probe.strict_ready() {
            self.strict_capabilities()
        } else {
            SandboxCapabilities::unavailable()
        }
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        self.execute_cancellable(request, &CancellationToken::new())
    }

    fn execute_cancellable(
        &self,
        request: &CommandRequest,
        cancellation: &CancellationToken,
    ) -> CommandResult {
        self.execute_inner(
            LinuxExecutionInput {
                command_id: request.command_id.clone(),
                argv: request.argv.clone(),
                cwd: request.cwd.clone(),
                timeout_seconds: request.timeout_seconds,
                network: request.network.mode.clone(),
                filesystem: request.filesystem.clone(),
                environment: request.environment.clone(),
            },
            cancellation,
        )
    }

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        self.execute_script_cancellable(request, &CancellationToken::new())
    }

    fn execute_script_cancellable(
        &self,
        request: &CommandScriptRequest,
        cancellation: &CancellationToken,
    ) -> CommandResult {
        if request.script.trim().is_empty() {
            return CommandResult::policy_denied(&request.command_id, SANDBOX_POLICY_DENIED)
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Unavailable);
        }
        self.execute_inner(
            LinuxExecutionInput {
                command_id: request.command_id.clone(),
                argv: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    request.script.clone(),
                ],
                cwd: request.cwd.clone(),
                timeout_seconds: request.timeout_seconds,
                network: request.network.mode.clone(),
                filesystem: request.filesystem.clone(),
                environment: request.environment.clone(),
            },
            cancellation,
        )
    }
}

#[derive(Debug, Clone)]
struct ProtectedPath {
    path: PathBuf,
    is_dir: bool,
}

#[derive(Debug)]
struct PreparedCommand {
    workspace: PathBuf,
    cwd: PathBuf,
    executable: PathBuf,
    /// 非标准 executable 及已识别 toolchain 布局所需的最小只读路径。
    runtime_read_paths: Vec<PathBuf>,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    timeout: Duration,
    network: SandboxNetworkMode,
    filesystem: SandboxFilesystemMode,
    protected_paths: Vec<ProtectedPath>,
    before: Option<WorkspaceSnapshot>,
    transaction: Option<WorkspaceTransaction>,
}

impl PreparedCommand {
    fn from_parts(
        _command_id: &str,
        mut argv: Vec<String>,
        cwd: String,
        timeout_seconds: u64,
        network: SandboxNetworkMode,
        filesystem: super::SandboxFilesystemPolicy,
        environment: CommandEnvironmentPolicy,
    ) -> Result<Self, LinuxSandboxError> {
        if argv.is_empty() || argv.iter().any(|part| part.as_bytes().contains(&0)) {
            return Err(LinuxSandboxError::PolicyDenied(SANDBOX_POLICY_DENIED));
        }
        let workspace = canonical_directory(Path::new(&filesystem.workspace_root))
            .map_err(|_| LinuxSandboxError::PolicyDenied(SANDBOX_POLICY_DENIED))?;
        let cwd_path = resolve_cwd(&workspace, Path::new(&cwd))?;
        if is_protected_relative(&workspace, &cwd_path) {
            return Err(LinuxSandboxError::PolicyDenied(
                SANDBOX_PROTECTED_PATH_DENIED,
            ));
        }
        let mut env = sanitized_environment(&environment);
        let resolved = resolve_executable(&argv[0], &cwd_path, &env)?;
        if std::iter::once(&resolved.executable)
            .chain(std::iter::once(&resolved.invocation))
            .chain(resolved.runtime_read_paths.iter())
            .any(|path| is_protected_path(&path.to_string_lossy()))
        {
            return Err(LinuxSandboxError::PolicyDenied(
                SANDBOX_PROTECTED_PATH_DENIED,
            ));
        }
        for (name, value) in &resolved.environment {
            set_environment_value(&mut env, name, value);
        }
        argv[0] = resolved.invocation.to_string_lossy().into_owned();
        let protected_paths = collect_protected_paths(&workspace)
            .map_err(|_| LinuxSandboxError::PolicyDenied(SANDBOX_PROTECTED_PATH_DENIED))?;
        validate_workspace_hardlinks(&workspace)
            .map_err(|_| LinuxSandboxError::PolicyDenied(SANDBOX_HARDLINK_DENIED))?;
        let before = if matches!(filesystem.mode, SandboxFilesystemMode::WorkspaceWrite) {
            Some(
                snapshot_workspace(&workspace)
                    .map_err(|_| LinuxSandboxError::WorkspaceObservationUnavailable)?,
            )
        } else {
            None
        };
        let transaction = if matches!(filesystem.mode, SandboxFilesystemMode::WorkspaceWrite) {
            Some(WorkspaceTransaction::new(&workspace).map_err(|_| {
                LinuxSandboxError::CapabilityNotSupported(LinuxCapability::OverlayFilesystem)
            })?)
        } else {
            None
        };
        Ok(Self {
            workspace,
            cwd: cwd_path,
            executable: resolved.executable,
            runtime_read_paths: resolved.runtime_read_paths,
            argv,
            env,
            timeout: Duration::from_secs(timeout_seconds),
            network,
            filesystem: filesystem.mode,
            protected_paths,
            before,
            transaction,
        })
    }
}

#[derive(Debug)]
/// Parent-owned OverlayFS backing storage that never aliases the real workspace.
struct WorkspaceTransaction {
    root: PathBuf,
    upper: PathBuf,
    work: PathBuf,
}

impl WorkspaceTransaction {
    fn new(workspace: &Path) -> Result<Self, std::io::Error> {
        let temporary_root = std::env::temp_dir();
        for _ in 0..64 {
            let sequence = TRANSACTION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let root = temporary_root.join(format!(
                "singularity-workspace-view-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&root) {
                Ok(()) => {
                    let upper = root.join("upper");
                    let work = root.join("work");
                    if let Err(error) =
                        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
                            .and_then(|_| fs::create_dir(&upper))
                            .and_then(|_| fs::create_dir(&work))
                            .and_then(|_| seed_internal_hardlinks(workspace, &upper))
                    {
                        make_tree_owner_accessible(&root);
                        let _ = fs::remove_dir_all(&root);
                        return Err(error);
                    }
                    return Ok(Self { root, upper, work });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "workspace transaction namespace exhausted",
        ))
    }
}

/// Copy internal hardlink groups into the upper layer so writes preserve link semantics.
fn seed_internal_hardlinks(workspace: &Path, upper: &Path) -> Result<(), std::io::Error> {
    fn visit(
        directory: &Dir,
        relative_parent: &Path,
        groups: &mut BTreeMap<(u64, u64), Vec<PathBuf>>,
    ) -> Result<(), std::io::Error> {
        let mut entries = directory.entries()?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let relative = relative_parent.join(&name);
            let Some(name_text) = name.to_str() else {
                return Err(std::io::Error::other("non-Unicode hardlink path"));
            };
            let Some(relative_text) = relative.to_str() else {
                return Err(std::io::Error::other("non-Unicode hardlink path"));
            };
            if is_protected_path(name_text) || is_protected_path(relative_text) {
                continue;
            }
            let metadata = directory.symlink_metadata(&name)?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                let child = directory.open_dir_nofollow(&name)?;
                visit(&child, &relative, groups)?;
            } else if metadata.is_file() && cap_fs_ext::MetadataExt::nlink(&metadata) > 1 {
                groups
                    .entry((
                        cap_fs_ext::MetadataExt::dev(&metadata),
                        cap_fs_ext::MetadataExt::ino(&metadata),
                    ))
                    .or_default()
                    .push(relative);
            }
        }
        Ok(())
    }

    let mut groups = BTreeMap::new();
    let workspace_directory = Dir::open_ambient_dir(workspace, ambient_authority())?;
    visit(&workspace_directory, Path::new(""), &mut groups)?;
    for paths in groups.into_values().filter(|paths| paths.len() > 1) {
        let first_upper = upper.join(&paths[0]);
        if let Some(parent) = first_upper.parent() {
            create_seed_directory_path(
                &workspace_directory,
                upper,
                parent.strip_prefix(upper).unwrap_or(parent),
            )?;
        }
        let mut first_source = workspace_directory.open(&paths[0])?;
        if file_has_extended_attributes(&first_source)? {
            return Err(std::io::Error::other(
                "hardlinked workspace file has extended attributes",
            ));
        }
        let mut first_destination = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&first_upper)?;
        io::copy(&mut first_source, &mut first_destination)?;
        let first_metadata = workspace_directory.symlink_metadata(&paths[0])?;
        fs::set_permissions(
            &first_upper,
            fs::Permissions::from_mode(cap_std::fs::PermissionsExt::mode(
                &first_metadata.permissions(),
            )),
        )?;
        for relative in paths.iter().skip(1) {
            let destination = upper.join(relative);
            if let Some(parent) = destination.parent() {
                create_seed_directory_path(
                    &workspace_directory,
                    upper,
                    parent.strip_prefix(upper).unwrap_or(parent),
                )?;
            }
            fs::hard_link(&first_upper, destination)?;
        }
    }
    Ok(())
}

fn file_has_extended_attributes(file: &cap_std::fs::File) -> Result<bool, std::io::Error> {
    let size = unsafe { libc::flistxattr(file.as_raw_fd(), ptr::null_mut(), 0) };
    if size < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(size != 0)
}

fn create_seed_directory_path(
    workspace: &Dir,
    upper: &Path,
    relative: &Path,
) -> Result<(), std::io::Error> {
    let mut current = PathBuf::new();
    for component in relative.components() {
        current.push(component);
        let destination = upper.join(&current);
        if !destination.exists() {
            fs::create_dir(&destination)?;
            let source = workspace.symlink_metadata(&current)?;
            fs::set_permissions(
                &destination,
                fs::Permissions::from_mode(cap_std::fs::PermissionsExt::mode(
                    &source.permissions(),
                )),
            )?;
        }
    }
    Ok(())
}

impl Drop for WorkspaceTransaction {
    fn drop(&mut self) {
        make_tree_owner_accessible(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn canonical_directory(path: &Path) -> Result<PathBuf, std::io::Error> {
    let canonical = fs::canonicalize(path)?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(std::io::Error::other("not a directory"))
    }
}

fn resolve_cwd(workspace: &Path, requested: &Path) -> Result<PathBuf, LinuxSandboxError> {
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        workspace.join(requested)
    };
    let cwd = canonical_directory(&candidate)
        .map_err(|_| LinuxSandboxError::PolicyDenied(SANDBOX_CWD_DENIED))?;
    if cwd.starts_with(workspace) {
        Ok(cwd)
    } else {
        Err(LinuxSandboxError::PolicyDenied(SANDBOX_CWD_DENIED))
    }
}

fn sanitized_environment(policy: &CommandEnvironmentPolicy) -> Vec<(String, String)> {
    let mut values = std::env::vars()
        .filter(|(name, _)| !is_secret_env_name(name) && !is_unsafe_env_name(name))
        .filter(|(name, _)| {
            policy != &CommandEnvironmentPolicy::EvaluationIsolated
                || !is_evaluation_host_environment(name)
        })
        .collect::<Vec<_>>();
    values.retain(|(name, _)| name != "HOME" && name != "TMPDIR" && name != "PWD");
    values.push(("HOME".to_string(), SANDBOX_HOME.to_string()));
    values.push(("TMPDIR".to_string(), "/run".to_string()));
    values
}

fn is_secret_env_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    [
        "API_KEY",
        "AUTH",
        "CREDENTIAL",
        "PASSWORD",
        "SECRET",
        "TOKEN",
    ]
    .into_iter()
    .any(|marker| name.contains(marker))
}

fn is_unsafe_env_name(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "BASH_ENV"
            | "ENV"
            | "GCONV_PATH"
            | "LD_AUDIT"
            | "LD_DEBUG"
            | "LD_DEBUG_OUTPUT"
            | "LD_LIBRARY_PATH"
            | "LD_PRELOAD"
            | "LD_PROFILE"
            | "NODE_OPTIONS"
            | "PERL5OPT"
            | "PYTHONINSPECT"
            | "PYTHONSTARTUP"
            | "RUBYOPT"
    )
}

fn is_evaluation_host_environment(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    name.starts_with("SINGULARITY_")
        || matches!(
            name.as_str(),
            "CARGO_TARGET_DIR"
                | "CARGO_BUILD_TARGET"
                | "CARGO_ENCODED_RUSTFLAGS"
                | "RUSTFLAGS"
                | "RUSTDOCFLAGS"
                | "RUSTC_WRAPPER"
                | "RUSTC_WORKSPACE_WRAPPER"
                | "NODE_OPTIONS"
                | "NODE_PATH"
                | "PYTHONHOME"
                | "PYTHONPATH"
                | "VIRTUAL_ENV"
                | "GOFLAGS"
                | "GOWORK"
        )
}

fn env_value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn set_environment_value(env: &mut Vec<(String, String)>, name: &str, value: &str) {
    env.retain(|(key, _)| key != name);
    env.push((name.to_string(), value.to_string()));
}

fn resolve_executable(
    requested: &str,
    cwd: &Path,
    env: &[(String, String)],
) -> Result<ResolvedExecutable, LinuxSandboxError> {
    let (invocation, executable) = resolve_executable_paths(requested, cwd, env)?;
    let mut runtime_read_paths = runtime_read_paths(&invocation, &executable, env);
    let mut environment = runtime_environment(&invocation, &executable, env);
    if let Some(shebang) = resolve_shebang(&executable, cwd, env)? {
        for path in shebang.runtime_read_paths {
            push_unique_path(&mut runtime_read_paths, path);
        }
        for (name, value) in shebang.environment {
            if !environment.iter().any(|(existing, _)| existing == &name) {
                environment.push((name, value));
            }
        }
    }
    Ok(ResolvedExecutable {
        executable,
        invocation,
        runtime_read_paths,
        environment,
    })
}

fn resolve_executable_paths(
    requested: &str,
    cwd: &Path,
    env: &[(String, String)],
) -> Result<(PathBuf, PathBuf), LinuxSandboxError> {
    let requested_path = Path::new(requested);
    let candidate = if requested_path.is_absolute() || requested_path.components().count() > 1 {
        if requested_path.is_absolute() {
            requested_path.to_path_buf()
        } else {
            cwd.join(requested_path)
        }
    } else {
        let Some(path) = env_value(env, "PATH") else {
            return Err(LinuxSandboxError::ExecutableUnavailable);
        };
        let mut found = None;
        for directory in std::env::split_paths(path) {
            if !directory.is_absolute() {
                continue;
            }
            let candidate = directory.join(requested_path);
            if candidate.is_file() {
                found = Some(candidate);
                break;
            }
        }
        found.ok_or(LinuxSandboxError::ExecutableUnavailable)?
    };
    let file_name = candidate
        .file_name()
        .ok_or(LinuxSandboxError::ExecutableUnavailable)?;
    let parent = candidate
        .parent()
        .ok_or(LinuxSandboxError::ExecutableUnavailable)?;
    let invocation = fs::canonicalize(parent)
        .map_err(|_| LinuxSandboxError::ExecutableUnavailable)?
        .join(file_name);
    let canonical =
        fs::canonicalize(&invocation).map_err(|_| LinuxSandboxError::ExecutableUnavailable)?;
    let metadata =
        fs::metadata(&canonical).map_err(|_| LinuxSandboxError::ExecutableUnavailable)?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(LinuxSandboxError::ExecutableUnavailable);
    }
    Ok((invocation, canonical))
}

/// 已解析的 executable 将实际 exec target 与调用身份分开保存。
#[derive(Debug, Clone)]
struct ResolvedExecutable {
    /// 传给 `execve` 的规范化目标。
    executable: PathBuf,
    /// 保留最终 symlink 名称的绝对 `argv[0]`，用于 venv 与 rustup proxy 身份。
    invocation: PathBuf,
    /// Landlock 额外允许的文件或明确 toolchain 目录。
    runtime_read_paths: Vec<PathBuf>,
    /// 已识别运行时所需的安全环境覆盖。
    environment: Vec<(String, String)>,
}

#[derive(Debug)]
struct ResolvedShebang {
    runtime_read_paths: Vec<PathBuf>,
    environment: Vec<(String, String)>,
}

fn runtime_read_paths(
    invocation: &Path,
    executable: &Path,
    env: &[(String, String)],
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if !is_standard_runtime_path(executable) {
        push_unique_path(&mut paths, executable.to_path_buf());
    }
    if let Some(root) = python_venv_root(invocation) {
        for candidate in [
            root.join("pyvenv.cfg"),
            root.join("bin"),
            root.join("lib"),
            root.join("lib64"),
        ] {
            push_existing_canonical_path(&mut paths, &candidate);
        }
    }
    if let Some(root) = rustup_home(invocation, executable, env) {
        push_unique_path(&mut paths, root);
    }
    if let Some(root) =
        nvm_node_version_root(invocation).or_else(|| nvm_node_version_root(executable))
    {
        push_unique_path(&mut paths, root);
    }
    if invocation.file_name().is_some_and(|name| name == "node")
        || executable.file_name().is_some_and(|name| name == "node")
    {
        for candidate in [
            Path::new("/etc/ssl/openssl.cnf"),
            Path::new("/etc/ssl/certs"),
        ] {
            push_existing_canonical_path(&mut paths, candidate);
        }
    }
    paths
}

fn runtime_environment(
    invocation: &Path,
    executable: &Path,
    env: &[(String, String)],
) -> Vec<(String, String)> {
    let mut environment = Vec::new();
    if let Some(root) = python_venv_root(invocation) {
        environment.push((
            "VIRTUAL_ENV".to_string(),
            root.to_string_lossy().into_owned(),
        ));
    }
    if let Some(root) = rustup_home(invocation, executable, env) {
        environment.push((
            "RUSTUP_HOME".to_string(),
            root.to_string_lossy().into_owned(),
        ));
        environment.push(("CARGO_HOME".to_string(), format!("{SANDBOX_HOME}/cargo")));
    }
    environment
}

fn is_standard_runtime_path(path: &Path) -> bool {
    [
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/usr/local/bin",
        "/lib",
        "/lib64",
        "/usr/lib",
        "/usr/lib64",
        "/usr/share/nodejs",
        "/proc",
    ]
    .into_iter()
    .any(|root| path.starts_with(Path::new(root)))
}

fn python_venv_root(invocation: &Path) -> Option<PathBuf> {
    let bin = invocation.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    let root = bin.parent()?;
    root.join("pyvenv.cfg")
        .is_file()
        .then(|| root.to_path_buf())
}

fn rustup_home(invocation: &Path, executable: &Path, env: &[(String, String)]) -> Option<PathBuf> {
    const RUSTUP_PROXIES: [&str; 15] = [
        "cargo",
        "cargo-clippy",
        "cargo-fmt",
        "cargo-fuzz",
        "cargo-miri",
        "clippy-driver",
        "rls",
        "rust-analyzer",
        "rustc",
        "rustdoc",
        "rustfmt",
        "rust-gdb",
        "rust-gdbgui",
        "rust-lldb",
        "rustup",
    ];
    if executable.file_name()? != "rustup"
        || !RUSTUP_PROXIES.contains(&invocation.file_name()?.to_str()?)
    {
        return None;
    }
    let cargo_home = match env_value(env, "CARGO_HOME") {
        Some(configured) => fs::canonicalize(configured).ok()?,
        None => {
            let cargo_bin = invocation.parent()?;
            let cargo_home = cargo_bin.parent()?;
            if cargo_bin.file_name()? != "bin" || cargo_home.file_name()? != ".cargo" {
                return None;
            }
            fs::canonicalize(cargo_home).ok()?
        }
    };
    let invocation_parent = fs::canonicalize(invocation.parent()?).ok()?;
    if invocation_parent != cargo_home.join("bin") {
        return None;
    }
    let rustup = match env_value(env, "RUSTUP_HOME") {
        Some(configured) => PathBuf::from(configured),
        None => cargo_home.parent()?.join(".rustup"),
    };
    fs::canonicalize(rustup).ok().filter(|path| path.is_dir())
}

fn nvm_node_version_root(path: &Path) -> Option<PathBuf> {
    for root in path.ancestors() {
        let node = root.parent()?;
        let versions = node.parent()?;
        let nvm = versions.parent()?;
        if node.file_name()? == "node"
            && versions.file_name()? == "versions"
            && nvm.file_name()? == ".nvm"
        {
            return fs::canonicalize(root).ok().filter(|path| path.is_dir());
        }
    }
    None
}

fn resolve_shebang(
    executable: &Path,
    cwd: &Path,
    env: &[(String, String)],
) -> Result<Option<ResolvedShebang>, LinuxSandboxError> {
    let mut file = File::open(executable).map_err(|_| LinuxSandboxError::ExecutableUnavailable)?;
    let mut header = [0u8; 256];
    let n = file
        .read(&mut header)
        .map_err(|_| LinuxSandboxError::ExecutableUnavailable)?;
    if n < 2 || header[..2] != *b"#!" {
        return Ok(None);
    }
    let line_end = header[2..n]
        .iter()
        .position(|&b| b == 10)
        .map(|p| p + 2)
        .unwrap_or(n);
    let line = std::str::from_utf8(&header[2..line_end])
        .map_err(|_| LinuxSandboxError::ExecutableUnavailable)?
        .trim();
    let mut parts = line.split_ascii_whitespace();
    let interpreter = parts
        .next()
        .ok_or(LinuxSandboxError::ExecutableUnavailable)?;
    if !Path::new(interpreter).is_absolute() {
        return Err(LinuxSandboxError::ExecutableUnavailable);
    }
    let (invocation, canonical) = resolve_executable_paths(interpreter, cwd, env)?;
    let mut paths = runtime_read_paths(&invocation, &canonical, env);
    let mut environment = runtime_environment(&invocation, &canonical, env);
    if canonical == Path::new("/usr/bin/env") {
        let remaining = parts.collect::<Vec<_>>();
        let program = match remaining.as_slice() {
            ["-S", program, ..] | ["--", program, ..] | [program, ..]
                if !program.starts_with('-') =>
            {
                *program
            }
            _ => return Err(LinuxSandboxError::ExecutableUnavailable),
        };
        let (program_invocation, program_executable) = resolve_executable_paths(program, cwd, env)?;
        for path in runtime_read_paths(&program_invocation, &program_executable, env) {
            push_unique_path(&mut paths, path);
        }
        for (name, value) in runtime_environment(&program_invocation, &program_executable, env) {
            if !environment.iter().any(|(existing, _)| existing == &name) {
                environment.push((name, value));
            }
        }
    }
    Ok(Some(ResolvedShebang {
        runtime_read_paths: paths,
        environment,
    }))
}

fn push_existing_canonical_path(paths: &mut Vec<PathBuf>, candidate: &Path) {
    if let Ok(canonical) = fs::canonicalize(candidate) {
        push_unique_path(paths, canonical);
    }
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn runtime_read_access(path: &Path) -> Option<u64> {
    if path.is_dir() {
        Some(LANDLOCK_ACCESS_FS_READ | LANDLOCK_ACCESS_FS_EXECUTE)
    } else if path.is_file() {
        Some(LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_EXECUTE)
    } else {
        None
    }
}

fn is_protected_relative(workspace: &Path, path: &Path) -> bool {
    path.strip_prefix(workspace).ok().is_some_and(|relative| {
        relative
            .components()
            .any(|component| is_protected_path(&component.as_os_str().to_string_lossy()))
    })
}

fn collect_protected_paths(workspace: &Path) -> Result<Vec<ProtectedPath>, std::io::Error> {
    fn visit(directory: &Path, paths: &mut Vec<ProtectedPath>) -> Result<(), std::io::Error> {
        let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            let name = entry.file_name();
            let protected = is_protected_path(&name.to_string_lossy());
            if protected {
                if metadata.file_type().is_symlink() {
                    return Err(std::io::Error::other("protected symlink"));
                }
                paths.push(ProtectedPath {
                    is_dir: metadata.is_dir(),
                    path,
                });
                continue;
            }
            if metadata.is_dir() {
                visit(&path, paths)?;
            }
        }
        Ok(())
    }

    let mut paths = Vec::new();
    visit(workspace, &mut paths)?;
    paths.sort_by_key(|item| item.path.components().count());
    let mut top_level = Vec::new();
    for item in paths {
        if top_level
            .iter()
            .any(|parent: &ProtectedPath| item.path.starts_with(&parent.path))
        {
            continue;
        }
        top_level.push(item);
    }
    Ok(top_level)
}

/// Reject regular files whose filesystem link count is not fully visible in the workspace.
fn validate_workspace_hardlinks(workspace: &Path) -> Result<(), ()> {
    fn visit(current: &Path, identities: &mut BTreeMap<(u64, u64), (u64, u64)>) -> Result<(), ()> {
        let mut entries = fs::read_dir(current)
            .map_err(|_| ())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ())?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|_| ())?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                visit(&path, identities)?;
            } else if metadata.is_file() {
                let identity = (metadata.dev(), metadata.ino());
                let links = identities.entry(identity).or_insert((0, metadata.nlink()));
                if links.1 != metadata.nlink() {
                    return Err(());
                }
                links.0 = links.0.checked_add(1).ok_or(())?;
            }
        }
        Ok(())
    }

    let mut identities = BTreeMap::new();
    visit(workspace, &mut identities)?;
    identities
        .values()
        .all(|(visible_links, filesystem_links)| visible_links == filesystem_links)
        .then_some(())
        .ok_or(())
}

struct ChildContext {
    ready_read: RawFd,
    status_write: RawFd,
    stdout_write: RawFd,
    stderr_write: RawFd,
    parent_fds: Vec<RawFd>,
    workspace: PathBuf,
    cwd: PathBuf,
    executable: PathBuf,
    argv: Vec<CString>,
    env: Vec<CString>,
    filesystem: SandboxFilesystemMode,
    network: SandboxNetworkMode,
    protected_paths: Vec<ProtectedPath>,
    runtime_read_paths: Vec<PathBuf>,
    overlay_upper: Option<PathBuf>,
    overlay_work: Option<PathBuf>,
}

fn run_prepared_command(
    command_id: &str,
    prepared: PreparedCommand,
    cancellation: &CancellationToken,
) -> CommandResult {
    let started = Instant::now();
    let overlay_upper = prepared
        .transaction
        .as_ref()
        .map(|transaction| transaction.upper.clone());
    let overlay_work = prepared
        .transaction
        .as_ref()
        .map(|transaction| transaction.work.clone());
    let Some((stdout_read, stdout_write)) = pipe_cloexec() else {
        return LinuxSandboxError::Unavailable.into_result(command_id);
    };
    let Some((stderr_read, stderr_write)) = pipe_cloexec() else {
        close_fd(stdout_read);
        close_fd(stdout_write);
        return LinuxSandboxError::Unavailable.into_result(command_id);
    };
    let Some((status_read, status_write)) = pipe_cloexec() else {
        close_fd(stdout_read);
        close_fd(stdout_write);
        close_fd(stderr_read);
        close_fd(stderr_write);
        return LinuxSandboxError::Unavailable.into_result(command_id);
    };
    let Some((ready_read, ready_write)) = pipe_cloexec() else {
        for fd in [
            stdout_read,
            stdout_write,
            stderr_read,
            stderr_write,
            status_read,
            status_write,
        ] {
            close_fd(fd);
        }
        return LinuxSandboxError::Unavailable.into_result(command_id);
    };

    let argv = match prepared
        .argv
        .iter()
        .map(|value| CString::new(value.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(argv) => argv,
        Err(_) => {
            for fd in [
                stdout_read,
                stdout_write,
                stderr_read,
                stderr_write,
                status_read,
                status_write,
                ready_read,
                ready_write,
            ] {
                close_fd(fd);
            }
            return LinuxSandboxError::PolicyDenied(SANDBOX_POLICY_DENIED).into_result(command_id);
        }
    };
    let env = match prepared
        .env
        .iter()
        .map(|(key, value)| CString::new(format!("{key}={value}")))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(env) => env,
        Err(_) => {
            for fd in [
                stdout_read,
                stdout_write,
                stderr_read,
                stderr_write,
                status_read,
                status_write,
                ready_read,
                ready_write,
            ] {
                close_fd(fd);
            }
            return LinuxSandboxError::PolicyDenied(SANDBOX_POLICY_DENIED).into_result(command_id);
        }
    };
    let context = Box::new(ChildContext {
        ready_read,
        status_write,
        stdout_write,
        stderr_write,
        parent_fds: vec![stdout_read, stderr_read, status_read, ready_write],
        workspace: prepared.workspace.clone(),
        cwd: prepared.cwd.clone(),
        executable: prepared.executable.clone(),
        argv,
        env,
        filesystem: prepared.filesystem.clone(),
        network: prepared.network.clone(),
        protected_paths: prepared.protected_paths.clone(),
        runtime_read_paths: prepared.runtime_read_paths.clone(),
        overlay_upper,
        overlay_work,
    });
    let mut context = context;
    let mut stack = vec![0u8; 1024 * 1024];
    let stack_top = ((stack.as_mut_ptr() as usize + stack.len()) & !15usize) as *mut libc::c_void;
    let flags = libc::CLONE_NEWUSER
        | libc::CLONE_NEWNS
        | libc::CLONE_NEWPID
        | if prepared.network == SandboxNetworkMode::Denied {
            libc::CLONE_NEWNET
        } else {
            0
        }
        | libc::SIGCHLD;
    let child = unsafe {
        libc::clone(
            child_main,
            stack_top,
            flags,
            (&mut *context) as *mut ChildContext as *mut libc::c_void,
        )
    };
    if child < 0 {
        for fd in [
            stdout_read,
            stdout_write,
            stderr_read,
            stderr_write,
            status_read,
            status_write,
            ready_read,
            ready_write,
        ] {
            close_fd(fd);
        }
        return LinuxSandboxError::Unavailable.into_result(command_id);
    }
    for fd in [stdout_write, stderr_write, status_write, ready_read] {
        close_fd(fd);
    }
    unsafe {
        libc::setpgid(child, child);
    }
    if write_user_namespace_maps(child).is_err() {
        kill_process_group(child);
        wait_for_exit(child);
        for fd in [stdout_read, stderr_read, status_read, ready_write] {
            close_fd(fd);
        }
        return LinuxSandboxError::CapabilityNotSupported(LinuxCapability::UserNamespace)
            .into_result(command_id);
    }
    let ready = [1u8];
    let ready_written = unsafe { libc::write(ready_write, ready.as_ptr().cast(), ready.len()) };
    close_fd(ready_write);
    if ready_written != ready.len() as isize {
        kill_process_group(child);
        wait_for_exit(child);
        for fd in [stdout_read, stderr_read, status_read] {
            close_fd(fd);
        }
        return LinuxSandboxError::Unavailable.into_result(command_id);
    }

    let stdout_thread = spawn_reader(stdout_read);
    let stderr_thread = spawn_reader(stderr_read);
    let (status, interrupted) = wait_for_child(child, prepared.timeout, cancellation);
    let captured = CapturedOutput {
        stdout: stdout_thread.join().unwrap_or_default(),
        stderr: stderr_thread.join().unwrap_or_default(),
    };
    let child_setup = read_child_setup_status(status_read);
    if let Some(kind) = child_setup {
        let error = match kind {
            CHILD_SETUP_CAPABILITY => {
                LinuxSandboxError::CapabilityNotSupported(LinuxCapability::Landlock)
            }
            CHILD_SETUP_OVERLAY_FILESYSTEM => {
                LinuxSandboxError::CapabilityNotSupported(LinuxCapability::OverlayFilesystem)
            }
            _ => LinuxSandboxError::Unavailable,
        };
        let (mutation, summary) = observed_workspace_change(&prepared.before, &prepared.workspace);
        let result = error
            .into_result(command_id)
            .with_workspace_mutation(mutation);
        return match summary {
            Some(summary) => result.with_workspace_change_summary(summary),
            None => result,
        };
    }
    if interrupted == InterruptKind::None
        && let Some(transaction) = prepared.transaction.as_ref()
    {
        let Some(before) = prepared.before.as_ref() else {
            return LinuxSandboxError::WorkspaceObservationUnavailable.into_result(command_id);
        };
        if overlay_contains_protected_change(&transaction.upper) {
            return CommandResult::policy_denied(command_id, WORKSPACE_TRANSACTION_DENIED)
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
                .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Strict);
        }
        if let Err(error) =
            commit_workspace_transaction(&prepared.workspace, before, &transaction.upper)
        {
            return match error {
                WorkspaceTransactionError::PolicyDenied => {
                    CommandResult::policy_denied(command_id, WORKSPACE_TRANSACTION_DENIED)
                        .with_workspace_mutation(WorkspaceMutation::Unknown)
                        .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Strict)
                }
                WorkspaceTransactionError::CapabilityNotSupported => {
                    LinuxSandboxError::CapabilityNotSupported(LinuxCapability::WorkspaceTransaction)
                        .into_result(command_id)
                }
            };
        }
    }
    let (mutation, summary) = observed_workspace_change(&prepared.before, &prepared.workspace);
    let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    let mut result = if interrupted == InterruptKind::Cancelled {
        interrupted_result(
            command_id,
            status,
            duration_ms,
            CommandExecutionStatus::Cancelled,
            CommandSemanticStatus::Cancelled,
            SANDBOX_CHILD_CANCELLED,
            &captured,
        )
    } else if interrupted == InterruptKind::TimedOut {
        interrupted_result(
            command_id,
            status,
            duration_ms,
            CommandExecutionStatus::TimedOut,
            CommandSemanticStatus::TimedOut,
            SANDBOX_CHILD_TIMED_OUT,
            &captured,
        )
    } else {
        CommandResult::executed(
            command_id,
            process_exit_code(status),
            duration_ms,
            captured.stdout.preview.clone(),
            captured.stderr.preview.clone(),
            captured.stdout.truncated || captured.stderr.truncated,
        )
    };
    result = result
        .with_workspace_mutation(mutation)
        .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Strict);
    match summary {
        Some(summary) => result.with_workspace_change_summary(summary),
        None => result,
    }
}

fn overlay_contains_protected_change(upper: &Path) -> bool {
    fn visit(directory: &Path, relative_parent: &Path) -> Result<bool, ()> {
        let entries = fs::read_dir(directory)
            .map_err(|_| ())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ())?;
        for entry in entries {
            let name = entry.file_name();
            let relative = relative_parent.join(&name);
            let name_text = name.to_str().ok_or(())?;
            let relative_text = relative.to_str().ok_or(())?;
            if is_protected_path(name_text) || is_protected_path(relative_text) {
                return Ok(true);
            }
            let metadata = fs::symlink_metadata(entry.path()).map_err(|_| ())?;
            if metadata.is_dir() && visit(&entry.path(), &relative)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    visit(upper, Path::new("")).unwrap_or(true)
}

const MAX_TRANSACTION_ENTRIES: usize = 100_000;
const MAX_TRANSACTION_DEPTH: usize = 256;

#[derive(Debug, Clone)]
/// A fully validated mutation to apply to the real workspace.
enum WorkspaceOperation {
    Delete(PathBuf),
    Replace(PathBuf),
    SetMetadata {
        relative: PathBuf,
        mode: u32,
        access: cap_std::time::SystemTime,
        modified: cap_std::time::SystemTime,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceTransactionError {
    PolicyDenied,
    CapabilityNotSupported,
}

/// Same-filesystem staging and backup directories used by the commit phase.
struct CommitArea {
    root: PathBuf,
    stage: PathBuf,
    backup: PathBuf,
}

/// Pinned directory identity and timestamps restored after entry-level renames.
struct DirectoryTimes {
    directory: Dir,
    access: cap_std::time::SystemTime,
    modified: cap_std::time::SystemTime,
}

/// Capability handles and rollback state retained until post-commit verification succeeds.
struct AppliedWorkspaceOperations {
    workspace_directory: Dir,
    stage_directory: Dir,
    backup_directory: Dir,
    moved: Vec<PathBuf>,
    installed: Vec<PathBuf>,
    metadata: Vec<DirectoryMetadata>,
    directory_times: Vec<DirectoryTimes>,
}

/// Original metadata retained so rollback restores an existing directory exactly.
struct DirectoryMetadata {
    relative: PathBuf,
    mode: u32,
    access: cap_std::time::SystemTime,
    modified: cap_std::time::SystemTime,
}

impl CommitArea {
    fn new(workspace: &Path) -> Result<Self, WorkspaceTransactionError> {
        let parent = workspace
            .parent()
            .ok_or(WorkspaceTransactionError::CapabilityNotSupported)?;
        for _ in 0..64 {
            let sequence = TRANSACTION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let root = parent.join(format!(
                ".singularity-workspace-commit-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&root) {
                Ok(()) => {
                    let stage = root.join("stage");
                    let backup = root.join("backup");
                    if fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
                        .and_then(|_| fs::create_dir(&stage))
                        .and_then(|_| fs::create_dir(&backup))
                        .is_err()
                    {
                        make_tree_owner_accessible(&root);
                        let _ = fs::remove_dir_all(&root);
                        return Err(WorkspaceTransactionError::CapabilityNotSupported);
                    }
                    return Ok(Self {
                        root,
                        stage,
                        backup,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(WorkspaceTransactionError::CapabilityNotSupported),
            }
        }
        Err(WorkspaceTransactionError::CapabilityNotSupported)
    }
}

impl Drop for CommitArea {
    fn drop(&mut self) {
        make_tree_owner_accessible(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Validate the complete upper representation, detect drift, and atomically install each change.
fn commit_workspace_transaction(
    workspace: &Path,
    before: &WorkspaceSnapshot,
    upper: &Path,
) -> Result<(), WorkspaceTransactionError> {
    let area = CommitArea::new(workspace)?;
    let mut operations = Vec::new();
    let mut visited = 0usize;
    let mut staged_links = BTreeMap::new();
    plan_overlay_directory(
        workspace,
        upper,
        Path::new(""),
        &area.stage,
        0,
        &mut visited,
        &mut operations,
        &mut staged_links,
    )?;
    let current = snapshot_workspace(workspace)
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    if &current != before {
        return Err(WorkspaceTransactionError::PolicyDenied);
    }
    let applied = apply_workspace_operations(workspace, &area, &operations)?;
    snapshot_workspace(workspace)
        .and_then(|after| before.change_summary(&after))
        .map_err(|_| {
            rollback_partial(
                &applied.workspace_directory,
                &applied.stage_directory,
                &applied.backup_directory,
                &applied.moved,
                &applied.installed,
                &applied.metadata,
                &applied.directory_times,
            );
            WorkspaceTransactionError::CapabilityNotSupported
        })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// Convert OverlayFS upper entries into a bounded, no-follow commit plan.
fn plan_overlay_directory(
    workspace: &Path,
    upper_directory: &Path,
    relative_parent: &Path,
    stage: &Path,
    depth: usize,
    visited: &mut usize,
    operations: &mut Vec<WorkspaceOperation>,
    staged_links: &mut BTreeMap<(u64, u64), PathBuf>,
) -> Result<(), WorkspaceTransactionError> {
    if depth > MAX_TRANSACTION_DEPTH || has_extended_attributes(upper_directory)? {
        return Err(WorkspaceTransactionError::PolicyDenied);
    }
    let mut entries = fs::read_dir(upper_directory)
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        *visited = visited.saturating_add(1);
        if *visited > MAX_TRANSACTION_ENTRIES {
            return Err(WorkspaceTransactionError::PolicyDenied);
        }
        let name = entry.file_name();
        let relative = relative_parent.join(&name);
        let name_text = name
            .to_str()
            .ok_or(WorkspaceTransactionError::PolicyDenied)?;
        let relative_text = relative
            .to_str()
            .ok_or(WorkspaceTransactionError::PolicyDenied)?;
        if is_protected_path(name_text) || is_protected_path(relative_text) {
            return Err(WorkspaceTransactionError::PolicyDenied);
        }
        let source = entry.path();
        let metadata = fs::symlink_metadata(&source)
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        let file_type = metadata.file_type();
        if file_type.is_char_device() && metadata.rdev() == 0 {
            operations.push(WorkspaceOperation::Delete(relative));
            continue;
        }
        if has_extended_attributes(&source)? {
            return Err(WorkspaceTransactionError::PolicyDenied);
        }
        if file_type.is_dir() {
            let destination_metadata = fs::symlink_metadata(workspace.join(&relative)).ok();
            if destination_metadata
                .as_ref()
                .is_some_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            {
                plan_overlay_directory(
                    workspace,
                    &source,
                    &relative,
                    stage,
                    depth + 1,
                    visited,
                    operations,
                    staged_links,
                )?;
                let desired_mode = metadata.permissions().mode();
                let desired_access = metadata
                    .accessed()
                    .map(cap_std::time::SystemTime::from_std)
                    .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
                let desired_modified = metadata
                    .modified()
                    .map(cap_std::time::SystemTime::from_std)
                    .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
                if destination_metadata.as_ref().is_some_and(|destination| {
                    destination.permissions().mode() != desired_mode
                        || destination.modified().ok() != Some(desired_modified.into_std())
                }) {
                    operations.push(WorkspaceOperation::SetMetadata {
                        relative,
                        mode: desired_mode,
                        access: desired_access,
                        modified: desired_modified,
                    });
                }
            } else {
                copy_upper_tree(
                    &source,
                    &stage.join(&relative),
                    depth + 1,
                    visited,
                    staged_links,
                )?;
                operations.push(WorkspaceOperation::Replace(relative));
            }
            continue;
        }
        if file_type.is_file() || file_type.is_symlink() {
            let destination = workspace.join(&relative);
            if upper_object_matches_workspace(&source, &destination, &metadata)? {
                continue;
            }
            copy_upper_object(&source, &stage.join(&relative), &metadata, staged_links)?;
            operations.push(WorkspaceOperation::Replace(relative));
            continue;
        }
        return Err(WorkspaceTransactionError::PolicyDenied);
    }
    Ok(())
}

fn copy_upper_tree(
    source: &Path,
    destination: &Path,
    depth: usize,
    visited: &mut usize,
    staged_links: &mut BTreeMap<(u64, u64), PathBuf>,
) -> Result<(), WorkspaceTransactionError> {
    if depth > MAX_TRANSACTION_DEPTH || has_extended_attributes(source)? {
        return Err(WorkspaceTransactionError::PolicyDenied);
    }
    let metadata = fs::symlink_metadata(source)
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    }
    fs::create_dir(destination).map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    let mut entries = fs::read_dir(source)
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        *visited = visited.saturating_add(1);
        if *visited > MAX_TRANSACTION_ENTRIES {
            return Err(WorkspaceTransactionError::PolicyDenied);
        }
        let name = entry.file_name();
        let name_text = name
            .to_str()
            .ok_or(WorkspaceTransactionError::PolicyDenied)?;
        if is_protected_path(name_text) {
            return Err(WorkspaceTransactionError::PolicyDenied);
        }
        let source = entry.path();
        let destination = destination.join(&name);
        let child_metadata = fs::symlink_metadata(&source)
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        if has_extended_attributes(&source)? {
            return Err(WorkspaceTransactionError::PolicyDenied);
        }
        let file_type = child_metadata.file_type();
        if file_type.is_char_device() && child_metadata.rdev() == 0 {
            return Err(WorkspaceTransactionError::PolicyDenied);
        }
        if file_type.is_dir() {
            copy_upper_tree(&source, &destination, depth + 1, visited, staged_links)?;
        } else if file_type.is_file() || file_type.is_symlink() {
            copy_upper_object(&source, &destination, &child_metadata, staged_links)?;
        } else {
            return Err(WorkspaceTransactionError::PolicyDenied);
        }
    }
    fs::set_permissions(
        destination,
        fs::Permissions::from_mode(metadata.permissions().mode()),
    )
    .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    copy_object_times(destination, &metadata, false)
}

fn copy_upper_object(
    source: &Path,
    destination: &Path,
    metadata: &fs::Metadata,
    staged_links: &mut BTreeMap<(u64, u64), PathBuf>,
) -> Result<(), WorkspaceTransactionError> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    }
    if metadata.file_type().is_symlink() {
        let target =
            fs::read_link(source).map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        std::os::unix::fs::symlink(target, destination)
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        copy_object_times(destination, metadata, true)
    } else {
        let identity = (metadata.dev(), metadata.ino());
        if metadata.nlink() > 1
            && let Some(first) = staged_links.get(&identity)
        {
            fs::hard_link(first, destination)
                .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        } else {
            fs::copy(source, destination)
                .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
            if metadata.nlink() > 1 {
                staged_links.insert(identity, destination.to_path_buf());
            }
        }
        fs::set_permissions(
            destination,
            fs::Permissions::from_mode(metadata.permissions().mode()),
        )
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        copy_object_times(destination, metadata, false)
    }
}

fn copy_object_times(
    destination: &Path,
    metadata: &fs::Metadata,
    symlink: bool,
) -> Result<(), WorkspaceTransactionError> {
    let parent = destination
        .parent()
        .ok_or(WorkspaceTransactionError::CapabilityNotSupported)?;
    let name = destination
        .file_name()
        .ok_or(WorkspaceTransactionError::CapabilityNotSupported)?;
    let directory = Dir::open_ambient_dir(parent, ambient_authority())
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    let access = metadata
        .accessed()
        .map(cap_std::time::SystemTime::from_std)
        .map(cap_fs_ext::SystemTimeSpec::Absolute)
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    let modified = metadata
        .modified()
        .map(cap_std::time::SystemTime::from_std)
        .map(cap_fs_ext::SystemTimeSpec::Absolute)
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    let result = if symlink {
        directory.set_symlink_times(name, Some(access), Some(modified))
    } else {
        directory.set_times(name, Some(access), Some(modified))
    };
    result.map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)
}

fn upper_object_matches_workspace(
    upper: &Path,
    workspace: &Path,
    upper_metadata: &fs::Metadata,
) -> Result<bool, WorkspaceTransactionError> {
    let Ok(workspace_metadata) = fs::symlink_metadata(workspace) else {
        return Ok(false);
    };
    if upper_metadata.file_type().is_symlink() != workspace_metadata.file_type().is_symlink()
        || upper_metadata.is_file() != workspace_metadata.is_file()
        || upper_metadata.permissions().mode() != workspace_metadata.permissions().mode()
        || upper_metadata.len() != workspace_metadata.len()
        || upper_metadata.modified().ok() != workspace_metadata.modified().ok()
    {
        return Ok(false);
    }
    if upper_metadata.file_type().is_symlink() {
        return Ok(fs::read_link(upper)
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?
            == fs::read_link(workspace)
                .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?);
    }
    if !upper_metadata.is_file() {
        return Ok(false);
    }
    let mut upper_file =
        File::open(upper).map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    let mut workspace_file =
        File::open(workspace).map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    let mut upper_buffer = [0u8; 64 * 1024];
    let mut workspace_buffer = [0u8; 64 * 1024];
    loop {
        let upper_read = upper_file
            .read(&mut upper_buffer)
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        let workspace_read = workspace_file
            .read(&mut workspace_buffer)
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        if upper_read != workspace_read
            || upper_buffer[..upper_read] != workspace_buffer[..workspace_read]
        {
            return Ok(false);
        }
        if upper_read == 0 {
            return Ok(true);
        }
    }
}

fn has_extended_attributes(path: &Path) -> Result<bool, WorkspaceTransactionError> {
    let path = path_cstring(path).map_err(|_| WorkspaceTransactionError::PolicyDenied)?;
    let size = unsafe { libc::llistxattr(path.as_ptr(), ptr::null_mut(), 0) };
    if size < 0 {
        return Err(WorkspaceTransactionError::CapabilityNotSupported);
    }
    Ok(size != 0)
}

/// Apply a validated plan through capability-relative renames with reversible backups.
fn apply_workspace_operations(
    workspace: &Path,
    area: &CommitArea,
    operations: &[WorkspaceOperation],
) -> Result<AppliedWorkspaceOperations, WorkspaceTransactionError> {
    let workspace_directory = Dir::open_ambient_dir(workspace, ambient_authority())
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    let stage_directory = Dir::open_ambient_dir(&area.stage, ambient_authority())
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    let backup_directory = Dir::open_ambient_dir(&area.backup, ambient_authority())
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    let directory_times = capture_parent_directory_times(workspace, operations)?;
    let mut moved = Vec::new();
    let mut installed = Vec::new();
    let mut changed_metadata = Vec::new();
    for operation in operations {
        let relative = match operation {
            WorkspaceOperation::Delete(relative) | WorkspaceOperation::Replace(relative) => {
                relative
            }
            WorkspaceOperation::SetMetadata { .. } => continue,
        };
        match workspace_directory.symlink_metadata(relative) {
            Ok(_) => {
                if let Some(parent) = relative.parent()
                    && !parent.as_os_str().is_empty()
                    && backup_directory.create_dir_all(parent).is_err()
                {
                    rollback_partial(
                        &workspace_directory,
                        &stage_directory,
                        &backup_directory,
                        &moved,
                        &installed,
                        &changed_metadata,
                        &directory_times,
                    );
                    return Err(WorkspaceTransactionError::CapabilityNotSupported);
                }
                if workspace_directory
                    .rename(relative, &backup_directory, relative)
                    .is_err()
                {
                    rollback_partial(
                        &workspace_directory,
                        &stage_directory,
                        &backup_directory,
                        &moved,
                        &installed,
                        &changed_metadata,
                        &directory_times,
                    );
                    return Err(WorkspaceTransactionError::CapabilityNotSupported);
                }
                moved.push(relative.clone());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                rollback_partial(
                    &workspace_directory,
                    &stage_directory,
                    &backup_directory,
                    &moved,
                    &installed,
                    &changed_metadata,
                    &directory_times,
                );
                return Err(WorkspaceTransactionError::CapabilityNotSupported);
            }
        }
    }
    for operation in operations {
        match operation {
            WorkspaceOperation::Replace(relative) => {
                let Some(parent) = relative.parent() else {
                    rollback_partial(
                        &workspace_directory,
                        &stage_directory,
                        &backup_directory,
                        &moved,
                        &installed,
                        &changed_metadata,
                        &directory_times,
                    );
                    return Err(WorkspaceTransactionError::CapabilityNotSupported);
                };
                if !parent.as_os_str().is_empty()
                    && workspace_directory
                        .open_dir(parent)
                        .and_then(|directory| directory.dir_metadata())
                        .is_err()
                {
                    rollback_partial(
                        &workspace_directory,
                        &stage_directory,
                        &backup_directory,
                        &moved,
                        &installed,
                        &changed_metadata,
                        &directory_times,
                    );
                    return Err(WorkspaceTransactionError::CapabilityNotSupported);
                }
                if stage_directory
                    .rename(relative, &workspace_directory, relative)
                    .is_err()
                {
                    rollback_partial(
                        &workspace_directory,
                        &stage_directory,
                        &backup_directory,
                        &moved,
                        &installed,
                        &changed_metadata,
                        &directory_times,
                    );
                    return Err(WorkspaceTransactionError::CapabilityNotSupported);
                }
                installed.push(relative.clone());
            }
            WorkspaceOperation::SetMetadata { .. } => {}
            WorkspaceOperation::Delete(_) => {}
        }
    }
    restore_directory_times(&directory_times);
    for operation in operations {
        let WorkspaceOperation::SetMetadata {
            relative,
            mode,
            access,
            modified,
        } = operation
        else {
            continue;
        };
        let old_metadata =
            match workspace_directory
                .symlink_metadata(relative)
                .and_then(|metadata| {
                    Ok(DirectoryMetadata {
                        relative: relative.clone(),
                        mode: cap_std::fs::PermissionsExt::mode(&metadata.permissions()),
                        access: metadata.accessed()?,
                        modified: metadata.modified()?,
                    })
                }) {
                Ok(metadata) => metadata,
                Err(_) => {
                    rollback_partial(
                        &workspace_directory,
                        &stage_directory,
                        &backup_directory,
                        &moved,
                        &installed,
                        &changed_metadata,
                        &directory_times,
                    );
                    return Err(WorkspaceTransactionError::CapabilityNotSupported);
                }
            };
        changed_metadata.push(old_metadata);
        let permissions = cap_std::fs::Permissions::from_std(fs::Permissions::from_mode(*mode));
        if workspace_directory
            .set_permissions(relative, permissions)
            .and_then(|_| {
                workspace_directory.set_times(
                    relative,
                    Some(cap_fs_ext::SystemTimeSpec::Absolute(*access)),
                    Some(cap_fs_ext::SystemTimeSpec::Absolute(*modified)),
                )
            })
            .is_err()
        {
            rollback_partial(
                &workspace_directory,
                &stage_directory,
                &backup_directory,
                &moved,
                &installed,
                &changed_metadata,
                &directory_times,
            );
            return Err(WorkspaceTransactionError::CapabilityNotSupported);
        }
    }
    Ok(AppliedWorkspaceOperations {
        workspace_directory,
        stage_directory,
        backup_directory,
        moved,
        installed,
        metadata: changed_metadata,
        directory_times,
    })
}

fn rollback_partial(
    workspace_directory: &Dir,
    stage_directory: &Dir,
    backup_directory: &Dir,
    moved: &[PathBuf],
    installed: &[PathBuf],
    metadata: &[DirectoryMetadata],
    directory_times: &[DirectoryTimes],
) {
    for entry in metadata.iter().rev() {
        let permissions =
            cap_std::fs::Permissions::from_std(fs::Permissions::from_mode(entry.mode));
        let _ = workspace_directory
            .set_permissions(&entry.relative, permissions)
            .and_then(|_| {
                workspace_directory.set_times(
                    &entry.relative,
                    Some(cap_fs_ext::SystemTimeSpec::Absolute(entry.access)),
                    Some(cap_fs_ext::SystemTimeSpec::Absolute(entry.modified)),
                )
            });
    }
    for relative in installed.iter().rev() {
        if let Some(parent) = relative.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = stage_directory.create_dir_all(parent);
        }
        let _ = workspace_directory.rename(relative, stage_directory, relative);
    }
    for relative in moved.iter().rev() {
        let _ = backup_directory.rename(relative, workspace_directory, relative);
    }
    restore_directory_times(directory_times);
}

fn capture_parent_directory_times(
    workspace: &Path,
    operations: &[WorkspaceOperation],
) -> Result<Vec<DirectoryTimes>, WorkspaceTransactionError> {
    let workspace_directory = Dir::open_ambient_dir(workspace, ambient_authority())
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    let mut paths = operations
        .iter()
        .filter_map(|operation| match operation {
            WorkspaceOperation::Delete(relative) | WorkspaceOperation::Replace(relative) => {
                relative.parent().map(Path::to_path_buf)
            }
            WorkspaceOperation::SetMetadata { .. } => None,
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .map(|path| {
            let directory = if path.as_os_str().is_empty() {
                workspace_directory.try_clone()
            } else {
                workspace_directory.open_dir(&path)
            }
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
            let metadata = directory
                .dir_metadata()
                .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
            Ok(DirectoryTimes {
                directory,
                access: metadata
                    .accessed()
                    .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?,
                modified: metadata
                    .modified()
                    .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?,
            })
        })
        .collect()
}

fn restore_directory_times(times: &[DirectoryTimes]) {
    for entry in times {
        let _ = entry.directory.set_times(
            ".",
            Some(cap_fs_ext::SystemTimeSpec::Absolute(entry.access)),
            Some(cap_fs_ext::SystemTimeSpec::Absolute(entry.modified)),
        );
    }
}

fn make_tree_owner_accessible(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                make_tree_owner_accessible(&entry.path());
            }
        }
    }
}

fn observed_workspace_change(
    before: &Option<WorkspaceSnapshot>,
    workspace: &Path,
) -> (WorkspaceMutation, Option<WorkspaceChangeSummary>) {
    let Some(before) = before else {
        return (WorkspaceMutation::Unknown, None);
    };
    match snapshot_workspace(workspace).and_then(|after| before.change_summary(&after)) {
        Ok(None) => (WorkspaceMutation::Unchanged, None),
        Ok(Some(summary)) => (WorkspaceMutation::Changed, Some(summary)),
        Err(_) => (WorkspaceMutation::Unknown, None),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptKind {
    None,
    Cancelled,
    TimedOut,
}

#[derive(Debug, Default)]
struct ReaderResult {
    preview: String,
    truncated: bool,
}

#[derive(Debug, Default)]
struct CapturedOutput {
    stdout: ReaderResult,
    stderr: ReaderResult,
}

fn spawn_reader(fd: RawFd) -> thread::JoinHandle<ReaderResult> {
    thread::spawn(move || {
        let mut file = unsafe { File::from_raw_fd(fd) };
        let mut buffer = [0u8; 8192];
        let mut output = Vec::new();
        let mut truncated = false;
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let previous_len = output.len();
                    if previous_len < DEFAULT_MAX_OUTPUT_CHARS {
                        let remaining = DEFAULT_MAX_OUTPUT_CHARS - previous_len;
                        output.extend_from_slice(&buffer[..read.min(remaining)]);
                    }
                    if previous_len.saturating_add(read) > DEFAULT_MAX_OUTPUT_CHARS {
                        truncated = true;
                    }
                }
                Err(_) => {
                    truncated = true;
                    break;
                }
            }
        }
        ReaderResult {
            preview: String::from_utf8_lossy(&output).into_owned(),
            truncated,
        }
    })
}

fn interrupted_result(
    command_id: &str,
    status: i32,
    duration_ms: u64,
    execution_status: CommandExecutionStatus,
    semantic_status: CommandSemanticStatus,
    fallback: &str,
    captured: &CapturedOutput,
) -> CommandResult {
    let mut result = CommandResult::executed(
        command_id,
        process_exit_code(status),
        duration_ms,
        &captured.stdout.preview,
        &captured.stderr.preview,
        captured.stdout.truncated || captured.stderr.truncated,
    );
    result.execution_status = execution_status.clone();
    result.semantic_status = semantic_status;
    result.exit_code = None;
    result.timed_out = execution_status == CommandExecutionStatus::TimedOut;
    if result.stderr_preview.is_empty() {
        result.stderr_preview = fallback.to_string();
    }
    result
}

fn wait_for_child(
    pid: libc::pid_t,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> (i32, InterruptKind) {
    let started = Instant::now();
    let mut status = 0;
    loop {
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if waited == pid {
            return (status, InterruptKind::None);
        }
        if waited < 0 {
            if unsafe { *libc::__errno_location() } == libc::EINTR {
                continue;
            }
            return (status, InterruptKind::None);
        }
        if cancellation.is_cancelled() {
            kill_process_group(pid);
            wait_for_exit(pid);
            return (status, InterruptKind::Cancelled);
        }
        if started.elapsed() >= timeout {
            kill_process_group(pid);
            wait_for_exit(pid);
            return (status, InterruptKind::TimedOut);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_exit(pid: libc::pid_t) {
    let mut status = 0;
    loop {
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        if waited == pid || waited < 0 && unsafe { *libc::__errno_location() } != libc::EINTR {
            return;
        }
    }
}

fn kill_process_group(pid: libc::pid_t) {
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
        libc::kill(pid, libc::SIGKILL);
    }
}

fn process_exit_code(status: i32) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        255
    }
}

fn pipe_cloexec() -> Option<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    let result = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    (result == 0).then_some((fds[0], fds[1]))
}

fn close_fd(fd: RawFd) {
    if fd >= 0 {
        unsafe {
            libc::close(fd);
        }
    }
}

fn read_child_setup_status(fd: RawFd) -> Option<u8> {
    let mut value = [0u8; 1];
    let read = unsafe { libc::read(fd, value.as_mut_ptr().cast(), value.len()) };
    close_fd(fd);
    (read == 1).then_some(value[0])
}

fn write_user_namespace_maps(pid: libc::pid_t) -> Result<(), ()> {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let setgroups = PathBuf::from(format!("/proc/{pid}/setgroups"));
    if setgroups.exists() {
        fs::write(setgroups, "deny").map_err(|_| ())?;
    }
    fs::write(format!("/proc/{pid}/uid_map"), format!("0 {uid} 1\n")).map_err(|_| ())?;
    fs::write(format!("/proc/{pid}/gid_map"), format!("0 {gid} 1\n")).map_err(|_| ())
}

extern "C" fn child_main(argument: *mut libc::c_void) -> libc::c_int {
    let result = std::panic::catch_unwind(|| unsafe { child_main_inner(&mut *(argument.cast())) });
    match result {
        Ok(Ok(())) => 126,
        Ok(Err(kind)) => {
            let context = unsafe { &mut *(argument.cast::<ChildContext>()) };
            child_fail(context.status_write, kind);
            126
        }
        Err(_) => {
            let context = unsafe { &mut *(argument.cast::<ChildContext>()) };
            child_fail(context.status_write, CHILD_SETUP_UNAVAILABLE);
            126
        }
    }
}

unsafe fn child_main_inner(context: &mut ChildContext) -> Result<(), u8> {
    for fd in context.parent_fds.iter().copied() {
        close_fd(fd);
    }
    if !wait_for_namespace_map(context.ready_read) {
        return Err(CHILD_SETUP_CAPABILITY);
    }
    close_fd(context.ready_read);
    if unsafe { libc::dup2(context.stdout_write, libc::STDOUT_FILENO) } < 0
        || unsafe { libc::dup2(context.stderr_write, libc::STDERR_FILENO) } < 0
    {
        return Err(CHILD_SETUP_UNAVAILABLE);
    }
    if context.stdout_write != libc::STDOUT_FILENO {
        close_fd(context.stdout_write);
    }
    if context.stderr_write != libc::STDERR_FILENO {
        close_fd(context.stderr_write);
    }
    if unsafe { libc::setpgid(0, 0) } != 0 {
        return Err(CHILD_SETUP_UNAVAILABLE);
    }
    if unsafe { libc::setresgid(0, 0, 0) } != 0 || unsafe { libc::setresuid(0, 0, 0) } != 0 {
        return Err(CHILD_SETUP_CAPABILITY);
    }
    mount_private_root().map_err(|_| CHILD_SETUP_CAPABILITY)?;
    mount_proc().map_err(|_| CHILD_SETUP_CAPABILITY)?;
    mount_private_tmp_and_dev().map_err(|_| CHILD_SETUP_CAPABILITY)?;
    if context.filesystem == SandboxFilesystemMode::WorkspaceWrite {
        mount_workspace_overlay(context).map_err(|_| CHILD_SETUP_OVERLAY_FILESYSTEM)?;
    }
    mount_protected_paths(&context.protected_paths).map_err(|_| CHILD_SETUP_CAPABILITY)?;
    if context.filesystem == SandboxFilesystemMode::ReadOnly {
        mount_readonly_workspace(&context.workspace).map_err(|_| CHILD_SETUP_CAPABILITY)?;
    }
    fs::create_dir_all(SANDBOX_HOME).map_err(|_| CHILD_SETUP_UNAVAILABLE)?;
    if unsafe {
        libc::chdir(
            path_cstring(&context.cwd)
                .map_err(|_| CHILD_SETUP_UNAVAILABLE)?
                .as_ptr(),
        )
    } != 0
    {
        return Err(CHILD_SETUP_UNAVAILABLE);
    }
    drop_linux_capabilities().map_err(|_| CHILD_SETUP_CAPABILITY)?;
    install_landlock(context).map_err(|_| CHILD_SETUP_CAPABILITY)?;
    install_seccomp_filter(context.network == SandboxNetworkMode::Denied)
        .map_err(|_| CHILD_SETUP_CAPABILITY)?;
    close_all_extra_fds();
    let mut argv = context
        .argv
        .iter()
        .map(|value| value.as_ptr())
        .collect::<Vec<_>>();
    argv.push(ptr::null());
    let mut env = context
        .env
        .iter()
        .map(|value| value.as_ptr())
        .collect::<Vec<_>>();
    env.push(ptr::null());
    let executable = path_cstring(&context.executable).map_err(|_| CHILD_SETUP_UNAVAILABLE)?;
    unsafe {
        libc::execve(executable.as_ptr(), argv.as_ptr(), env.as_ptr());
    }
    Err(CHILD_SETUP_UNAVAILABLE)
}

fn mount_workspace_overlay(context: &ChildContext) -> Result<(), ()> {
    let upper = context.overlay_upper.as_deref().ok_or(())?;
    let work = context.overlay_work.as_deref().ok_or(())?;
    let options = format!(
        "lowerdir={},upperdir={},workdir={}",
        overlay_option_path(&context.workspace)?,
        overlay_option_path(upper)?,
        overlay_option_path(work)?,
    );
    mount(
        Some("overlay"),
        &context.workspace,
        Some("overlay"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some(&options),
    )
}

fn overlay_option_path(path: &Path) -> Result<String, ()> {
    let value = path.to_str().ok_or(())?;
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | ',' | ':') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    Ok(escaped)
}

fn wait_for_namespace_map(fd: RawFd) -> bool {
    let mut ready = [0u8; 1];
    unsafe { libc::read(fd, ready.as_mut_ptr().cast(), ready.len()) == 1 && ready[0] == 1 }
}

fn child_fail(fd: RawFd, kind: u8) {
    let value = [kind];
    unsafe {
        libc::write(fd, value.as_ptr().cast(), value.len());
    }
    close_fd(fd);
}

fn path_cstring(path: &Path) -> Result<CString, ()> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| ())
}

fn mount_private_root() -> Result<(), ()> {
    mount(
        None,
        Path::new("/"),
        None,
        libc::MS_REC | libc::MS_PRIVATE,
        None,
    )
}

fn mount_proc() -> Result<(), ()> {
    mount(
        Some("proc"),
        Path::new("/proc"),
        Some("proc"),
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    )
}

fn mount_private_tmp_and_dev() -> Result<(), ()> {
    mount(
        Some("tmpfs"),
        Path::new("/run"),
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        Some("size=64m,mode=1777"),
    )?;
    mount(
        Some("tmpfs"),
        Path::new("/dev"),
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("size=4m,mode=755"),
    )?;
    for name in ["null", "zero", "random", "urandom"] {
        let path = Path::new("/dev").join(name);
        OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .map_err(|_| ())?;
    }
    Ok(())
}

fn mount_protected_paths(paths: &[ProtectedPath]) -> Result<(), ()> {
    for (index, protected) in paths.iter().enumerate() {
        let placeholder = if protected.is_dir {
            let path = Path::new("/run").join(format!(".protected-dir-{index}"));
            fs::create_dir(&path).map_err(|_| ())?;
            path
        } else {
            let path = Path::new("/run").join(format!(".protected-file-{index}"));
            OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&path)
                .map_err(|_| ())?;
            path
        };
        fs::set_permissions(&placeholder, fs::Permissions::from_mode(0o000)).map_err(|_| ())?;
        mount(
            Some(placeholder.to_string_lossy().as_ref()),
            &protected.path,
            None,
            libc::MS_BIND | if protected.is_dir { libc::MS_REC } else { 0 },
            None,
        )?;
        mount(
            None,
            &protected.path,
            None,
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
            None,
        )?;
    }
    Ok(())
}

fn mount_readonly_workspace(workspace: &Path) -> Result<(), ()> {
    mount(
        Some(workspace.to_string_lossy().as_ref()),
        workspace,
        None,
        libc::MS_BIND | libc::MS_REC,
        None,
    )?;
    mount(
        None,
        workspace,
        None,
        libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_REC,
        None,
    )
}

fn mount(
    source: Option<&str>,
    target: &Path,
    filesystem: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<(), ()> {
    let source = source.map(CString::new).transpose().map_err(|_| ())?;
    let target = path_cstring(target)?;
    let filesystem = filesystem.map(CString::new).transpose().map_err(|_| ())?;
    let data = data.map(CString::new).transpose().map_err(|_| ())?;
    let result = unsafe {
        libc::mount(
            source.as_ref().map_or(ptr::null(), |value| value.as_ptr()),
            target.as_ptr(),
            filesystem
                .as_ref()
                .map_or(ptr::null(), |value| value.as_ptr()),
            flags,
            data.as_ref()
                .map_or(ptr::null_mut(), |value| value.as_ptr().cast_mut().cast()),
        )
    };
    (result == 0).then_some(()).ok_or(())
}

fn drop_linux_capabilities() -> Result<(), ()> {
    let mut header = CapabilityHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [
        CapabilityData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
        CapabilityData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
    ];
    let result = unsafe { libc::syscall(libc::SYS_capset, &mut header, data.as_mut_ptr()) };
    (result == 0).then_some(()).ok_or(())
}

fn install_landlock(context: &ChildContext) -> Result<(), ()> {
    let attributes = LandlockRulesetAttr {
        handled_access_fs: LANDLOCK_ACCESS_FS_ALL,
    };
    let ruleset = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            &attributes,
            std::mem::size_of::<LandlockRulesetAttr>(),
            0usize,
            0usize,
        )
    };
    if ruleset < 0 {
        return Err(());
    }
    let ruleset = ruleset as RawFd;
    let workspace_access = match context.filesystem {
        SandboxFilesystemMode::ReadOnly => LANDLOCK_ACCESS_FS_READ | LANDLOCK_ACCESS_FS_EXECUTE,
        SandboxFilesystemMode::WorkspaceWrite => {
            LANDLOCK_ACCESS_FS_READ | LANDLOCK_ACCESS_FS_EXECUTE | LANDLOCK_ACCESS_FS_WRITE
        }
    };
    let runtime_read = LANDLOCK_ACCESS_FS_READ | LANDLOCK_ACCESS_FS_EXECUTE;
    add_landlock_rule(ruleset, &context.workspace, workspace_access)?;
    for root in [
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/usr/local/bin",
        "/lib",
        "/lib64",
        "/usr/lib",
        "/usr/lib64",
        "/usr/share/nodejs",
        "/proc",
    ] {
        let root = Path::new(root);
        if root.is_dir() {
            add_landlock_rule(ruleset, root, runtime_read)?;
        }
    }
    for path in &context.runtime_read_paths {
        let access = runtime_read_access(path).ok_or(())?;
        add_landlock_rule(ruleset, path, access)?;
    }
    if Path::new("/run").is_dir() {
        add_landlock_rule(
            ruleset,
            Path::new("/run"),
            LANDLOCK_ACCESS_FS_READ | LANDLOCK_ACCESS_FS_WRITE,
        )?;
    }
    if Path::new("/dev").is_dir() {
        add_landlock_rule(
            ruleset,
            Path::new("/dev"),
            LANDLOCK_ACCESS_FS_READ | LANDLOCK_ACCESS_FS_WRITE,
        )?;
    }
    unsafe {
        libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    }
    let restricted = unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset, 0usize) };
    close_fd(ruleset);
    (restricted == 0).then_some(()).ok_or(())
}

fn add_landlock_rule(ruleset: RawFd, path: &Path, allowed_access: u64) -> Result<(), ()> {
    let path = path_cstring(path)?;
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(());
    }
    let rule = LandlockPathBeneathAttr {
        allowed_access,
        parent_fd: fd,
    };
    let result = unsafe {
        libc::syscall(
            SYS_LANDLOCK_ADD_RULE,
            ruleset,
            LANDLOCK_RULE_TYPE_PATH_BENEATH,
            &rule,
            0usize,
        )
    };
    close_fd(fd);
    (result == 0).then_some(()).ok_or(())
}

fn install_seccomp_filter(network_denied: bool) -> Result<(), ()> {
    if unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(());
    }
    let mut filter = vec![
        SockFilter {
            code: BPF_LD_W_ABS,
            jt: 0,
            jf: 0,
            k: SECCOMP_DATA_ARCH_OFFSET,
        },
        SockFilter {
            code: BPF_JMP_JEQ_K,
            jt: 1,
            jf: 0,
            k: audit_arch(),
        },
        SockFilter {
            code: BPF_RET_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_KILL_PROCESS,
        },
        SockFilter {
            code: BPF_LD_W_ABS,
            jt: 0,
            jf: 0,
            k: SECCOMP_DATA_NR_OFFSET,
        },
    ];
    let mut blocked = vec![
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_sethostname,
        libc::SYS_setdomainname,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_reboot,
        libc::SYS_setsid,
        libc::SYS_setuid,
        libc::SYS_setgid,
        libc::SYS_setresuid,
        libc::SYS_setresgid,
        libc::SYS_capset,
        libc::SYS_prctl,
    ];
    if network_denied {
        // `socketpair` and message/data transfer syscalls remain available for local process IPC.
        // A fresh network namespace, closed inherited FDs, and the socket/connect/bind family
        // below prevent those IPC primitives from becoming external network access.
        blocked.extend([
            libc::SYS_socket,
            libc::SYS_connect,
            libc::SYS_bind,
            libc::SYS_listen,
            libc::SYS_accept,
            libc::SYS_accept4,
            libc::SYS_getsockname,
            libc::SYS_getpeername,
            libc::SYS_getsockopt,
            libc::SYS_setsockopt,
            libc::SYS_shutdown,
        ]);
    }
    blocked.sort_unstable();
    blocked.dedup();
    for syscall in blocked {
        filter.push(SockFilter {
            code: BPF_JMP_JEQ_K,
            jt: 0,
            jf: 1,
            k: syscall as u32,
        });
        filter.push(SockFilter {
            code: BPF_RET_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ERRNO | libc::EPERM as u32,
        });
    }
    filter.push(SockFilter {
        code: BPF_RET_K,
        jt: 0,
        jf: 0,
        k: SECCOMP_RET_ALLOW,
    });
    let program = SockFprog {
        len: filter.len().try_into().map_err(|_| ())?,
        filter: filter.as_ptr(),
    };
    let result = unsafe {
        libc::prctl(
            PR_SET_SECCOMP,
            SECCOMP_MODE_FILTER,
            &program as *const SockFprog,
            0,
            0,
        )
    };
    (result == 0).then_some(()).ok_or(())
}

const fn audit_arch() -> u32 {
    if cfg!(target_arch = "x86_64") {
        AUDIT_ARCH_X86_64
    } else if cfg!(target_arch = "aarch64") {
        AUDIT_ARCH_AARCH64
    } else if cfg!(target_arch = "arm") {
        AUDIT_ARCH_ARM
    } else {
        0
    }
}

fn close_all_extra_fds() {
    let closed = unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) };
    if closed == 0 {
        return;
    }
    let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    let limit = if limit > 0 { limit as RawFd } else { 65_536 };
    for fd in 3..limit {
        close_fd(fd);
    }
}
