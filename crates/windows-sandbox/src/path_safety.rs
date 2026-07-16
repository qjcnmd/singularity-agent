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
    FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileCaseSensitiveInfo, GetFileInformationByHandleEx, SYNCHRONIZE,
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

#[cfg(test)]
mod tests {
    use super::ProtectedMetadataError;
    use super::enable_case_sensitive_directory_for_test;
    use super::ensure_case_insensitive_acl_path;

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
}
