//! Protected store-file opening and handle/path identity validation.

use super::*;

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_file_identity {
    use std::fs::File;
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::windows::io::{AsRawHandle, RawHandle};

    #[repr(C)]
    struct FileTime {
        _low_date_time: u32,
        _high_date_time: u32,
    }

    #[repr(C)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        _creation_time: FileTime,
        _last_access_time: FileTime,
        _last_write_time: FileTime,
        volume_serial_number: u32,
        _file_size_high: u32,
        _file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "GetFileInformationByHandle"]
        fn get_file_information_by_handle(
            file: RawHandle,
            information: *mut ByHandleFileInformation,
        ) -> i32;
    }

    pub(super) fn read(file: &File) -> io::Result<(u32, u64, u32, u32)> {
        let mut information = MaybeUninit::<ByHandleFileInformation>::zeroed();
        // SAFETY: `file` owns a live Windows handle and `information` points to
        // writable storage of the exact C ABI layout required by the API.
        let result = unsafe {
            get_file_information_by_handle(file.as_raw_handle(), information.as_mut_ptr())
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Windows initialized the complete structure when the call
        // returned nonzero.
        let information = unsafe { information.assume_init() };
        let file_index =
            (u64::from(information.file_index_high) << 32) | u64::from(information.file_index_low);
        Ok((
            information.volume_serial_number,
            file_index,
            information.number_of_links,
            information.file_attributes,
        ))
    }
}

#[derive(Debug)]
pub(crate) struct StoreIdentityGuard {
    pub(crate) path: PathBuf,
    pub(crate) identity: StoreFileIdentity,
    pub(crate) _file: File,
    pub(crate) parent: CapabilityDir,
    pub(crate) file_name: OsString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoreFileIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    Windows {
        volume_serial_number: u32,
        file_index: u64,
    },
    #[cfg(not(any(unix, windows)))]
    Unsupported,
}

impl StoreIdentityGuard {
    pub(crate) fn open(path: &Path, create: bool) -> StoreResult<Self> {
        if path == Path::new(":memory:") {
            return Err(StoreError::InvalidState(
                "file identity guard cannot protect an in-memory store".to_string(),
            ));
        }
        let initial = open_protected_store_file(path, create)?;
        let identity = checked_store_file_identity(&initial.file)?;
        let canonical_path = std::fs::canonicalize(&initial.absolute_path).map_err(|error| {
            StoreError::InvalidState(format!("cannot canonicalize protected store path: {error}"))
        })?;
        let canonical = open_protected_store_file(&canonical_path, false)?;
        let path_identity = checked_store_file_identity(&canonical.file)?;
        if identity != path_identity {
            return Err(StoreError::InvalidState(
                "store path identity changed while opening".to_string(),
            ));
        }
        Ok(Self {
            path: canonical_path,
            identity,
            _file: initial.file,
            parent: canonical.parent,
            file_name: canonical.file_name,
        })
    }

    pub(crate) fn verify(&self) -> StoreResult<()> {
        let file_identity = checked_store_file_identity(&self._file)?;
        let parent_file = open_store_file_at(&self.parent, &self.file_name, false)?;
        let parent_identity = checked_store_file_identity(&parent_file)?;
        let namespace_file = open_protected_store_file(&self.path, false)?;
        let namespace_identity = checked_store_file_identity(&namespace_file.file)?;
        if file_identity != self.identity
            || parent_identity != self.identity
            || namespace_identity != self.identity
        {
            return Err(StoreError::InvalidState(
                "store file identity changed after initialization".to_string(),
            ));
        }
        Ok(())
    }
}

pub(crate) struct ProtectedStoreFile {
    pub(crate) absolute_path: PathBuf,
    pub(crate) parent: CapabilityDir,
    pub(crate) file_name: OsString,
    pub(crate) file: File,
}

pub(crate) fn open_protected_store_file(
    path: &Path,
    create: bool,
) -> StoreResult<ProtectedStoreFile> {
    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                StoreError::InvalidState(format!("cannot resolve current directory: {error}"))
            })?
            .join(path)
    };
    let mut root = PathBuf::new();
    let mut names = Vec::<OsString>::new();
    for component in absolute_path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                if !names.is_empty() {
                    return Err(StoreError::InvalidState(
                        "store path contains a misplaced root component".to_string(),
                    ));
                }
                root.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(StoreError::InvalidState(
                    "store path must not contain parent-directory components".to_string(),
                ));
            }
            Component::Normal(name) => names.push(name.to_os_string()),
        }
    }
    let file_name = names.pop().ok_or_else(|| {
        StoreError::InvalidState("store path must include a file name".to_string())
    })?;
    if root.as_os_str().is_empty() {
        return Err(StoreError::InvalidState(
            "store path must resolve from an absolute filesystem root".to_string(),
        ));
    }
    let mut parent =
        CapabilityDir::open_ambient_dir(&root, ambient_authority()).map_err(|error| {
            StoreError::InvalidState(format!("cannot open store filesystem root: {error}"))
        })?;
    for name in names {
        parent = parent.open_dir_nofollow(&name).map_err(|error| {
            StoreError::InvalidState(format!(
                "cannot open store parent without following links: {error}"
            ))
        })?;
    }
    let file = open_store_file_at(&parent, &file_name, create)?;
    Ok(ProtectedStoreFile {
        absolute_path,
        parent,
        file_name,
        file,
    })
}

pub(crate) fn open_store_file_at(
    parent: &CapabilityDir,
    name: &OsString,
    create: bool,
) -> StoreResult<File> {
    let mut options = CapabilityOpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .follow(FollowSymlinks::No);
    parent
        .open_with(name, &options)
        .map(cap_std::fs::File::into_std)
        .map_err(|error| {
            StoreError::InvalidState(format!(
                "cannot open protected store file without following links: {error}"
            ))
        })
}

#[cfg(all(test, windows))]
pub(crate) fn open_store_file(path: &Path, create: bool) -> StoreResult<File> {
    open_protected_store_file(path, create).map(|protected| protected.file)
}

pub(crate) fn checked_store_file_identity(file: &File) -> StoreResult<StoreFileIdentity> {
    let metadata = file.metadata().map_err(|error| {
        StoreError::InvalidState(format!("cannot inspect protected store file: {error}"))
    })?;
    if !metadata.is_file() {
        return Err(StoreError::InvalidState(
            "store path must identify a regular file".to_string(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() > 1 {
            return Err(StoreError::InvalidState(
                "store file must not have multiple hard links".to_string(),
            ));
        }
        Ok(StoreFileIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        let (volume_serial_number, file_index, number_of_links, file_attributes) =
            windows_file_identity::read(file).map_err(|error| {
                StoreError::InvalidState(format!(
                    "cannot inspect Windows store file identity: {error}"
                ))
            })?;
        if file_attributes & 0x0000_0400 != 0 {
            return Err(StoreError::InvalidState(
                "store file must not be a reparse point".to_string(),
            ));
        }
        if number_of_links != 1 {
            return Err(StoreError::InvalidState(
                "store file must not have multiple hard links".to_string(),
            ));
        }
        Ok(StoreFileIdentity::Windows {
            volume_serial_number,
            file_index,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(StoreError::InvalidState(
            "store file identity is unsupported on this platform".to_string(),
        ))
    }
}

// 配置 SQLite 的并发、外键、WAL 与安全删除 pragma。
