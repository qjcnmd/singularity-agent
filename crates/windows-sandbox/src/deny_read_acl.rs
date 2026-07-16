use crate::acl::DenyReadAclFingerprint;
use crate::acl::add_deny_read_ace_with_ownership_before_set;
use crate::acl::add_deny_read_ace_with_ownership_to_handle_before_set;
use crate::acl::add_deny_write_ace_to_handle;
use crate::acl::revoke_deny_read_ace_with_fingerprint;
use crate::path_normalization::lexical_path_key;
use anyhow::Context;
use anyhow::Result;
use cap_std::fs::Dir;
use dunce::canonicalize;
use serde::Deserialize;
use serde::Serialize;
use singularity_core::PROTECTED_GIT_DIR_NAME;
use std::collections::HashSet;
use std::error::Error;
use std::ffi::{OsStr, c_void};
use std::fmt;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_DIR_NOT_EMPTY;
use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows_sys::Win32::Foundation::ERROR_PATH_NOT_FOUND;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
use windows_sys::Win32::Foundation::STATUS_SUCCESS;
use windows_sys::Win32::Foundation::UNICODE_STRING;
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_FLAG_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_FLAG_POSIX_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO_EX;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_LIST_DIRECTORY;
use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::FileCaseSensitiveInfo;
use windows_sys::Win32::Storage::FileSystem::FileDispositionInfoEx;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;
use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Kernel::OBJ_CASE_INSENSITIVE;
use windows_sys::Win32::System::SystemServices::FILE_CS_FLAG_CASE_SENSITIVE_DIR;
use windows_sys::Win32::System::WindowsProgramming::FILE_CASE_SENSITIVE_INFO;
use windows_sys::Win32::System::WindowsProgramming::FILE_CREATED;

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// Build the exact ACL paths that should receive a deny-read ACE.
///
/// We keep both the lexical policy path and, when it already exists, the
/// canonical target. The lexical path covers the path users configured and lets
/// missing exact denies be materialized later; the canonical path also covers
/// an existing reparse-point target so a sandbox cannot read the same object
/// through the resolved location.
pub fn plan_deny_read_acl_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut planned = Vec::new();
    let mut seen = HashSet::new();
    for path in paths {
        push_planned_path(&mut planned, &mut seen, path.to_path_buf());
        if std::fs::symlink_metadata(path).is_ok()
            && let Ok(canonical) = canonicalize(path)
        {
            push_planned_path(&mut planned, &mut seen, canonical);
        }
    }
    planned
}

fn push_planned_path(planned: &mut Vec<PathBuf>, seen: &mut HashSet<String>, path: PathBuf) {
    if seen.insert(lexical_path_key(&path)) {
        planned.push(path);
    }
}

fn is_reparse_point_attributes(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// A protected directory pinned to the exact object created or opened during materialization.
pub struct MaterializedDirectory {
    file: std::fs::File,
    created_by_runtime: bool,
}

impl MaterializedDirectory {
    /// Returns whether this operation created the final directory object.
    pub fn created_by_runtime(&self) -> bool {
        self.created_by_runtime
    }

    fn handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE
    }

    /// Applies deny-write through the pinned object rather than resolving its path again.
    ///
    /// # Safety
    /// Caller must ensure `psid` points to a valid SID.
    pub unsafe fn add_deny_write_ace(&self, psid: *mut c_void) -> Result<bool> {
        unsafe { add_deny_write_ace_to_handle(self.handle(), psid) }
    }

    unsafe fn add_deny_read_ace_with_ownership_before_set(
        &self,
        psid: *mut c_void,
        before_set: &mut dyn FnMut(&DenyReadAclFingerprint) -> Result<()>,
    ) -> Result<crate::acl::DenyAceAddResult> {
        unsafe {
            add_deny_read_ace_with_ownership_to_handle_before_set(self.handle(), psid, before_set)
        }
    }

    /// Removes this object only when this call created it and it is still empty.
    pub fn cleanup_if_empty(self) -> Result<()> {
        self.cleanup_if_empty_with_hook(|| {})
    }

    fn cleanup_if_empty_with_hook(self, before_delete: impl FnOnce()) -> Result<()> {
        if !self.created_by_runtime {
            return Ok(());
        }
        unsafe { remove_empty_runtime_sentinel_by_handle(&self.file, before_delete) }
    }
}

fn absolute_path_components(path: &Path) -> Result<(PathBuf, Vec<std::ffi::OsString>)> {
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
                        "deny path must use a disk or UNC filesystem prefix: {}",
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
                "deny path must be an absolute normalized path: {}",
                path.display()
            ),
        }
    }
    if !has_filesystem_prefix || !rooted || descendants.is_empty() {
        anyhow::bail!("deny path must name a directory below a filesystem root");
    }
    Ok((anchor, descendants))
}

fn reject_case_sensitive_directory(flags: u32, path: &Path) -> Result<()> {
    if flags & FILE_CS_FLAG_CASE_SENSITIVE_DIR != 0 {
        return Err(anyhow::Error::new(
            ProtectedMetadataError::CaseSensitiveDirectoryUnsupported {
                path: path.to_path_buf(),
            },
        ));
    }
    Ok(())
}

fn ensure_case_insensitive_directory(file: &std::fs::File, path: &Path) -> Result<()> {
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

fn validate_plain_directory(file: &std::fs::File, path: &Path) -> Result<()> {
    let metadata = file
        .metadata()
        .context("inspect pinned materialized deny directory")?;
    if !metadata.is_dir() || is_reparse_point_attributes(metadata.file_attributes()) {
        anyhow::bail!("materialized deny target is not a plain directory");
    }
    ensure_case_insensitive_directory(file, path)
}

fn open_filesystem_root(anchor: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options
        .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options
        .open(anchor)
        .with_context(|| format!("open deny path filesystem root {}", anchor.display()))?;
    validate_plain_directory(&file, anchor)?;
    Ok(file)
}

fn relative_name(name: &OsStr) -> Result<(Vec<u16>, UNICODE_STRING)> {
    let mut wide = name.encode_wide().collect::<Vec<_>>();
    if wide.is_empty() || wide.iter().any(|value| matches!(*value, 0 | 47 | 58 | 92)) {
        anyhow::bail!("invalid relative deny path component");
    }
    let byte_length = wide
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or_else(|| anyhow::anyhow!("relative deny path component is too long"))?;
    let name = UNICODE_STRING {
        Length: byte_length,
        MaximumLength: byte_length,
        Buffer: wide.as_mut_ptr(),
    };
    Ok((wide, name))
}

/// Opens one validated component relative to a stable parent kernel handle.
///
/// `NtCreateFile.RootDirectory` provides the Windows handle-relative primitive that Win32
/// `CreateDirectoryW` lacks. Delete sharing stays disabled so the parent or returned object cannot
/// be replaced before the next traversal, ACL, or cleanup operation.
fn nt_open_relative(
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

fn open_or_create_directory_at(
    parent: &std::fs::File,
    name: &OsStr,
    desired_access: u32,
    path: &Path,
) -> Result<(std::fs::File, bool)> {
    let (file, information) = nt_open_relative(
        parent,
        name,
        desired_access,
        FILE_OPEN_IF,
        FILE_DIRECTORY_FILE,
    )
    .with_context(|| {
        format!(
            "open or create deny path component {}",
            name.to_string_lossy()
        )
    })?;
    validate_plain_directory(&file, path)?;
    Ok((file, information == FILE_CREATED as usize))
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
                .with_context(|| format!("open existing deny path parent {}", path.display()));
        }
    };
    validate_plain_directory(&file, path)?;
    Ok(Some(file))
}

fn ancestor_has_git_marker(directory: &std::fs::File) -> Result<bool> {
    match nt_open_relative(
        directory,
        OsStr::new(PROTECTED_GIT_DIR_NAME),
        0,
        FILE_OPEN,
        0,
    ) {
        Ok(_) => Ok(true),
        Err(error)
            if matches!(
                error.raw_os_error().map(|code| code as u32),
                Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error).context("inspect ancestor Git marker"),
    }
}

/// Rejects a path whose existing parent chain uses NTFS per-directory case sensitivity.
///
/// The sandbox's path keys and Windows ACL opens are intentionally case-insensitive. Admitting a
/// case-sensitive parent would let distinct names collapse into one policy/state entry.
pub fn ensure_case_insensitive_path_ancestors(path: &Path) -> Result<()> {
    let (anchor, descendants) = absolute_path_components(path)?;
    let mut current = open_filesystem_root(&anchor)?;
    let mut current_path = anchor;
    for component in descendants.iter().take(descendants.len() - 1) {
        current_path.push(component);
        let Some(next) = open_existing_directory_at(&current, component, &current_path)? else {
            return Ok(());
        };
        current = next;
    }
    Ok(())
}

/// Creates or opens an absolute directory through pinned, no-follow native handles.
pub fn ensure_directory_materialized(path: &Path) -> Result<MaterializedDirectory> {
    ensure_directory_materialized_with_hook(path, || {})
}

fn ensure_directory_materialized_with_hook(
    path: &Path,
    before_final_create: impl FnOnce(),
) -> Result<MaterializedDirectory> {
    let (anchor, descendants) = absolute_path_components(path)?;
    let mut current = open_filesystem_root(&anchor)?;
    let mut current_path = anchor;
    let check_git_ancestors = is_missing_git_marker(path);
    let mut before_final_create = Some(before_final_create);
    let mut ancestor_git_marker = None;
    for (index, component) in descendants.iter().enumerate() {
        let is_final = index + 1 == descendants.len();
        if check_git_ancestors && !is_final && ancestor_has_git_marker(&current)? {
            ancestor_git_marker = Some(current_path.join(PROTECTED_GIT_DIR_NAME));
        }
        if is_final && let Some(ancestor_git_marker) = ancestor_git_marker {
            return Err(anyhow::Error::new(
                ProtectedMetadataError::NestedGitMarkerUnsupported {
                    path: path.to_path_buf(),
                    ancestor_git_marker,
                },
            ));
        }
        if is_final {
            before_final_create.take().expect("single final hook")();
            let final_path = current_path.join(component);
            let (file, created_by_runtime) = open_or_create_directory_at(
                &current,
                component,
                DELETE | READ_CONTROL | WRITE_DAC | FILE_LIST_DIRECTORY,
                &final_path,
            )?;
            ensure_case_insensitive_directory(&current, &current_path)?;
            return Ok(MaterializedDirectory {
                file,
                created_by_runtime,
            });
        }
        current_path.push(component);
        current =
            open_or_create_directory_at(&current, component, FILE_LIST_DIRECTORY, &current_path)?.0;
    }
    unreachable!("absolute path parser requires at least one descendant")
}

/// Deletes only the pinned object created by this runtime.
unsafe fn remove_empty_runtime_sentinel_by_handle(
    file: &std::fs::File,
    before_delete: impl FnOnce(),
) -> Result<()> {
    let handle = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    (|| -> Result<()> {
        let mut info = std::mem::zeroed();
        if GetFileInformationByHandle(handle, &mut info) == 0 {
            anyhow::bail!(
                "query runtime sentinel identity for safe cleanup failed: {}",
                GetLastError()
            );
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
            || is_reparse_point_attributes(info.dwFileAttributes)
        {
            anyhow::bail!("refusing to clean non-directory or reparse sentinel");
        }
        let directory = Dir::from_std_file(
            file.try_clone()
                .context("clone pinned runtime sentinel handle")?,
        );
        let mut entries = directory
            .entries()
            .context("inspect pinned runtime sentinel contents")?;
        if entries.next().is_some() {
            return Ok(());
        }
        before_delete();

        let disposition = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        };
        if SetFileInformationByHandle(
            handle,
            FileDispositionInfoEx,
            &disposition as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        ) == 0
        {
            let code = GetLastError();
            if code == ERROR_DIR_NOT_EMPTY {
                return Ok(());
            }
            anyhow::bail!("remove empty sentinel by handle failed: {code}");
        }
        Ok(())
    })()
}

fn is_missing_git_marker(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(PROTECTED_GIT_DIR_NAME))
}

/// Identifies a protected metadata path that cannot be safely materialized without changing
/// repository discovery semantics.
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
        }
    }
}

impl Error for ProtectedMetadataError {}

/// Materializes a missing protected path, rejecting a nested `.git` sentinel under an existing
/// ancestor repository so Git's ancestor discovery remains unchanged.
pub fn ensure_missing_protected_path_materialized(path: &Path) -> Result<MaterializedDirectory> {
    ensure_directory_materialized(path)
}

pub(crate) struct AppliedDenyReadAcls {
    pub(crate) enforced_paths: Vec<PathBuf>,
    pub(crate) newly_managed_paths: Vec<ManagedDenyReadAcl>,
}

/// A deny-read repair whose complete SID fingerprint was created by this runtime.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ManagedDenyReadAcl {
    pub(crate) path: PathBuf,
    pub(crate) fingerprint: DenyReadAclFingerprint,
}

/// Applies deny-read ACEs to explicit paths and returns every enforced path.
///
/// Missing paths are materialized as directories before the ACE is applied so a sandboxed command
/// cannot create a previously absent denied path and then read from it in the same run.
///
/// # Safety
/// Caller must pass a valid SID pointer for the sandbox principal being denied.
pub unsafe fn apply_deny_read_acls(paths: &[PathBuf], psid: *mut c_void) -> Result<Vec<PathBuf>> {
    Ok(unsafe { apply_deny_read_acls_with_ownership(paths, psid) }?.enforced_paths)
}

/// Applies deny-read ACEs and identifies only the paths whose managed ACE was added by this call.
///
/// Existing sufficient ACEs still enforce the requested boundary, but they are not claimed as
/// runtime-owned. If any path fails, only ACEs added by this call are revoked before the error is
/// returned.
///
/// # Safety
/// Caller must pass a valid SID pointer for the sandbox principal being denied.
pub(crate) unsafe fn apply_deny_read_acls_with_ownership(
    paths: &[PathBuf],
    psid: *mut c_void,
) -> Result<AppliedDenyReadAcls> {
    unsafe { apply_deny_read_acls_with_ownership_before_set(paths, psid, &mut |_| Ok(())) }
}

/// Applies deny-read ACLs after journaling every new runtime-owned fingerprint.
///
/// # Safety
/// Caller must pass a valid SID pointer for the sandbox principal being denied.
pub(crate) unsafe fn apply_deny_read_acls_with_ownership_before_set(
    paths: &[PathBuf],
    psid: *mut c_void,
    before_set: &mut dyn FnMut(&ManagedDenyReadAcl) -> Result<()>,
) -> Result<AppliedDenyReadAcls> {
    for path in paths {
        ensure_case_insensitive_path_ancestors(path)?;
    }
    let planned = plan_deny_read_acl_paths(paths);
    let mut applied = Vec::new();
    let mut seen = HashSet::new();
    let mut added_in_this_call: Vec<ManagedDenyReadAcl> = Vec::new();
    let mut pinned_materialized = Vec::new();
    for path in planned {
        let result = (|| -> Result<(Option<ManagedDenyReadAcl>, Option<MaterializedDirectory>)> {
            let materialized = match std::fs::symlink_metadata(&path) {
                Ok(_) => None,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    Some(ensure_missing_protected_path_materialized(&path)?)
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("inspect deny-read ACL target {}", path.display())
                    });
                }
            };
            let mut journal = |fingerprint: &DenyReadAclFingerprint| {
                before_set(&ManagedDenyReadAcl {
                    path: path.clone(),
                    fingerprint: fingerprint.clone(),
                })
            };
            let mutation = match &materialized {
                Some(materialized) => unsafe {
                    materialized.add_deny_read_ace_with_ownership_before_set(psid, &mut journal)
                },
                None => unsafe {
                    add_deny_read_ace_with_ownership_before_set(&path, psid, &mut journal)
                },
            }
            .with_context(|| format!("apply deny-read ACE to {}", path.display()))?;
            if mutation.runtime_owned {
                let fingerprint = mutation.fingerprint.ok_or_else(|| {
                    anyhow::anyhow!("managed deny-read mutation omitted its ownership fingerprint")
                })?;
                return Ok((
                    Some(ManagedDenyReadAcl {
                        path: path.clone(),
                        fingerprint,
                    }),
                    materialized,
                ));
            }
            Ok((None, materialized))
        })();
        let (managed, materialized) = match result {
            Ok(result) => result,
            Err(err) => {
                for added_path in &added_in_this_call {
                    if let Err(rollback_err) = unsafe {
                        revoke_deny_read_ace_with_fingerprint(
                            &added_path.path,
                            psid,
                            &added_path.fingerprint,
                        )
                    } {
                        return Err(err.context(format!(
                            "deny-read rollback failed for {}: {rollback_err}",
                            added_path.path.display()
                        )));
                    }
                }
                return Err(err);
            }
        };
        if let Some(managed) = managed {
            added_in_this_call.push(managed);
        }
        if let Some(materialized) = materialized {
            pinned_materialized.push(materialized);
        }
        push_planned_path(&mut applied, &mut seen, path);
    }
    Ok(AppliedDenyReadAcls {
        enforced_paths: applied,
        newly_managed_paths: added_in_this_call,
    })
}

#[cfg(test)]
mod tests {
    use super::ProtectedMetadataError;
    use super::apply_deny_read_acls;
    use super::apply_deny_read_acls_with_ownership_before_set;
    use super::ensure_directory_materialized;
    use super::ensure_directory_materialized_with_hook;
    use super::plan_deny_read_acl_paths;
    use super::reject_case_sensitive_directory;
    use crate::acl::dacl_has_read_deny_for_sid;
    use crate::acl::fetch_dacl_handle;
    use crate::acl::revoke_deny_read_ace;
    use crate::token::LocalSid;
    use pretty_assertions::assert_eq;
    use std::collections::HashSet;
    use std::ffi::c_void;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;
    use windows_sys::Win32::Foundation::HLOCAL;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
    use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::FileCaseSensitiveInfo;
    use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;
    use windows_sys::Win32::System::SystemServices::FILE_CS_FLAG_CASE_SENSITIVE_DIR;
    use windows_sys::Win32::System::WindowsProgramming::FILE_CASE_SENSITIVE_INFO;

    #[test]
    fn plan_preserves_missing_paths() {
        let tmp = TempDir::new().expect("tempdir");
        let missing = tmp.path().join("future-secret.env");

        assert_eq!(
            plan_deny_read_acl_paths(std::slice::from_ref(&missing)),
            vec![missing]
        );
    }

    #[test]
    fn plan_includes_existing_canonical_targets() {
        let tmp = TempDir::new().expect("tempdir");
        let existing = tmp.path().join("secret.env");
        std::fs::write(&existing, "secret").expect("write secret");

        let planned: HashSet<PathBuf> = plan_deny_read_acl_paths(std::slice::from_ref(&existing))
            .into_iter()
            .collect();
        let expected: HashSet<PathBuf> = [
            existing.clone(),
            dunce::canonicalize(&existing).expect("canonical path"),
        ]
        .into_iter()
        .collect();

        assert_eq!(planned, expected);
    }

    #[test]
    fn materialized_empty_sentinel_is_conservatively_cleaned() {
        let tmp = TempDir::new().expect("tempdir");
        let parent = tmp.path().join("workspace");
        std::fs::create_dir(&parent).expect("create parent");
        let sentinel = parent.join(".singularity");

        let materialized = ensure_directory_materialized(&sentinel).expect("materialize sentinel");
        assert!(materialized.created_by_runtime());
        assert!(sentinel.is_dir());
        materialized
            .cleanup_if_empty()
            .expect("cleanup empty sentinel");
        assert!(!sentinel.exists());

        let materialized =
            ensure_directory_materialized(&sentinel).expect("materialize sentinel again");
        assert!(materialized.created_by_runtime());
        std::fs::write(sentinel.join("user-state"), b"keep").expect("write child");
        materialized
            .cleanup_if_empty()
            .expect("non-empty sentinel is left in place");
        assert!(sentinel.exists());
    }

    #[test]
    fn parent_replacement_cannot_redirect_handle_relative_create() {
        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path().join("workspace");
        let moved = tmp.path().join("workspace-moved");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&workspace).expect("create workspace");
        std::fs::create_dir(&outside).expect("create outside");
        let sentinel = workspace.join(".singularity");
        let rename_failed = std::cell::Cell::new(false);
        let existed_before_atomic_create = std::cell::Cell::new(false);

        let materialized = ensure_directory_materialized_with_hook(&sentinel, || {
            existed_before_atomic_create.set(sentinel.exists());
            rename_failed.set(std::fs::rename(&workspace, &moved).is_err());
        })
        .expect("materialize through pinned parent");

        assert!(
            !existed_before_atomic_create.get(),
            "hook must run before the atomic final create"
        );
        assert!(rename_failed.get(), "pinned parent must reject replacement");
        assert!(sentinel.is_dir());
        assert!(!outside.join(".singularity").exists());
        materialized.cleanup_if_empty().expect("cleanup sentinel");
    }

    #[test]
    fn materialization_rejects_non_filesystem_absolute_prefixes() {
        assert!(
            ensure_directory_materialized(Path::new(r"\rooted-without-drive\sentinel")).is_err()
        );
        assert!(ensure_directory_materialized(Path::new(r"\\.\PIPE\sentinel")).is_err());
    }

    #[test]
    fn case_sensitive_flag_is_typed_unsupported() {
        let path = Path::new(r"C:\workspace");
        reject_case_sensitive_directory(0, path).expect("ordinary directory");
        let error = reject_case_sensitive_directory(FILE_CS_FLAG_CASE_SENSITIVE_DIR, path)
            .expect_err("case-sensitive directory must fail closed");
        assert_eq!(
            error.downcast_ref::<ProtectedMetadataError>(),
            Some(&ProtectedMetadataError::CaseSensitiveDirectoryUnsupported {
                path: path.to_path_buf(),
            })
        );
    }

    #[test]
    fn enabled_case_sensitive_directory_is_rejected_before_acl_planning() {
        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path().join("case-sensitive-workspace");
        std::fs::create_dir(&workspace).expect("create workspace");
        let mut options = std::fs::OpenOptions::new();
        options
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
        let directory = options.open(&workspace).expect("open workspace attributes");
        let information = FILE_CASE_SENSITIVE_INFO {
            Flags: FILE_CS_FLAG_CASE_SENSITIVE_DIR,
        };
        if unsafe {
            SetFileInformationByHandle(
                directory.as_raw_handle() as _,
                FileCaseSensitiveInfo,
                &information as *const _ as *const c_void,
                std::mem::size_of::<FILE_CASE_SENSITIVE_INFO>() as u32,
            )
        } == 0
        {
            // Enabling the NTFS flag requires an elevated token on some hosts. The pure flag test
            // above still fixes the contract; CI or elevated environments exercise this path.
            return;
        }
        drop(directory);
        let protected = workspace.join("secret.pem");
        let sid = LocalSid::from_string("S-1-5-21-1111111111-2222222222-3333333333-4444")
            .expect("test capability SID");

        let error = unsafe { apply_deny_read_acls(std::slice::from_ref(&protected), sid.as_ptr()) }
            .expect_err("enabled case-sensitive parent must fail closed");

        assert_eq!(
            error.downcast_ref::<ProtectedMetadataError>(),
            Some(&ProtectedMetadataError::CaseSensitiveDirectoryUnsupported { path: workspace })
        );
        assert!(!protected.exists(), "failure must precede materialization");
    }

    #[test]
    fn pinned_cleanup_cannot_delete_a_replacement_object() {
        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path().join("workspace");
        let replacement = workspace.join(".singularity-replacement");
        std::fs::create_dir(&workspace).expect("create workspace");
        let sentinel = workspace.join(".singularity");
        let materialized = ensure_directory_materialized(&sentinel).expect("materialize sentinel");
        let replacement_failed = std::cell::Cell::new(false);

        materialized
            .cleanup_if_empty_with_hook(|| {
                replacement_failed.set(std::fs::rename(&sentinel, &replacement).is_err());
            })
            .expect("cleanup pinned sentinel");

        assert!(
            replacement_failed.get(),
            "pinned sentinel must reject replacement before deletion"
        );
        assert!(!sentinel.exists());
        assert!(!replacement.exists());
    }

    #[test]
    fn materialization_rejects_a_reparse_parent() {
        let tmp = TempDir::new().expect("tempdir");
        let target = tmp.path().join("target");
        let alias = tmp.path().join("alias");
        std::fs::create_dir(&target).expect("create target");
        let link = format!("\"{}\"", alias.display());
        let target_arg = format!("\"{}\"", target.display());
        let junction_created = std::process::Command::new("cmd.exe")
            .raw_arg("/c")
            .raw_arg("mklink")
            .raw_arg("/J")
            .raw_arg(&link)
            .raw_arg(&target_arg)
            .output()
            .is_ok_and(|output| output.status.success() && alias.exists());
        assert!(junction_created, "junction fixture must be available");

        assert!(ensure_directory_materialized(&alias.join("sentinel")).is_err());
        assert!(!target.join("sentinel").exists());
    }

    #[test]
    fn existing_git_marker_is_acl_protected() {
        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("create workspace");
        let existing_git = workspace.join(".git");
        std::fs::create_dir(&existing_git).expect("create existing git marker");
        let sid = LocalSid::from_string("S-1-5-21-1111111111-2222222222-3333333333-4444")
            .expect("test capability SID");

        let applied =
            unsafe { apply_deny_read_acls(std::slice::from_ref(&existing_git), sid.as_ptr()) }
                .expect("protect existing .git");

        assert_eq!(applied, vec![existing_git.clone()]);
        let (p_dacl, p_sd) = unsafe { fetch_dacl_handle(&existing_git) }.expect("fetch .git ACL");
        assert!(unsafe { dacl_has_read_deny_for_sid(p_dacl, sid.as_ptr()) });
        unsafe {
            LocalFree(p_sd as HLOCAL);
            revoke_deny_read_ace(&existing_git, sid.as_ptr()).expect("restore .git ACL");
        }
    }

    #[test]
    fn journal_failure_prevents_deny_read_acl_mutation() {
        let tmp = TempDir::new().expect("tempdir");
        let protected = tmp.path().join("protected");
        std::fs::create_dir(&protected).expect("create protected path");
        let sid = LocalSid::from_string("S-1-5-21-1111111111-2222222222-3333333333-4444")
            .expect("test capability SID");
        let result = unsafe {
            apply_deny_read_acls_with_ownership_before_set(
                std::slice::from_ref(&protected),
                sid.as_ptr(),
                &mut |_| anyhow::bail!("injected ownership journal failure"),
            )
        };
        let error = match result {
            Ok(_) => panic!("journal failure must abort before ACL mutation"),
            Err(error) => error,
        };

        assert!(
            format!("{error:#}").contains("injected ownership journal failure"),
            "{error:#}"
        );
        let (p_dacl, p_sd) = unsafe { fetch_dacl_handle(&protected) }.expect("fetch unchanged ACL");
        assert!(!unsafe { dacl_has_read_deny_for_sid(p_dacl, sid.as_ptr()) });
        unsafe {
            LocalFree(p_sd as HLOCAL);
        }
    }

    #[test]
    fn missing_protected_marker_is_materialized_and_acl_protected() {
        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("create workspace");
        let missing_marker = workspace.join(".agents");
        let sid = LocalSid::from_string("S-1-5-21-1111111111-2222222222-3333333333-4444")
            .expect("test capability SID");

        let applied =
            unsafe { apply_deny_read_acls(std::slice::from_ref(&missing_marker), sid.as_ptr()) }
                .expect("materialize and protect missing marker");

        assert_eq!(applied, vec![missing_marker.clone()]);
        assert!(missing_marker.is_dir());
        let (p_dacl, p_sd) =
            unsafe { fetch_dacl_handle(&missing_marker) }.expect("fetch marker ACL");
        assert!(unsafe { dacl_has_read_deny_for_sid(p_dacl, sid.as_ptr()) });
        unsafe {
            LocalFree(p_sd as HLOCAL);
            revoke_deny_read_ace(&missing_marker, sid.as_ptr()).expect("restore marker ACL");
        }
    }

    #[test]
    fn revoke_removes_an_existing_deny_read_ace() {
        let tmp = TempDir::new().expect("tempdir");
        let protected = tmp.path().join("protected");
        std::fs::create_dir(&protected).expect("create protected path");
        let sid = LocalSid::from_string("S-1-5-21-1111111111-2222222222-3333333333-4444")
            .expect("test capability SID");

        unsafe {
            apply_deny_read_acls(std::slice::from_ref(&protected), sid.as_ptr())
                .expect("apply deny-read ACL");
            revoke_deny_read_ace(&protected, sid.as_ptr()).expect("revoke deny-read ACL");
        }
        let (dacl, security_descriptor) =
            unsafe { fetch_dacl_handle(&protected) }.expect("fetch reconciled ACL");
        assert!(!unsafe { dacl_has_read_deny_for_sid(dacl, sid.as_ptr()) });
        unsafe {
            LocalFree(security_descriptor as HLOCAL);
        }
    }

    #[test]
    fn missing_nested_git_marker_under_ancestor_is_typed_unsupported_without_sentinel() {
        let tmp = TempDir::new().expect("tempdir");
        let repository = tmp.path().join("repository");
        let workspace = repository.join("nested");
        std::fs::create_dir_all(&workspace).expect("create nested workspace");
        let ancestor_git = repository.join(".git");
        std::fs::create_dir(&ancestor_git).expect("create ancestor git marker");
        let nested_git = workspace.join(".git");
        let sid = LocalSid::from_string("S-1-1-0").expect("world SID");

        let error =
            unsafe { apply_deny_read_acls(std::slice::from_ref(&nested_git), sid.as_ptr()) }
                .expect_err("nested .git marker must fail closed");
        let typed = error
            .downcast_ref::<ProtectedMetadataError>()
            .expect("typed nested metadata error");
        assert_eq!(
            typed,
            &ProtectedMetadataError::NestedGitMarkerUnsupported {
                path: nested_git.clone(),
                ancestor_git_marker: ancestor_git.clone(),
            }
        );
        assert!(!nested_git.exists());
    }

    #[test]
    fn plan_includes_lexical_and_canonical_reparse_targets() {
        let tmp = TempDir::new().expect("tempdir");
        let target = tmp.path().join("target");
        let alias = tmp.path().join("protected-link");
        std::fs::create_dir(&target).expect("create target");
        let link = format!("\"{}\"", alias.display());
        let target_arg = format!("\"{}\"", target.display());
        let junction_created = std::process::Command::new("cmd.exe")
            .raw_arg("/c")
            .raw_arg("mklink")
            .raw_arg("/J")
            .raw_arg(&link)
            .raw_arg(&target_arg)
            .output()
            .is_ok_and(|output| output.status.success() && alias.exists());
        assert!(junction_created, "junction fixture must be available");

        let planned: HashSet<PathBuf> = plan_deny_read_acl_paths(std::slice::from_ref(&alias))
            .into_iter()
            .collect();
        assert!(planned.contains(&alias));
        assert!(planned.contains(&dunce::canonicalize(target).expect("canonical target")));
    }
}
