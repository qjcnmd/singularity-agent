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
    let home_identity = crate::CanonicalWorkspacePath::from_saved(canonical_home)?;
    let mut directories = vec![home.to_path_buf()];
    for directory in instruction_directories(&root, cwd) {
        // 该目录就是数据根，已在列表中，跳过。
        if crate::CanonicalWorkspacePath::from_saved(&directory)? != home_identity {
            directories.push(directory);
        }
    }
    let mut files = Vec::new();
    for directory in directories {
        let path = directory.join(PROJECT_INSTRUCTIONS_FILE_NAME);
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!("project_instruction_file_read_failed:{}:{error}", path.display()));
            }
        };
        if !content.trim().is_empty() {
            files.push(InstructionFile { path, content, global: directory == home });
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
