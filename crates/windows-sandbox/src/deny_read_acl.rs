use crate::acl::DenyReadAclFingerprint;
use crate::acl::add_deny_read_ace_with_ownership_before_set;
use crate::acl::add_deny_read_ace_with_ownership_to_handle_before_set;
use crate::acl::add_deny_write_ace_to_handle;
use crate::acl::revoke_deny_read_ace_with_fingerprint;
use crate::path_normalization::lexical_path_key;
use crate::path_safety::FILE_ATTRIBUTE_REPARSE_POINT;
use crate::path_safety::ProtectedMetadataError;
use crate::path_safety::absolute_path_components;
use crate::path_safety::ensure_case_insensitive_acl_path;
use crate::path_safety::ensure_case_insensitive_directory;
use crate::path_safety::nt_open_relative;
use crate::path_safety::open_filesystem_root;
use crate::path_safety::validate_plain_directory;
use anyhow::Context;
use anyhow::Result;
use cap_std::fs::Dir;
use serde::Deserialize;
use serde::Serialize;
use singularity_core::PROTECTED_GIT_DIR_NAME;
use std::collections::HashSet;
use std::ffi::{OsStr, c_void};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use windows_sys::Wdk::Storage::FileSystem::{FILE_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF};
use windows_sys::Win32::Foundation::ERROR_DIR_NOT_EMPTY;
use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows_sys::Win32::Foundation::ERROR_PATH_NOT_FOUND;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_FLAG_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_FLAG_POSIX_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO_EX;
use windows_sys::Win32::Storage::FileSystem::FILE_LIST_DIRECTORY;
use windows_sys::Win32::Storage::FileSystem::FileDispositionInfoEx;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;
use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
use windows_sys::Win32::System::WindowsProgramming::FILE_CREATED;

/// Build the exact ACL paths that should receive a deny-read ACE.
///
/// Missing exact denies remain eligible for later materialization. Existing reparse targets are
/// rejected during preflight because pathname ACL enforcement intentionally never follows them.
pub fn plan_deny_read_acl_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut planned = Vec::new();
    let mut seen = HashSet::new();
    for path in paths {
        ensure_case_insensitive_acl_path(path)?;
        push_planned_path(&mut planned, &mut seen, path.to_path_buf());
    }
    Ok(planned)
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

/// Creates or opens an absolute directory through pinned, no-follow native handles.
pub fn ensure_directory_materialized(path: &Path) -> Result<MaterializedDirectory> {
    ensure_directory_materialized_with_hook(path, || {})
}

fn ensure_directory_materialized_with_hook(
    path: &Path,
    before_final_create: impl FnOnce(),
) -> Result<MaterializedDirectory> {
    let (anchor, descendants) = absolute_path_components(path)?;
    if descendants.is_empty() {
        anyhow::bail!("protected materialization path must be below a filesystem root");
    }
    let mut current = open_filesystem_root(&anchor, FILE_LIST_DIRECTORY)?;
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
            if let Err(error) = ensure_case_insensitive_directory(&current, &current_path) {
                if created_by_runtime
                    && let Err(cleanup_error) =
                        unsafe { remove_empty_runtime_sentinel_by_handle(&file, || {}) }
                {
                    return Err(error.context(format!(
                        "cleanup failed after final parent case-sensitivity check: {cleanup_error}"
                    )));
                }
                return Err(error);
            }
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
        // SAFETY: `file` pins the directory handle for this synchronous identity check and the
        // Win32 output structure is initialized through a valid mutable pointer.
        let mut info = unsafe { std::mem::zeroed() };
        let info_ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
        if info_ok == 0 {
            anyhow::bail!(
                "query runtime sentinel identity for safe cleanup failed: {}",
                unsafe { GetLastError() }
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
        let delete_ok = unsafe {
            SetFileInformationByHandle(
                handle,
                FileDispositionInfoEx,
                &disposition as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
            )
        };
        if delete_ok == 0 {
            let code = unsafe { GetLastError() };
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
    let planned = plan_deny_read_acl_paths(paths)?;
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
    use super::apply_deny_read_acls;
    use super::apply_deny_read_acls_with_ownership_before_set;
    use super::ensure_directory_materialized;
    use super::ensure_directory_materialized_with_hook;
    use super::plan_deny_read_acl_paths;
    use crate::acl::dacl_has_read_deny_for_sid;
    use crate::acl::fetch_dacl_handle;
    use crate::acl::revoke_deny_read_ace;
    use crate::path_safety::CaseSensitivityTestOutcome;
    use crate::path_safety::ProtectedMetadataError;
    use crate::path_safety::override_case_sensitivity_for_test;
    use crate::path_safety::reject_case_sensitive_directory;
    use crate::token::LocalSid;
    use pretty_assertions::assert_eq;
    use std::os::windows::process::CommandExt;
    use std::path::Path;
    use tempfile::TempDir;
    use windows_sys::Win32::Foundation::HLOCAL;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::System::SystemServices::FILE_CS_FLAG_CASE_SENSITIVE_DIR;

    #[test]
    fn plan_preserves_missing_paths() {
        let tmp = TempDir::new().expect("tempdir");
        let missing = tmp.path().join("future-secret.env");

        assert_eq!(
            plan_deny_read_acl_paths(std::slice::from_ref(&missing)).expect("plan missing path"),
            vec![missing]
        );
    }

    #[test]
    fn plan_preserves_existing_plain_paths() {
        let tmp = TempDir::new().expect("tempdir");
        let existing = tmp.path().join("secret.env");
        std::fs::write(&existing, "secret").expect("write secret");

        assert_eq!(
            plan_deny_read_acl_paths(std::slice::from_ref(&existing))
                .expect("plan existing target"),
            vec![existing]
        );
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
        let _case_sensitive = override_case_sensitivity_for_test(
            &workspace,
            CaseSensitivityTestOutcome::CaseSensitive,
        );
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
    fn final_reparse_target_is_rejected_during_acl_planning() {
        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path().join("workspace");
        let target = tmp.path().join("target");
        let alias = workspace.join("alias");
        std::fs::create_dir(&workspace).expect("create workspace");
        std::fs::create_dir(&target).expect("create target");
        let junction_created = std::process::Command::new("cmd.exe")
            .raw_arg("/c")
            .raw_arg("mklink")
            .raw_arg("/J")
            .raw_arg(format!("\"{}\"", alias.display()))
            .raw_arg(format!("\"{}\"", target.display()))
            .output()
            .is_ok_and(|output| output.status.success() && alias.exists());
        assert!(junction_created, "junction fixture must be available");
        let error = plan_deny_read_acl_paths(std::slice::from_ref(&alias))
            .expect_err("reparse target must be rejected before ACL side effects");

        assert_eq!(
            error.downcast_ref::<ProtectedMetadataError>(),
            Some(&ProtectedMetadataError::ReparseTargetUnsupported { path: alias })
        );
    }

    #[test]
    fn final_parent_recheck_failure_rolls_back_created_sentinel() {
        let tmp = TempDir::new().expect("tempdir");
        let workspace = tmp.path().join("workspace");
        let sentinel = workspace.join(".singularity");
        std::fs::create_dir(&workspace).expect("create workspace");
        let case_sensitivity_override = std::cell::RefCell::new(None);

        let result = ensure_directory_materialized_with_hook(&sentinel, || {
            case_sensitivity_override.replace(Some(override_case_sensitivity_for_test(
                &workspace,
                CaseSensitivityTestOutcome::CaseSensitive,
            )));
        });
        let error = match result {
            Ok(materialized) => {
                materialized
                    .cleanup_if_empty()
                    .expect("cleanup unexpectedly accepted sentinel");
                panic!("case-sensitive parent must fail closed after final create");
            }
            Err(error) => error,
        };

        assert_eq!(
            error.downcast_ref::<ProtectedMetadataError>(),
            Some(&ProtectedMetadataError::CaseSensitiveDirectoryUnsupported { path: workspace })
        );
        assert!(
            !sentinel.exists(),
            "failed materialization must remove its newly created sentinel"
        );
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
}
