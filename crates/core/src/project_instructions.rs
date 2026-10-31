//! 完整加载全局与项目 AGENTS.md：先读应用主目录，再从项目根逐层读到工作目录。

use std::io;
use std::path::{Path, PathBuf};

pub(crate) const PROJECT_INSTRUCTIONS_FILE_NAME: &str = "AGENTS.md";

/// 从用户数据目录、以及项目根到 cwd 的各层目录加载指令。
pub fn load_agent_instructions(cwd: &Path, home: &Path) -> Result<Vec<InstructionFile>, String> {
    let canonical = crate::canonicalize_workspace(cwd)?;
    let cwd = canonical.as_path();
    let root = crate::workspace::project_root(cwd)?;
    let canonical_home = std::fs::canonicalize(home)
        .map_err(|error| format!("project_instruction_directory_read_failed:{}:{error}", home.display()))?;
    let home_key = crate::workspace_key(&canonical_home);
    // 项目目录等于数据根时已在首项作为全局指令加载，跳过重复项。
    let directories = std::iter::once((home.to_path_buf(), true)).chain(
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
                return Err(format!("project_instruction_file_read_failed:{}:{error}", path.display()));
            }
        };
        if !content.trim().is_empty() {
            files.push(InstructionFile { path, content, global });
        }
    }
    Ok(files)
}

/// 一份实际加载的指令文件。
#[derive(Debug, Clone)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub content: String,
    pub global: bool,
}

/// 返回 workspace root 到 cwd 之间需要检查指令的目录，两端都包含。
fn instruction_directories(workspace_root: &Path, cwd: &Path) -> Vec<PathBuf> {
    // workspace root 是 cwd 的祖先，strip_prefix 会成功。
    let depth = cwd
        .strip_prefix(workspace_root)
        .expect("cwd 必在 workspace root 之下")
        .components()
        .count();
    let mut directories = cwd.ancestors().take(depth + 1).map(Path::to_path_buf).collect::<Vec<_>>();
    directories.reverse();
    directories
}
