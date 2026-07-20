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
use std::io::Read;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use singularity_core::{CancellationToken, is_protected_path};

use super::{
    CommandEnvironmentPolicy, CommandExecutionStatus, CommandRequest, CommandResult,
    CommandScriptRequest, CommandSemanticStatus, DEFAULT_MAX_OUTPUT_CHARS, SandboxBackend,
    SandboxBackendEnforcement, SandboxCapabilities, SandboxFilesystemMode, SandboxNetworkMode,
    WorkspaceMutation,
};

const BACKEND_NAME: &str = "linux";
const SANDBOX_UNAVAILABLE: &str = "linux sandbox unavailable";
const SANDBOX_POLICY_DENIED: &str = "linux sandbox policy denied";
const SANDBOX_PROTECTED_PATH_DENIED: &str = "linux sandbox protected path denied";
const SANDBOX_CWD_DENIED: &str = "linux sandbox cwd is outside workspace";
const SANDBOX_EXECUTABLE_UNAVAILABLE: &str = "linux sandbox executable unavailable";
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
    MountNamespace,
    NetworkNamespace,
    NoNewPrivs,
    Seccomp,
    Landlock,
    ProcessTreeCleanup,
}

impl LinuxCapability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserNamespace => "user_namespace",
            Self::MountNamespace => "mount_namespace",
            Self::NetworkNamespace => "network_namespace",
            Self::NoNewPrivs => "no_new_privs",
            Self::Seccomp => "seccomp",
            Self::Landlock => "landlock",
            Self::ProcessTreeCleanup => "process_tree_cleanup",
        }
    }
}

/// Read-only capability facts collected without exposing kernel handles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxSandboxProbe {
    pub user_namespace: bool,
    pub mount_namespace: bool,
    pub network_namespace: bool,
    pub no_new_privs: bool,
    pub seccomp: bool,
    pub landlock_abi: Option<u32>,
    pub process_tree_cleanup: bool,
    pub cgroup_v2: bool,
    pub cgroup_delegated: bool,
}

impl LinuxSandboxProbe {
    pub fn strict_ready(&self) -> bool {
        self.user_namespace
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
    CapabilityNotSupported(LinuxCapability),
    PolicyDenied(&'static str),
    ExecutableUnavailable,
}

impl LinuxSandboxError {
    fn into_result(self, command_id: &str) -> CommandResult {
        match self {
            Self::Unavailable => CommandResult::backend_error(command_id, SANDBOX_UNAVAILABLE),
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
            copy_on_write: false,
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

#[derive(Debug, Clone)]
struct PreparedCommand {
    workspace: PathBuf,
    cwd: PathBuf,
    executable: PathBuf,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    timeout: Duration,
    network: SandboxNetworkMode,
    filesystem: SandboxFilesystemMode,
    protected_paths: Vec<ProtectedPath>,
    before: Option<WorkspaceSnapshot>,
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
        let env = sanitized_environment(&environment);
        let executable = resolve_executable(&argv[0], &cwd_path, &env)?;
        if is_protected_path(&executable.to_string_lossy()) {
            return Err(LinuxSandboxError::PolicyDenied(
                SANDBOX_PROTECTED_PATH_DENIED,
            ));
        }
        argv[0] = executable.to_string_lossy().into_owned();
        let protected_paths = collect_protected_paths(&workspace)
            .map_err(|_| LinuxSandboxError::PolicyDenied(SANDBOX_PROTECTED_PATH_DENIED))?;
        let before = snapshot_workspace(&workspace);
        Ok(Self {
            workspace,
            cwd: cwd_path,
            executable,
            argv,
            env,
            timeout: Duration::from_secs(timeout_seconds),
            network,
            filesystem: filesystem.mode,
            protected_paths,
            before,
        })
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

fn resolve_executable(
    requested: &str,
    cwd: &Path,
    env: &[(String, String)],
) -> Result<PathBuf, LinuxSandboxError> {
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
    let canonical =
        fs::canonicalize(candidate).map_err(|_| LinuxSandboxError::ExecutableUnavailable)?;
    let metadata =
        fs::metadata(&canonical).map_err(|_| LinuxSandboxError::ExecutableUnavailable)?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(LinuxSandboxError::ExecutableUnavailable);
    }
    Ok(canonical)
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint {
    file_type: u32,
    size: u64,
    mode: u32,
    device: u64,
    inode: u64,
    modified_seconds: i64,
    modified_nanos: i64,
    link_target: Option<PathBuf>,
}

type WorkspaceSnapshot = BTreeMap<PathBuf, FileFingerprint>;

fn snapshot_workspace(workspace: &Path) -> Option<WorkspaceSnapshot> {
    fn visit(root: &Path, current: &Path, snapshot: &mut WorkspaceSnapshot) -> Result<(), ()> {
        let mut entries = fs::read_dir(current)
            .map_err(|_| ())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ())?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let relative = path.strip_prefix(root).map_err(|_| ())?.to_path_buf();
            let component = entry.file_name();
            if is_protected_path(&component.to_string_lossy()) {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).map_err(|_| ())?;
            let modified = metadata.modified().map_err(|_| ())?;
            let since_epoch = modified.duration_since(UNIX_EPOCH).map_err(|_| ())?;
            let link_target = metadata
                .file_type()
                .is_symlink()
                .then(|| fs::read_link(&path))
                .transpose()
                .ok()
                .flatten();
            snapshot.insert(
                relative,
                FileFingerprint {
                    file_type: metadata.mode() & libc::S_IFMT,
                    size: metadata.size(),
                    mode: metadata.mode(),
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    modified_seconds: since_epoch.as_secs() as i64,
                    modified_nanos: since_epoch.subsec_nanos() as i64,
                    link_target,
                },
            );
            if metadata.is_dir() {
                visit(root, &path, snapshot)?;
            }
        }
        Ok(())
    }

    let mut snapshot = BTreeMap::new();
    visit(workspace, workspace, &mut snapshot)
        .ok()
        .map(|_| snapshot)
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
}

fn run_prepared_command(
    command_id: &str,
    prepared: PreparedCommand,
    cancellation: &CancellationToken,
) -> CommandResult {
    let started = Instant::now();
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
    let mutation = match (&prepared.before, snapshot_workspace(&prepared.workspace)) {
        (Some(before), Some(after)) if before == &after => WorkspaceMutation::Unchanged,
        (Some(_), Some(_)) => WorkspaceMutation::Changed,
        _ => WorkspaceMutation::Unknown,
    };
    if let Some(kind) = child_setup {
        let error = if kind == CHILD_SETUP_CAPABILITY {
            LinuxSandboxError::CapabilityNotSupported(LinuxCapability::Landlock)
        } else {
            LinuxSandboxError::Unavailable
        };
        return error
            .into_result(command_id)
            .with_workspace_mutation(mutation);
    }
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
    result
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
        "/proc",
    ] {
        let root = Path::new(root);
        if root.is_dir() {
            add_landlock_rule(ruleset, root, runtime_read)?;
        }
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
        blocked.extend([
            libc::SYS_socket,
            libc::SYS_socketpair,
            libc::SYS_connect,
            libc::SYS_bind,
            libc::SYS_listen,
            libc::SYS_accept,
            libc::SYS_accept4,
            libc::SYS_sendto,
            libc::SYS_sendmsg,
            libc::SYS_recvfrom,
            libc::SYS_recvmsg,
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
