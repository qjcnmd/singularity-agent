use std::path::Path;
use std::path::PathBuf;

pub fn canonicalize_path(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Returns a stable Windows path identity without the Win32 verbatim prefix.
///
/// `dunce::canonicalize` can retain `\\?\` for long paths. That prefix changes
/// Win32 parsing rules, but it does not identify a different filesystem object.
pub fn normalized_path_text(path: &Path) -> String {
    let mut text = path.to_string_lossy().replace('/', "\\");
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        text = format!(r"\\{rest}");
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        text = rest.to_string();
    }
    while text.len() > 3 && text.ends_with('\\') {
        text.pop();
    }
    text
}

pub fn lexical_path_key(path: &Path) -> String {
    normalized_path_text(path)
        .replace('\\', "/")
        .to_ascii_lowercase()
}

pub fn canonical_path_key(path: &Path) -> String {
    lexical_path_key(&canonicalize_path_allow_missing(path))
}

/// Expands an existing Windows short-name alias without changing its path form.
#[cfg(windows)]
pub fn expand_windows_path_alias(path: &Path) -> PathBuf {
    long_path_name(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Resolves the nearest existing ancestor while preserving any missing final components.
pub fn canonicalize_path_allow_missing(path: &Path) -> PathBuf {
    let mut cursor = path.to_path_buf();
    let mut missing_tail = Vec::new();
    loop {
        if let Ok(mut canonical) = dunce::canonicalize(&cursor) {
            #[cfg(windows)]
            {
                canonical = expand_windows_path_alias(&canonical);
            }
            for component in missing_tail.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
        let Some(file_name) = cursor.file_name().map(ToOwned::to_owned) else {
            return path.to_path_buf();
        };
        let Some(parent) = cursor.parent() else {
            return path.to_path_buf();
        };
        missing_tail.push(file_name);
        cursor = parent.to_path_buf();
    }
}

#[cfg(windows)]
fn long_path_name(path: &Path) -> std::io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    use windows_sys::Win32::Storage::FileSystem::GetLongPathNameW;

    let ordinary = PathBuf::from(normalized_path_text(path));
    let mut input = ordinary.as_os_str().encode_wide().collect::<Vec<_>>();
    input.push(0);
    let mut output = vec![0_u16; 260];
    loop {
        // SAFETY: `input` is NUL-terminated and immutable for the call. `output`
        // owns `len` writable UTF-16 elements, and the API receives that exact
        // capacity. A successful result is decoded only within the returned size.
        let written = unsafe {
            GetLongPathNameW(
                input.as_ptr(),
                output.as_mut_ptr(),
                output.len().try_into().unwrap_or(u32::MAX),
            )
        };
        if written == 0 {
            let error = std::io::Error::last_os_error();
            let expanded = expand_remaining_short_components(&ordinary)?;
            if expanded == ordinary {
                return Err(error);
            }
            return preserve_windows_path_form(path, &expanded);
        }
        let written = written as usize;
        if written < output.len() {
            output.truncate(written);
            let expanded =
                expand_remaining_short_components(&PathBuf::from(OsString::from_wide(&output)))?;
            return preserve_windows_path_form(path, &expanded);
        }
        output.resize(written.saturating_add(1), 0);
    }
}

#[cfg(windows)]
fn preserve_windows_path_form(original: &Path, expanded: &Path) -> std::io::Result<PathBuf> {
    let original = original.to_string_lossy();
    let expanded = expanded.to_string_lossy();
    if original.starts_with(r"\\?\UNC\") {
        let expanded = expanded.strip_prefix(r"\\").unwrap_or(&expanded);
        return Ok(PathBuf::from(format!(r"\\?\UNC\{expanded}")));
    }
    if original.starts_with(r"\\?\") && !expanded.starts_with(r"\\?\") {
        return Ok(PathBuf::from(format!(r"\\?\{expanded}")));
    }
    Ok(PathBuf::from(expanded.as_ref()))
}

#[cfg(windows)]
fn expand_remaining_short_components(path: &Path) -> std::io::Result<PathBuf> {
    let mut probe = PathBuf::new();
    let mut expanded = PathBuf::new();
    for component in path.components() {
        if let std::path::Component::Normal(name) = component {
            if is_short_name_alias(name) {
                expanded.push(long_entry_name(&probe, name)?);
            } else {
                expanded.push(name);
            }
            probe.push(name);
        } else {
            probe.push(component.as_os_str());
            expanded.push(component.as_os_str());
        }
    }
    Ok(expanded)
}

#[cfg(windows)]
fn is_short_name_alias(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_uppercase();
    let stem = name.split('.').next().unwrap_or_default();
    stem.rsplit_once('~').is_some_and(|(prefix, suffix)| {
        !prefix.is_empty()
            && !suffix.is_empty()
            && suffix.len() <= 6
            && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

#[cfg(windows)]
fn long_entry_name(
    parent: &Path,
    short_name: &std::ffi::OsStr,
) -> std::io::Result<std::ffi::OsString> {
    use std::mem::MaybeUninit;
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Foundation::{ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        FindClose, FindFirstFileW, FindNextFileW, WIN32_FIND_DATAW,
    };

    let search = parent.join("*");
    let mut input = search.as_os_str().encode_wide().collect::<Vec<_>>();
    input.push(0);
    let mut data = MaybeUninit::<WIN32_FIND_DATAW>::uninit();
    // SAFETY: `input` is NUL-terminated and immutable for the call. `data`
    // points to writable storage for the documented output structure.
    let handle = unsafe { FindFirstFileW(input.as_ptr(), data.as_mut_ptr()) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let mut result = None;
    loop {
        // SAFETY: the successful first/next search call initialized `data`,
        // and the reference is not retained across the next write.
        let entry = unsafe { data.assume_init_ref() };
        let long_name = wide_name(&entry.cFileName);
        let alternate_name = wide_name(&entry.cAlternateFileName);
        if alternate_name
            .to_string_lossy()
            .eq_ignore_ascii_case(&short_name.to_string_lossy())
        {
            result = Some(long_name);
            break;
        }
        // SAFETY: `handle` remains live and `data` is writable output storage.
        if unsafe { FindNextFileW(handle, data.as_mut_ptr()) } == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
                // SAFETY: `handle` is the live search handle returned above.
                unsafe { FindClose(handle) };
                return Err(error);
            }
            break;
        }
    }
    // SAFETY: `handle` is the live search handle returned above and is closed once.
    if unsafe { FindClose(handle) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    match result {
        Some(long_name) => Ok(long_name),
        None => long_directory_name_by_identity(parent, short_name),
    }
}

#[cfg(windows)]
fn wide_name(value: &[u16]) -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt as _;

    let length = value
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(value.len());
    std::ffi::OsString::from_wide(&value[..length])
}

#[cfg(windows)]
/// Resolves directory aliases on filesystems that omit `cAlternateFileName`.
fn long_directory_name_by_identity(
    parent: &Path,
    short_name: &std::ffi::OsStr,
) -> std::io::Result<std::ffi::OsString> {
    let target_identity = directory_identity(&parent.join(short_name))?;
    let mut result = None;
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        if name
            .to_string_lossy()
            .eq_ignore_ascii_case(&short_name.to_string_lossy())
        {
            continue;
        }
        let Ok(identity) = directory_identity(&entry.path()) else {
            continue;
        };
        if identity == target_identity && result.replace(name).is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "short-name directory identity is ambiguous",
            ));
        }
    }
    result.ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
}

#[cfg(windows)]
fn directory_identity(path: &Path) -> std::io::Result<(u32, u64)> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    };

    let mut options = std::fs::OpenOptions::new();
    options
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    let directory = options.open(path)?;
    if !directory.metadata()?.is_dir() {
        return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
    }
    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the file keeps the handle live and `information` is writable output storage.
    if unsafe {
        GetFileInformationByHandle(directory.as_raw_handle() as isize, information.as_mut_ptr())
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a successful call initialized the complete output structure.
    let information = unsafe { information.assume_init() };
    let volume = information.dwVolumeSerialNumber;
    let index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    if volume == 0 || index == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory identity is unavailable",
        ));
    }
    Ok((volume, index))
}

#[cfg(test)]
mod tests {
    use super::canonical_path_key;
    use super::canonicalize_path_allow_missing;
    #[cfg(windows)]
    use super::expand_remaining_short_components;
    use super::lexical_path_key;
    #[cfg(windows)]
    use super::long_path_name;
    use super::normalized_path_text;
    use pretty_assertions::assert_eq;
    use std::path::Path;
    use std::path::PathBuf;

    #[test]
    fn canonical_path_key_normalizes_case_and_separators() {
        let windows_style = Path::new(r"C:\Users\Dev\Repo");
        let slash_style = Path::new("c:/users/dev/repo");

        assert_eq!(
            canonical_path_key(windows_style),
            canonical_path_key(slash_style)
        );
    }

    #[test]
    fn verbatim_disk_paths_share_their_ordinary_identity() {
        assert_eq!(
            lexical_path_key(Path::new(r"\\?\C:\Users\Dev\Repo")),
            lexical_path_key(Path::new(r"C:\Users\Dev\Repo"))
        );
    }

    #[test]
    fn verbatim_unc_paths_share_their_ordinary_identity() {
        assert_eq!(
            normalized_path_text(Path::new(r"\\?\UNC\server\share\repo")),
            r"\\server\share\repo"
        );
    }

    #[test]
    fn missing_state_leaf_uses_canonical_existing_parent_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join("state");
        std::fs::create_dir(&state_dir).expect("create state directory");
        let ordinary = state_dir.join("future.json");
        let verbatim = PathBuf::from(format!(r"\\?\{}", ordinary.display()));

        assert_eq!(
            lexical_path_key(&canonicalize_path_allow_missing(&ordinary)),
            lexical_path_key(&canonicalize_path_allow_missing(&verbatim))
        );
    }

    #[cfg(windows)]
    #[test]
    fn missing_tail_uses_the_long_identity_of_an_existing_short_name_ancestor() {
        let short_program_files = Path::new(r"C:\PROGRA~1");
        if !short_program_files.exists() {
            return;
        }

        let short = short_program_files
            .join("SingularityMissing")
            .join("state.json");
        let long = Path::new(r"C:\Program Files")
            .join("SingularityMissing")
            .join("state.json");

        assert_eq!(
            lexical_path_key(&canonicalize_path_allow_missing(&short)),
            lexical_path_key(&canonicalize_path_allow_missing(&long))
        );
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_short_name_is_expanded_without_changing_path_form() {
        let short_program_files = Path::new(r"C:\PROGRA~1");
        if !short_program_files.exists() {
            return;
        }

        let expanded =
            long_path_name(Path::new(r"\\?\C:\PROGRA~1")).expect("expand verbatim short name");

        assert!(expanded.to_string_lossy().starts_with(r"\\?\"));
        assert_eq!(
            lexical_path_key(&expanded),
            lexical_path_key(Path::new(r"C:\Program Files"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn remaining_short_component_uses_the_entry_long_name() {
        let short_program_files = Path::new(r"C:\PROGRA~1");
        if !short_program_files.exists() {
            return;
        }

        assert_eq!(
            lexical_path_key(
                &expand_remaining_short_components(short_program_files)
                    .expect("expand remaining short component")
            ),
            lexical_path_key(Path::new(r"C:\Program Files"))
        );
    }
}
