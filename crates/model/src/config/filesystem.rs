//! Filesystem trust-boundary seam for provider configuration.
//!
//! Path normalization, reparse checks, private-file handling and atomic
//! replacement are kept behind the configuration module's existing helpers.

use super::*;

pub(super) enum BoundedTextError {
    TooLarge,
    Read(std::io::Error),
}

impl BoundedTextError {
    pub(super) fn is_invalid_data(&self) -> bool {
        match self {
            Self::TooLarge => true,
            Self::Read(error) => error.kind() == std::io::ErrorKind::InvalidData,
        }
    }
}

pub(super) fn read_bounded_text(path: &Path, max_bytes: usize) -> Result<String, BoundedTextError> {
    let mut file = std::fs::File::open(path).map_err(BoundedTextError::Read)?;
    read_bounded_text_from_file(&mut file, max_bytes)
}

pub(super) fn read_bounded_text_from_file(
    file: &mut std::fs::File,
    max_bytes: usize,
) -> Result<String, BoundedTextError> {
    use std::io::Read;
    let max_bytes_u64 = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    let metadata_len = file.metadata().map_err(BoundedTextError::Read)?.len();
    if metadata_len > max_bytes_u64 {
        return Err(BoundedTextError::TooLarge);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata_len).unwrap_or(max_bytes));
    file.take(max_bytes_u64.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(BoundedTextError::Read)?;
    if bytes.len() > max_bytes {
        return Err(BoundedTextError::TooLarge);
    }
    String::from_utf8(bytes).map_err(|_| {
        BoundedTextError::Read(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "input was not UTF-8",
        ))
    })
}

pub(super) fn write_json_file(
    path: &Path,
    contents: &str,
    secret: bool,
) -> Result<(), ProviderError> {
    let parent = path
        .parent()
        .ok_or_else(|| super::user::user_config_error("user provider config path has no parent"))?;
    super::user::ensure_no_reparse_components(parent, true)?;
    std::fs::create_dir_all(parent).map_err(|_| {
        super::user::user_config_error("user provider config directory could not be created")
    })?;
    super::user::ensure_no_reparse_components(parent, false)?;
    if super::user::path_exists_or_missing(
        path,
        "user provider config path could not be inspected",
    )? {
        super::user::ensure_no_reparse_components(path, false)?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    let temporary = parent.join(format!(".{file_name}.tmp-{}", Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = if secret {
            let file = super::user::create_private_secret_file(&temporary)?;
            super::user::ensure_private_secret_handle(&file)?;
            file
        } else {
            let mut options = std::fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(&temporary).map_err(|_| {
                super::user::user_config_error("user provider config file could not be opened")
            })?
        };
        use std::io::Write;
        file.write_all(contents.as_bytes()).map_err(|_| {
            super::user::user_config_error("user provider config file could not be written")
        })?;
        file.sync_all().map_err(|_| {
            super::user::user_config_error("user provider config file could not be synced")
        })?;
        if secret {
            super::user::ensure_private_secret_handle(&file)?;
        }
        drop(file);
        atomic_replace_file(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn atomic_replace_file(from: &Path, to: &Path) -> Result<(), ProviderError> {
    #[cfg(windows)]
    {
        windows_atomic_replace(from, to)?;
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to).map_err(|_| {
            super::user::user_config_error("user provider config file could not be committed")
        })?;
    }
    Ok(())
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_atomic_replace(from: &Path, to: &Path) -> Result<(), ProviderError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let mut from_wide = from.as_os_str().encode_wide().collect::<Vec<_>>();
    from_wide.push(0);
    let mut to_wide = to.as_os_str().encode_wide().collect::<Vec<_>>();
    to_wide.push(0);
    if unsafe {
        MoveFileExW(
            from_wide.as_ptr(),
            to_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(super::user::user_config_error(
            "user provider config file could not be committed",
        ));
    }
    Ok(())
}
