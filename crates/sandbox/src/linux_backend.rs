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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _};
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use sha2::{Digest, Sha256};
use singularity_core::{CancellationToken, is_protected_path};

use super::{
    CommandEnvironmentPolicy, CommandExecutionStatus, CommandRequest, CommandResult,
    CommandScriptRequest, CommandSemanticStatus, DEFAULT_MAX_OUTPUT_CHARS, ExecutableAvailability,
    SandboxBackend, SandboxBackendEnforcement, SandboxCapabilities, SandboxFilesystemMode,
    SandboxNetworkMode, SandboxPreflightFact, SandboxPreflightReport, WorkspaceChangeSummary,
    WorkspaceMutation, WorkspaceSnapshot, snapshot_trusted_workspace, snapshot_workspace,
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

/// Read-only system roots needed by dynamically dispatched executables.
///
/// `/usr/libexec` is intentionally an execute/read-only root rather than a
/// tool-specific exception: compiler drivers commonly dispatch helpers from
/// this standard system location (for example, `collect2`).
const STANDARD_RUNTIME_ROOTS: [&str; 12] = [
    "/bin",
    "/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/usr/local/bin",
    "/lib",
    "/lib64",
    "/usr/lib",
    "/usr/lib64",
    "/usr/libexec",
    "/usr/share/nodejs",
    "/proc",
];

/// Host name resolution and TLS trust inputs required by network-enabled processes.
const NETWORK_RUNTIME_READ_PATHS: [&str; 7] = [
    "/etc/resolv.conf",
    "/etc/nsswitch.conf",
    "/etc/hosts",
    "/etc/host.conf",
    "/etc/gai.conf",
    "/etc/ssl/openssl.cnf",
    "/etc/ssl/certs",
];

// Serialize only clone through the child's inherited-FD cleanup ack; command execution stays parallel.
static CHILD_LAUNCH_GATE: Mutex<()> = Mutex::new(());

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
const WORKSPACE_TRANSACTION_DRIFT: &str = "linux sandbox workspace transaction drift";
const WORKSPACE_TRANSACTION_ROLLBACK_FAILED: &str =
    "linux sandbox workspace transaction rollback failed";
const WORKSPACE_TRANSACTION_CLEANUP_FAILED: &str =
    "linux sandbox workspace transaction cleanup failed after verification";

static TRANSACTION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransactionTestPoint {
    WorkspaceMove,
    MetadataMutation,
    InstalledMetadataMutation,
    RollbackInstalledMove,
    FinalVerification,
}

#[cfg(test)]
#[derive(Debug)]
struct TransactionTestDirective {
    points: Vec<TransactionTestPoint>,
    reached: bool,
    released: bool,
    fail_rollback: bool,
}

#[cfg(test)]
static TRANSACTION_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn transaction_test_control() -> &'static (
    std::sync::Mutex<Option<TransactionTestDirective>>,
    std::sync::Condvar,
) {
    static CONTROL: OnceLock<(
        std::sync::Mutex<Option<TransactionTestDirective>>,
        std::sync::Condvar,
    )> = OnceLock::new();
    CONTROL.get_or_init(|| (std::sync::Mutex::new(None), std::sync::Condvar::new()))
}

#[cfg(test)]
fn transaction_test_pause(point: TransactionTestPoint) {
    let (control, condition) = transaction_test_control();
    let mut directive = control.lock().expect("transaction test control");
    if directive
        .as_ref()
        .is_none_or(|directive| directive.points.first().copied() != Some(point))
    {
        return;
    }
    directive.as_mut().expect("directive").reached = true;
    condition.notify_all();
    while directive
        .as_ref()
        .is_some_and(|directive| !directive.released)
    {
        directive = condition.wait(directive).expect("transaction test wait");
    }
    let directive = directive.as_mut().expect("transaction test directive");
    directive.points.remove(0);
    directive.reached = false;
    directive.released = false;
    condition.notify_all();
}

#[cfg(not(test))]
fn transaction_test_pause(_: TransactionTestPoint) {}

#[cfg(test)]
fn transaction_test_should_fail_rollback() -> bool {
    transaction_test_control()
        .0
        .lock()
        .expect("transaction test control")
        .as_ref()
        .is_some_and(|directive| directive.fail_rollback)
}

#[cfg(not(test))]
fn transaction_test_should_fail_rollback() -> bool {
    false
}

#[cfg(test)]
fn arm_transaction_test(point: TransactionTestPoint, fail_rollback: bool) {
    arm_transaction_test_sequence(&[point], fail_rollback);
}

#[cfg(test)]
fn arm_transaction_test_sequence(points: &[TransactionTestPoint], fail_rollback: bool) {
    assert!(
        !points.is_empty(),
        "transaction test sequence must not be empty"
    );
    let (control, _) = transaction_test_control();
    *control.lock().expect("transaction test control") = Some(TransactionTestDirective {
        points: points.to_vec(),
        reached: false,
        released: false,
        fail_rollback,
    });
}

#[cfg(test)]
fn wait_for_transaction_test_point() {
    let (control, condition) = transaction_test_control();
    let directive = control.lock().expect("transaction test control");
    let (mut directive, timeout) = condition
        .wait_timeout_while(directive, Duration::from_secs(10), |directive| {
            directive
                .as_ref()
                .is_some_and(|directive| !directive.reached)
        })
        .expect("transaction test point wait");
    if timeout.timed_out() {
        if let Some(directive) = directive.as_mut() {
            directive.released = true;
        }
        condition.notify_all();
        panic!("transaction test point was not reached");
    }
    assert!(
        directive
            .as_ref()
            .is_some_and(|directive| directive.reached)
    );
}

#[cfg(test)]
fn release_transaction_test_point() {
    let (control, condition) = transaction_test_control();
    let mut directive = control.lock().expect("transaction test control");
    directive
        .as_mut()
        .expect("transaction test directive")
        .released = true;
    condition.notify_all();
}

#[cfg(test)]
fn clear_transaction_test() {
    *transaction_test_control()
        .0
        .lock()
        .expect("transaction test control") = None;
}

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
            seccomp: probe_seccomp(),
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
    let filter = prepare_seccomp_filter(false);
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let success = probe_no_new_privs() && install_seccomp_filter(&filter).is_ok();
        unsafe { libc::_exit(i32::from(!success)) };
    }
    if pid < 0 {
        return false;
    }
    let mut status = 0;
    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
    waited == pid && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
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
            let command_id = input.command_id.clone();
            return match PreparedCommand::from_input(input) {
                Ok(prepared) => run_prepared_command(&command_id, prepared, cancellation),
                Err(error) => error.into_result(&command_id),
            };
        };
        LinuxSandboxError::CapabilityNotSupported(capability).into_result(&input.command_id)
    }
}

struct LinuxExecutionInput {
    command_id: String,
    argv: Vec<String>,
    runtime_executables: Vec<String>,
    cwd: String,
    timeout_seconds: u64,
    network: SandboxNetworkMode,
    filesystem: super::SandboxFilesystemPolicy,
    environment: CommandEnvironmentPolicy,
    trusted_workspace_preparation: bool,
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

    fn preflight(
        &self,
        workspace: &Path,
        cancellation: &CancellationToken,
    ) -> SandboxPreflightReport {
        let mut report = super::baseline_sandbox_preflight(self);
        report.os = "linux".to_string();
        report.kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|value| value.trim().chars().take(128).collect());
        report.filesystem = linux_filesystem_fact(workspace);
        let probe = &self.probe;
        report.user_namespace = fact(probe.user_namespace);
        report.mount_namespace = fact(probe.mount_namespace);
        report.pid_namespace = fact(probe.pid_namespace);
        report.network_namespace = fact(probe.network_namespace);
        report.no_new_privs = fact(probe.no_new_privs);
        report.seccomp = fact(probe.seccomp);
        report.landlock = fact(probe.landlock_abi.is_some_and(|abi| abi >= 3));
        if !probe.strict_ready() {
            report.outcome = super::SandboxPreflightOutcome::Unsupported;
            report.error_code = Some("sandbox_preflight_linux_capability_missing".to_string());
            for (available, name) in [
                (probe.user_namespace, "user_namespace"),
                (probe.mount_namespace, "mount_namespace"),
                (probe.pid_namespace, "pid_namespace"),
                (probe.network_namespace, "network_namespace"),
                (probe.no_new_privs, "no_new_privs"),
                (probe.seccomp, "seccomp"),
                (probe.landlock_abi.is_some_and(|abi| abi >= 3), "landlock"),
                (probe.process_tree_cleanup, "process_tree_cleanup"),
            ] {
                if !available && !report.missing_capabilities.iter().any(|item| item == name) {
                    report.missing_capabilities.push(name.to_string());
                }
            }
            return report;
        }
        if cancellation.is_cancelled() {
            report.unsupported("sandbox_preflight_cancelled", &["cancellation"]);
            return report;
        }
        const PROBE_FILE: &str = "singularity-preflight.txt";
        let result = super::preflight_command(
            self,
            workspace,
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("printf preflight > {PROBE_FILE}"),
            ],
            SandboxNetworkMode::Denied,
            cancellation,
            "write",
        );
        if super::preflight_write_verified(&result, PROBE_FILE) {
            // A strict write probe traverses the same transactional execution path as
            // evaluation commands, including the overlay mount setup; static
            // `copy_on_write` capability alone is not treated as execution evidence.
            report.overlayfs = SandboxPreflightFact::Passed;
            report.transactional_workspace = SandboxPreflightFact::Passed;
            let host_network_namespace = fs::read_link("/proc/self/ns/net")
                .ok()
                .map(|path| path.to_string_lossy().into_owned());
            let network_result = super::preflight_command(
                self,
                workspace,
                vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "readlink /proc/self/ns/net".to_string(),
                ],
                SandboxNetworkMode::Denied,
                cancellation,
                "network_namespace",
            );
            let network_denied = super::preflight_unchanged_verified(&network_result)
                && host_network_namespace.is_some_and(|host| {
                    !network_result.stdout_preview.trim().is_empty()
                        && network_result.stdout_preview.trim() != host
                });
            let protected_denied = preflight_protected_write_denied(self, workspace, cancellation);
            report.network_denied = fact(network_denied);
            report.protected_paths = fact(protected_denied);
            if network_denied && protected_denied {
                report.outcome = super::SandboxPreflightOutcome::Supported;
                report.error_code = None;
            } else {
                let mut missing = Vec::new();
                if !network_denied {
                    missing.push("network_denied");
                }
                if !protected_denied {
                    missing.push("protected_metadata_admission");
                }
                report.unsupported("sandbox_preflight_policy_probe_failed", &missing);
            }
        } else {
            report.overlayfs = SandboxPreflightFact::Failed;
            report.transactional_workspace = SandboxPreflightFact::Failed;
            report.network_denied = SandboxPreflightFact::Failed;
            report.protected_paths = SandboxPreflightFact::Failed;
            report.unsupported(
                "sandbox_preflight_write_unverified",
                &[
                    "overlay_filesystem",
                    "transactional_workspace",
                    "network_denied",
                    "protected_metadata_admission",
                ],
            );
        }
        report
    }

    fn probe_executable(
        &self,
        workspace: &Path,
        executable: &str,
        environment: &CommandEnvironmentPolicy,
    ) -> ExecutableAvailability {
        let Ok(cwd) = canonical_directory(workspace) else {
            return ExecutableAvailability::Unknown;
        };
        let env = sanitized_environment(environment);
        match resolve_executable(executable, &cwd, &env) {
            Ok(_) => ExecutableAvailability::Available,
            Err(LinuxSandboxError::ExecutableUnavailable) => ExecutableAvailability::Unavailable,
            Err(_) => ExecutableAvailability::Unknown,
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
                runtime_executables: Vec::new(),
                cwd: request.cwd.clone(),
                timeout_seconds: request.timeout_seconds,
                network: request.network.mode.clone(),
                filesystem: request.filesystem.clone(),
                environment: request.environment.clone(),
                trusted_workspace_preparation: request.is_trusted_workspace_preparation(),
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
                runtime_executables: request.runtime_executables.clone(),
                cwd: request.cwd.clone(),
                timeout_seconds: request.timeout_seconds,
                network: request.network.mode.clone(),
                filesystem: request.filesystem.clone(),
                environment: request.environment.clone(),
                trusted_workspace_preparation: false,
            },
            cancellation,
        )
    }
}

fn fact(available: bool) -> SandboxPreflightFact {
    if available {
        SandboxPreflightFact::Passed
    } else {
        SandboxPreflightFact::Failed
    }
}

fn preflight_protected_write_denied(
    backend: &LinuxSandboxBackend,
    workspace: &Path,
    cancellation: &CancellationToken,
) -> bool {
    const SENTINEL: &str = "protected-sentinel";
    let protected_dir = workspace.join(".git");
    let protected_file = protected_dir.join("singularity-preflight-protected.txt");
    if fs::create_dir(&protected_dir).is_err() || fs::write(&protected_file, SENTINEL).is_err() {
        let _ = fs::remove_dir_all(&protected_dir);
        return false;
    }
    let read_result = super::preflight_command(
        backend,
        workspace,
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "cat .git/singularity-preflight-protected.txt".to_string(),
        ],
        SandboxNetworkMode::Denied,
        cancellation,
        "protected_read",
    );
    let write_result = super::preflight_command(
        backend,
        workspace,
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf tampered > .git/singularity-preflight-protected.txt".to_string(),
        ],
        SandboxNetworkMode::Denied,
        cancellation,
        "protected_write",
    );
    let preserved = fs::read_to_string(&protected_file).ok().as_deref() == Some(SENTINEL);
    let cleanup_succeeded = fs::remove_dir_all(&protected_dir).is_ok();
    preserved
        && cleanup_succeeded
        && protected_probe_denied(&read_result)
        && !read_result.stdout_preview.contains(SENTINEL)
        && protected_probe_denied(&write_result)
}

fn protected_probe_denied(result: &CommandResult) -> bool {
    let classified = match result.execution_status {
        CommandExecutionStatus::PolicyDenied => {
            result.semantic_status == CommandSemanticStatus::PolicyBlocked
        }
        CommandExecutionStatus::Completed => {
            result.semantic_status != CommandSemanticStatus::Succeeded
                && result.sandbox.enforcement == SandboxBackendEnforcement::Strict
        }
        _ => false,
    };
    classified
        && result.workspace_mutation != WorkspaceMutation::Changed
        && !result.sandbox.local_process_fallback
}

fn linux_filesystem_fact(workspace: &Path) -> Option<String> {
    let canonical_workspace =
        fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").ok()?;
    linux_filesystem_from_mountinfo(&mountinfo, &canonical_workspace)
}

fn linux_filesystem_from_mountinfo(mountinfo: &str, workspace: &Path) -> Option<String> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let (mount_fields, filesystem_fields) = line.split_once(" - ")?;
            let fields = mount_fields.split_whitespace().collect::<Vec<_>>();
            let mountpoint = fields.get(4).map(|value| decode_mountinfo_path(value))?;
            let mountpoint = Path::new(&mountpoint);
            if workspace != mountpoint && !workspace.starts_with(mountpoint) {
                return None;
            }
            let filesystem = filesystem_fields.split_whitespace().next()?.to_string();
            Some((mountpoint.components().count(), filesystem))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, filesystem)| filesystem.chars().take(64).collect())
}

fn decode_mountinfo_path(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if index + 3 < bytes.len() && bytes[index] == b'\\' {
            let octal = &bytes[index + 1..index + 4];
            if octal.iter().all(|byte| (b'0'..=b'7').contains(byte)) {
                decoded.push((octal[0] - b'0') * 64 + (octal[1] - b'0') * 8 + octal[2] - b'0');
                index += 4;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
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
    protect_workspace_metadata: bool,
    protected_paths: Vec<ProtectedPath>,
    before: Option<WorkspaceSnapshot>,
    transaction: Option<WorkspaceTransaction>,
}

impl PreparedCommand {
    fn from_input(input: LinuxExecutionInput) -> Result<Self, LinuxSandboxError> {
        let LinuxExecutionInput {
            command_id: _,
            mut argv,
            runtime_executables,
            cwd,
            timeout_seconds,
            network,
            filesystem,
            environment,
            trusted_workspace_preparation,
        } = input;
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
        let mut runtime_read_paths = resolved.runtime_read_paths;
        let mut runtime_environment = resolved.environment;
        let mut declared_executable_paths = Vec::new();
        for executable in runtime_executables {
            let declared = resolve_executable(&executable, &cwd_path, &env)?;
            push_unique_path(&mut declared_executable_paths, declared.executable.clone());
            push_unique_path(&mut declared_executable_paths, declared.invocation.clone());
            for path in declared.runtime_read_paths {
                push_unique_path(&mut runtime_read_paths, path);
            }
            for (name, value) in declared.environment {
                if runtime_environment
                    .iter()
                    .any(|(existing_name, existing_value)| {
                        existing_name == &name && existing_value != &value
                    })
                {
                    return Err(LinuxSandboxError::ExecutableUnavailable);
                }
                if !runtime_environment
                    .iter()
                    .any(|(existing_name, _)| existing_name == &name)
                {
                    runtime_environment.push((name, value));
                }
            }
        }
        if network == SandboxNetworkMode::Allowed {
            for candidate in NETWORK_RUNTIME_READ_PATHS {
                push_existing_canonical_path(&mut runtime_read_paths, Path::new(candidate));
            }
        }
        if std::iter::once(&resolved.executable)
            .chain(std::iter::once(&resolved.invocation))
            .chain(declared_executable_paths.iter())
            .chain(runtime_read_paths.iter())
            .any(|path| is_protected_path(&path.to_string_lossy()))
        {
            return Err(LinuxSandboxError::PolicyDenied(
                SANDBOX_PROTECTED_PATH_DENIED,
            ));
        }
        for (name, value) in &runtime_environment {
            set_environment_value(&mut env, name, value);
        }
        argv[0] = resolved.invocation.to_string_lossy().into_owned();
        let protect_workspace_metadata = !trusted_workspace_preparation;
        let protected_paths = if protect_workspace_metadata {
            collect_protected_paths(&workspace)
                .map_err(|_| LinuxSandboxError::PolicyDenied(SANDBOX_PROTECTED_PATH_DENIED))?
        } else {
            Vec::new()
        };
        validate_workspace_hardlinks(&workspace)
            .map_err(|_| LinuxSandboxError::PolicyDenied(SANDBOX_HARDLINK_DENIED))?;
        let before = if matches!(filesystem.mode, SandboxFilesystemMode::WorkspaceWrite) {
            Some(
                if protect_workspace_metadata {
                    snapshot_workspace(&workspace)
                } else {
                    snapshot_trusted_workspace(&workspace)
                }
                .map_err(|_| LinuxSandboxError::WorkspaceObservationUnavailable)?,
            )
        } else {
            None
        };
        let transaction = if matches!(filesystem.mode, SandboxFilesystemMode::WorkspaceWrite) {
            Some(
                WorkspaceTransaction::new(&workspace, protect_workspace_metadata).map_err(
                    |_| {
                        LinuxSandboxError::CapabilityNotSupported(
                            LinuxCapability::OverlayFilesystem,
                        )
                    },
                )?,
            )
        } else {
            None
        };
        Ok(Self {
            workspace,
            cwd: cwd_path,
            executable: resolved.executable,
            runtime_read_paths,
            argv,
            env,
            timeout: Duration::from_secs(timeout_seconds),
            network,
            filesystem: filesystem.mode,
            protect_workspace_metadata,
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
    fn new(workspace: &Path, protect_workspace_metadata: bool) -> Result<Self, std::io::Error> {
        let temporary_root = fs::canonicalize(std::env::temp_dir())?;
        if temporary_root.starts_with(workspace) {
            return Err(std::io::Error::other(
                "workspace transaction storage resolves inside the workspace",
            ));
        }
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
                            .and_then(|_| {
                                seed_workspace_view(workspace, &upper, protect_workspace_metadata)
                            })
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

/// Materialize ordinary workspace objects into the upper layer with exact hardlink metadata.
fn seed_workspace_view(
    workspace: &Path,
    upper: &Path,
    protect_workspace_metadata: bool,
) -> Result<(), std::io::Error> {
    fn visit(
        directory: &Dir,
        upper: &Path,
        relative_parent: &Path,
        protect_workspace_metadata: bool,
        links: &mut BTreeMap<(u64, u64), PathBuf>,
    ) -> Result<bool, std::io::Error> {
        let mut entries = directory.entries()?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        let mut contains_protected = false;
        for entry in entries {
            let name = entry.file_name();
            let relative = relative_parent.join(&name);
            let Some(name_text) = name.to_str() else {
                return Err(std::io::Error::other("non-Unicode hardlink path"));
            };
            let Some(relative_text) = relative.to_str() else {
                return Err(std::io::Error::other("non-Unicode hardlink path"));
            };
            if protect_workspace_metadata
                && (is_protected_path(name_text) || is_protected_path(relative_text))
            {
                contains_protected = true;
                continue;
            }
            let metadata = directory.symlink_metadata(&name)?;
            let destination = upper.join(&relative);
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                let child = directory.open_dir_nofollow(&name)?;
                fs::create_dir(&destination)?;
                let child_contains_protected =
                    visit(&child, upper, &relative, protect_workspace_metadata, links)?;
                if !child_contains_protected {
                    set_user_overlay_opaque(&destination)?;
                }
                contains_protected |= child_contains_protected;
                fs::set_permissions(
                    &destination,
                    fs::Permissions::from_mode(cap_std::fs::PermissionsExt::mode(
                        &metadata.permissions(),
                    )),
                )?;
                copy_seed_times(&destination, &metadata, false)?;
            } else if metadata.is_file() {
                let identity = (
                    cap_fs_ext::MetadataExt::dev(&metadata),
                    cap_fs_ext::MetadataExt::ino(&metadata),
                );
                if let Some(first) = links.get(&identity) {
                    fs::hard_link(first, &destination)?;
                } else {
                    let mut source = directory.open(&name)?;
                    if file_has_extended_attributes(&source)? {
                        return Err(std::io::Error::other(
                            "workspace file has extended attributes",
                        ));
                    }
                    let mut destination_file = OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(&destination)?;
                    io::copy(&mut source, &mut destination_file)?;
                    links.insert(identity, destination.clone());
                }
                fs::set_permissions(
                    &destination,
                    fs::Permissions::from_mode(cap_std::fs::PermissionsExt::mode(
                        &metadata.permissions(),
                    )),
                )?;
                copy_seed_times(&destination, &metadata, false)?;
            } else if metadata.file_type().is_symlink() {
                let target = directory.read_link(&name)?;
                std::os::unix::fs::symlink(target, &destination)?;
                copy_seed_times(&destination, &metadata, true)?;
            } else {
                return Err(std::io::Error::other(
                    "workspace contains an unsupported object",
                ));
            }
        }
        Ok(contains_protected)
    }

    let mut links = BTreeMap::new();
    let workspace_directory = Dir::open_ambient_dir(workspace, ambient_authority())?;
    visit(
        &workspace_directory,
        upper,
        Path::new(""),
        protect_workspace_metadata,
        &mut links,
    )?;
    Ok(())
}

fn file_has_extended_attributes(file: &cap_std::fs::File) -> Result<bool, std::io::Error> {
    let size = unsafe { libc::flistxattr(file.as_raw_fd(), ptr::null_mut(), 0) };
    if size < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(size != 0)
}

fn copy_seed_times(
    destination: &Path,
    metadata: &cap_std::fs::Metadata,
    symlink: bool,
) -> Result<(), std::io::Error> {
    let parent = destination
        .parent()
        .ok_or_else(|| std::io::Error::other("seed destination has no parent"))?;
    let name = destination
        .file_name()
        .ok_or_else(|| std::io::Error::other("seed destination has no name"))?;
    let directory = Dir::open_ambient_dir(parent, ambient_authority())?;
    let access = cap_fs_ext::SystemTimeSpec::Absolute(metadata.accessed()?);
    let modified = cap_fs_ext::SystemTimeSpec::Absolute(metadata.modified()?);
    if symlink {
        directory.set_symlink_times(name, Some(access), Some(modified))
    } else {
        directory.set_times(name, Some(access), Some(modified))
    }
}

fn set_user_overlay_opaque(path: &Path) -> Result<(), std::io::Error> {
    let path = path_cstring(path).map_err(|_| std::io::Error::other("invalid overlay path"))?;
    let name = c"user.overlay.opaque";
    let value = b"y";
    let result = unsafe {
        libc::lsetxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
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
    STANDARD_RUNTIME_ROOTS
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
    fd_table_ready_write: RawFd,
    preserved_fds: [RawFd; 5],
    fd_fallback_limit: RawFd,
    filesystem_operations: Vec<ChildFilesystemOperation>,
    landlock_rules: Vec<LandlockRule>,
    seccomp_filter: Vec<SockFilter>,
    cwd: CString,
    executable: CString,
    _argv: Vec<CString>,
    argv_pointers: Vec<*const libc::c_char>,
    _env: Vec<CString>,
    env_pointers: Vec<*const libc::c_char>,
}

struct LandlockRule {
    path: CString,
    allowed_access: u64,
}

enum ChildFilesystemOperation {
    Mount {
        source: Option<CString>,
        target: CString,
        filesystem: Option<CString>,
        flags: libc::c_ulong,
        data: Option<CString>,
        error: u8,
    },
    CreateDirectory {
        path: CString,
        mode: libc::mode_t,
        allow_existing: bool,
        error: u8,
    },
    CreateFile {
        path: CString,
        mode: libc::mode_t,
        error: u8,
    },
    SetMode {
        path: CString,
        mode: libc::mode_t,
        error: u8,
    },
}

fn run_prepared_command(
    command_id: &str,
    prepared: PreparedCommand,
    cancellation: &CancellationToken,
) -> CommandResult {
    let started = Instant::now();
    let argv = match prepared
        .argv
        .iter()
        .map(|value| CString::new(value.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(argv) => argv,
        Err(_) => {
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
            return LinuxSandboxError::PolicyDenied(SANDBOX_POLICY_DENIED).into_result(command_id);
        }
    };
    let filesystem_operations = match prepare_child_filesystem(&prepared) {
        Ok(operations) => operations,
        Err(error) => return error.into_result(command_id),
    };
    let landlock_rules = match prepare_landlock_rules(&prepared) {
        Ok(rules) => rules,
        Err(error) => return error.into_result(command_id),
    };
    let seccomp_filter = prepare_seccomp_filter(prepared.network == SandboxNetworkMode::Denied);
    let cwd = match path_cstring(&prepared.cwd) {
        Ok(cwd) => cwd,
        Err(()) => return LinuxSandboxError::Unavailable.into_result(command_id),
    };
    let executable = match path_cstring(&prepared.executable) {
        Ok(executable) => executable,
        Err(()) => return LinuxSandboxError::Unavailable.into_result(command_id),
    };
    let mut argv_pointers = argv.iter().map(|value| value.as_ptr()).collect::<Vec<_>>();
    argv_pointers.push(ptr::null());
    let mut env_pointers = env.iter().map(|value| value.as_ptr()).collect::<Vec<_>>();
    env_pointers.push(ptr::null());
    let launch_gate = match acquire_child_launch_gate(started, prepared.timeout, cancellation) {
        Ok(guard) => guard,
        Err(LaunchGateError::Unavailable) => {
            return LinuxSandboxError::Unavailable.into_result(command_id);
        }
        Err(interrupted) => {
            let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
            let result = match interrupted {
                LaunchGateError::Cancelled => CommandResult::cancelled(command_id, duration_ms),
                LaunchGateError::TimedOut => CommandResult::timed_out(command_id, duration_ms),
                LaunchGateError::Unavailable => unreachable!(),
            };
            return result
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
                .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Strict);
        }
    };
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
    let Some((fd_table_ready_read, fd_table_ready_write)) = pipe_cloexec() else {
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
    };
    let mut preserved_fds = [
        ready_read,
        status_write,
        stdout_write,
        stderr_write,
        fd_table_ready_write,
    ];
    preserved_fds.sort_unstable();
    let fd_fallback_limit = open_file_limit();
    let context = Box::new(ChildContext {
        ready_read,
        status_write,
        stdout_write,
        stderr_write,
        fd_table_ready_write,
        preserved_fds,
        fd_fallback_limit,
        filesystem_operations,
        landlock_rules,
        seccomp_filter,
        cwd,
        executable,
        _argv: argv,
        argv_pointers,
        _env: env,
        env_pointers,
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
            fd_table_ready_read,
            fd_table_ready_write,
        ] {
            close_fd(fd);
        }
        return LinuxSandboxError::Unavailable.into_result(command_id);
    }
    let fd_table_ready = wait_for_fd_table_ready(
        child,
        fd_table_ready_read,
        started,
        prepared.timeout,
        cancellation,
    );
    if fd_table_ready != FdTableReady::Ready {
        for fd in [
            stdout_read,
            stdout_write,
            stderr_read,
            stderr_write,
            status_read,
            status_write,
            ready_read,
            ready_write,
            fd_table_ready_read,
            fd_table_ready_write,
        ] {
            close_fd(fd);
        }
        let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        return match fd_table_ready {
            FdTableReady::Cancelled => CommandResult::cancelled(command_id, duration_ms)
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
                .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Strict),
            FdTableReady::TimedOut => CommandResult::timed_out(command_id, duration_ms)
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
                .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Strict),
            FdTableReady::Exited | FdTableReady::Failed => {
                LinuxSandboxError::Unavailable.into_result(command_id)
            }
            FdTableReady::Ready => unreachable!(),
        };
    }
    for fd in [
        stdout_write,
        stderr_write,
        status_write,
        ready_read,
        fd_table_ready_read,
        fd_table_ready_write,
    ] {
        close_fd(fd);
    }
    drop(launch_gate);
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
    let mut trusted_transaction_changed = None;
    if interrupted == InterruptKind::None
        && let Some(transaction) = prepared.transaction.as_ref()
    {
        let Some(before) = prepared.before.as_ref() else {
            return LinuxSandboxError::WorkspaceObservationUnavailable.into_result(command_id);
        };
        if prepared.protect_workspace_metadata
            && overlay_contains_protected_change(&transaction.upper)
        {
            return CommandResult::policy_denied(command_id, WORKSPACE_TRANSACTION_DENIED)
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
                .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Strict);
        }
        match commit_workspace_transaction(
            &prepared.workspace,
            before,
            &transaction.upper,
            cancellation,
            prepared.protect_workspace_metadata,
        ) {
            Ok(changed) => {
                if !prepared.protect_workspace_metadata {
                    trusted_transaction_changed = Some(changed);
                }
            }
            Err(error) => {
                return match error {
                    WorkspaceTransactionError::PolicyDenied => {
                        CommandResult::policy_denied(command_id, WORKSPACE_TRANSACTION_DENIED)
                            .with_workspace_mutation(WorkspaceMutation::Unknown)
                            .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Strict)
                    }
                    WorkspaceTransactionError::Drift => {
                        CommandResult::policy_denied(command_id, WORKSPACE_TRANSACTION_DRIFT)
                            .with_workspace_mutation(WorkspaceMutation::Unknown)
                            .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Strict)
                    }
                    WorkspaceTransactionError::Cancelled => CommandResult::cancelled(
                        command_id,
                        started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                    )
                    .with_workspace_mutation(WorkspaceMutation::Unchanged)
                    .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Strict),
                    WorkspaceTransactionError::RollbackFailed => CommandResult::backend_error(
                        command_id,
                        WORKSPACE_TRANSACTION_ROLLBACK_FAILED,
                    )
                    .with_workspace_mutation(WorkspaceMutation::Unknown)
                    .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Strict),
                    WorkspaceTransactionError::CleanupFailed => CommandResult::backend_error(
                        command_id,
                        WORKSPACE_TRANSACTION_CLEANUP_FAILED,
                    )
                    .with_workspace_mutation(WorkspaceMutation::Unknown)
                    .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Strict),
                    WorkspaceTransactionError::CapabilityNotSupported => {
                        LinuxSandboxError::CapabilityNotSupported(
                            LinuxCapability::WorkspaceTransaction,
                        )
                        .into_result(command_id)
                    }
                };
            }
        }
    }
    let (mutation, summary) = if let Some(changed) = trusted_transaction_changed {
        (
            if changed {
                WorkspaceMutation::Changed
            } else {
                WorkspaceMutation::Unchanged
            },
            None,
        )
    } else {
        observed_workspace_change(&prepared.before, &prepared.workspace)
    };
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum TransactionObjectKind {
    Directory,
    File([u8; 32]),
    Symlink(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransactionObjectState {
    kind: TransactionObjectKind,
    mode: u32,
    modified: cap_std::time::SystemTime,
    device: u64,
    inode: u64,
    length: u64,
    workspace_links: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransactionTreeState {
    entries: BTreeMap<PathBuf, TransactionObjectState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStatEvidence {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanos: i64,
    changed_seconds: i64,
    changed_nanos: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceTransactionError {
    PolicyDenied,
    Drift,
    Cancelled,
    RollbackFailed,
    CleanupFailed,
    CapabilityNotSupported,
}

/// Same-filesystem staging and backup directories used by the commit phase.
struct CommitArea {
    root: PathBuf,
    stage: PathBuf,
    backup: PathBuf,
    metadata: PathBuf,
    preserve: bool,
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
    metadata_directory: Dir,
    moved: Vec<MovedWorkspaceObject>,
    installed: Vec<InstalledWorkspaceObject>,
    detached_metadata: Vec<MovedWorkspaceObject>,
    directory_times: Vec<DirectoryTimes>,
}

struct MovedWorkspaceObject {
    relative: PathBuf,
    expected_before: TransactionTreeState,
}

struct InstalledWorkspaceObject {
    relative: PathBuf,
    expected_after: TransactionTreeState,
    source: InstalledWorkspaceSource,
}

#[derive(Clone, Copy)]
enum InstalledWorkspaceSource {
    Stage,
    Metadata(DirectoryMetadataRevision),
}

#[derive(Debug, Clone, Copy)]
struct DirectoryMetadataRevision {
    mode: u32,
    access: cap_std::time::SystemTime,
    modified: cap_std::time::SystemTime,
}

impl TransactionTreeState {
    fn capture(path: &Path) -> Result<Self, WorkspaceTransactionError> {
        let root = Dir::open_ambient_dir(path, ambient_authority())
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        Self::capture_from_dir(&root)
    }

    fn capture_from_dir(root: &Dir) -> Result<Self, WorkspaceTransactionError> {
        let mut state = Self {
            entries: BTreeMap::new(),
        };
        let mut visited = 0usize;
        let mut bytes = 0u64;
        capture_transaction_directory(
            root,
            Path::new(""),
            0,
            &mut visited,
            &mut bytes,
            &mut state,
        )?;
        state.recompute_workspace_links();
        Ok(state)
    }

    fn recompute_workspace_links(&mut self) {
        let mut counts = BTreeMap::new();
        for entry in self.entries.values() {
            if matches!(entry.kind, TransactionObjectKind::File(_)) {
                *counts.entry((entry.device, entry.inode)).or_insert(0u64) += 1;
            }
        }
        for entry in self.entries.values_mut() {
            entry.workspace_links = counts
                .get(&(entry.device, entry.inode))
                .copied()
                .unwrap_or(0);
        }
    }

    fn remove_subtree(&mut self, relative: &Path) {
        self.entries
            .retain(|path, _| path != relative && !path.starts_with(relative));
    }

    fn subtree(&self, relative: &Path) -> Self {
        let mut subtree = Self {
            entries: self
                .entries
                .iter()
                .filter(|(path, _)| *path == relative || path.starts_with(relative))
                .map(|(path, entry)| (path.clone(), entry.clone()))
                .collect(),
        };
        subtree.recompute_workspace_links();
        subtree
    }

    fn insert_subtree(&mut self, subtree: &Self) {
        self.entries.extend(
            subtree
                .entries
                .iter()
                .map(|(path, entry)| (path.clone(), entry.clone())),
        );
    }

    fn matches_ignoring_directory_times(
        &self,
        expected: &Self,
        volatile_directories: &std::collections::BTreeSet<PathBuf>,
        protect_workspace_metadata: bool,
    ) -> bool {
        let mut actual_entries = self.entries.iter().filter(|(path, _)| {
            !protect_workspace_metadata || transaction_path_is_comparable(path)
        });
        let expected_entries = expected.entries.iter().filter(|(path, _)| {
            !protect_workspace_metadata || transaction_path_is_comparable(path)
        });
        if actual_entries
            .clone()
            .map(|(path, _)| path)
            .ne(expected_entries.clone().map(|(path, _)| path))
        {
            return false;
        }
        actual_entries.all(|(path, actual)| {
            let Some(expected) = expected.entries.get(path) else {
                return false;
            };
            transaction_object_matches(
                actual,
                expected,
                volatile_directories.contains(path),
                protect_workspace_metadata && transaction_path_is_protected_root(path),
            )
        })
    }
}

fn transaction_path_is_protected_root(path: &Path) -> bool {
    let mut protected_seen = false;
    for component in path.components() {
        let std::path::Component::Normal(component) = component else {
            return false;
        };
        if is_protected_path(&component.to_string_lossy()) {
            protected_seen = true;
        } else if protected_seen {
            return false;
        }
    }
    protected_seen
}

fn transaction_path_is_comparable(path: &Path) -> bool {
    let mut protected_seen = false;
    for component in path.components() {
        let std::path::Component::Normal(component) = component else {
            return false;
        };
        if protected_seen {
            return false;
        }
        protected_seen = is_protected_path(&component.to_string_lossy());
    }
    true
}

fn transaction_object_matches(
    actual: &TransactionObjectState,
    expected: &TransactionObjectState,
    ignore_directory_modified: bool,
    protected_root: bool,
) -> bool {
    if protected_root {
        let same_kind = matches!(
            (&actual.kind, &expected.kind),
            (
                TransactionObjectKind::Directory,
                TransactionObjectKind::Directory
            ) | (
                TransactionObjectKind::File(_),
                TransactionObjectKind::File(_)
            ) | (
                TransactionObjectKind::Symlink(_),
                TransactionObjectKind::Symlink(_)
            )
        );
        if !same_kind || actual.mode != expected.mode || actual.device != expected.device {
            return false;
        }
        if actual.inode != expected.inode {
            return false;
        }
        return matches!(
            (&actual.kind, &expected.kind),
            (
                TransactionObjectKind::Directory,
                TransactionObjectKind::Directory
            )
        ) || actual == expected;
    }
    if matches!(actual.kind, TransactionObjectKind::Directory)
        && matches!(expected.kind, TransactionObjectKind::Directory)
    {
        let mut normalized = actual.clone();
        normalized.length = expected.length;
        if ignore_directory_modified {
            normalized.modified = expected.modified;
        }
        normalized == *expected
    } else {
        actual == expected
    }
}

fn capture_transaction_directory(
    directory: &Dir,
    relative_parent: &Path,
    depth: usize,
    visited: &mut usize,
    bytes: &mut u64,
    state: &mut TransactionTreeState,
) -> Result<(), WorkspaceTransactionError> {
    if depth > MAX_TRANSACTION_DEPTH {
        return Err(WorkspaceTransactionError::CapabilityNotSupported);
    }
    let mut entries = directory
        .entries()
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        *visited = visited.saturating_add(1);
        if *visited > MAX_TRANSACTION_ENTRIES {
            return Err(WorkspaceTransactionError::CapabilityNotSupported);
        }
        let name = entry.file_name();
        let relative = relative_parent.join(&name);
        let metadata = directory
            .symlink_metadata(&name)
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        let mode = cap_std::fs::PermissionsExt::mode(&metadata.permissions());
        let modified = metadata
            .modified()
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        let device = cap_fs_ext::MetadataExt::dev(&metadata);
        let inode = cap_fs_ext::MetadataExt::ino(&metadata);
        let length = metadata.len();
        let kind = if metadata.is_dir() && !metadata.file_type().is_symlink() {
            let child = directory
                .open_dir_nofollow(&name)
                .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
            capture_transaction_directory(&child, &relative, depth + 1, visited, bytes, state)?;
            TransactionObjectKind::Directory
        } else if metadata.is_file() {
            *bytes = bytes.saturating_add(length);
            if *bytes > 512 * 1024 * 1024 {
                return Err(WorkspaceTransactionError::CapabilityNotSupported);
            }
            let mut options = cap_std::fs::OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            let mut file = directory
                .open_with(&name, &options)
                .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
            let stat_before = stable_file_stat(&file)?;
            if stat_before.device != device
                || stat_before.inode != inode
                || stat_before.length != length
            {
                return Err(WorkspaceTransactionError::Drift);
            }
            let mut hasher = Sha256::new();
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let read = file
                    .read(&mut buffer)
                    .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            if stable_file_stat(&file)? != stat_before {
                return Err(WorkspaceTransactionError::Drift);
            }
            TransactionObjectKind::File(hasher.finalize().into())
        } else if metadata.file_type().is_symlink() {
            let target = directory
                .read_link(&name)
                .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
            TransactionObjectKind::Symlink(target.as_os_str().as_bytes().to_vec())
        } else {
            return Err(WorkspaceTransactionError::CapabilityNotSupported);
        };
        let after = directory
            .symlink_metadata(&name)
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        if cap_fs_ext::MetadataExt::dev(&after) != device
            || cap_fs_ext::MetadataExt::ino(&after) != inode
            || after.len() != length
            || after
                .modified()
                .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?
                != modified
        {
            return Err(WorkspaceTransactionError::Drift);
        }
        state.entries.insert(
            relative,
            TransactionObjectState {
                kind,
                mode,
                modified,
                device,
                inode,
                length,
                workspace_links: 0,
            },
        );
    }
    Ok(())
}

fn stable_file_stat(
    file: &cap_std::fs::File,
) -> Result<FileStatEvidence, WorkspaceTransactionError> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(WorkspaceTransactionError::CapabilityNotSupported);
    }
    let stat = unsafe { stat.assume_init() };
    Ok(FileStatEvidence {
        device: stat.st_dev,
        inode: stat.st_ino,
        length: stat
            .st_size
            .try_into()
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?,
        modified_seconds: stat.st_mtime,
        modified_nanos: stat.st_mtime_nsec,
        changed_seconds: stat.st_ctime,
        changed_nanos: stat.st_ctime_nsec,
    })
}

fn expected_transaction_state(
    before: &TransactionTreeState,
    stage: &TransactionTreeState,
    operations: &[WorkspaceOperation],
) -> Result<TransactionTreeState, WorkspaceTransactionError> {
    let mut expected = before.clone();
    for operation in operations {
        match operation {
            WorkspaceOperation::Delete(relative) => expected.remove_subtree(relative),
            WorkspaceOperation::Replace(relative) => {
                let subtree = stage.subtree(relative);
                if !subtree.entries.contains_key(relative) {
                    return Err(WorkspaceTransactionError::CapabilityNotSupported);
                }
                expected.remove_subtree(relative);
                expected.insert_subtree(&subtree);
            }
            WorkspaceOperation::SetMetadata {
                relative,
                mode,
                modified,
                ..
            } => {
                let entry = expected
                    .entries
                    .get_mut(relative)
                    .ok_or(WorkspaceTransactionError::Drift)?;
                entry.mode = *mode;
                entry.modified = *modified;
            }
        }
    }
    expected.recompute_workspace_links();
    Ok(expected)
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
                    let metadata = root.join("metadata");
                    if fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
                        .and_then(|_| fs::create_dir(&stage))
                        .and_then(|_| fs::create_dir(&backup))
                        .and_then(|_| fs::create_dir(&metadata))
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
                        metadata,
                        preserve: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(WorkspaceTransactionError::CapabilityNotSupported),
            }
        }
        Err(WorkspaceTransactionError::CapabilityNotSupported)
    }

    fn preserve_recovery(&mut self) {
        self.preserve = true;
    }

    fn cleanup(&mut self) -> Result<(), WorkspaceTransactionError> {
        make_tree_owner_accessible(&self.root);
        fs::remove_dir_all(&self.root)
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)
    }
}

impl Drop for CommitArea {
    fn drop(&mut self) {
        if self.preserve {
            return;
        }
        make_tree_owner_accessible(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Validate the complete upper representation, detect drift, and atomically install each change.
fn commit_workspace_transaction(
    workspace: &Path,
    before: &WorkspaceSnapshot,
    upper: &Path,
    cancellation: &CancellationToken,
    protect_workspace_metadata: bool,
) -> Result<bool, WorkspaceTransactionError> {
    let mut area = CommitArea::new(workspace)?;
    let mut operations = Vec::new();
    let mut visited = 0usize;
    let mut staged_links = BTreeMap::new();
    plan_overlay_directory(
        workspace,
        upper,
        upper,
        Path::new(""),
        &area.stage,
        0,
        &mut visited,
        &mut operations,
        &mut staged_links,
        protect_workspace_metadata,
    )?;
    if cancellation.is_cancelled() {
        return Err(WorkspaceTransactionError::Cancelled);
    }
    let final_volatile_directories = operation_parent_directories(&operations);
    let before_state = TransactionTreeState::capture(workspace)?;
    let current = if protect_workspace_metadata {
        snapshot_workspace(workspace)
    } else {
        snapshot_trusted_workspace(workspace)
    }
    .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    if !before.transaction_baseline_matches(&current) {
        return Err(WorkspaceTransactionError::Drift);
    }
    let stage_state = TransactionTreeState::capture(&area.stage)?;
    let expected = expected_transaction_state(&before_state, &stage_state, &operations)?;
    if protect_workspace_metadata && protected_transaction_state_changed(&before_state, &expected) {
        return Err(WorkspaceTransactionError::PolicyDenied);
    }
    if cancellation.is_cancelled() {
        return Err(WorkspaceTransactionError::Cancelled);
    }
    if !TransactionTreeState::capture(workspace)?.matches_ignoring_directory_times(
        &before_state,
        &std::collections::BTreeSet::new(),
        protect_workspace_metadata,
    ) {
        return Err(WorkspaceTransactionError::Drift);
    }
    let mut applied = AppliedWorkspaceOperations::new(workspace, &area, &operations)?;
    if let Err(error) = apply_workspace_operations(
        workspace,
        &operations,
        &before_state,
        &stage_state,
        cancellation,
        &mut applied,
        protect_workspace_metadata,
    ) {
        return Err(rollback_or_preserve(
            &mut area,
            &applied,
            error,
            protect_workspace_metadata,
        ));
    }
    transaction_test_pause(TransactionTestPoint::FinalVerification);
    if cancellation.is_cancelled() {
        return Err(rollback_or_preserve(
            &mut area,
            &applied,
            WorkspaceTransactionError::Cancelled,
            protect_workspace_metadata,
        ));
    }
    let final_state = match TransactionTreeState::capture(workspace) {
        Ok(state) => state,
        Err(error) => {
            return Err(rollback_or_preserve(
                &mut area,
                &applied,
                error,
                protect_workspace_metadata,
            ));
        }
    };
    if !final_state.matches_ignoring_directory_times(
        &expected,
        &final_volatile_directories,
        protect_workspace_metadata,
    ) {
        return Err(rollback_or_preserve(
            &mut area,
            &applied,
            WorkspaceTransactionError::Drift,
            protect_workspace_metadata,
        ));
    }
    if cancellation.is_cancelled() {
        return Err(rollback_or_preserve(
            &mut area,
            &applied,
            WorkspaceTransactionError::Cancelled,
            protect_workspace_metadata,
        ));
    }
    if area.cleanup().is_err() {
        area.preserve_recovery();
        return Err(WorkspaceTransactionError::CleanupFailed);
    }
    Ok(!operations.is_empty())
}

fn protected_transaction_state_changed(
    before: &TransactionTreeState,
    expected: &TransactionTreeState,
) -> bool {
    before.entries.iter().any(|(path, entry)| {
        let protected = path
            .components()
            .filter_map(|component| component.as_os_str().to_str())
            .any(is_protected_path);
        protected && expected.entries.get(path) != Some(entry)
    })
}

fn rollback_or_preserve(
    area: &mut CommitArea,
    applied: &AppliedWorkspaceOperations,
    cause: WorkspaceTransactionError,
    protect_workspace_metadata: bool,
) -> WorkspaceTransactionError {
    let rollback = rollback_partial(applied, protect_workspace_metadata);
    if rollback.is_err() {
        area.preserve_recovery();
        WorkspaceTransactionError::RollbackFailed
    } else {
        cause
    }
}

#[allow(clippy::too_many_arguments)]
/// Convert OverlayFS upper entries into a bounded, no-follow commit plan.
fn plan_overlay_directory(
    workspace: &Path,
    upper_root: &Path,
    upper_directory: &Path,
    relative_parent: &Path,
    stage: &Path,
    depth: usize,
    visited: &mut usize,
    operations: &mut Vec<WorkspaceOperation>,
    staged_links: &mut BTreeMap<(u64, u64), PathBuf>,
    protect_workspace_metadata: bool,
) -> Result<(), WorkspaceTransactionError> {
    if depth > MAX_TRANSACTION_DEPTH {
        return Err(WorkspaceTransactionError::PolicyDenied);
    }
    let opaque = validate_overlay_attributes(upper_directory, upper_root, true)?;
    let mut entries = fs::read_dir(upper_directory)
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    entries.sort_by_key(|entry| entry.file_name());
    let upper_names = entries
        .iter()
        .map(fs::DirEntry::file_name)
        .collect::<std::collections::BTreeSet<_>>();
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
        if protect_workspace_metadata
            && (is_protected_path(name_text) || is_protected_path(relative_text))
        {
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
        validate_overlay_attributes(&source, upper_root, file_type.is_dir())?;
        if file_type.is_dir() {
            let destination_metadata = fs::symlink_metadata(workspace.join(&relative)).ok();
            if destination_metadata
                .as_ref()
                .is_some_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            {
                plan_overlay_directory(
                    workspace,
                    upper_root,
                    &source,
                    &relative,
                    stage,
                    depth + 1,
                    visited,
                    operations,
                    staged_links,
                    protect_workspace_metadata,
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
                let Some(destination) = destination_metadata.as_ref() else {
                    return Err(WorkspaceTransactionError::CapabilityNotSupported);
                };
                if destination.permissions().mode() != desired_mode
                    || destination.modified().ok() != Some(desired_modified.into_std())
                {
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
                    upper_root,
                    depth + 1,
                    visited,
                    staged_links,
                    protect_workspace_metadata,
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
    if opaque {
        let workspace_directory = workspace.join(relative_parent);
        if let Ok(entries) = fs::read_dir(workspace_directory) {
            let mut entries = entries
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
            entries.sort_by_key(fs::DirEntry::file_name);
            for entry in entries {
                let name = entry.file_name();
                if upper_names.contains(&name) {
                    continue;
                }
                let relative = relative_parent.join(&name);
                let name_text = name
                    .to_str()
                    .ok_or(WorkspaceTransactionError::PolicyDenied)?;
                let relative_text = relative
                    .to_str()
                    .ok_or(WorkspaceTransactionError::PolicyDenied)?;
                if protect_workspace_metadata
                    && (is_protected_path(name_text) || is_protected_path(relative_text))
                {
                    return Err(WorkspaceTransactionError::PolicyDenied);
                }
                operations.push(WorkspaceOperation::Delete(relative));
            }
        }
    }
    Ok(())
}

fn copy_upper_tree(
    source: &Path,
    destination: &Path,
    upper_root: &Path,
    depth: usize,
    visited: &mut usize,
    staged_links: &mut BTreeMap<(u64, u64), PathBuf>,
    protect_workspace_metadata: bool,
) -> Result<(), WorkspaceTransactionError> {
    if depth > MAX_TRANSACTION_DEPTH {
        return Err(WorkspaceTransactionError::PolicyDenied);
    }
    validate_overlay_attributes(source, upper_root, true)?;
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
        if protect_workspace_metadata && is_protected_path(name_text) {
            return Err(WorkspaceTransactionError::PolicyDenied);
        }
        let source = entry.path();
        let destination = destination.join(&name);
        let child_metadata = fs::symlink_metadata(&source)
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        let file_type = child_metadata.file_type();
        validate_overlay_attributes(&source, upper_root, file_type.is_dir())?;
        if file_type.is_char_device() && child_metadata.rdev() == 0 {
            return Err(WorkspaceTransactionError::PolicyDenied);
        }
        if file_type.is_dir() {
            copy_upper_tree(
                &source,
                &destination,
                upper_root,
                depth + 1,
                visited,
                staged_links,
                protect_workspace_metadata,
            )?;
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

fn validate_overlay_attributes(
    path: &Path,
    upper_root: &Path,
    directory: bool,
) -> Result<bool, WorkspaceTransactionError> {
    let is_upper_root = path == upper_root;
    let path = path_cstring(path).map_err(|_| WorkspaceTransactionError::PolicyDenied)?;
    let size = unsafe { libc::llistxattr(path.as_ptr(), ptr::null_mut(), 0) };
    if size < 0 {
        return Err(WorkspaceTransactionError::CapabilityNotSupported);
    }
    if size == 0 {
        return Ok(false);
    }
    let mut names = vec![0u8; size as usize];
    let read = unsafe { libc::llistxattr(path.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    if read < 0 || read as usize != names.len() {
        return Err(WorkspaceTransactionError::CapabilityNotSupported);
    }
    let mut opaque = false;
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let allowed = if directory && name == b"user.overlay.opaque" {
            opaque = overlay_attribute_value(path.as_ptr(), name)? == b"y";
            opaque
        } else if is_upper_root && name == b"user.overlay.uuid" {
            overlay_attribute_value(path.as_ptr(), name)?.len() == 16
        } else {
            false
        };
        if !allowed {
            return Err(WorkspaceTransactionError::PolicyDenied);
        }
    }
    Ok(opaque)
}

fn overlay_attribute_value(
    path: *const libc::c_char,
    name: &[u8],
) -> Result<Vec<u8>, WorkspaceTransactionError> {
    let name = CString::new(name).map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    let size = unsafe { libc::lgetxattr(path, name.as_ptr(), ptr::null_mut(), 0) };
    if size < 0 {
        return Err(WorkspaceTransactionError::CapabilityNotSupported);
    }
    let mut value = vec![0u8; size as usize];
    let read =
        unsafe { libc::lgetxattr(path, name.as_ptr(), value.as_mut_ptr().cast(), value.len()) };
    if read < 0 || read as usize != value.len() {
        return Err(WorkspaceTransactionError::CapabilityNotSupported);
    }
    Ok(value)
}

fn rename_noreplace(
    source_root: &Dir,
    source: &Path,
    destination_root: &Dir,
    destination: &Path,
) -> Result<(), std::io::Error> {
    let (source_directory, source_name) = open_relative_parent_nofollow(source_root, source)?;
    let (destination_directory, destination_name) =
        open_relative_parent_nofollow(destination_root, destination)?;
    let source = path_cstring(&source_name)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid source"))?;
    let destination = path_cstring(&destination_name).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid destination")
    })?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_directory.as_raw_fd(),
            source.as_ptr(),
            destination_directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn open_relative_parent_nofollow(
    root: &Dir,
    relative: &Path,
) -> Result<(Dir, PathBuf), std::io::Error> {
    if relative.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "absolute transaction path",
        ));
    }
    let name = relative.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "missing transaction object name",
        )
    })?;
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let directory = open_relative_directory_nofollow(root, parent)?;
    Ok((directory, PathBuf::from(name)))
}

fn open_relative_directory_nofollow(root: &Dir, relative: &Path) -> Result<Dir, std::io::Error> {
    if relative.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "absolute transaction directory",
        ));
    }
    let mut directory = root.try_clone()?;
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid transaction parent",
            ));
        };
        directory = directory.open_dir_nofollow(component)?;
    }
    Ok(directory)
}

fn pinned_directory_metadata(
    directory: &Dir,
) -> Result<DirectoryMetadataRevision, WorkspaceTransactionError> {
    let metadata = directory
        .dir_metadata()
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    Ok(DirectoryMetadataRevision {
        mode: cap_std::fs::PermissionsExt::mode(&metadata.permissions()),
        access: metadata
            .accessed()
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?,
        modified: metadata
            .modified()
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?,
    })
}

fn set_pinned_directory_metadata(
    directory: &Dir,
    revision: DirectoryMetadataRevision,
) -> Result<(), std::io::Error> {
    let permissions = cap_std::fs::Permissions::from_std(fs::Permissions::from_mode(revision.mode));
    directory.set_permissions(".", permissions).and_then(|_| {
        directory.set_times(
            ".",
            Some(cap_fs_ext::SystemTimeSpec::Absolute(revision.access)),
            Some(cap_fs_ext::SystemTimeSpec::Absolute(revision.modified)),
        )
    })
}

fn restore_pinned_directory_metadata(
    directory: &Dir,
    revision: DirectoryMetadataRevision,
) -> Result<(), WorkspaceTransactionError> {
    set_pinned_directory_metadata(directory, revision)
        .map_err(|_| WorkspaceTransactionError::RollbackFailed)
}

fn update_pinned_directory_metadata(
    directory: &Dir,
    mode: u32,
    access: cap_std::time::SystemTime,
    modified: cap_std::time::SystemTime,
) -> Result<DirectoryMetadataRevision, WorkspaceTransactionError> {
    let original = pinned_directory_metadata(directory)?;
    let requested = DirectoryMetadataRevision {
        mode,
        access,
        modified,
    };
    if set_pinned_directory_metadata(directory, requested).is_err() {
        restore_pinned_directory_metadata(directory, original)?;
        return Err(WorkspaceTransactionError::CapabilityNotSupported);
    }
    let observed = match pinned_directory_metadata(directory) {
        Ok(observed) => observed,
        Err(error) => {
            restore_pinned_directory_metadata(directory, original)?;
            return Err(error);
        }
    };
    if observed.mode != mode || observed.modified != modified {
        restore_pinned_directory_metadata(directory, original)?;
        return Err(WorkspaceTransactionError::CapabilityNotSupported);
    }
    Ok(original)
}

impl AppliedWorkspaceOperations {
    fn new(
        workspace: &Path,
        area: &CommitArea,
        operations: &[WorkspaceOperation],
    ) -> Result<Self, WorkspaceTransactionError> {
        Ok(Self {
            workspace_directory: Dir::open_ambient_dir(workspace, ambient_authority())
                .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?,
            stage_directory: Dir::open_ambient_dir(&area.stage, ambient_authority())
                .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?,
            backup_directory: Dir::open_ambient_dir(&area.backup, ambient_authority())
                .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?,
            metadata_directory: Dir::open_ambient_dir(&area.metadata, ambient_authority())
                .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?,
            moved: Vec::new(),
            installed: Vec::new(),
            detached_metadata: Vec::new(),
            directory_times: capture_parent_directory_times(workspace, operations)?,
        })
    }
}

/// Apply a validated plan through no-replace capability-relative renames.
fn apply_workspace_operations(
    workspace: &Path,
    operations: &[WorkspaceOperation],
    before: &TransactionTreeState,
    stage: &TransactionTreeState,
    cancellation: &CancellationToken,
    applied: &mut AppliedWorkspaceOperations,
    protect_workspace_metadata: bool,
) -> Result<(), WorkspaceTransactionError> {
    let mut current = before.clone();
    let volatile_directories = operation_parent_directories(operations);
    ensure_workspace_state(
        workspace,
        &current,
        &volatile_directories,
        protect_workspace_metadata,
    )?;

    for operation in operations {
        let relative = match operation {
            WorkspaceOperation::Delete(relative) | WorkspaceOperation::Replace(relative) => {
                relative
            }
            WorkspaceOperation::SetMetadata { .. } => continue,
        };
        check_transaction_cancellation(cancellation)?;
        ensure_workspace_state(
            workspace,
            &current,
            &volatile_directories,
            protect_workspace_metadata,
        )?;
        transaction_test_pause(TransactionTestPoint::WorkspaceMove);
        check_transaction_cancellation(cancellation)?;
        let expected_before = before.subtree(relative);
        if expected_before.entries.is_empty() {
            match applied.workspace_directory.symlink_metadata(relative) {
                Ok(_) => return Err(WorkspaceTransactionError::Drift),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(WorkspaceTransactionError::CapabilityNotSupported),
            }
            continue;
        }
        if let Some(parent) = relative.parent()
            && !parent.as_os_str().is_empty()
        {
            applied
                .backup_directory
                .create_dir_all(parent)
                .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        }
        rename_noreplace(
            &applied.workspace_directory,
            relative,
            &applied.backup_directory,
            relative,
        )
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists
                || error.kind() == std::io::ErrorKind::NotFound
            {
                WorkspaceTransactionError::Drift
            } else {
                WorkspaceTransactionError::CapabilityNotSupported
            }
        })?;
        let backup = TransactionTreeState::capture_from_dir(&applied.backup_directory)?;
        let moved_state = backup.subtree(relative);
        applied.moved.push(MovedWorkspaceObject {
            relative: relative.clone(),
            expected_before: moved_state.clone(),
        });
        if !moved_state.matches_ignoring_directory_times(
            &expected_before,
            &std::collections::BTreeSet::new(),
            protect_workspace_metadata,
        ) {
            return Err(WorkspaceTransactionError::Drift);
        }
        current.remove_subtree(relative);
        current.recompute_workspace_links();
        ensure_workspace_state(
            workspace,
            &current,
            &volatile_directories,
            protect_workspace_metadata,
        )?;
    }

    for operation in operations {
        let (relative, expected_after, source) = match operation {
            WorkspaceOperation::Delete(_) | WorkspaceOperation::SetMetadata { .. } => continue,
            WorkspaceOperation::Replace(relative) => {
                let expected_after = stage.subtree(relative);
                if expected_after.entries.is_empty() {
                    return Err(WorkspaceTransactionError::CapabilityNotSupported);
                }
                (relative, expected_after, InstalledWorkspaceSource::Stage)
            }
        };
        check_transaction_cancellation(cancellation)?;
        ensure_workspace_state(
            workspace,
            &current,
            &volatile_directories,
            protect_workspace_metadata,
        )?;
        if let Some(parent) = relative.parent()
            && !parent.as_os_str().is_empty()
            && open_relative_directory_nofollow(&applied.workspace_directory, parent).is_err()
        {
            return Err(WorkspaceTransactionError::Drift);
        }
        let source_directory = match source {
            InstalledWorkspaceSource::Stage => &applied.stage_directory,
            InstalledWorkspaceSource::Metadata(_) => &applied.metadata_directory,
        };
        if let Err(error) = rename_noreplace(
            source_directory,
            relative,
            &applied.workspace_directory,
            relative,
        ) {
            if let InstalledWorkspaceSource::Metadata(original) = source {
                let directory =
                    open_relative_directory_nofollow(&applied.metadata_directory, relative)
                        .map_err(|_| WorkspaceTransactionError::RollbackFailed)?;
                restore_pinned_directory_metadata(&directory, original)?;
            }
            return Err(if error.kind() == std::io::ErrorKind::AlreadyExists {
                WorkspaceTransactionError::Drift
            } else {
                WorkspaceTransactionError::CapabilityNotSupported
            });
        }
        applied.installed.push(InstalledWorkspaceObject {
            relative: relative.clone(),
            expected_after: expected_after.clone(),
            source,
        });
        current.remove_subtree(relative);
        current.insert_subtree(&expected_after);
        current.recompute_workspace_links();
        ensure_workspace_state(
            workspace,
            &current,
            &volatile_directories,
            protect_workspace_metadata,
        )?;
    }

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
        check_transaction_cancellation(cancellation)?;
        ensure_workspace_state(
            workspace,
            &current,
            &volatile_directories,
            protect_workspace_metadata,
        )?;
        transaction_test_pause(TransactionTestPoint::MetadataMutation);
        check_transaction_cancellation(cancellation)?;
        let expected_before = current.subtree(relative);
        if expected_before.entries.is_empty() {
            return Err(WorkspaceTransactionError::Drift);
        }
        if let Some(parent) = relative.parent()
            && !parent.as_os_str().is_empty()
        {
            applied
                .metadata_directory
                .create_dir_all(parent)
                .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        }
        rename_noreplace(
            &applied.workspace_directory,
            relative,
            &applied.metadata_directory,
            relative,
        )
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists
                || error.kind() == std::io::ErrorKind::NotFound
            {
                WorkspaceTransactionError::Drift
            } else {
                WorkspaceTransactionError::CapabilityNotSupported
            }
        })?;
        let metadata_state = TransactionTreeState::capture_from_dir(&applied.metadata_directory)?;
        let detached = metadata_state.subtree(relative);
        applied.detached_metadata.push(MovedWorkspaceObject {
            relative: relative.clone(),
            expected_before: detached.clone(),
        });
        let detached_volatile_directories = detached
            .entries
            .iter()
            .filter_map(|(path, entry)| {
                matches!(entry.kind, TransactionObjectKind::Directory).then_some(path.clone())
            })
            .collect::<std::collections::BTreeSet<_>>();
        if !detached.matches_ignoring_directory_times(
            &expected_before,
            &detached_volatile_directories,
            protect_workspace_metadata,
        ) {
            return Err(WorkspaceTransactionError::Drift);
        }
        let directory = open_relative_directory_nofollow(&applied.metadata_directory, relative)
            .map_err(|_| WorkspaceTransactionError::Drift)?;
        let original = update_pinned_directory_metadata(&directory, *mode, *access, *modified)?;
        let mut expected_after = expected_before;
        let entry = expected_after
            .entries
            .get_mut(relative)
            .ok_or(WorkspaceTransactionError::Drift)?;
        entry.mode = *mode;
        entry.modified = *modified;
        let metadata_state = TransactionTreeState::capture_from_dir(&applied.metadata_directory)?;
        if !metadata_state
            .subtree(relative)
            .matches_ignoring_directory_times(
                &expected_after,
                &detached_volatile_directories,
                protect_workspace_metadata,
            )
        {
            restore_pinned_directory_metadata(&directory, original)?;
            return Err(WorkspaceTransactionError::Drift);
        }
        if let Err(error) = rename_noreplace(
            &applied.metadata_directory,
            relative,
            &applied.workspace_directory,
            relative,
        ) {
            restore_pinned_directory_metadata(&directory, original)?;
            return Err(if error.kind() == std::io::ErrorKind::AlreadyExists {
                WorkspaceTransactionError::Drift
            } else {
                WorkspaceTransactionError::CapabilityNotSupported
            });
        }
        let installed_directory =
            open_relative_directory_nofollow(&applied.workspace_directory, relative)
                .map_err(|_| WorkspaceTransactionError::Drift)?;
        let installed_metadata = installed_directory
            .dir_metadata()
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        let expected_entry = expected_after
            .entries
            .get(relative)
            .ok_or(WorkspaceTransactionError::Drift)?;
        if cap_fs_ext::MetadataExt::dev(&installed_metadata) != expected_entry.device
            || cap_fs_ext::MetadataExt::ino(&installed_metadata) != expected_entry.inode
        {
            return Err(WorkspaceTransactionError::Drift);
        }
        transaction_test_pause(TransactionTestPoint::InstalledMetadataMutation);
        set_pinned_directory_metadata(
            &installed_directory,
            DirectoryMetadataRevision {
                mode: *mode,
                access: *access,
                modified: *modified,
            },
        )
        .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
        applied.detached_metadata.pop();
        applied.installed.push(InstalledWorkspaceObject {
            relative: relative.clone(),
            expected_after: expected_after.clone(),
            source: InstalledWorkspaceSource::Metadata(original),
        });
        current.remove_subtree(relative);
        current.insert_subtree(&expected_after);
        current.recompute_workspace_links();
        ensure_workspace_state(
            workspace,
            &current,
            &volatile_directories,
            protect_workspace_metadata,
        )?;
    }

    restore_directory_times(&applied.directory_times)?;
    ensure_workspace_state(
        workspace,
        &current,
        &volatile_directories,
        protect_workspace_metadata,
    )?;
    Ok(())
}

fn operation_parent_directories(
    operations: &[WorkspaceOperation],
) -> std::collections::BTreeSet<PathBuf> {
    operations
        .iter()
        .filter_map(|operation| match operation {
            WorkspaceOperation::Delete(relative)
            | WorkspaceOperation::Replace(relative)
            | WorkspaceOperation::SetMetadata { relative, .. } => relative
                .parent()
                .filter(|path| !path.as_os_str().is_empty()),
        })
        .map(Path::to_path_buf)
        .collect()
}

fn ensure_workspace_state(
    workspace: &Path,
    expected: &TransactionTreeState,
    volatile_directories: &std::collections::BTreeSet<PathBuf>,
    protect_workspace_metadata: bool,
) -> Result<(), WorkspaceTransactionError> {
    let actual = TransactionTreeState::capture(workspace)?;
    if actual.matches_ignoring_directory_times(
        expected,
        volatile_directories,
        protect_workspace_metadata,
    ) {
        Ok(())
    } else {
        Err(WorkspaceTransactionError::Drift)
    }
}

fn check_transaction_cancellation(
    cancellation: &CancellationToken,
) -> Result<(), WorkspaceTransactionError> {
    if cancellation.is_cancelled() {
        Err(WorkspaceTransactionError::Cancelled)
    } else {
        Ok(())
    }
}

fn rollback_partial(
    applied: &AppliedWorkspaceOperations,
    protect_workspace_metadata: bool,
) -> Result<(), WorkspaceTransactionError> {
    if transaction_test_should_fail_rollback() {
        return Err(WorkspaceTransactionError::RollbackFailed);
    }
    let mut failed = false;
    let mut blocked_restore = std::collections::BTreeSet::new();
    for installed in applied.installed.iter().rev() {
        let current = TransactionTreeState::capture_from_dir(&applied.workspace_directory)?;
        let actual = current.subtree(&installed.relative);
        if actual.entries.is_empty() {
            failed = true;
            blocked_restore.insert(installed.relative.clone());
            continue;
        }
        if !actual.matches_ignoring_directory_times(
            &installed.expected_after,
            &std::collections::BTreeSet::new(),
            protect_workspace_metadata,
        ) {
            failed = true;
            continue;
        }
        transaction_test_pause(TransactionTestPoint::RollbackInstalledMove);
        let destination_directory = match installed.source {
            InstalledWorkspaceSource::Stage => &applied.stage_directory,
            InstalledWorkspaceSource::Metadata(_) => &applied.metadata_directory,
        };
        if let Some(parent) = installed.relative.parent()
            && !parent.as_os_str().is_empty()
            && destination_directory.create_dir_all(parent).is_err()
        {
            failed = true;
            continue;
        }
        if rename_noreplace(
            &applied.workspace_directory,
            &installed.relative,
            destination_directory,
            &installed.relative,
        )
        .is_err()
        {
            failed = true;
            continue;
        }
        let moved = match TransactionTreeState::capture_from_dir(destination_directory) {
            Ok(state) => state.subtree(&installed.relative),
            Err(_) => {
                blocked_restore.insert(installed.relative.clone());
                failed = true;
                continue;
            }
        };
        if !moved.matches_ignoring_directory_times(
            &installed.expected_after,
            &std::collections::BTreeSet::new(),
            protect_workspace_metadata,
        ) {
            let _ = rename_noreplace(
                destination_directory,
                &installed.relative,
                &applied.workspace_directory,
                &installed.relative,
            );
            blocked_restore.insert(installed.relative.clone());
            failed = true;
            continue;
        }
        if let InstalledWorkspaceSource::Metadata(original) = installed.source {
            let directory = match open_relative_directory_nofollow(
                &applied.metadata_directory,
                &installed.relative,
            ) {
                Ok(directory) => directory,
                Err(_) => {
                    failed = true;
                    continue;
                }
            };
            if restore_pinned_directory_metadata(&directory, original).is_err() {
                failed = true;
                continue;
            }
            if rename_noreplace(
                &applied.metadata_directory,
                &installed.relative,
                &applied.workspace_directory,
                &installed.relative,
            )
            .is_err()
            {
                failed = true;
            }
        }
    }
    for detached in applied.detached_metadata.iter().rev() {
        let current = TransactionTreeState::capture_from_dir(&applied.workspace_directory)?;
        if !current.subtree(&detached.relative).entries.is_empty() {
            failed = true;
            blocked_restore.insert(detached.relative.clone());
            continue;
        }
        let metadata = TransactionTreeState::capture_from_dir(&applied.metadata_directory)?;
        if !metadata
            .subtree(&detached.relative)
            .matches_ignoring_directory_times(
                &detached.expected_before,
                &std::collections::BTreeSet::new(),
                protect_workspace_metadata,
            )
        {
            failed = true;
            blocked_restore.insert(detached.relative.clone());
            continue;
        }
        if rename_noreplace(
            &applied.metadata_directory,
            &detached.relative,
            &applied.workspace_directory,
            &detached.relative,
        )
        .is_err()
        {
            failed = true;
        }
    }
    for moved in applied.moved.iter().rev() {
        if blocked_restore
            .iter()
            .any(|blocked| moved.relative == *blocked || moved.relative.starts_with(blocked))
        {
            failed = true;
            continue;
        }
        let current = TransactionTreeState::capture_from_dir(&applied.workspace_directory)?;
        if !current.subtree(&moved.relative).entries.is_empty() {
            failed = true;
            continue;
        }
        let backup = TransactionTreeState::capture_from_dir(&applied.backup_directory)?;
        if !backup
            .subtree(&moved.relative)
            .matches_ignoring_directory_times(
                &moved.expected_before,
                &std::collections::BTreeSet::new(),
                protect_workspace_metadata,
            )
        {
            failed = true;
            continue;
        }
        if rename_noreplace(
            &applied.backup_directory,
            &moved.relative,
            &applied.workspace_directory,
            &moved.relative,
        )
        .is_err()
        {
            failed = true;
        }
    }
    if restore_directory_times(&applied.directory_times).is_err() {
        failed = true;
    }
    if failed {
        Err(WorkspaceTransactionError::RollbackFailed)
    } else {
        Ok(())
    }
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
            WorkspaceOperation::Delete(relative)
            | WorkspaceOperation::Replace(relative)
            | WorkspaceOperation::SetMetadata { relative, .. } => {
                relative.parent().map(Path::to_path_buf)
            }
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
                open_relative_directory_nofollow(&workspace_directory, &path)
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

fn restore_directory_times(times: &[DirectoryTimes]) -> Result<(), WorkspaceTransactionError> {
    for entry in times {
        entry
            .directory
            .set_times(
                ".",
                Some(cap_fs_ext::SystemTimeSpec::Absolute(entry.access)),
                Some(cap_fs_ext::SystemTimeSpec::Absolute(entry.modified)),
            )
            .map_err(|_| WorkspaceTransactionError::CapabilityNotSupported)?;
    }
    Ok(())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FdTableReady {
    Ready,
    Exited,
    Cancelled,
    TimedOut,
    Failed,
}

enum LaunchGateError {
    Cancelled,
    TimedOut,
    Unavailable,
}

fn acquire_child_launch_gate(
    started: Instant,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<MutexGuard<'static, ()>, LaunchGateError> {
    loop {
        if cancellation.is_cancelled() {
            return Err(LaunchGateError::Cancelled);
        }
        if started.elapsed() >= timeout {
            return Err(LaunchGateError::TimedOut);
        }
        match CHILD_LAUNCH_GATE.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(_)) => return Err(LaunchGateError::Unavailable),
            Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn wait_for_fd_table_ready(
    pid: libc::pid_t,
    fd: RawFd,
    started: Instant,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> FdTableReady {
    loop {
        let mut status = 0;
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if waited == pid {
            return FdTableReady::Exited;
        }
        if waited < 0 && unsafe { *libc::__errno_location() } != libc::EINTR {
            return FdTableReady::Failed;
        }
        if cancellation.is_cancelled() {
            kill_process_group(pid);
            wait_for_exit(pid);
            return FdTableReady::Cancelled;
        }
        if started.elapsed() >= timeout {
            kill_process_group(pid);
            wait_for_exit(pid);
            return FdTableReady::TimedOut;
        }
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let polled = unsafe { libc::poll(&mut poll_fd, 1, 10) };
        if polled < 0 {
            if unsafe { *libc::__errno_location() } == libc::EINTR {
                continue;
            }
            kill_process_group(pid);
            wait_for_exit(pid);
            return FdTableReady::Failed;
        }
        if polled == 0 || poll_fd.revents & libc::POLLIN == 0 {
            continue;
        }
        let mut value = [0u8; 1];
        let read = unsafe { libc::read(fd, value.as_mut_ptr().cast(), value.len()) };
        if read == 1 && value[0] == 1 {
            return FdTableReady::Ready;
        }
        kill_process_group(pid);
        wait_for_exit(pid);
        return FdTableReady::Failed;
    }
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
    let context = unsafe { &mut *(argument.cast::<ChildContext>()) };
    // Close inherited host FDs before another sandbox invocation is allowed to clone.
    close_all_extra_fds_except(&context.preserved_fds, context.fd_fallback_limit);
    let ready = [1u8; 1];
    if unsafe {
        libc::write(
            context.fd_table_ready_write,
            ready.as_ptr().cast(),
            ready.len(),
        )
    } != 1
    {
        return 126;
    }
    close_fd(context.fd_table_ready_write);
    let failure = unsafe { child_main_inner(context) };
    if failure != 0 {
        child_fail(context.status_write, failure);
    }
    126
}

// After cloning a multithreaded host, this path must not allocate, acquire process-wide locks,
// or call glibc setxid wrappers. The parent precomputes dynamic data; the child only issues raw
// syscalls and traverses immutable memory until execve or fixed-size failure reporting.
unsafe fn child_main_inner(context: &ChildContext) -> u8 {
    if !wait_for_namespace_map(context.ready_read) {
        return CHILD_SETUP_CAPABILITY;
    }
    close_fd(context.ready_read);
    if unsafe { libc::dup2(context.stdout_write, libc::STDOUT_FILENO) } < 0
        || unsafe { libc::dup2(context.stderr_write, libc::STDERR_FILENO) } < 0
    {
        return CHILD_SETUP_UNAVAILABLE;
    }
    if context.stdout_write != libc::STDOUT_FILENO {
        close_fd(context.stdout_write);
    }
    if context.stderr_write != libc::STDERR_FILENO {
        close_fd(context.stderr_write);
    }
    if unsafe { libc::setpgid(0, 0) } != 0 {
        return CHILD_SETUP_UNAVAILABLE;
    }
    if unsafe { libc::syscall(libc::SYS_setresgid, 0, 0, 0) } != 0
        || unsafe { libc::syscall(libc::SYS_setresuid, 0, 0, 0) } != 0
    {
        return CHILD_SETUP_CAPABILITY;
    }
    for operation in &context.filesystem_operations {
        if let Err(error) = execute_child_filesystem_operation(operation) {
            return error;
        }
    }
    if unsafe { libc::chdir(context.cwd.as_ptr()) } != 0 {
        return CHILD_SETUP_UNAVAILABLE;
    }
    if drop_linux_capabilities().is_err()
        || install_landlock(&context.landlock_rules).is_err()
        || install_seccomp_filter(&context.seccomp_filter).is_err()
    {
        return CHILD_SETUP_CAPABILITY;
    }
    close_all_extra_fds(context.fd_fallback_limit);
    unsafe {
        libc::execve(
            context.executable.as_ptr(),
            context.argv_pointers.as_ptr(),
            context.env_pointers.as_ptr(),
        );
    }
    CHILD_SETUP_UNAVAILABLE
}

fn prepare_child_filesystem(
    prepared: &PreparedCommand,
) -> Result<Vec<ChildFilesystemOperation>, LinuxSandboxError> {
    let mut operations = vec![mount_operation(
        None,
        Path::new("/"),
        None,
        libc::MS_REC | libc::MS_PRIVATE,
        None,
        CHILD_SETUP_CAPABILITY,
    )?];
    operations.push(mount_operation(
        Some("proc"),
        Path::new("/proc"),
        Some("proc"),
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
        CHILD_SETUP_CAPABILITY,
    )?);
    for (target, data, flags) in [
        (
            "/run",
            "size=64m,mode=1777",
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        ),
        ("/dev", "size=4m,mode=755", libc::MS_NOSUID | libc::MS_NODEV),
    ] {
        operations.push(mount_operation(
            Some("tmpfs"),
            Path::new(target),
            Some("tmpfs"),
            flags,
            Some(data),
            CHILD_SETUP_CAPABILITY,
        )?);
    }
    for path in ["/dev/null", "/dev/zero", "/dev/random", "/dev/urandom"] {
        operations.push(ChildFilesystemOperation::CreateFile {
            path: CString::new(path).map_err(|_| LinuxSandboxError::Unavailable)?,
            mode: 0o666,
            error: CHILD_SETUP_CAPABILITY,
        });
    }
    if prepared.filesystem == SandboxFilesystemMode::WorkspaceWrite {
        let overlay_error =
            || LinuxSandboxError::CapabilityNotSupported(LinuxCapability::OverlayFilesystem);
        let transaction = prepared.transaction.as_ref().ok_or({
            LinuxSandboxError::CapabilityNotSupported(LinuxCapability::OverlayFilesystem)
        })?;
        let options = format!(
            "lowerdir={},upperdir={},workdir={},userxattr",
            overlay_option_path(&prepared.workspace).map_err(|_| overlay_error())?,
            overlay_option_path(&transaction.upper).map_err(|_| overlay_error())?,
            overlay_option_path(&transaction.work).map_err(|_| overlay_error())?,
        );
        operations.push(
            mount_operation(
                Some("overlay"),
                &prepared.workspace,
                Some("overlay"),
                libc::MS_NOSUID | libc::MS_NODEV,
                Some(&options),
                CHILD_SETUP_OVERLAY_FILESYSTEM,
            )
            .map_err(|_| overlay_error())?,
        );
    }
    for (index, protected) in prepared.protected_paths.iter().enumerate() {
        let placeholder = CString::new(format!(
            "/run/.protected-{}-{index}",
            if protected.is_dir { "dir" } else { "file" }
        ))
        .map_err(|_| LinuxSandboxError::Unavailable)?;
        if protected.is_dir {
            operations.push(ChildFilesystemOperation::CreateDirectory {
                path: placeholder.clone(),
                mode: 0o700,
                allow_existing: false,
                error: CHILD_SETUP_CAPABILITY,
            });
        } else {
            operations.push(ChildFilesystemOperation::CreateFile {
                path: placeholder.clone(),
                mode: 0o600,
                error: CHILD_SETUP_CAPABILITY,
            });
        }
        operations.push(ChildFilesystemOperation::SetMode {
            path: placeholder.clone(),
            mode: 0,
            error: CHILD_SETUP_CAPABILITY,
        });
        operations.push(mount_operation_cstring_source(
            Some(placeholder),
            &protected.path,
            None,
            libc::MS_BIND | if protected.is_dir { libc::MS_REC } else { 0 },
            None,
            CHILD_SETUP_CAPABILITY,
        )?);
        operations.push(mount_operation(
            None,
            &protected.path,
            None,
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
            None,
            CHILD_SETUP_CAPABILITY,
        )?);
    }
    if prepared.filesystem == SandboxFilesystemMode::ReadOnly {
        let source =
            path_cstring(&prepared.workspace).map_err(|_| LinuxSandboxError::Unavailable)?;
        operations.push(mount_operation_cstring_source(
            Some(source),
            &prepared.workspace,
            None,
            libc::MS_BIND | libc::MS_REC,
            None,
            CHILD_SETUP_CAPABILITY,
        )?);
        operations.push(mount_operation(
            None,
            &prepared.workspace,
            None,
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_REC,
            None,
            CHILD_SETUP_CAPABILITY,
        )?);
    }
    operations.push(ChildFilesystemOperation::CreateDirectory {
        path: CString::new(SANDBOX_HOME).map_err(|_| LinuxSandboxError::Unavailable)?,
        mode: 0o700,
        allow_existing: true,
        error: CHILD_SETUP_UNAVAILABLE,
    });
    Ok(operations)
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

fn prepare_landlock_rules(
    prepared: &PreparedCommand,
) -> Result<Vec<LandlockRule>, LinuxSandboxError> {
    let workspace_access = match prepared.filesystem {
        SandboxFilesystemMode::ReadOnly => LANDLOCK_ACCESS_FS_READ | LANDLOCK_ACCESS_FS_EXECUTE,
        SandboxFilesystemMode::WorkspaceWrite => {
            LANDLOCK_ACCESS_FS_READ | LANDLOCK_ACCESS_FS_EXECUTE | LANDLOCK_ACCESS_FS_WRITE
        }
    };
    let runtime_read = LANDLOCK_ACCESS_FS_READ | LANDLOCK_ACCESS_FS_EXECUTE;
    let mut rules = vec![LandlockRule {
        path: path_cstring(&prepared.workspace).map_err(|_| LinuxSandboxError::Unavailable)?,
        allowed_access: workspace_access,
    }];
    for root in STANDARD_RUNTIME_ROOTS {
        if Path::new(root).is_dir() {
            rules.push(LandlockRule {
                path: CString::new(root).map_err(|_| LinuxSandboxError::Unavailable)?,
                allowed_access: runtime_read,
            });
        }
    }
    for path in &prepared.runtime_read_paths {
        rules.push(LandlockRule {
            path: path_cstring(path).map_err(|_| LinuxSandboxError::Unavailable)?,
            allowed_access: runtime_read_access(path).ok_or(
                LinuxSandboxError::CapabilityNotSupported(LinuxCapability::Landlock),
            )?,
        });
    }
    for root in ["/run", "/dev"] {
        if Path::new(root).is_dir() {
            rules.push(LandlockRule {
                path: CString::new(root).map_err(|_| LinuxSandboxError::Unavailable)?,
                allowed_access: LANDLOCK_ACCESS_FS_READ | LANDLOCK_ACCESS_FS_WRITE,
            });
        }
    }
    Ok(rules)
}

fn install_landlock(rules: &[LandlockRule]) -> Result<(), ()> {
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
    for rule in rules {
        let fd = unsafe { libc::open(rule.path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            close_fd(ruleset);
            return Err(());
        }
        let attributes = LandlockPathBeneathAttr {
            allowed_access: rule.allowed_access,
            parent_fd: fd,
        };
        let added = unsafe {
            libc::syscall(
                SYS_LANDLOCK_ADD_RULE,
                ruleset,
                LANDLOCK_RULE_TYPE_PATH_BENEATH,
                &attributes,
                0usize,
            )
        };
        close_fd(fd);
        if added != 0 {
            close_fd(ruleset);
            return Err(());
        }
    }
    if unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        close_fd(ruleset);
        return Err(());
    }
    let restricted = unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset, 0usize) };
    close_fd(ruleset);
    (restricted == 0).then_some(()).ok_or(())
}

fn prepare_seccomp_filter(network_denied: bool) -> Vec<SockFilter> {
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
    filter
}

fn install_seccomp_filter(filter: &[SockFilter]) -> Result<(), ()> {
    if unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(());
    }
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

fn open_file_limit() -> RawFd {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return 65_536;
    }
    limit.rlim_cur.min(RawFd::MAX as libc::rlim_t) as RawFd
}

fn close_all_extra_fds(limit: RawFd) {
    close_all_extra_fds_except(&[], limit);
}

fn mount_operation(
    source: Option<&str>,
    target: &Path,
    filesystem: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
    error: u8,
) -> Result<ChildFilesystemOperation, LinuxSandboxError> {
    let source = source
        .map(CString::new)
        .transpose()
        .map_err(|_| LinuxSandboxError::Unavailable)?;
    mount_operation_cstring_source(source, target, filesystem, flags, data, error)
}

fn mount_operation_cstring_source(
    source: Option<CString>,
    target: &Path,
    filesystem: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
    error: u8,
) -> Result<ChildFilesystemOperation, LinuxSandboxError> {
    Ok(ChildFilesystemOperation::Mount {
        source,
        target: path_cstring(target).map_err(|_| LinuxSandboxError::Unavailable)?,
        filesystem: filesystem
            .map(CString::new)
            .transpose()
            .map_err(|_| LinuxSandboxError::Unavailable)?,
        flags,
        data: data
            .map(CString::new)
            .transpose()
            .map_err(|_| LinuxSandboxError::Unavailable)?,
        error,
    })
}

fn execute_child_filesystem_operation(operation: &ChildFilesystemOperation) -> Result<(), u8> {
    match operation {
        ChildFilesystemOperation::Mount {
            source,
            target,
            filesystem,
            flags,
            data,
            error,
        } => {
            let result = unsafe {
                libc::mount(
                    source.as_ref().map_or(ptr::null(), |value| value.as_ptr()),
                    target.as_ptr(),
                    filesystem
                        .as_ref()
                        .map_or(ptr::null(), |value| value.as_ptr()),
                    *flags,
                    data.as_ref()
                        .map_or(ptr::null_mut(), |value| value.as_ptr().cast_mut().cast()),
                )
            };
            (result == 0).then_some(()).ok_or(*error)
        }
        ChildFilesystemOperation::CreateDirectory {
            path,
            mode,
            allow_existing,
            error,
        } => {
            let result = unsafe { libc::mkdir(path.as_ptr(), *mode) };
            if result == 0
                || *allow_existing && unsafe { *libc::__errno_location() } == libc::EEXIST
            {
                Ok(())
            } else {
                Err(*error)
            }
        }
        ChildFilesystemOperation::CreateFile { path, mode, error } => {
            let fd = unsafe {
                libc::open(
                    path.as_ptr(),
                    libc::O_CREAT | libc::O_TRUNC | libc::O_WRONLY | libc::O_CLOEXEC,
                    *mode,
                )
            };
            if fd < 0 {
                Err(*error)
            } else {
                close_fd(fd);
                Ok(())
            }
        }
        ChildFilesystemOperation::SetMode { path, mode, error } => {
            (unsafe { libc::chmod(path.as_ptr(), *mode) } == 0)
                .then_some(())
                .ok_or(*error)
        }
    }
}

fn close_all_extra_fds_except(preserved: &[RawFd], limit: RawFd) {
    let mut first = 3u32;
    let mut close_range_supported = true;
    for fd in preserved.iter().copied().filter(|fd| *fd >= 3) {
        let fd = fd as u32;
        if first < fd && unsafe { libc::syscall(libc::SYS_close_range, first, fd - 1, 0u32) } != 0 {
            close_range_supported = false;
            break;
        }
        first = fd.saturating_add(1);
    }
    if close_range_supported
        && unsafe { libc::syscall(libc::SYS_close_range, first, u32::MAX, 0u32) } == 0
    {
        return;
    }
    for fd in 3..limit {
        if !preserved.contains(&fd) {
            close_fd(fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_probe_uses_the_real_isolated_command_environment() {
        let workspace = tempfile::tempdir().expect("workspace");
        let backend = LinuxSandboxBackend::new();

        assert_eq!(
            backend.probe_executable(
                workspace.path(),
                "/bin/sh",
                &CommandEnvironmentPolicy::EvaluationIsolated,
            ),
            ExecutableAvailability::Available
        );
        assert_eq!(
            backend.probe_executable(
                workspace.path(),
                "/definitely-missing/singularity-evaluation-executable",
                &CommandEnvironmentPolicy::EvaluationIsolated,
            ),
            ExecutableAvailability::Unavailable
        );
    }

    #[test]
    fn declared_runtime_closure_applies_to_a_model_script_without_enabling_network() {
        let workspace = tempfile::tempdir().expect("workspace");
        let backend = LinuxSandboxBackend::new();
        if backend.probe_executable(
            workspace.path(),
            "node",
            &CommandEnvironmentPolicy::EvaluationIsolated,
        ) != ExecutableAvailability::Available
        {
            return;
        }
        fs::write(
            workspace.path().join("inventory.mjs"),
            "export const total = 24;\n",
        )
        .expect("module");
        fs::write(
            workspace.path().join("smoke_test.mjs"),
            "import assert from 'node:assert/strict';\n\
             import { total } from './inventory.mjs';\n\
             assert.equal(total, 24);\n",
        )
        .expect("smoke test");
        let mut request = CommandScriptRequest::agent_requested(
            "node_smoke",
            "node smoke_test.mjs",
            workspace.path().to_string_lossy(),
            workspace.path().to_string_lossy(),
        );
        request.environment = CommandEnvironmentPolicy::EvaluationIsolated;
        request.runtime_executables = vec!["node".to_string()];

        let result = backend.execute_script(&request);

        assert_eq!(
            result.execution_status,
            CommandExecutionStatus::Completed,
            "{result:?}"
        );
        assert_eq!(
            result.semantic_status,
            CommandSemanticStatus::Succeeded,
            "{result:?}"
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert_eq!(
            result.sandbox.enforcement,
            SandboxBackendEnforcement::Strict
        );
        assert_eq!(request.network.mode, SandboxNetworkMode::Denied);
    }

    #[test]
    fn declared_rustup_environment_applies_to_a_model_script() {
        let workspace = tempfile::tempdir().expect("workspace");
        let backend = LinuxSandboxBackend::new();
        if backend.probe_executable(
            workspace.path(),
            "cargo",
            &CommandEnvironmentPolicy::EvaluationIsolated,
        ) != ExecutableAvailability::Available
        {
            return;
        }
        let mut request = CommandScriptRequest::agent_requested(
            "cargo_version",
            "cargo --version",
            workspace.path().to_string_lossy(),
            workspace.path().to_string_lossy(),
        );
        request.environment = CommandEnvironmentPolicy::EvaluationIsolated;
        request.runtime_executables = vec!["cargo".to_string()];

        let result = backend.execute_script(&request);

        assert_eq!(
            result.execution_status,
            CommandExecutionStatus::Completed,
            "{result:?}"
        );
        assert_eq!(
            result.semantic_status,
            CommandSemanticStatus::Succeeded,
            "{result:?}"
        );
        assert_eq!(result.exit_code, Some(0), "{result:?}");
        assert_eq!(
            result.sandbox.enforcement,
            SandboxBackendEnforcement::Strict
        );
        assert_eq!(request.network.mode, SandboxNetworkMode::Denied);
    }

    #[test]
    fn fd_table_ready_wait_observes_child_exit_while_parent_holds_writer() {
        let (read_fd, write_fd) = pipe_cloexec().expect("fd table ready pipe");
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed");
        if child == 0 {
            unsafe { libc::_exit(0) };
        }
        let result = wait_for_fd_table_ready(
            child,
            read_fd,
            Instant::now(),
            Duration::from_secs(1),
            &CancellationToken::new(),
        );
        close_fd(read_fd);
        close_fd(write_fd);
        assert_eq!(result, FdTableReady::Exited);
    }

    #[test]
    fn filesystem_fact_uses_the_longest_mountpoint_for_workspace() {
        let mountinfo = concat!(
            "36 29 0:32 / / rw,relatime - overlay overlay rw\n",
            "37 36 0:33 / /mnt/c rw,relatime - 9p drvfs rw\n",
            "38 36 0:34 / /mnt/c/project rw,relatime - ext4 /dev/vdb rw\n",
        );
        assert_eq!(
            linux_filesystem_from_mountinfo(mountinfo, Path::new("/mnt/c/project/src")),
            Some("ext4".to_string())
        );
        assert_eq!(
            linux_filesystem_from_mountinfo(mountinfo, Path::new("/mnt/c/other")),
            Some("9p".to_string())
        );
        assert_eq!(
            linux_filesystem_from_mountinfo(mountinfo, Path::new("/tmp")),
            Some("overlay".to_string())
        );
        let escaped = "40 29 0:40 / /workspace\\040with\\040space rw - ext4 /dev/vdc rw\n";
        assert_eq!(
            linux_filesystem_from_mountinfo(escaped, Path::new("/workspace with space/project")),
            Some("ext4".to_string())
        );
    }

    fn request(id: &str, script: &str, workspace: &Path) -> CommandRequest {
        CommandRequest::project_verification(
            id,
            vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
            workspace.to_str().expect("workspace path"),
            workspace.to_str().expect("workspace path"),
        )
    }

    fn strict_backend() -> LinuxSandboxBackend {
        let backend = LinuxSandboxBackend::new();
        assert!(backend.probe().strict_ready(), "{:?}", backend.probe());
        backend
    }

    #[test]
    fn trusted_workspace_preparation_commits_git_metadata_but_ordinary_commands_cannot() {
        let _serial = TRANSACTION_TEST_SERIAL
            .lock()
            .expect("transaction test serial");
        let workspace = tempfile::tempdir().expect("workspace");
        let trusted = CommandRequest::trusted_workspace_preparation(
            "trusted_git_init",
            vec![
                "git".to_string(),
                "init".to_string(),
                "--quiet".to_string(),
                "source".to_string(),
            ],
            workspace.path().to_string_lossy(),
            workspace.path().to_string_lossy(),
        );
        let backend = strict_backend();

        let initialized = backend.execute(&trusted);

        assert_eq!(
            initialized.execution_status,
            CommandExecutionStatus::Completed,
            "{initialized:?}"
        );
        assert_eq!(
            initialized.semantic_status,
            CommandSemanticStatus::Succeeded
        );
        assert_eq!(initialized.workspace_mutation, WorkspaceMutation::Changed);
        assert_eq!(
            initialized.sandbox.enforcement,
            SandboxBackendEnforcement::Strict
        );
        assert!(!initialized.sandbox.local_process_fallback);
        assert!(workspace.path().join("source/.git").is_dir());

        let update = CommandRequest::trusted_workspace_preparation(
            "trusted_git_config",
            vec![
                "git".to_string(),
                "-C".to_string(),
                "source".to_string(),
                "config".to_string(),
                "user.name".to_string(),
                "trusted".to_string(),
            ],
            workspace.path().to_string_lossy(),
            workspace.path().to_string_lossy(),
        );
        let updated = backend.execute(&update);
        assert_eq!(
            updated.execution_status,
            CommandExecutionStatus::Completed,
            "{updated:?}"
        );
        assert_eq!(updated.semantic_status, CommandSemanticStatus::Succeeded);
        assert_eq!(updated.workspace_mutation, WorkspaceMutation::Changed);

        let config = workspace.path().join("source/.git/config");
        let before = fs::read(&config).expect("git config before ordinary request");
        let ordinary = CommandRequest::project_verification(
            "ordinary_git_config",
            vec![
                "git".to_string(),
                "-C".to_string(),
                "source".to_string(),
                "config".to_string(),
                "user.name".to_string(),
                "forbidden".to_string(),
            ],
            workspace.path().to_string_lossy(),
            workspace.path().to_string_lossy(),
        );

        let denied = backend.execute(&ordinary);

        assert_eq!(
            denied.execution_status,
            CommandExecutionStatus::PolicyDenied
        );
        assert_ne!(denied.workspace_mutation, WorkspaceMutation::Changed);
        assert_eq!(fs::read(config).expect("git config after denial"), before);
    }

    #[test]
    fn concurrent_replacement_before_workspace_move_is_rejected_without_overwrite() {
        let _serial = TRANSACTION_TEST_SERIAL
            .lock()
            .expect("transaction test serial");
        let workspace = tempfile::tempdir().expect("workspace");
        let target = workspace.path().join("target.txt");
        fs::write(&target, "before").expect("fixture");
        arm_transaction_test(TransactionTestPoint::WorkspaceMove, false);
        let workspace_path = workspace.path().to_path_buf();
        let worker = thread::spawn(move || {
            strict_backend().execute(&request(
                "transaction_concurrent_replacement",
                "printf transaction > target.txt",
                &workspace_path,
            ))
        });

        wait_for_transaction_test_point();
        fs::write(&target, "concurrent").expect("concurrent replacement");
        release_transaction_test_point();
        let result = worker.join().expect("transaction worker");
        clear_transaction_test();

        assert_eq!(
            result.execution_status,
            CommandExecutionStatus::PolicyDenied
        );
        assert!(result.stderr_preview.contains("transaction drift"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "concurrent");
    }

    #[test]
    fn parent_symlink_replacement_cannot_redirect_workspace_move() {
        let _serial = TRANSACTION_TEST_SERIAL
            .lock()
            .expect("transaction test serial");
        let workspace = tempfile::tempdir().expect("workspace");
        let nested = workspace.path().join("nested");
        let protected = workspace.path().join(".git");
        fs::create_dir(&nested).expect("nested");
        fs::create_dir(&protected).expect("protected");
        fs::write(nested.join("target.txt"), "before").expect("ordinary fixture");
        let protected_target = protected.join("target.txt");
        fs::write(&protected_target, "protected").expect("protected fixture");
        let protected_before = fs::metadata(&protected).expect("protected metadata");
        let protected_identity = (
            protected_before.dev(),
            protected_before.ino(),
            protected_before.mtime(),
            protected_before.mtime_nsec(),
            protected_before.ctime(),
            protected_before.ctime_nsec(),
        );
        arm_transaction_test(TransactionTestPoint::WorkspaceMove, false);
        let workspace_path = workspace.path().to_path_buf();
        let worker = thread::spawn(move || {
            strict_backend().execute(&request(
                "transaction_parent_symlink_replacement",
                "printf transaction > nested/target.txt",
                &workspace_path,
            ))
        });

        wait_for_transaction_test_point();
        let displaced = workspace.path().join("displaced");
        fs::rename(&nested, &displaced).expect("displace ordinary parent");
        std::os::unix::fs::symlink(".git", &nested).expect("replacement symlink");
        release_transaction_test_point();
        let result = worker.join().expect("transaction worker");
        clear_transaction_test();

        assert_eq!(result.execution_status, CommandExecutionStatus::Unsupported);
        assert!(
            result
                .stderr_preview
                .contains("capability_not_supported:workspace_transaction")
        );
        assert_eq!(fs::read_to_string(&protected_target).unwrap(), "protected");
        let protected_after = fs::metadata(&protected).expect("protected metadata");
        assert_eq!(
            (
                protected_after.dev(),
                protected_after.ino(),
                protected_after.mtime(),
                protected_after.mtime_nsec(),
                protected_after.ctime(),
                protected_after.ctime_nsec(),
            ),
            protected_identity,
            "the protected directory must have no rename side effect"
        );
        assert_eq!(
            fs::read_to_string(displaced.join("target.txt")).unwrap(),
            "before"
        );
    }

    #[test]
    fn cancellation_after_child_exit_rolls_back_in_progress_commit() {
        let _serial = TRANSACTION_TEST_SERIAL
            .lock()
            .expect("transaction test serial");
        let workspace = tempfile::tempdir().expect("workspace");
        let cancellation = CancellationToken::new();
        arm_transaction_test(TransactionTestPoint::FinalVerification, false);
        let workspace_path = workspace.path().to_path_buf();
        let worker_cancellation = cancellation.clone();
        let worker = thread::spawn(move || {
            strict_backend().execute_cancellable(
                &request(
                    "transaction_cancel_after_child",
                    "printf transaction > output.txt",
                    &workspace_path,
                ),
                &worker_cancellation,
            )
        });

        wait_for_transaction_test_point();
        cancellation.cancel();
        release_transaction_test_point();
        let result = worker.join().expect("transaction worker");
        clear_transaction_test();

        assert_eq!(result.execution_status, CommandExecutionStatus::Cancelled);
        assert_eq!(result.workspace_mutation, WorkspaceMutation::Unchanged);
        assert!(!workspace.path().join("output.txt").exists());
    }

    #[test]
    fn trusted_protected_child_write_does_not_make_failed_command_unknown() {
        let _serial = TRANSACTION_TEST_SERIAL
            .lock()
            .expect("transaction test serial");
        let owner = tempfile::tempdir().expect("owner");
        let workspace = owner.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let protected = workspace.join(".singularity");
        fs::create_dir(&protected).expect("protected directory");
        let runtime_state = protected.join("runtime.sqlite");
        fs::write(&runtime_state, b"before").expect("runtime state");
        arm_transaction_test(TransactionTestPoint::FinalVerification, false);
        let workspace_path = workspace.clone();
        let worker = thread::spawn(move || {
            strict_backend().execute(&request(
                "trusted_protected_child_write",
                "exit 1",
                &workspace_path,
            ))
        });

        wait_for_transaction_test_point();
        fs::write(&runtime_state, b"trusted runtime update").expect("trusted child write");
        release_transaction_test_point();
        let result = worker.join().expect("transaction worker");
        clear_transaction_test();

        assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
        assert_eq!(result.semantic_status, CommandSemanticStatus::ExitNonzero);
        assert_eq!(result.workspace_mutation, WorkspaceMutation::Unchanged);
        assert_eq!(
            result.sandbox.enforcement,
            SandboxBackendEnforcement::Strict
        );
        assert!(!result.sandbox.local_process_fallback);
        assert_eq!(
            fs::read_to_string(&runtime_state).unwrap(),
            "trusted runtime update"
        );
        assert!(
            fs::read_dir(&workspace)
                .expect("workspace entries")
                .filter_map(Result::ok)
                .all(|entry| entry.file_name() == ".singularity")
        );
        assert!(
            fs::read_dir(owner.path())
                .expect("owner entries")
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".singularity-workspace-commit-"))
        );
    }

    #[test]
    fn concurrent_creation_before_final_verification_rolls_back_transaction_only() {
        let _serial = TRANSACTION_TEST_SERIAL
            .lock()
            .expect("transaction test serial");
        let owner = tempfile::tempdir().expect("owner");
        let workspace = owner.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let target = workspace.join("target.txt");
        fs::write(&target, "before").expect("fixture");
        arm_transaction_test(TransactionTestPoint::FinalVerification, false);
        let workspace_path = workspace.clone();
        let worker = thread::spawn(move || {
            strict_backend().execute(&request(
                "transaction_concurrent_final_creation",
                "printf transaction > target.txt",
                &workspace_path,
            ))
        });

        wait_for_transaction_test_point();
        fs::write(workspace.join("concurrent.txt"), "concurrent").expect("concurrent creation");
        release_transaction_test_point();
        let result = worker.join().expect("transaction worker");
        clear_transaction_test();

        assert_eq!(
            result.execution_status,
            CommandExecutionStatus::PolicyDenied
        );
        assert!(result.stderr_preview.contains("transaction drift"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "before");
        assert_eq!(
            fs::read_to_string(workspace.join("concurrent.txt")).unwrap(),
            "concurrent"
        );
        assert!(
            fs::read_dir(owner.path())
                .expect("owner entries")
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".singularity-workspace-commit-")),
            "successful rollback must remove its commit area"
        );
    }

    #[test]
    fn concurrent_delete_of_installed_object_is_not_resurrected_by_rollback() {
        let _serial = TRANSACTION_TEST_SERIAL
            .lock()
            .expect("transaction test serial");
        let owner = tempfile::tempdir().expect("owner");
        let workspace = owner.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let target = workspace.join("target.txt");
        fs::write(&target, "before").expect("fixture");
        arm_transaction_test(TransactionTestPoint::FinalVerification, false);
        let workspace_path = workspace.clone();
        let worker = thread::spawn(move || {
            strict_backend().execute(&request(
                "transaction_concurrent_delete",
                "printf transaction > target.txt",
                &workspace_path,
            ))
        });

        wait_for_transaction_test_point();
        fs::remove_file(&target).expect("concurrent delete");
        release_transaction_test_point();
        let result = worker.join().expect("transaction worker");
        clear_transaction_test();

        assert_eq!(
            result.execution_status,
            CommandExecutionStatus::BackendError
        );
        assert!(result.stderr_preview.contains("rollback failed"));
        assert!(
            !target.exists(),
            "rollback must not resurrect a concurrently deleted path"
        );
        let recovery = fs::read_dir(owner.path())
            .expect("recovery parent")
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".singularity-workspace-commit-")
            })
            .expect("preserved recovery area");
        assert_eq!(
            fs::read_to_string(recovery.path().join("backup/target.txt")).unwrap(),
            "before"
        );
    }

    #[test]
    fn concurrent_replacement_after_rollback_check_preserves_new_object() {
        let _serial = TRANSACTION_TEST_SERIAL
            .lock()
            .expect("transaction test serial");
        let owner = tempfile::tempdir().expect("owner");
        let workspace = owner.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        let target = workspace.join("target.txt");
        fs::write(&target, "before").expect("fixture");
        arm_transaction_test_sequence(
            &[
                TransactionTestPoint::FinalVerification,
                TransactionTestPoint::RollbackInstalledMove,
            ],
            false,
        );
        let workspace_path = workspace.clone();
        let worker = thread::spawn(move || {
            strict_backend().execute(&request(
                "transaction_rollback_replacement",
                "printf transaction > target.txt",
                &workspace_path,
            ))
        });

        wait_for_transaction_test_point();
        fs::write(workspace.join("concurrent.txt"), "concurrent").expect("final drift");
        release_transaction_test_point();

        wait_for_transaction_test_point();
        fs::remove_file(&target).expect("remove installed transaction object");
        fs::write(&target, "replacement").expect("install concurrent replacement");
        release_transaction_test_point();

        let result = worker.join().expect("transaction worker");
        clear_transaction_test();

        assert_eq!(
            result.execution_status,
            CommandExecutionStatus::BackendError
        );
        assert!(result.stderr_preview.contains("rollback failed"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "replacement");
        assert_eq!(
            fs::read_to_string(workspace.join("concurrent.txt")).unwrap(),
            "concurrent"
        );
        let recovery = fs::read_dir(owner.path())
            .expect("recovery parent")
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".singularity-workspace-commit-")
            })
            .expect("preserved recovery area");
        assert_eq!(
            fs::read_to_string(recovery.path().join("backup/target.txt")).unwrap(),
            "before"
        );
    }

    #[test]
    fn concurrent_directory_replacement_before_metadata_commit_is_not_modified() {
        let _serial = TRANSACTION_TEST_SERIAL
            .lock()
            .expect("transaction test serial");
        let owner = tempfile::tempdir().expect("owner");
        let workspace = owner.path().join("workspace");
        let target = workspace.join("target");
        fs::create_dir_all(&target).expect("target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
            .expect("target permissions");
        fs::write(target.join("original.txt"), "original").expect("original fixture");
        arm_transaction_test(TransactionTestPoint::MetadataMutation, false);
        let workspace_path = workspace.clone();
        let worker = thread::spawn(move || {
            strict_backend().execute(&request(
                "transaction_concurrent_metadata_replacement",
                "chmod 700 target",
                &workspace_path,
            ))
        });

        wait_for_transaction_test_point();
        let displaced = workspace.join("displaced");
        fs::rename(&target, &displaced).expect("displace original directory");
        fs::create_dir(&target).expect("replacement directory");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
            .expect("replacement permissions");
        fs::write(target.join("replacement.txt"), "concurrent").expect("replacement fixture");
        release_transaction_test_point();
        let result = worker.join().expect("transaction worker");
        clear_transaction_test();

        assert_eq!(
            result.execution_status,
            CommandExecutionStatus::PolicyDenied
        );
        assert!(result.stderr_preview.contains("transaction drift"));
        assert_eq!(
            fs::metadata(&target)
                .expect("replacement metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "the transaction must not chmod a concurrently installed directory"
        );
        assert_eq!(
            fs::read_to_string(target.join("replacement.txt")).unwrap(),
            "concurrent"
        );
        assert_eq!(
            fs::read_to_string(displaced.join("original.txt")).unwrap(),
            "original"
        );
    }

    #[test]
    fn concurrent_directory_replacement_after_metadata_install_is_not_modified() {
        let _serial = TRANSACTION_TEST_SERIAL
            .lock()
            .expect("transaction test serial");
        let owner = tempfile::tempdir().expect("owner");
        let workspace = owner.path().join("workspace");
        let target = workspace.join("target");
        fs::create_dir_all(&target).expect("target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
            .expect("target permissions");
        fs::write(target.join("original.txt"), "original").expect("original fixture");
        arm_transaction_test(TransactionTestPoint::InstalledMetadataMutation, false);
        let workspace_path = workspace.clone();
        let worker = thread::spawn(move || {
            strict_backend().execute(&request(
                "transaction_installed_metadata_replacement",
                "chmod 700 target",
                &workspace_path,
            ))
        });

        wait_for_transaction_test_point();
        let displaced = workspace.join("displaced");
        fs::rename(&target, &displaced).expect("displace installed directory");
        fs::create_dir(&target).expect("replacement directory");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))
            .expect("replacement permissions");
        fs::write(target.join("replacement.txt"), "concurrent").expect("replacement fixture");
        release_transaction_test_point();
        let result = worker.join().expect("transaction worker");
        clear_transaction_test();

        assert_eq!(
            result.execution_status,
            CommandExecutionStatus::BackendError
        );
        assert!(result.stderr_preview.contains("rollback failed"));
        assert_eq!(
            fs::metadata(&target)
                .expect("replacement metadata")
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "pinned metadata mutation must not chmod a concurrently installed directory"
        );
        assert_eq!(
            fs::read_to_string(target.join("replacement.txt")).unwrap(),
            "concurrent"
        );
        assert_eq!(
            fs::metadata(&displaced)
                .expect("displaced metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn rollback_failure_is_typed_and_preserves_recovery_backup() {
        let _serial = TRANSACTION_TEST_SERIAL
            .lock()
            .expect("transaction test serial");
        let owner = tempfile::tempdir().expect("owner");
        let workspace = owner.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace");
        fs::write(workspace.join("target.txt"), "before").expect("fixture");
        arm_transaction_test(TransactionTestPoint::FinalVerification, true);
        let workspace_path = workspace.clone();
        let worker = thread::spawn(move || {
            strict_backend().execute(&request(
                "transaction_rollback_failure",
                "printf transaction > target.txt",
                &workspace_path,
            ))
        });

        wait_for_transaction_test_point();
        fs::write(workspace.join("concurrent.txt"), "concurrent").expect("concurrent fixture");
        release_transaction_test_point();
        let result = worker.join().expect("transaction worker");
        clear_transaction_test();

        assert_eq!(
            result.execution_status,
            CommandExecutionStatus::BackendError
        );
        assert!(result.stderr_preview.contains("rollback failed"));
        assert_eq!(
            fs::read_to_string(workspace.join("concurrent.txt")).unwrap(),
            "concurrent"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("target.txt")).unwrap(),
            "transaction"
        );
        let recovery = fs::read_dir(owner.path())
            .expect("recovery parent")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".singularity-workspace-commit-"))
            })
            .expect("preserved recovery directory");
        assert_eq!(
            fs::read_to_string(recovery.join("backup/target.txt")).unwrap(),
            "before"
        );
    }
}
