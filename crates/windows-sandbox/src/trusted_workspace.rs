//! Handle-pinned transactions for controller-owned Windows workspaces.
//!
//! A trusted workspace preparation request is allowed to create controller metadata (for
//! example, a Git directory), but it is still an untrusted child process.  The lease in this
//! module keeps the run-owned root and its parent pinned while that child is alive.  Failed
//! preparations are moved to a private sibling with a handle-relative, no-replace rename.
//! The quarantine sibling is retained for a later controlled cleanup; this transaction never
//! recursively deletes a tree whose child provenance cannot be proven from the root lease alone.

#[cfg(target_os = "windows")]
mod windows {
    use crate::path_safety::open_existing_acl_target;
    use rand::rngs::SmallRng;
    use rand::{RngCore, SeedableRng};
    use serde::{Deserialize, Serialize};
    use std::ffi::{OsStr, c_void};
    use std::fs::File;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::{Path, PathBuf};
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_CREATE, FILE_DIRECTORY_FILE, FILE_RENAME_INFORMATION, FILE_RENAME_INFORMATION_0,
        NtSetInformationFile,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, INVALID_HANDLE_VALUE,
        STATUS_INVALID_PARAMETER, STATUS_OBJECT_NAME_COLLISION, STATUS_SUCCESS,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX, FILE_LIST_DIRECTORY,
        FILE_READ_ATTRIBUTES, FileDispositionInfoEx, GetFileInformationByHandle, READ_CONTROL,
        SetFileInformationByHandle, WRITE_DAC,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessId, OpenProcess, PROCESS_DUP_HANDLE,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    const MAX_QUARANTINE_ATTEMPTS: usize = 8;

    /// Stable error classes returned by the trusted workspace transaction.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum TrustedWorkspaceError {
        RootUnavailable,
        IdentityUnavailable,
        ReparseUnsupported,
        RootDrift,
        QuarantineFailed,
        CleanupFailed,
        AlreadyFinalized,
    }

    /// Serialized capability metadata used to duplicate the lease root into the setup helper.
    ///
    /// The handle value is meaningful only in `parent_pid`; the helper must duplicate it before
    /// any workspace ACL/path operation and then verify the resulting object identity.
    #[derive(Clone, Debug, Deserialize, Serialize)]
    pub struct TrustedWorkspaceSetupPin {
        pub parent_pid: u32,
        pub root_handle: u64,
        pub root_path: PathBuf,
        pub root_identity: (u32, u64, u32),
    }

    impl TrustedWorkspaceError {
        /// Stable, path-free error code suitable for a command diagnostic.
        pub const fn code(self) -> &'static str {
            match self {
                Self::RootUnavailable => "trusted_workspace_root_unavailable",
                Self::IdentityUnavailable => "trusted_workspace_identity_unavailable",
                Self::ReparseUnsupported => "trusted_workspace_reparse_unsupported",
                Self::RootDrift => "trusted_workspace_root_drift",
                Self::QuarantineFailed => "trusted_workspace_quarantine_failed",
                Self::CleanupFailed => "trusted_workspace_cleanup_failed",
                Self::AlreadyFinalized => "trusted_workspace_already_finalized",
            }
        }
    }

    impl std::fmt::Display for TrustedWorkspaceError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.code())
        }
    }

    impl std::error::Error for TrustedWorkspaceError {}

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct ObjectIdentity {
        volume_serial: u32,
        file_index: u64,
        links: u32,
    }

    /// A no-follow lease over one controller-owned workspace root and its parent directory.
    ///
    /// The handles deliberately omit `FILE_SHARE_DELETE`; ordinary pathname replacement is
    /// therefore rejected by the kernel for the whole child lifetime.  The final path/identity
    /// check still catches POSIX rename semantics or an alias that was changed before the lease
    /// was acquired.
    #[derive(Debug)]
    pub struct TrustedWorkspaceLease {
        parent: File,
        root: File,
        parent_path: PathBuf,
        root_name: std::ffi::OsString,
        parent_identity: ObjectIdentity,
        root_identity: ObjectIdentity,
        finished: bool,
    }

    fn root_parts(
        root: &Path,
    ) -> std::result::Result<(PathBuf, std::ffi::OsString), TrustedWorkspaceError> {
        let absolute_root = if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|_| TrustedWorkspaceError::RootUnavailable)?
                .join(root)
        };
        if absolute_root.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        }) {
            return Err(TrustedWorkspaceError::RootUnavailable);
        }
        let parent = absolute_root
            .parent()
            .ok_or(TrustedWorkspaceError::RootUnavailable)?
            .to_path_buf();
        let name = absolute_root
            .file_name()
            .ok_or(TrustedWorkspaceError::RootUnavailable)?
            .to_os_string();
        Ok((parent, name))
    }

    impl TrustedWorkspaceLease {
        /// Open and pin an existing plain directory and its plain parent without following a
        /// final reparse point.
        pub fn acquire(root: &Path) -> std::result::Result<Self, TrustedWorkspaceError> {
            let (lexical_parent, root_name) = root_parts(root)?;
            let parent = open_existing_acl_target(
                &lexical_parent,
                FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            )
            .map_err(classify_open_error)?;
            let parent_path = dunce::canonicalize(&lexical_parent)
                .map_err(|_| TrustedWorkspaceError::RootUnavailable)?;
            let root_file = open_existing_child(
                &parent,
                &root_name,
                DELETE | FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC,
            )
            .map_err(classify_open_error)?;
            let parent_identity = object_identity(&parent)?;
            let root_identity = object_identity(&root_file)?;
            if !parent_identity.is_valid() || !root_identity.is_valid() {
                return Err(TrustedWorkspaceError::IdentityUnavailable);
            }
            Ok(Self {
                parent,
                root: root_file,
                parent_path,
                root_name,
                parent_identity,
                root_identity,
                finished: false,
            })
        }

        /// Atomically create and pin a new plain workspace root below an existing plain parent.
        /// Existing leaves are rejected rather than opened or adopted.
        pub fn create(root: &Path) -> std::result::Result<Self, TrustedWorkspaceError> {
            let (lexical_parent, root_name) = root_parts(root)?;
            let parent = open_existing_acl_target(
                &lexical_parent,
                FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            )
            .map_err(classify_open_error)?;
            let parent_path = dunce::canonicalize(&lexical_parent)
                .map_err(|_| TrustedWorkspaceError::RootUnavailable)?;
            let (root_file, _) = crate::path_safety::nt_open_relative(
                &parent,
                &root_name,
                DELETE | FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC,
                FILE_CREATE,
                FILE_DIRECTORY_FILE,
            )
            .map_err(|_| TrustedWorkspaceError::RootUnavailable)?;
            let parent_identity = match object_identity(&parent) {
                Ok(identity) => identity,
                Err(error) => {
                    return match mark_delete(&root_file) {
                        Ok(()) => Err(error),
                        Err(cleanup_error) => Err(cleanup_error),
                    };
                }
            };
            let root_identity = match object_identity(&root_file) {
                Ok(identity) if identity.is_valid() => identity,
                Ok(_) | Err(TrustedWorkspaceError::IdentityUnavailable) => {
                    return match mark_delete(&root_file) {
                        Ok(()) => Err(TrustedWorkspaceError::IdentityUnavailable),
                        Err(error) => Err(error),
                    };
                }
                Err(error) => {
                    return match mark_delete(&root_file) {
                        Ok(()) => Err(error),
                        Err(cleanup_error) => Err(cleanup_error),
                    };
                }
            };
            if !parent_identity.is_valid() {
                return match mark_delete(&root_file) {
                    Ok(()) => Err(TrustedWorkspaceError::IdentityUnavailable),
                    Err(error) => Err(error),
                };
            }
            Ok(Self {
                parent,
                root: root_file,
                parent_path,
                root_name,
                parent_identity,
                root_identity,
                finished: false,
            })
        }

        /// Revalidate the pinned objects and the currently visible pathname.
        pub fn verify(&self) -> std::result::Result<(), TrustedWorkspaceError> {
            if self.finished {
                return Err(TrustedWorkspaceError::AlreadyFinalized);
            }
            let parent_identity = object_identity(&self.parent)?;
            let root_identity = object_identity(&self.root)?;
            if parent_identity != self.parent_identity || root_identity != self.root_identity {
                return Err(TrustedWorkspaceError::RootDrift);
            }
            let visible_parent = open_existing_acl_target(
                &self.parent_path,
                FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            )
            .map_err(|_| TrustedWorkspaceError::RootDrift)?;
            if object_identity(&visible_parent)? != self.parent_identity {
                return Err(TrustedWorkspaceError::RootDrift);
            }
            let visible_root = open_existing_child_for_verify(
                &visible_parent,
                &self.root_name,
                DELETE | FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            )?;
            if object_identity(&visible_root)? != self.root_identity {
                return Err(TrustedWorkspaceError::RootDrift);
            }
            Ok(())
        }

        /// Return the parent/root object identities captured when this lease was acquired.
        pub fn identity_fingerprint(&self) -> ((u32, u64, u32), (u32, u64, u32)) {
            (
                (
                    self.parent_identity.volume_serial,
                    self.parent_identity.file_index,
                    self.parent_identity.links,
                ),
                (
                    self.root_identity.volume_serial,
                    self.root_identity.file_index,
                    self.root_identity.links,
                ),
            )
        }

        /// Check that this lease names the same parent and root captured by an earlier owner.
        pub fn matches_identity(&self, fingerprint: ((u32, u64, u32), (u32, u64, u32))) -> bool {
            self.identity_fingerprint() == fingerprint
        }

        /// Produce the parent-process handle pin consumed by the elevated setup helper.
        pub fn setup_pin(
            &self,
        ) -> std::result::Result<TrustedWorkspaceSetupPin, TrustedWorkspaceError> {
            self.verify()?;
            let root_path = dunce::canonicalize(self.parent_path.join(&self.root_name))
                .map_err(|_| TrustedWorkspaceError::RootUnavailable)?;
            let root_handle = self.root.as_raw_handle() as usize;
            if root_handle == 0 || root_handle == INVALID_HANDLE_VALUE as usize {
                return Err(TrustedWorkspaceError::IdentityUnavailable);
            }
            Ok(TrustedWorkspaceSetupPin {
                parent_pid: unsafe { GetCurrentProcessId() },
                root_handle: root_handle as u64,
                root_path,
                root_identity: (
                    self.root_identity.volume_serial,
                    self.root_identity.file_index,
                    self.root_identity.links,
                ),
            })
        }

        /// Duplicate the pinned root handle for capability-relative inspection by the sandbox
        /// adapter without reopening the pathname or weakening the no-delete-sharing lease.
        pub fn duplicate_root_handle(&self) -> std::result::Result<File, TrustedWorkspaceError> {
            if self.finished {
                return Err(TrustedWorkspaceError::AlreadyFinalized);
            }
            self.root
                .try_clone()
                .map_err(|_| TrustedWorkspaceError::IdentityUnavailable)
        }

        /// Borrow the pinned root handle for resolver path validation while the lease is alive.
        pub(crate) fn root_handle(&self) -> &File {
            &self.root
        }

        /// Retain the root after a successful strict command.
        pub fn commit(&mut self) -> std::result::Result<(), TrustedWorkspaceError> {
            self.verify()?;
            self.finished = true;
            Ok(())
        }

        /// Move the owned root to a same-parent quarantine without recursively deleting it.
        ///
        /// This method intentionally does not consult a caller cancellation token.  Once a
        /// trusted command has stopped, rollback must either finish or return a recovery-needed
        /// error; cancelling cleanup would leave an ambiguous live root.
        pub fn rollback(&mut self) -> std::result::Result<(), TrustedWorkspaceError> {
            self.verify()?;
            let _quarantine = self.quarantine()?;
            self.finished = true;
            Ok(())
        }

        fn quarantine(&self) -> std::result::Result<PathBuf, TrustedWorkspaceError> {
            let mut rng = SmallRng::from_entropy();
            for _ in 0..MAX_QUARANTINE_ATTEMPTS {
                let nonce = rng.next_u64();
                let name = format!(".singularity-trusted-recovery-{nonce:016x}");
                let path = self.parent_path.join(&name);
                // `NtSetInformationFile` returns an NTSTATUS and does not guarantee that the
                // Win32 last-error slot is updated.  Retry only the kernel's no-replace collision
                // status; every other status is a stable quarantine failure.
                match rename_no_replace(
                    &self.root,
                    self.parent.as_raw_handle() as HANDLE,
                    OsStr::new(&name),
                ) {
                    Ok(()) => return Ok(path),
                    Err(status) if status == STATUS_OBJECT_NAME_COLLISION => continue,
                    Err(_) => return Err(TrustedWorkspaceError::QuarantineFailed),
                }
            }
            Err(TrustedWorkspaceError::QuarantineFailed)
        }
    }

    impl Drop for TrustedWorkspaceLease {
        fn drop(&mut self) {
            // A pending lease is intentionally not deleted from Drop.  Every caller must make
            // the commit/rollback decision explicitly so a panic or process crash cannot turn an
            // unknown root into a falsely successful cleanup.
        }
    }

    fn object_identity(file: &File) -> std::result::Result<ObjectIdentity, TrustedWorkspaceError> {
        let mut info = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) } == 0 {
            return Err(TrustedWorkspaceError::IdentityUnavailable);
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(TrustedWorkspaceError::ReparseUnsupported);
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(TrustedWorkspaceError::RootUnavailable);
        }
        Ok(ObjectIdentity {
            volume_serial: info.dwVolumeSerialNumber,
            file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            links: info.nNumberOfLinks,
        })
    }

    /// Duplicate and verify a parent-held trusted root handle in the current helper process.
    ///
    /// A dead parent, a reused PID/handle, or an object identity mismatch is rejected before the
    /// caller can perform any ACL or path side effect. Once duplicated, the returned handle keeps
    /// the root pinned even if the parent process exits.
    pub fn duplicate_setup_root_handle(
        pin: &TrustedWorkspaceSetupPin,
    ) -> std::result::Result<File, TrustedWorkspaceError> {
        if pin.parent_pid == 0 || pin.root_handle == 0 {
            return Err(TrustedWorkspaceError::IdentityUnavailable);
        }
        let source = unsafe {
            OpenProcess(
                PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pin.parent_pid,
            )
        };
        if source == 0 || source == INVALID_HANDLE_VALUE {
            return Err(TrustedWorkspaceError::IdentityUnavailable);
        }
        let source_pid = unsafe { GetProcessId(source) };
        if source_pid != pin.parent_pid {
            unsafe {
                CloseHandle(source);
            }
            return Err(TrustedWorkspaceError::IdentityUnavailable);
        }
        let mut duplicate: HANDLE = 0;
        let duplicated = unsafe {
            DuplicateHandle(
                source,
                pin.root_handle as HANDLE,
                GetCurrentProcess(),
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        };
        unsafe {
            CloseHandle(source);
        }
        if duplicated == 0 || duplicate == 0 || duplicate == INVALID_HANDLE_VALUE {
            if duplicate != 0 && duplicate != INVALID_HANDLE_VALUE {
                unsafe {
                    CloseHandle(duplicate);
                }
            }
            return Err(TrustedWorkspaceError::IdentityUnavailable);
        }
        let file = unsafe { File::from_raw_handle(duplicate as _) };
        let identity = object_identity(&file)?;
        if (identity.volume_serial, identity.file_index, identity.links) != pin.root_identity {
            return Err(TrustedWorkspaceError::RootDrift);
        }
        Ok(file)
    }

    impl ObjectIdentity {
        fn is_valid(self) -> bool {
            self.file_index != 0 && self.links != 0
        }
    }

    fn classify_open_error(error: impl std::fmt::Display) -> TrustedWorkspaceError {
        if error.to_string().contains("unsupported_reparse_acl_target") {
            TrustedWorkspaceError::ReparseUnsupported
        } else {
            TrustedWorkspaceError::RootUnavailable
        }
    }

    fn open_existing_child(
        parent: &File,
        name: &OsStr,
        desired_access: u32,
    ) -> std::result::Result<File, TrustedWorkspaceError> {
        let (file, _) = crate::path_safety::nt_open_relative(
            parent,
            name,
            desired_access,
            windows_sys::Wdk::Storage::FileSystem::FILE_OPEN,
            0,
        )
        .map_err(|_| TrustedWorkspaceError::RootDrift)?;
        Ok(file)
    }

    fn open_existing_child_for_verify(
        parent: &File,
        name: &OsStr,
        desired_access: u32,
    ) -> std::result::Result<File, TrustedWorkspaceError> {
        let mut wide = name.encode_wide().collect::<Vec<_>>();
        let byte_length = wide
            .len()
            .checked_mul(std::mem::size_of::<u16>())
            .and_then(|length| u16::try_from(length).ok())
            .ok_or(TrustedWorkspaceError::RootDrift)?;
        if wide.is_empty() || wide.iter().any(|value| matches!(*value, 0 | 47 | 58 | 92)) {
            return Err(TrustedWorkspaceError::RootDrift);
        }
        let mut object_name = windows_sys::Win32::Foundation::UNICODE_STRING {
            Length: byte_length,
            MaximumLength: byte_length,
            Buffer: wide.as_mut_ptr(),
        };
        let object_attributes = windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.as_raw_handle() as HANDLE,
            ObjectName: &mut object_name,
            Attributes: windows_sys::Win32::System::Kernel::OBJ_CASE_INSENSITIVE as u32,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut handle = windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        let mut io_status =
            unsafe { std::mem::zeroed::<windows_sys::Win32::System::IO::IO_STATUS_BLOCK>() };
        let status = unsafe {
            windows_sys::Wdk::Storage::FileSystem::NtCreateFile(
                &mut handle,
                (desired_access & !DELETE)
                    | FILE_READ_ATTRIBUTES
                    | windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE,
                &object_attributes,
                &mut io_status,
                std::ptr::null(),
                windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
                windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
                    | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE
                    | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE,
                windows_sys::Wdk::Storage::FileSystem::FILE_OPEN,
                windows_sys::Wdk::Storage::FileSystem::FILE_OPEN_REPARSE_POINT
                    | windows_sys::Wdk::Storage::FileSystem::FILE_SYNCHRONOUS_IO_NONALERT,
                std::ptr::null(),
                0,
            )
        };
        if status != windows_sys::Win32::Foundation::STATUS_SUCCESS {
            if handle != 0 && handle != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
                unsafe {
                    windows_sys::Win32::Foundation::CloseHandle(handle);
                }
            }
            return Err(TrustedWorkspaceError::RootDrift);
        }
        if handle == 0 || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(TrustedWorkspaceError::RootDrift);
        }
        Ok(unsafe { File::from_raw_handle(handle as _) })
    }

    fn rename_no_replace(file: &File, parent: HANDLE, name: &OsStr) -> Result<(), i32> {
        let wide = name.encode_wide().collect::<Vec<_>>();
        let header = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
        let Some(bytes) = header.checked_add(wide.len().saturating_mul(std::mem::size_of::<u16>()))
        else {
            return Err(STATUS_INVALID_PARAMETER);
        };
        let mut storage = vec![0u8; bytes];
        let info = storage.as_mut_ptr() as *mut FILE_RENAME_INFORMATION;
        unsafe {
            (*info).Anonymous = FILE_RENAME_INFORMATION_0 { ReplaceIfExists: 0 };
            (*info).RootDirectory = parent;
            (*info).FileNameLength = u32::try_from(wide.len().saturating_mul(2)).unwrap_or(0);
            std::ptr::copy_nonoverlapping(wide.as_ptr(), (*info).FileName.as_mut_ptr(), wide.len());
            let mut io_status = std::mem::zeroed();
            let status = NtSetInformationFile(
                file.as_raw_handle() as HANDLE,
                &mut io_status,
                info.cast(),
                u32::try_from(bytes).unwrap_or(0),
                10,
            );
            if status == STATUS_SUCCESS {
                Ok(())
            } else {
                Err(status)
            }
        }
    }

    /// Abort cleanup for a just-created empty root when identity admission fails; rollback never
    /// calls this helper and only quarantines an already-admitted tree.
    fn mark_delete(file: &File) -> std::result::Result<(), TrustedWorkspaceError> {
        let disposition = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_FLAG_DELETE
                | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
                | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        };
        let deleted = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle() as HANDLE,
                FileDispositionInfoEx,
                &disposition as *const _ as *const c_void,
                std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
            )
        };
        if deleted == 0 {
            Err(TrustedWorkspaceError::CleanupFailed)
        } else {
            Ok(())
        }
    }

    pub use TrustedWorkspaceError as Error;
}

#[cfg(target_os = "windows")]
pub use windows::{
    Error as TrustedWorkspaceError, TrustedWorkspaceLease, TrustedWorkspaceSetupPin,
    duplicate_setup_root_handle,
};

#[cfg(all(target_os = "windows", test))]
mod tests {
    use super::{TrustedWorkspaceError, TrustedWorkspaceLease, duplicate_setup_root_handle};
    use std::fs;
    use std::os::windows::fs::symlink_dir;
    use std::path::{Path, PathBuf};

    fn recovery_root(parent: &Path) -> Option<PathBuf> {
        fs::read_dir(parent)
            .expect("read recovery parent")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .starts_with(".singularity-trusted-recovery-")
                })
            })
    }

    #[test]
    fn lease_pins_plain_root_and_commit_retains_it() {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("workspace");
        fs::create_dir(&root).expect("workspace");

        let mut lease = TrustedWorkspaceLease::acquire(&root).expect("acquire");
        lease.verify().expect("verify");
        lease.commit().expect("commit");

        assert!(root.is_dir());
    }

    #[test]
    fn setup_pin_duplicates_root_and_survives_parent_lease_drop() {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("workspace");
        fs::create_dir(&root).expect("root");
        let lease = TrustedWorkspaceLease::acquire(&root).expect("acquire");
        let pin = lease.setup_pin().expect("setup pin");
        let duplicate = duplicate_setup_root_handle(&pin).expect("duplicate pinned root");
        drop(lease);
        assert!(duplicate.metadata().expect("metadata").is_dir());
    }

    #[test]
    fn lease_rejects_reparse_root_and_parent() {
        let parent = tempfile::tempdir().expect("parent");
        let target = parent.path().join("target");
        let root_alias = parent.path().join("root-alias");
        fs::create_dir(&target).expect("target");
        if symlink_dir(&target, &root_alias).is_err() {
            return;
        }
        assert_eq!(
            TrustedWorkspaceLease::acquire(&root_alias).expect_err("reparse root"),
            TrustedWorkspaceError::ReparseUnsupported
        );

        let parent_target = parent.path().join("parent-target");
        let parent_alias = parent.path().join("parent-alias");
        let root = parent_alias.join("workspace");
        fs::create_dir(&parent_target).expect("parent target");
        fs::create_dir(parent_target.join("workspace")).expect("workspace");
        if symlink_dir(&parent_target, &parent_alias).is_err() {
            return;
        }
        assert_eq!(
            TrustedWorkspaceLease::acquire(&root).expect_err("reparse parent"),
            TrustedWorkspaceError::ReparseUnsupported
        );
    }

    #[test]
    fn lease_blocks_path_replacement_while_held() {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("workspace");
        let replacement = parent.path().join("replacement");
        fs::create_dir(&root).expect("workspace");
        let mut lease = TrustedWorkspaceLease::acquire(&root).expect("acquire");

        assert!(fs::rename(&root, &replacement).is_err());
        lease.rollback().expect("rollback");
        assert!(!root.exists());
    }

    #[test]
    fn rollback_quarantines_owned_tree_and_retains_recovery() {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("workspace");
        fs::create_dir(&root).expect("workspace");
        fs::write(root.join("created.txt"), "created").expect("created file");

        {
            let mut lease = TrustedWorkspaceLease::acquire(&root).expect("acquire");
            lease.rollback().expect("rollback");
        }

        assert!(!root.exists());
        assert!(recovery_root(parent.path()).is_some_and(|path| path.is_dir()));
    }

    #[test]
    fn rollback_quarantines_hardlink_tree_and_preserves_external_target() {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("workspace");
        let external = parent.path().join("external.txt");
        fs::create_dir(&root).expect("workspace");
        fs::write(&external, "external").expect("external");
        if fs::hard_link(&external, root.join("linked.txt")).is_err() {
            return;
        }

        let mut lease = TrustedWorkspaceLease::acquire(&root).expect("acquire");
        lease.rollback().expect("quarantine root");
        assert!(!root.exists());
        assert_eq!(
            fs::read_to_string(&external).expect("external content"),
            "external"
        );
        assert!(recovery_root(parent.path()).is_some_and(|path| path.is_dir()));
    }

    #[test]
    fn rollback_quarantines_reparse_child_and_preserves_target() {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("workspace");
        let target = parent.path().join("target");
        fs::create_dir(&root).expect("workspace");
        fs::create_dir(&target).expect("target");
        if symlink_dir(&target, root.join("linked-dir")).is_err() {
            return;
        }

        let mut lease = TrustedWorkspaceLease::acquire(&root).expect("acquire");
        lease.rollback().expect("quarantine root");
        assert!(!root.exists());
        assert!(target.is_dir());
        assert!(recovery_root(parent.path()).is_some_and(|path| path.is_dir()));
    }

    #[test]
    fn create_rejects_existing_root_without_adopting_it() {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("workspace");
        fs::create_dir(&root).expect("workspace");

        assert_eq!(
            TrustedWorkspaceLease::create(&root).expect_err("existing root must be rejected"),
            TrustedWorkspaceError::RootUnavailable
        );
        assert!(root.is_dir());
    }

    #[test]
    fn create_atomically_owns_new_root() {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("workspace");

        let mut lease = TrustedWorkspaceLease::create(&root).expect("create");
        assert!(root.is_dir());
        lease.rollback().expect("rollback");
        assert!(!root.exists());
        assert!(recovery_root(parent.path()).is_some_and(|path| path.is_dir()));
    }
}
