use std::io;
use std::path::{Path, PathBuf};

pub(crate) const PROJECT_INSTRUCTIONS_FILE_NAME: &str = "AGENTS.md";

pub fn load_agent_instructions(cwd: &Path, home: &Path) -> Result<Vec<InstructionFile>, String> {
    let canonical = crate::canonicalize_workspace(cwd)?;
    let cwd = canonical.as_path();
    let root = crate::workspace::project_root(cwd)?;
    let (global_dir, home_key) = match std::fs::canonicalize(home) {
        Ok(canonical_home) => {
            let key = crate::workspace_key(&canonical_home);
            (canonical_home, key)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            (home.to_path_buf(), crate::workspace_key(home))
        }
        Err(error) => {
            return Err(format!(
                "cannot inspect project instruction directory ({}): {error}",
                crate::display_path(home),
            ));
        }
    };
    let directories = std::iter::once((global_dir, true)).chain(
        instruction_directories(&root, cwd)
            .into_iter()
            .filter(|directory| crate::workspace_key(directory) != home_key)
            .map(|directory| (directory, false)),
    );
    let mut files = Vec::new();
    for (directory, global) in directories {
        let path = directory.join(PROJECT_INSTRUCTIONS_FILE_NAME);
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "cannot read project instruction file ({}): {error}",
                    crate::display_path(&path),
                ));
            }
        };
        let stripped = content.strip_prefix('\u{feff}').unwrap_or(&content);
        if !stripped.trim().is_empty() {
            let content = if stripped.len() == content.len() {
                content
            } else {
                stripped.to_owned()
            };
            files.push(InstructionFile { path, content, global });
        }
    }
    Ok(files)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub content: String,
    pub global: bool,
}

fn instruction_directories(workspace_root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let depth = cwd
        .strip_prefix(workspace_root)
        .expect("cwd 必在 workspace root 之下")
        .components()
        .count();
    let mut directories = cwd.ancestors().take(depth + 1).map(Path::to_path_buf).collect::<Vec<_>>();
    directories.reverse();
    directories
}
