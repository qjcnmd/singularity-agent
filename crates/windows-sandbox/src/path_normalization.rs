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
            // GetLongPathNameW rejects some otherwise resolvable spellings (for example
            // certain alias forms on server SKUs). Fall back to per-component identity
            // expansion; only propagate the original error when nothing was expanded.
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

/// Reapplies the input's verbatim/UNC form to an expanded ordinary-form path.
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
        probe.push(component.as_os_str());
        if let std::path::Component::Normal(name) = component {
            if is_short_name_alias(name) {
                expanded.push(long_entry_name(&probe)?);
            } else {
                expanded.push(name);
            }
        } else {
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
fn long_entry_name(path: &Path) -> std::io::Result<std::ffi::OsString> {
    use std::mem::MaybeUninit;
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{FindClose, FindFirstFileW, WIN32_FIND_DATAW};

    let mut input = path.as_os_str().encode_wide().collect::<Vec<_>>();
    input.push(0);
    let mut data = MaybeUninit::<WIN32_FIND_DATAW>::uninit();
    // SAFETY: `input` is NUL-terminated and immutable for the call. `data`
    // points to writable storage for the documented output structure.
    let handle = unsafe { FindFirstFileW(input.as_ptr(), data.as_mut_ptr()) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a valid search handle means `FindFirstFileW` initialized `data`.
    let data = unsafe { data.assume_init() };
    // SAFETY: `handle` is the live search handle returned above and is closed once.
    if unsafe { FindClose(handle) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let length = data
        .cFileName
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(data.cFileName.len());
    Ok(std::ffi::OsString::from_wide(&data.cFileName[..length]))
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
