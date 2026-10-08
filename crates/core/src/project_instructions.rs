//! 全局与项目指令文件（AGENTS.md）的加载与合并：先读应用主目录，再从 workspace root
//! 逐层检索到 cwd 按层级合并；单文件超 32KB 取预算内前缀，合并总预算 64KB（含分隔符）
//! 用尽即停，被放弃的内容记 truncated()，I/O 错误和预算内前缀的非法 UTF-8 直接失败。

use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub(crate) const PROJECT_INSTRUCTIONS_FILE_NAME: &str = "AGENTS.md";
const PROJECT_INSTRUCTIONS_MAX_FILE_BYTES: usize = 32 * 1024;
const PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES: usize = 64 * 1024;
const PROJECT_INSTRUCTIONS_SEPARATOR: &str = "\n\n";

/// 当前 workspace 加载到的项目指令：正文以及它有没有被截断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInstructions {
    /// 按 workspace root 到 cwd 的顺序合并后的正文，发给模型的就是这一份。
    content: String,
    /// 是否因为单文件超限或合并预算用尽，导致正文被截断。
    truncated: bool,
}

impl ProjectInstructions {
    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

/// 从用户数据目录、以及项目根到 cwd 的各层目录加载指令；文件读取与总预算两处共用同一实现。
pub fn load_agent_instructions(cwd: &Path, home: &Path) -> Result<Option<ProjectInstructions>, String> {
    let canonical = crate::canonicalize_workspace(cwd)?;
    let cwd = canonical.as_path();
    let root = crate::workspace::project_root(cwd)?;
    let canonical_home = match std::fs::canonicalize(home) {
        Ok(path) => path,
        // 数据目录可能尚未创建：沿用原路径继续加载。
        Err(error) if error.kind() == io::ErrorKind::NotFound => home.to_path_buf(),
        Err(error) => {
            return Err(format!("project_instruction_directory_read_failed:{}:{error}", home.display()));
        }
    };
    let home_identity = crate::CanonicalWorkspacePath::from_saved(canonical_home)?;
    let mut directories = vec![home.to_path_buf()];
    for directory in instruction_directories(&root, cwd) {
        // 该目录就是数据根，已在列表中：跳过以免重复纳入。
        if crate::CanonicalWorkspacePath::from_saved(&directory)? != home_identity {
            directories.push(directory);
        }
    }
    load_instruction_directories(&root, directories)
}

fn load_instruction_directories(
    workspace_root: &Path,
    directories: Vec<PathBuf>,
) -> Result<Option<ProjectInstructions>, String> {
    let mut content = String::new();
    let mut truncated = false;
    for directory in directories {
        let ordinary_relative = directory
            .strip_prefix(workspace_root)
            .unwrap_or(&directory)
            .join(PROJECT_INSTRUCTIONS_FILE_NAME);
        let instruction_file = read_project_instruction_file(&directory, &ordinary_relative)?;
        let Some(instruction_file) = instruction_file else {
            continue;
        };
        if instruction_file.truncated {
            truncated = true;
        }
        // 空文件既不占用预算，也不算截断。
        if instruction_file.text.trim().is_empty() {
            continue;
        }
        let source_text = format!(
            "# Instructions from {}\n\n{}",
            directory.join(PROJECT_INSTRUCTIONS_FILE_NAME).display(),
            instruction_file.text
        );
        let remaining = PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES - content.len();
        let separator_len = if content.is_empty() {
            0
        } else {
            PROJECT_INSTRUCTIONS_SEPARATOR.len()
        };
        let (take, source_truncated) =
            crate::utf8_prefix(&source_text, remaining.saturating_sub(separator_len));
        if !take.trim().is_empty() {
            if !content.is_empty() {
                content.push_str(PROJECT_INSTRUCTIONS_SEPARATOR);
            }
            content.push_str(take);
        }
        if source_truncated {
            truncated = true;
            break;
        }
    }

    if content.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ProjectInstructions { content, truncated }))
    }
}

struct ProjectInstructionFile {
    /// 进入模型视图的文件文本（已按单文件预算截断为有效 UTF-8 前缀）。
    text: String,
    /// 这个文件是否因为超过单文件预算而被截断。
    truncated: bool,
}

/// 返回 workspace root 到 cwd 之间需要检查指令的目录，两端都包含。
fn instruction_directories(workspace_root: &Path, cwd: &Path) -> Vec<PathBuf> {
    // workspace root 是 cwd 的祖先，strip_prefix 一定成功。
    let depth = cwd
        .strip_prefix(workspace_root)
        .expect("cwd 必在 workspace root 之下")
        .components()
        .count();
    let mut directories = cwd.ancestors().take(depth + 1).map(Path::to_path_buf).collect::<Vec<_>>();
    directories.reverse();
    directories
}

fn read_project_instruction_file(
    directory: &Path,
    relative_path: &Path,
) -> Result<Option<ProjectInstructionFile>, String> {
    let path = directory.join(PROJECT_INSTRUCTIONS_FILE_NAME);
    let mut bytes = Vec::new();
    match std::fs::File::open(&path)
        .and_then(|file| file.take((PROJECT_INSTRUCTIONS_MAX_FILE_BYTES + 1) as u64).read_to_end(&mut bytes))
    {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!("project_instruction_file_read_failed:{}:{error}", relative_path.display()));
        }
    }
    let truncated = bytes.len() > PROJECT_INSTRUCTIONS_MAX_FILE_BYTES;
    let retained = &bytes[..bytes.len().min(PROJECT_INSTRUCTIONS_MAX_FILE_BYTES)];
    let text = match std::str::from_utf8(retained) {
        Ok(text) => text,
        // 预算边界可能落在一个完整文件中的 UTF-8 字符内部，只舍弃这个未完整纳入的字符。
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&retained[..error.valid_up_to()])
                .expect("valid_up_to must describe valid UTF-8")
        }
        Err(_) => {
            return Err(format!("project_instruction_invalid_utf8:{}", relative_path.display()));
        }
    };
    Ok(Some(ProjectInstructionFile { text: text.to_string(), truncated }))
}
