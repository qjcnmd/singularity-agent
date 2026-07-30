use crate::path_normalization::canonicalize_path_allow_missing;
#[cfg(test)]
use crate::path_normalization::lexical_path_key;
use anyhow::Context;
use anyhow::Result;
#[cfg(test)]
use std::cell::RefCell;
use std::error::Error;
use std::ffi::{OsStr, c_void};
use std::fmt;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
    NtCreateFile,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, RtlNtStatusToDosError, STATUS_SUCCESS, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileCaseSensitiveInfo, FileIdInfo, GetFileInformationByHandleEx, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Kernel::OBJ_CASE_INSENSITIVE;
use windows_sys::Win32::System::SystemServices::FILE_CS_FLAG_CASE_SENSITIVE_DIR;
use windows_sys::Win32::System::WindowsProgramming::FILE_CASE_SENSITIVE_INFO;

pub(crate) const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// Typed failures while admitting Windows paths into case-insensitive sandbox state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtectedMetadataError {
    NestedGitMarkerUnsupported {
        path: PathBuf,
        ancestor_git_marker: PathBuf,
    },
    CaseSensitiveDirectoryUnsupported {
        path: PathBuf,
    },
    CaseSensitivityQueryFailed {
        path: PathBuf,
        code: u32,
    },
    ReparseTargetUnsupported {
        path: PathBuf,
    },
}

impl fmt::Display for ProtectedMetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NestedGitMarkerUnsupported {
                path,
                ancestor_git_marker,
            } => write!(
                f,
                "unsupported_nested_git_marker: {} (ancestor={})",
                path.display(),
                ancestor_git_marker.display()
            ),
            Self::CaseSensitiveDirectoryUnsupported { path } => write!(
                f,
                "unsupported_case_sensitive_directory: {}",
                path.display()
            ),
            Self::CaseSensitivityQueryFailed { path, code } => write!(
                f,
                "case_sensitivity_query_failed: {} (code={code})",
                path.display()
            ),
            Self::ReparseTargetUnsupported { path } => {
                write!(f, "unsupported_reparse_acl_target: {}", path.display())
            }
        }
    }
}

impl Error for ProtectedMetadataError {}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaseSensitivityTestOutcome {
    CaseSensitive,
    QueryFailed(u32),
}

#[cfg(test)]
thread_local! {
    static CASE_SENSITIVITY_TEST_OVERRIDE:
        RefCell<Option<(String, CaseSensitivityTestOutcome)>> = const { RefCell::new(None) };
}

/// Restores the previous thread-local case-sensitivity query seam when a test completes.
#[cfg(test)]
pub(crate) struct CaseSensitivityTestGuard {
    previous: Option<(String, CaseSensitivityTestOutcome)>,
}

#[cfg(test)]
impl Drop for CaseSensitivityTestGuard {
    fn drop(&mut self) {
        CASE_SENSITIVITY_TEST_OVERRIDE.with(|current| {
            current.replace(self.previous.take());
        });
    }
}

/// Overrides one path's handle-based case-sensitivity query on the current test thread.
#[cfg(test)]
pub(crate) fn override_case_sensitivity_for_test(
    path: &Path,
    outcome: CaseSensitivityTestOutcome,
) -> CaseSensitivityTestGuard {
    let key = lexical_path_key(&canonicalize_path_allow_missing(path));
    let previous =
        CASE_SENSITIVITY_TEST_OVERRIDE.with(|current| current.replace(Some((key, outcome))));
    CaseSensitivityTestGuard { previous }
}

#[cfg(test)]
fn case_sensitivity_test_override(path: &Path) -> Option<CaseSensitivityTestOutcome> {
    let key = lexical_path_key(&canonicalize_path_allow_missing(path));
    CASE_SENSITIVITY_TEST_OVERRIDE.with(|current| {
        current
            .borrow()
            .as_ref()
            .filter(|(override_key, _)| *override_key == key)
            .map(|(_, outcome)| *outcome)
    })
}

pub(crate) fn absolute_path_components(path: &Path) -> Result<(PathBuf, Vec<std::ffi::OsString>)> {
    let mut anchor = PathBuf::new();
    let mut descendants = Vec::new();
    let mut has_filesystem_prefix = false;
    let mut rooted = false;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) if !rooted && descendants.is_empty() => {
                if !matches!(
                    prefix.kind(),
                    Prefix::Disk(_)
                        | Prefix::VerbatimDisk(_)
                        | Prefix::UNC(_, _)
                        | Prefix::VerbatimUNC(_, _)
                ) {
                    anyhow::bail!(
                        "protected path must use a disk or UNC filesystem prefix: {}",
                        path.display()
                    );
                }
                anchor.push(component.as_os_str());
                has_filesystem_prefix = true;
            }
            Component::RootDir if has_filesystem_prefix && !rooted && descendants.is_empty() => {
                anchor.push(component.as_os_str());
                rooted = true;
            }
            Component::Normal(component) if rooted => descendants.push(component.to_os_string()),
            _ => anyhow::bail!(
                "protected path must be absolute and normalized: {}",
                path.display()
            ),
        }
    }
    if !has_filesystem_prefix || !rooted {
        anyhow::bail!("protected path must name a disk or UNC filesystem object");
    }
    Ok((anchor, descendants))
}

pub(crate) fn reject_case_sensitive_directory(flags: u32, path: &Path) -> Result<()> {
    if flags & FILE_CS_FLAG_CASE_SENSITIVE_DIR != 0 {
        return Err(anyhow::Error::new(
            ProtectedMetadataError::CaseSensitiveDirectoryUnsupported {
                path: path.to_path_buf(),
            },
        ));
    }
    Ok(())
}

pub(crate) fn ensure_case_insensitive_directory(file: &std::fs::File, path: &Path) -> Result<()> {
    #[cfg(test)]
    if let Some(outcome) = case_sensitivity_test_override(path) {
        return match outcome {
            CaseSensitivityTestOutcome::CaseSensitive => {
                reject_case_sensitive_directory(FILE_CS_FLAG_CASE_SENSITIVE_DIR, path)
            }
            CaseSensitivityTestOutcome::QueryFailed(code) => Err(anyhow::Error::new(
                ProtectedMetadataError::CaseSensitivityQueryFailed {
                    path: path.to_path_buf(),
                    code,
                },
            )),
        };
    }

    let mut information = FILE_CASE_SENSITIVE_INFO { Flags: 0 };
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileCaseSensitiveInfo,
            &mut information as *mut _ as *mut c_void,
            std::mem::size_of::<FILE_CASE_SENSITIVE_INFO>() as u32,
        )
    } == 0
    {
        let code = unsafe { GetLastError() };
        return Err(anyhow::Error::new(
            ProtectedMetadataError::CaseSensitivityQueryFailed {
                path: path.to_path_buf(),
                code,
            },
        ));
    }
    reject_case_sensitive_directory(information.Flags, path)
}

#[cfg(test)]
pub(crate) fn enable_case_sensitive_directory_for_test(path: &Path) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options
        .access_mode(windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    let directory = options.open(path)?;
    let information = FILE_CASE_SENSITIVE_INFO {
        Flags: FILE_CS_FLAG_CASE_SENSITIVE_DIR,
    };
    let enabled = unsafe {
        windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle(
            directory.as_raw_handle() as _,
            FileCaseSensitiveInfo,
            &information as *const _ as *const c_void,
            std::mem::size_of::<FILE_CASE_SENSITIVE_INFO>() as u32,
        ) != 0
    };
    if enabled {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(crate) fn validate_plain_directory(file: &std::fs::File, path: &Path) -> Result<()> {
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect pinned directory {}", path.display()))?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        anyhow::bail!("protected path component is not a plain directory");
    }
    ensure_case_insensitive_directory(file, path)
}

pub(crate) fn open_filesystem_root(anchor: &Path, desired_access: u32) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options
        .access_mode(desired_access | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options
        .open(anchor)
        .with_context(|| format!("open protected filesystem root {}", anchor.display()))?;
    validate_plain_directory(&file, anchor)?;
    Ok(file)
}

fn relative_name(name: &OsStr) -> Result<(Vec<u16>, UNICODE_STRING)> {
    let mut wide = name.encode_wide().collect::<Vec<_>>();
    if wide.is_empty() || wide.iter().any(|value| matches!(*value, 0 | 47 | 58 | 92)) {
        anyhow::bail!("invalid relative protected path component");
    }
    let byte_length = wide
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| anyhow::anyhow!("relative protected path component is too long"))?;
    let name = UNICODE_STRING {
        Length: byte_length,
        MaximumLength: byte_length,
        Buffer: wide.as_mut_ptr(),
    };
    Ok((wide, name))
}

/// Opens one component relative to a stable parent without following a reparse point.
pub(crate) fn nt_open_relative(
    parent: &std::fs::File,
    name: &OsStr,
    desired_access: u32,
    create_disposition: u32,
    create_options: u32,
) -> std::io::Result<(std::fs::File, usize)> {
    let (_wide, mut name) =
        relative_name(name).map_err(|error| std::io::Error::other(format!("{error:#}")))?;
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle() as HANDLE,
        ObjectName: &mut name,
        Attributes: OBJ_CASE_INSENSITIVE as u32,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle = INVALID_HANDLE_VALUE;
    let mut io_status = unsafe { std::mem::zeroed::<IO_STATUS_BLOCK>() };
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            &attributes,
            &mut io_status,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            create_disposition,
            create_options | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if status != STATUS_SUCCESS {
        if handle != 0 && handle != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(handle);
            }
        }
        return Err(std::io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    if handle == 0 || handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::other(
            "NtCreateFile succeeded without a valid handle",
        ));
    }
    let file = unsafe { std::fs::File::from_raw_handle(handle as _) };
    Ok((file, io_status.Information))
}

fn open_existing_directory_at(
    parent: &std::fs::File,
    name: &OsStr,
    path: &Path,
) -> Result<Option<std::fs::File>> {
    let file = match nt_open_relative(
        parent,
        name,
        FILE_LIST_DIRECTORY,
        FILE_OPEN,
        FILE_DIRECTORY_FILE,
    ) {
        Ok((file, _)) => file,
        Err(error)
            if matches!(
                error.raw_os_error().map(|code| code as u32),
                Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)
            ) =>
        {
            return Ok(None);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("open protected path parent {}", path.display()));
        }
    };
    validate_plain_directory(&file, path)?;
    Ok(Some(file))
}

/// Rejects a path whose existing parent chain uses NTFS per-directory case sensitivity.
pub fn ensure_case_insensitive_path_ancestors(path: &Path) -> Result<()> {
    let (anchor, descendants) = absolute_path_components(path)?;
    let mut current = open_filesystem_root(&anchor, FILE_LIST_DIRECTORY)?;
    let mut current_path = anchor;
    for component in descendants.iter().take(descendants.len().saturating_sub(1)) {
        current_path.push(component);
        let Some(next) = open_existing_directory_at(&current, component, &current_path)? else {
            return Ok(());
        };
        current = next;
    }
    Ok(())
}

/// Preflights an ACL path before a batch performs any SID, state, or filesystem side effect.
pub fn ensure_case_insensitive_acl_path(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => open_existing_acl_target(path, 0).map(drop),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure_case_insensitive_path_ancestors(path)
        }
        Err(error) => Err(error)
            .with_context(|| format!("inspect ACL path before side effects {}", path.display())),
    }
}

/// Resolves and validates the one identity used for both state locking and state-file I/O.
pub(crate) fn canonicalize_case_insensitive_state_path(path: &Path) -> Result<PathBuf> {
    let canonical = canonicalize_path_allow_missing(path);
    ensure_case_insensitive_acl_path(&canonical)?;
    Ok(canonical)
}

/// Rejects an existing directory if it or any parent uses per-directory case sensitivity.
pub(crate) fn ensure_case_insensitive_directory_path(path: &Path) -> Result<()> {
    let (anchor, descendants) = absolute_path_components(path)?;
    let mut current = open_filesystem_root(&anchor, FILE_LIST_DIRECTORY)?;
    let mut current_path = anchor;
    for component in descendants {
        current_path.push(&component);
        current = open_existing_directory_at(&current, &component, &current_path)?
            .ok_or_else(|| anyhow::anyhow!("case-sensitivity directory path is missing"))?;
    }
    Ok(())
}

/// Validates one already bounded directory without reopening profile ancestors.
pub(crate) fn ensure_case_insensitive_directory_direct(path: &Path) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options
        .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let directory = options
        .open(path)
        .with_context(|| format!("open validated workspace directory {}", path.display()))?;
    validate_plain_directory(&directory, path)
}

/// Opens an existing ACL target through verified, handle-relative, case-insensitive parents.
pub(crate) fn open_existing_acl_target(path: &Path, desired_access: u32) -> Result<std::fs::File> {
    let (anchor, descendants) = absolute_path_components(path)?;
    if descendants.is_empty() {
        return open_filesystem_root(&anchor, desired_access);
    }

    let mut current = open_filesystem_root(&anchor, FILE_LIST_DIRECTORY)?;
    let mut current_path = anchor;
    for component in &descendants[..descendants.len() - 1] {
        current_path.push(component);
        current = open_existing_directory_at(&current, component, &current_path)?
            .ok_or_else(|| std::io::Error::from_raw_os_error(ERROR_PATH_NOT_FOUND as i32))?;
    }

    let final_component = descendants
        .last()
        .expect("non-root path has a final component");
    let (file, _) = nt_open_relative(&current, final_component, desired_access, FILE_OPEN, 0)
        .with_context(|| format!("open ACL target {}", path.display()))?;

    let metadata = file
        .metadata()
        .with_context(|| format!("inspect pinned ACL target {}", path.display()))?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(anyhow::Error::new(
            ProtectedMetadataError::ReparseTargetUnsupported {
                path: path.to_path_buf(),
            },
        ));
    }
    if metadata.is_dir() {
        ensure_case_insensitive_directory(&file, path)?;
    }

    // Close the only check/open race before any ACL read or write occurs. The target handle is
    // already pinned, so a later pathname change cannot redirect the operation.
    ensure_case_insensitive_directory(&current, &current_path)?;
    Ok(file)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkspaceObjectIdentity {
    volume_serial: u64,
    file_id: [u8; 16],
}

/// Keeps a workspace parent and root bound to the same no-follow objects for one command.
///
/// The handles deliberately omit `FILE_SHARE_DELETE` and request no write-control capability;
/// the controller retains this lease until the elevated capture returns after Job Object cleanup.
/// Every visible-path verification is relative to the held parent handle.
#[derive(Debug)]
pub struct WorkspaceRootLease {
    chain: PinnedDirectoryChain,
}

impl WorkspaceRootLease {
    /// Opens and pins an existing plain workspace root with minimum directory-read access.
    pub fn acquire(root: &Path) -> Result<Self> {
        let (_, descendants) = absolute_path_components(root)?;
        if descendants.is_empty() {
            anyhow::bail!("workspace root must name a directory below a volume");
        }
        Ok(Self {
            chain: pin_directory_chain(root, FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)?,
        })
    }

    /// Verifies that both pinned objects and their visible names still refer to the admission set.
    pub fn verify(&self) -> Result<()> {
        revalidate_directory_chain(&self.chain)?;
        Ok(())
    }

    /// Borrows the no-follow root handle while this lease remains alive.
    pub fn root_handle(&self) -> &std::fs::File {
        &self
            .chain
            .ancestors
            .last()
            .expect("workspace root chain is non-empty")
            .handle
    }
}

fn workspace_object_identity(file: &std::fs::File) -> Result<WorkspaceObjectIdentity> {
    let mut information: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let queried = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            &mut information as *mut _ as *mut c_void,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if queried == 0 {
        return Err(std::io::Error::last_os_error()).context("workspace object identity query");
    }
    Ok(WorkspaceObjectIdentity {
        volume_serial: information.VolumeSerialNumber,
        file_id: information.FileId.Identifier,
    })
}

fn pin_directory_chain(root: &Path, desired_access: u32) -> Result<PinnedDirectoryChain> {
    let (anchor, descendants) = absolute_path_components(root)?;
    let anchor_handle = open_filesystem_root(&anchor, desired_access)?;
    let anchor_identity = workspace_object_identity(&anchor_handle)?;
    let mut ancestors = vec![PinnedWorkspaceAncestor {
        expected_path: anchor.clone(),
        component: None,
        handle: anchor_handle,
        identity: anchor_identity,
    }];
    let mut current_path = anchor;
    for component in descendants {
        let parent = ancestors
            .last()
            .expect("directory chain must retain its current parent");
        let (next, _) = nt_open_relative(
            &parent.handle,
            &component,
            desired_access,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
        )
        .with_context(|| {
            format!(
                "open workspace directory component {}",
                component.to_string_lossy()
            )
        })?;
        current_path.push(&component);
        validate_plain_directory(&next, &current_path)?;
        let identity = workspace_object_identity(&next)?;
        ancestors.push(PinnedWorkspaceAncestor {
            expected_path: current_path.clone(),
            component: Some(component),
            handle: next,
            identity,
        });
    }
    Ok(PinnedDirectoryChain { ancestors })
}

/// Opens a workspace root or descendant relative to an already pinned root handle.
///
/// `Ok(None)` means the lexical target is outside `root_path` and the caller may use its normal
/// opener. A target lexically inside the root is never reopened by pathname: every component is
/// opened from the previous handle with `FILE_OPEN_REPARSE_POINT`, and any reparse object is
/// rejected before the handle is returned.
pub fn open_pinned_workspace_path(
    root_handle: &std::fs::File,
    root_path: &Path,
    target: &Path,
    desired_access: u32,
) -> Result<Option<std::fs::File>> {
    let (root_anchor, root_descendants) = absolute_path_components(root_path)?;
    let (target_anchor, target_descendants) = absolute_path_components(target)?;
    if !root_anchor
        .to_string_lossy()
        .eq_ignore_ascii_case(&target_anchor.to_string_lossy())
        || target_descendants.len() < root_descendants.len()
        || !root_descendants
            .iter()
            .zip(&target_descendants)
            .all(|(left, right)| {
                left.to_string_lossy()
                    .eq_ignore_ascii_case(&right.to_string_lossy())
            })
    {
        return Ok(None);
    }

    let mut current = root_handle
        .try_clone()
        .context("duplicate pinned workspace root handle")?;
    validate_plain_directory(&current, root_path)?;
    let descendants = &target_descendants[root_descendants.len()..];
    if descendants.is_empty() {
        validate_pinned_target(&current, target)?;
        return Ok(Some(current));
    }
    let mut current_path = root_path.to_path_buf();
    for (index, component) in descendants.iter().enumerate() {
        let is_final = index + 1 == descendants.len();
        let access = if is_final {
            desired_access
        } else {
            FILE_LIST_DIRECTORY
        };
        let options = if is_final { 0 } else { FILE_DIRECTORY_FILE };
        let (next, _) = nt_open_relative(&current, component, access, FILE_OPEN, options)
            .with_context(|| format!("open pinned workspace component {}", target.display()))?;
        current_path.push(component);
        if !is_final {
            validate_plain_directory(&next, &current_path)?;
        }
        current = next;
    }
    validate_pinned_target(&current, target)?;
    Ok(Some(current))
}

/// Pins an existing protected target or the parent that proves its first missing component.
///
/// Every retained handle omits delete sharing and must be held through the protected-path ACL
/// setup boundary.
pub(crate) struct PinnedWorkspacePath {
    expected_path: PathBuf,
    chain: PinnedDirectoryChain,
    state: PinnedWorkspacePathState,
}

#[derive(Debug)]
struct PinnedDirectoryChain {
    ancestors: Vec<PinnedWorkspaceAncestor>,
}

#[derive(Debug)]
struct PinnedWorkspaceAncestor {
    expected_path: PathBuf,
    component: Option<std::ffi::OsString>,
    handle: std::fs::File,
    identity: WorkspaceObjectIdentity,
}

enum PinnedWorkspacePathState {
    Existing {
        component: Option<std::ffi::OsString>,
        handle: std::fs::File,
        identity: WorkspaceObjectIdentity,
    },
    Missing {
        component: std::ffi::OsString,
    },
}

fn open_workspace_identity_path(
    root_path: &Path,
    target: &Path,
) -> Result<Option<(PinnedDirectoryChain, PinnedWorkspacePathState)>> {
    let (root_anchor, root_descendants) = absolute_path_components(root_path)?;
    let (target_anchor, target_descendants) = absolute_path_components(target)?;
    if !root_anchor
        .to_string_lossy()
        .eq_ignore_ascii_case(&target_anchor.to_string_lossy())
        || target_descendants.len() < root_descendants.len()
        || !root_descendants
            .iter()
            .zip(&target_descendants)
            .all(|(left, right)| {
                left.to_string_lossy()
                    .eq_ignore_ascii_case(&right.to_string_lossy())
            })
    {
        return Ok(None);
    }

    let root_handle = open_filesystem_root(root_path, FILE_LIST_DIRECTORY)?;
    let root_identity = workspace_object_identity(&root_handle)?;
    let mut ancestors = vec![PinnedWorkspaceAncestor {
        expected_path: root_path.to_path_buf(),
        component: None,
        handle: root_handle,
        identity: root_identity,
    }];
    let descendants = &target_descendants[root_descendants.len()..];
    if descendants.is_empty() {
        let root = ancestors.first().expect("root ancestor was retained");
        validate_pinned_target(&root.handle, target)?;
        let handle = root.handle.try_clone()?;
        return Ok(Some((
            PinnedDirectoryChain { ancestors },
            PinnedWorkspacePathState::Existing {
                component: None,
                handle,
                identity: root_identity,
            },
        )));
    }
    let mut current_path = root_path.to_path_buf();
    for (index, component) in descendants.iter().enumerate() {
        let is_final = index + 1 == descendants.len();
        let options = if is_final { 0 } else { FILE_DIRECTORY_FILE };
        let parent = ancestors
            .last()
            .expect("root ancestor must remain available");
        let next = match nt_open_relative(&parent.handle, component, 0, FILE_OPEN, options) {
            Ok((next, _)) => next,
            Err(error)
                if matches!(
                    error.raw_os_error().map(|code| code as u32),
                    Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)
                ) =>
            {
                return Ok(Some((
                    PinnedDirectoryChain { ancestors },
                    PinnedWorkspacePathState::Missing {
                        component: component.clone(),
                    },
                )));
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("open protected workspace identity {}", target.display())
                });
            }
        };
        current_path.push(component);
        if !is_final {
            validate_plain_directory(&next, &current_path)?;
        }
        if is_final {
            validate_pinned_target(&next, target)?;
            let identity = workspace_object_identity(&next)?;
            return Ok(Some((
                PinnedDirectoryChain { ancestors },
                PinnedWorkspacePathState::Existing {
                    component: Some(component.clone()),
                    handle: next,
                    identity,
                },
            )));
        }
        let identity = workspace_object_identity(&next)?;
        ancestors.push(PinnedWorkspaceAncestor {
            expected_path: current_path.clone(),
            component: Some(component.clone()),
            handle: next,
            identity,
        });
    }
    unreachable!("a non-empty workspace descendant must produce a leaf state")
}

pub(crate) fn pin_existing_workspace_paths(
    root_path: &Path,
    targets: &[crate::AbsolutePathBuf],
) -> Result<Vec<PinnedWorkspacePath>> {
    let mut handles = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for target in targets {
        let target = target.as_path();
        let key = target.to_string_lossy().to_ascii_lowercase();
        if !seen.insert(key) {
            continue;
        }
        match open_workspace_identity_path(root_path, target) {
            Ok(Some((chain, state))) => {
                handles.push(PinnedWorkspacePath {
                    expected_path: dunce::simplified(target).to_path_buf(),
                    chain,
                    state,
                });
            }
            Ok(None) => {
                anyhow::bail!(
                    "protected path is outside the pinned workspace: {}",
                    target.display()
                );
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect protected workspace target {}", target.display())
                });
            }
        }
    }
    Ok(handles)
}

/// Revalidate that every pinned object still occupies the path admitted before ACL setup.
pub(crate) fn revalidate_pinned_workspace_paths(pins: &[PinnedWorkspacePath]) -> Result<()> {
    for pin in pins {
        let visible_parent = revalidate_pinned_ancestors(pin)?;
        match &pin.state {
            PinnedWorkspacePathState::Missing { component } => {
                match nt_open_relative(&visible_parent, component, 0, FILE_OPEN, 0) {
                    Err(error)
                        if matches!(
                            error.raw_os_error().map(|code| code as u32),
                            Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)
                        ) => {}
                    Ok(_) => {
                        anyhow::bail!(
                            "protected workspace target appeared during sandbox setup: {}",
                            pin.expected_path.display()
                        );
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "revalidate missing protected workspace target {}",
                                pin.expected_path.display()
                            )
                        });
                    }
                }
            }
            PinnedWorkspacePathState::Existing {
                component,
                handle,
                identity,
            } => {
                if workspace_object_identity(handle)? != *identity {
                    anyhow::bail!(
                        "protected workspace target handle identity changed: {}",
                        pin.expected_path.display()
                    );
                }
                let Some(component) = component else {
                    let visible_root = pin
                        .chain
                        .ancestors
                        .first()
                        .expect("root ancestor was retained");
                    if workspace_object_identity(&visible_parent)? != *identity
                        || workspace_object_identity(&visible_root.handle)? != *identity
                    {
                        anyhow::bail!(
                            "protected workspace root identity changed: {}",
                            pin.expected_path.display()
                        );
                    }
                    continue;
                };
                let (visible, _) = nt_open_relative(&visible_parent, component, 0, FILE_OPEN, 0)
                    .with_context(|| {
                        format!(
                            "revalidate protected workspace target {}",
                            pin.expected_path.display()
                        )
                    })?;
                validate_pinned_target(&visible, &pin.expected_path)?;
                if workspace_object_identity(&visible)? != *identity {
                    anyhow::bail!("protected workspace target identity changed during setup");
                }
            }
        }
    }
    Ok(())
}

fn revalidate_pinned_ancestors(pin: &PinnedWorkspacePath) -> Result<std::fs::File> {
    revalidate_directory_chain(&pin.chain)
}

fn revalidate_directory_chain(chain: &PinnedDirectoryChain) -> Result<std::fs::File> {
    let root = chain
        .ancestors
        .first()
        .ok_or_else(|| anyhow::anyhow!("workspace pin has no root ancestor"))?;
    if workspace_object_identity(&root.handle)? != root.identity {
        anyhow::bail!("workspace root handle identity changed");
    }
    let mut current = open_filesystem_root(&root.expected_path, FILE_LIST_DIRECTORY)?;
    if workspace_object_identity(&current)? != root.identity {
        anyhow::bail!("workspace root pathname identity changed");
    }
    validate_plain_directory(&current, &root.expected_path)?;
    for ancestor in chain.ancestors.iter().skip(1) {
        let component = ancestor
            .component
            .as_ref()
            .expect("non-root ancestor must have a relative component");
        let (next, _) = nt_open_relative(
            &current,
            component,
            FILE_LIST_DIRECTORY,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
        )
        .with_context(|| {
            format!(
                "revalidate protected workspace ancestor {}",
                ancestor.expected_path.display()
            )
        })?;
        validate_plain_directory(&next, &ancestor.expected_path)?;
        if workspace_object_identity(&ancestor.handle)? != ancestor.identity
            || workspace_object_identity(&next)? != ancestor.identity
        {
            anyhow::bail!(
                "protected workspace ancestor identity changed: {}",
                ancestor.expected_path.display()
            );
        }
        current = next;
    }
    Ok(current)
}

/// Revalidates objects that existed before setup and reports whether any target was absent.
///
/// A missing target may be materialized by deny-ACL setup, so callers must run another bounded
/// setup pass and pin that new object before starting the child.
pub(crate) fn revalidate_existing_pinned_workspace_paths(
    pins: &[PinnedWorkspacePath],
) -> Result<bool> {
    let mut included_missing = false;
    for pin in pins {
        match &pin.state {
            PinnedWorkspacePathState::Existing { .. } => {
                revalidate_pinned_workspace_paths(std::slice::from_ref(pin))?;
            }
            PinnedWorkspacePathState::Missing { .. } => {
                revalidate_pinned_workspace_paths(std::slice::from_ref(pin))?;
                included_missing = true;
            }
        }
    }
    Ok(included_missing)
}

fn validate_pinned_target(file: &std::fs::File, path: &Path) -> Result<()> {
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect pinned workspace target {}", path.display()))?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(anyhow::Error::new(
            ProtectedMetadataError::ReparseTargetUnsupported {
                path: path.to_path_buf(),
            },
        ));
    }
    if metadata.is_dir() {
        ensure_case_insensitive_directory(file, path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::CaseSensitivityTestOutcome;
    use super::ProtectedMetadataError;
    use super::WorkspaceRootLease;
    use super::enable_case_sensitive_directory_for_test;
    use super::ensure_case_insensitive_acl_path;
    use super::open_filesystem_root;
    use super::open_pinned_workspace_path;
    use super::override_case_sensitivity_for_test;
    use super::pin_existing_workspace_paths;
    use super::revalidate_pinned_workspace_paths;
    use crate::AbsolutePathBuf;
    use std::fs;
    use windows_sys::Win32::Storage::FileSystem::FILE_LIST_DIRECTORY;

    #[test]
    #[ignore = "requires permission to enable an NTFS per-directory case-sensitive flag"]
    fn real_ntfs_case_sensitive_directory_is_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        enable_case_sensitive_directory_for_test(temp.path())
            .expect("enable real NTFS case-sensitive directory fixture");
        let protected = temp.path().join("protected");

        let error = ensure_case_insensitive_acl_path(&protected)
            .expect_err("real case-sensitive directory must fail closed");

        assert_eq!(
            error.downcast_ref::<ProtectedMetadataError>(),
            Some(&ProtectedMetadataError::CaseSensitiveDirectoryUnsupported {
                path: temp.path().to_path_buf(),
            })
        );
    }

    #[test]
    fn pinned_workspace_rejects_case_sensitive_intermediate_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("root");
        let nested = root.join("nested");
        let leaf = nested.join("leaf.txt");
        fs::create_dir(&root).expect("root");
        fs::create_dir(&nested).expect("nested");
        fs::write(&leaf, b"payload").expect("leaf");

        let root_handle = open_filesystem_root(&root, FILE_LIST_DIRECTORY).expect("root handle");
        let _override =
            override_case_sensitivity_for_test(&nested, CaseSensitivityTestOutcome::CaseSensitive);
        let error = open_pinned_workspace_path(&root_handle, &root, &leaf, 0)
            .expect_err("case-sensitive intermediate directory must fail closed");
        assert_eq!(
            error.downcast_ref::<ProtectedMetadataError>(),
            Some(&ProtectedMetadataError::CaseSensitiveDirectoryUnsupported { path: nested })
        );
    }

    #[test]
    fn pinned_protected_target_rejects_a_path_replacement_at_setup_boundary() {
        let temp = tempfile::tempdir().expect("tempdir");
        let protected = temp.path().join(".env");
        let replacement = temp.path().join(".env.replacement");
        fs::write(&protected, b"secret").expect("protected file");
        let target =
            AbsolutePathBuf::from_absolute_path_checked(&protected).expect("absolute target");

        let pins = pin_existing_workspace_paths(temp.path(), &[target]).expect("pin target");
        assert_eq!(
            pins.len(),
            1,
            "the existing protected object must be pinned"
        );
        fs::rename(&protected, &replacement).expect("adversarial replacement");
        assert!(
            revalidate_pinned_workspace_paths(&pins).is_err(),
            "the setup boundary must reject a replaced protected object"
        );
    }

    #[test]
    fn pinned_missing_protected_target_rejects_creation_at_setup_boundary() {
        let temp = tempfile::tempdir().expect("tempdir");
        let protected = temp.path().join("missing").join(".env");
        let target =
            AbsolutePathBuf::from_absolute_path_checked(&protected).expect("absolute target");

        let pins = pin_existing_workspace_paths(temp.path(), &[target]).expect("pin absence");
        assert_eq!(
            pins.len(),
            1,
            "missing protected paths must retain an absence proof"
        );
        fs::create_dir(temp.path().join("missing")).expect("adversarial parent creation");
        assert!(
            revalidate_pinned_workspace_paths(&pins).is_err(),
            "the setup boundary must reject a newly materialized protected path chain"
        );
    }

    #[test]
    fn workspace_root_lease_rejects_an_intermediate_parent_replacement() {
        let temp = tempfile::tempdir().expect("tempdir");
        let parent = temp.path().join("parent");
        let root = parent.join("workspace");
        let displaced = temp.path().join("parent-displaced");
        fs::create_dir_all(&root).expect("workspace");
        let lease = WorkspaceRootLease::acquire(&root).expect("root lease");

        let replaced = fs::rename(&parent, &displaced).is_ok();
        if replaced {
            fs::create_dir(&parent).expect("replacement parent");
            fs::create_dir(&root).expect("replacement workspace");
            assert!(
                lease.verify().is_err(),
                "a same-content parent replacement must fail the root lease"
            );
        } else {
            lease.verify().expect("unchanged root lease");
        }
    }

    #[test]
    fn pinned_target_revalidates_every_retained_ancestor_before_leaf() {
        let temp = tempfile::tempdir().expect("tempdir");
        let parent = temp.path().join("parent");
        let workspace = parent.join("workspace");
        let protected = workspace.join("nested").join("secret.env");
        let displaced = workspace.join("nested-displaced");
        fs::create_dir_all(protected.parent().expect("nested parent")).expect("nested");
        fs::write(&protected, b"secret").expect("protected file");
        let target = AbsolutePathBuf::from_absolute_path_checked(&protected)
            .expect("absolute protected path");
        let pins = pin_existing_workspace_paths(&workspace, &[target]).expect("pin target");

        let replaced = fs::rename(protected.parent().expect("nested parent"), &displaced).is_ok();
        if replaced {
            fs::create_dir_all(protected.parent().expect("replacement nested parent"))
                .expect("replacement nested");
            fs::write(&protected, b"secret").expect("replacement protected file");
            assert!(
                revalidate_pinned_workspace_paths(&pins).is_err(),
                "a replaced ancestor must fail before the leaf is considered"
            );
        } else {
            revalidate_pinned_workspace_paths(&pins).expect("unchanged pinned target");
        }
    }
}
