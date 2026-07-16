use crate::acl::DenyReadAclFingerprint;
use crate::acl::add_deny_read_ace_with_ownership_before_set;
use crate::acl::add_deny_read_ace_with_ownership_to_handle_before_set;
use crate::acl::add_deny_write_ace_to_handle;
use crate::acl::revoke_deny_read_ace_with_fingerprint;
use crate::path_normalization::lexical_path_key;
use anyhow::Context;
use anyhow::Result;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use dunce::canonicalize;
use serde::Deserialize;
use serde::Serialize;
use singularity_core::PROTECTED_GIT_DIR_NAME;
use std::collections::HashSet;
use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf};
use windows_sys::Win32::Foundation::ERROR_DIR_NOT_EMPTY;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_FLAG_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_FLAG_POSIX_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO_EX;
use windows_sys::Win32::Storage::FileSystem::FILE_LIST_DIRECTORY;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::FileDispositionInfoEx;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;
use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;

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
    let mut rooted = false;
    for component in path.components() {
        match component {
            Component::Prefix(_) if !rooted && descendants.is_empty() => {
                anchor.push(component.as_os_str());
            }
            Component::RootDir if !rooted && descendants.is_empty() => {
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
    if !rooted || descendants.is_empty() {
        anyhow::bail!("deny path must name a directory below a filesystem root");
    }
    Ok((anchor, descendants))
}

fn open_acl_directory(parent: &Dir, name: &std::ffi::OsStr) -> Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options
        .access_mode(DELETE | READ_CONTROL | WRITE_DAC | FILE_LIST_DIRECTORY)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .follow(FollowSymlinks::No)
        .maybe_dir(true);
    let file = parent
        .open_with(name, &options)
        .with_context(|| {
            format!(
                "open materialized deny directory {}",
                name.to_string_lossy()
            )
        })?
        .into_std();
    let metadata = file
        .metadata()
        .context("inspect pinned materialized deny directory")?;
    if !metadata.is_dir() || is_reparse_point_attributes(metadata.file_attributes()) {
        anyhow::bail!("materialized deny target is not a plain directory");
    }
    Ok(file)
}

fn ancestor_has_git_marker(directory: &Dir) -> Result<bool> {
    match directory.symlink_metadata(PROTECTED_GIT_DIR_NAME) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("inspect ancestor Git marker"),
    }
}

/// Creates or opens an absolute directory through pinned, no-follow capabilities.
pub fn ensure_directory_materialized(path: &Path) -> Result<MaterializedDirectory> {
    ensure_directory_materialized_with_hook(path, || {})
}

fn ensure_directory_materialized_with_hook(
    path: &Path,
    before_final_create: impl FnOnce(),
) -> Result<MaterializedDirectory> {
    let (anchor, descendants) = absolute_path_components(path)?;
    let mut current = Dir::open_ambient_dir(&anchor, ambient_authority())
        .with_context(|| format!("open deny path filesystem root {}", anchor.display()))?;
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
        let created = match current.create_dir(component) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("create deny path component {}", component.to_string_lossy())
                });
            }
        };
        if is_final {
            if created {
                // The capability-relative create already completed; the hook is placed before the
                // final object open so tests can prove a pathname replacement cannot redirect it.
                before_final_create.take().expect("single final hook")();
            }
            return Ok(MaterializedDirectory {
                file: open_acl_directory(&current, component)?,
                created_by_runtime: created,
            });
        }
        current = current.open_dir_nofollow(component).with_context(|| {
            format!(
                "open deny path component without following reparse points {}",
                component.to_string_lossy()
            )
        })?;
        current_path.push(component);
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
    use crate::acl::dacl_has_read_deny_for_sid;
    use crate::acl::fetch_dacl_handle;
    use crate::acl::revoke_deny_read_ace;
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
    fn pinned_materialization_cannot_be_redirected_after_create() {
        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path().join("workspace");
        let moved = tmp.path().join("workspace-moved");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&workspace).expect("create workspace");
        std::fs::create_dir(&outside).expect("create outside");
        let sentinel = workspace.join(".singularity");
        let rename_failed = std::cell::Cell::new(false);

        let materialized = ensure_directory_materialized_with_hook(&sentinel, || {
            rename_failed.set(std::fs::rename(&workspace, &moved).is_err());
        })
        .expect("materialize through pinned parent");

        assert!(rename_failed.get(), "pinned parent must reject replacement");
        assert!(sentinel.is_dir());
        assert!(!outside.join(".singularity").exists());
        materialized.cleanup_if_empty().expect("cleanup sentinel");
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
