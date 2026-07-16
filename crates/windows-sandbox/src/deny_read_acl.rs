use crate::acl::add_deny_read_ace;
use crate::acl::path_contains_reparse_component;
use crate::acl::revoke_ace;
use crate::acl::verify_target_identity_against;
use crate::path_normalization::lexical_path_key;
use crate::winutil::to_wide;
use anyhow::Context;
use anyhow::Result;
use dunce::canonicalize;
use singularity_core::PROTECTED_GIT_DIR_NAME;
use std::collections::HashSet;
use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::fs::Metadata;
use std::path::Path;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_DIR_NOT_EMPTY;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_FLAG_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_FLAG_POSIX_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO_EX;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_LIST_DIRECTORY;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::FileDispositionInfoEx;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;

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

fn is_reparse_point(metadata: &Metadata) -> bool {
    std::os::windows::fs::MetadataExt::file_attributes(metadata) & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn is_reparse_point_attributes(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn validate_materialized_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect materialized directory {}", path.display()))?;
    if !metadata.is_dir() {
        anyhow::bail!("deny path is not a directory: {}", path.display());
    }
    if is_reparse_point(&metadata) {
        anyhow::bail!(
            "refusing to materialize through reparse directory {}",
            path.display()
        );
    }
    Ok(())
}

/// Creates a missing directory path without following a reparse point in any ancestor.
///
/// The returned flag is true only when this call created at least one component. Callers may
/// use it to attempt conservative cleanup after a later ACL failure.
pub fn ensure_directory_materialized(path: &Path) -> Result<bool> {
    if path_contains_reparse_component(path)? {
        anyhow::bail!(
            "refusing to materialize through reparse path {}",
            path.display()
        );
    }
    let mut missing = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        match std::fs::symlink_metadata(&cursor) {
            Ok(_) => {
                validate_materialized_directory(&cursor)?;
                break;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                missing.push(cursor.clone());
                let Some(parent) = cursor.parent() else {
                    anyhow::bail!("cannot resolve parent of deny path {}", path.display());
                };
                cursor = parent.to_path_buf();
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "inspect parent of materialized directory {}",
                        path.display()
                    )
                });
            }
        }
    }

    let mut created = false;
    for directory in missing.iter().rev() {
        match std::fs::create_dir(directory) {
            Ok(()) => {
                created = true;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("create deny path directory {}", directory.display())
                });
            }
        }
        validate_materialized_directory(directory)?;
        if path_contains_reparse_component(directory)? {
            anyhow::bail!(
                "reparse state changed while materializing deny path {}",
                directory.display()
            );
        }
    }
    Ok(created)
}

/// Removes only a runtime-created, empty, non-reparse sentinel directly below `expected_parent`.
///
/// Any uncertainty about ownership, identity, reparse state, or emptiness leaves the path in
/// place and returns an error where continuing could hide a failed security setup.
pub fn cleanup_empty_runtime_sentinel(
    path: &Path,
    expected_parent: &Path,
    created_by_runtime: bool,
) -> Result<()> {
    if !created_by_runtime {
        return Ok(());
    }
    let parent = std::fs::canonicalize(expected_parent)
        .with_context(|| format!("canonicalize sentinel parent {}", expected_parent.display()))?;
    let path_parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("sentinel has no parent: {}", path.display()))?;
    let canonical_path_parent = std::fs::canonicalize(path_parent).with_context(|| {
        format!(
            "canonicalize sentinel path parent {}",
            path_parent.display()
        )
    })?;
    if canonical_path_parent != parent {
        anyhow::bail!("sentinel parent changed: {}", path.display());
    }
    let expected_identity = canonicalize(path)
        .with_context(|| format!("canonicalize runtime sentinel {}", path.display()))?;
    unsafe { remove_empty_runtime_sentinel_by_handle(path, &expected_identity) }
}

/// Deletes only the object whose identity was checked on an open handle.
///
/// `remove_dir(path)` would re-resolve the path after the emptiness check and could delete a
/// replacement or reparse point. The handle disposition keeps cleanup bound to the runtime-created
/// directory; a concurrent child insertion makes the disposition fail with `ERROR_DIR_NOT_EMPTY`.
unsafe fn remove_empty_runtime_sentinel_by_handle(
    path: &Path,
    expected_identity: &Path,
) -> Result<()> {
    if path_contains_reparse_component(path)? {
        anyhow::bail!("refusing to clean a reparse sentinel {}", path.display());
    }
    let wpath = to_wide(path);
    let handle = CreateFileW(
        wpath.as_ptr(),
        DELETE | READ_CONTROL | FILE_LIST_DIRECTORY,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        std::ptr::null_mut(),
        OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        0,
    );
    if handle == 0 || handle == INVALID_HANDLE_VALUE {
        anyhow::bail!(
            "open runtime sentinel for safe cleanup failed: {}",
            GetLastError()
        );
    }
    let result = (|| -> Result<()> {
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
            anyhow::bail!(
                "refusing to clean non-directory or reparse sentinel {}",
                path.display()
            );
        }
        if path_contains_reparse_component(path)? {
            anyhow::bail!(
                "sentinel reparse state changed during cleanup: {}",
                path.display()
            );
        }
        verify_target_identity_against(handle, expected_identity)?;
        let mut entries = std::fs::read_dir(path)
            .with_context(|| format!("inspect sentinel contents {}", path.display()))?;
        if entries.next().is_some() {
            return Ok(());
        }

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
    })();
    CloseHandle(handle);
    result
}

fn canonical_identity(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path)
        .with_context(|| format!("resolve ACL target identity {}", path.display()))
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
        }
    }
}

impl Error for ProtectedMetadataError {}

fn existing_git_ancestor_for_missing_marker(path: &Path) -> Result<Option<PathBuf>> {
    let parent = path.parent().ok_or_else(|| {
        anyhow::anyhow!("protected metadata path has no parent: {}", path.display())
    })?;
    for ancestor in parent.ancestors().skip(1) {
        let marker = ancestor.join(PROTECTED_GIT_DIR_NAME);
        match std::fs::symlink_metadata(&marker) {
            Ok(_) => return Ok(Some(marker)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspect ancestor Git marker while materializing {}",
                        path.display()
                    )
                });
            }
        }
    }
    Ok(None)
}

/// Materializes a missing protected path, rejecting a nested `.git` sentinel under an existing
/// ancestor repository so Git's ancestor discovery remains unchanged.
pub fn ensure_missing_protected_path_materialized(path: &Path) -> Result<bool> {
    if is_missing_git_marker(path)
        && let Some(ancestor_git_marker) = existing_git_ancestor_for_missing_marker(path)?
    {
        return Err(anyhow::Error::new(
            ProtectedMetadataError::NestedGitMarkerUnsupported {
                path: path.to_path_buf(),
                ancestor_git_marker,
            },
        ));
    }
    ensure_directory_materialized(path)
}

/// Applies deny-read ACEs to explicit paths. Missing paths are materialized as
/// directories before the ACE is applied so a sandboxed command cannot create a
/// previously absent denied path and then read from it in the same run.
/// If any path fails, deny ACEs applied by this call are revoked before the
/// error is returned so a one-shot sandbox run does not leave partial state.
///
/// # Safety
/// Caller must pass a valid SID pointer for the sandbox principal being denied.
pub unsafe fn apply_deny_read_acls(paths: &[PathBuf], psid: *mut c_void) -> Result<Vec<PathBuf>> {
    let planned = plan_deny_read_acl_paths(paths);
    let mut applied = Vec::new();
    let mut seen = HashSet::new();
    let mut added_in_this_call: Vec<PathBuf> = Vec::new();
    for path in planned {
        let result = (|| -> Result<bool> {
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    ensure_missing_protected_path_materialized(&path)?;
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("inspect deny-read ACL target {}", path.display())
                    });
                }
            }
            let before = canonical_identity(&path)?;
            let added = add_deny_read_ace(&path, psid)
                .with_context(|| format!("apply deny-read ACE to {}", path.display()))?;
            let after = canonical_identity(&path)?;
            if before != after {
                anyhow::bail!(
                    "ACL target identity changed while applying deny-read ACE: {}",
                    path.display()
                );
            }
            Ok(added)
        })();
        let added = match result {
            Ok(added) => added,
            Err(err) => {
                for added_path in &added_in_this_call {
                    if let Err(rollback_err) = revoke_ace(added_path, psid) {
                        return Err(err.context(format!(
                            "deny-read rollback failed for {}: {rollback_err}",
                            added_path.display()
                        )));
                    }
                }
                return Err(err);
            }
        };
        if added {
            added_in_this_call.push(path.clone());
        }
        push_planned_path(&mut applied, &mut seen, path);
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::ProtectedMetadataError;
    use super::apply_deny_read_acls;
    use super::cleanup_empty_runtime_sentinel;
    use super::ensure_directory_materialized;
    use super::plan_deny_read_acl_paths;
    use crate::acl::dacl_has_read_deny_for_sid;
    use crate::acl::fetch_dacl_handle;
    use crate::acl::revoke_ace;
    use crate::token::LocalSid;
    use pretty_assertions::assert_eq;
    use std::collections::HashSet;
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use windows_sys::Win32::Foundation::HLOCAL;
    use windows_sys::Win32::Foundation::LocalFree;

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

        assert!(ensure_directory_materialized(&sentinel).expect("materialize sentinel"));
        assert!(sentinel.is_dir());
        cleanup_empty_runtime_sentinel(&sentinel, &parent, true).expect("cleanup empty sentinel");
        assert!(!sentinel.exists());

        assert!(ensure_directory_materialized(&sentinel).expect("materialize sentinel again"));
        std::fs::write(sentinel.join("user-state"), b"keep").expect("write child");
        cleanup_empty_runtime_sentinel(&sentinel, &parent, true)
            .expect("non-empty sentinel is left in place");
        assert!(sentinel.exists());
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
            revoke_ace(&existing_git, sid.as_ptr()).expect("restore .git ACL");
        }
    }

    #[test]
    fn missing_git_marker_without_ancestor_is_materialized_and_acl_protected() {
        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("create workspace");
        let missing_git = workspace.join(".git");
        let sid = LocalSid::from_string("S-1-5-21-1111111111-2222222222-3333333333-4444")
            .expect("test capability SID");

        let applied =
            unsafe { apply_deny_read_acls(std::slice::from_ref(&missing_git), sid.as_ptr()) }
                .expect("materialize and protect missing .git");

        assert_eq!(applied, vec![missing_git.clone()]);
        assert!(missing_git.is_dir());
        let (p_dacl, p_sd) = unsafe { fetch_dacl_handle(&missing_git) }.expect("fetch .git ACL");
        assert!(unsafe { dacl_has_read_deny_for_sid(p_dacl, sid.as_ptr()) });
        unsafe {
            LocalFree(p_sd as HLOCAL);
            revoke_ace(&missing_git, sid.as_ptr()).expect("restore .git ACL");
        }
        cleanup_empty_runtime_sentinel(&missing_git, &workspace, true)
            .expect("cleanup runtime-created .git sentinel");
        assert!(!missing_git.exists());
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
