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
    lexical_path_key(&canonicalize_path(path))
}

#[cfg(test)]
mod tests {
    use super::canonical_path_key;
    use super::lexical_path_key;
    use super::normalized_path_text;
    use pretty_assertions::assert_eq;
    use std::path::Path;

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
}
