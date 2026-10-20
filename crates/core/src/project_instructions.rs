//! 全局与项目指令文件（AGENTS.md）的加载与合并：先读应用主目录，再从 workspace root
//! 逐层检索到 cwd 按层级合并；单文件超 32KB 取预算内前缀，合并总预算 64KB（含分隔符）
//! 用尽即停；I/O 错误和预算内前缀的非法 UTF-8 直接失败。

use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub(crate) const PROJECT_INSTRUCTIONS_FILE_NAME: &str = "AGENTS.md";
const PROJECT_INSTRUCTIONS_MAX_FILE_BYTES: usize = 32 * 1024;
const PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES: usize = 64 * 1024;
const PROJECT_INSTRUCTIONS_SEPARATOR: &str = "\n\n";

/// 从用户数据目录、以及项目根到 cwd 的各层目录加载指令；文件读取与总预算两处共用同一实现。
pub fn load_agent_instructions(cwd: &Path, home: &Path) -> Result<Option<String>, String> {
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
    load_instruction_directories(directories)
}

fn load_instruction_directories(directories: Vec<PathBuf>) -> Result<Option<String>, String> {
    let mut content = String::new();
    for directory in directories {
        let instruction_file = read_project_instruction_file(&directory)?;
        let Some(instruction_file) = instruction_file else {
            continue;
        };
        // 空文件不占预算，也不算截断。
        if instruction_file.trim().is_empty() {
            continue;
        }
        let source_text = format!(
            "# Instructions from {}\n\n{}",
            directory.join(PROJECT_INSTRUCTIONS_FILE_NAME).display(),
            instruction_file
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
            break;
        }
    }

    if content.is_empty() {
        Ok(None)
    } else {
        Ok(Some(content))
    }
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

fn read_project_instruction_file(directory: &Path) -> Result<Option<String>, String> {
    let path = directory.join(PROJECT_INSTRUCTIONS_FILE_NAME);
    let mut bytes = Vec::new();
    match std::fs::File::open(&path)
        .and_then(|file| file.take((PROJECT_INSTRUCTIONS_MAX_FILE_BYTES + 1) as u64).read_to_end(&mut bytes))
    {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!("project_instruction_file_read_failed:{}:{error}", path.display()));
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
            return Err(format!("project_instruction_invalid_utf8:{}", path.display()));
        }
    };
    Ok(Some(text.to_string()))
}
