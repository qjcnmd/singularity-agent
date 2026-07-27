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

/// Resolves the nearest existing ancestor while preserving any missing final components.
pub fn canonicalize_path_allow_missing(path: &Path) -> PathBuf {
    let mut cursor = path.to_path_buf();
    let mut missing_tail = Vec::new();
    loop {
        if let Ok(mut canonical) = dunce::canonicalize(&cursor) {
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

#[cfg(test)]
mod tests {
    use super::canonical_path_key;
    use super::canonicalize_path_allow_missing;
    use super::lexical_path_key;
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
}
