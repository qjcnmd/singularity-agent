//! 项目级指令文件（`AGENTS.md`）加载与合并模块。
//!
//! 支持从工作区根目录（Workspace Root）逐层向下检索至当前工作目录（CWD），
//! 并按照层级顺序合并指令内容。单文件超 32KB 时按预算截断为前缀纳入；合并总
//! 预算 64KB（文件间分隔符计入）耗尽后不再纳入后续文件。截断只在确有内容被
//! 预算放弃时发生，通过 [`ProjectInstructions::truncated()`] 暴露而非报错；
//! 真正的 I/O 错误（读取失败、非法 UTF-8 等）仍 fail closed。

use std::fmt::{Display, Formatter};
use std::io;
use std::path::{Path, PathBuf};

/// 项目指令文件名。
pub(crate) const PROJECT_INSTRUCTIONS_FILE_NAME: &str = "AGENTS.md";
/// 单个项目指令文件的最大字节数。
const PROJECT_INSTRUCTIONS_MAX_FILE_BYTES: usize = 32 * 1024;
/// 合并项目指令的最大总字节数。
const PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES: usize = 64 * 1024;
const PROJECT_INSTRUCTIONS_SEPARATOR: &str = "\n\n";
const PROJECT_ROOT_MARKER: &str = ".git";

/// 当前 workspace 读取到的项目指令集合及其可验证来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInstructions {
    /// 按 workspace root 到 cwd 顺序合并、且唯一发送给模型的正文。
    content: String,
    /// 是否因单文件超限或合并预算用尽而截断了项目指令正文。
    truncated: bool,
}

impl ProjectInstructions {
    /// 返回模型可见的合并指令正文。
    pub fn content(&self) -> &str {
        &self.content
    }

    /// 项目指令是否因预算超限而被截断。
    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

/// 项目指令读取失败的稳定原因分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectInstructionErrorCode {
    WorkspaceRootUnavailable,
    WorkspaceRootNotDirectory,
    WorkingDirectoryUnavailable,
    WorkingDirectoryNotDirectory,
    MetadataReadFailed,
    UnsupportedFileType,
    FileReadFailed,
    InvalidUtf8,
}

impl ProjectInstructionErrorCode {
    /// 返回稳定的错误代码字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceRootUnavailable => "project_instruction_workspace_root_unavailable",
            Self::WorkspaceRootNotDirectory => "project_instruction_workspace_root_not_directory",
            Self::WorkingDirectoryUnavailable => {
                "project_instruction_working_directory_unavailable"
            }
            Self::WorkingDirectoryNotDirectory => {
                "project_instruction_working_directory_not_directory"
            }
            Self::MetadataReadFailed => "project_instruction_metadata_read_failed",
            Self::UnsupportedFileType => "project_instruction_unsupported_file_type",
            Self::FileReadFailed => "project_instruction_file_read_failed",
            Self::InvalidUtf8 => "project_instruction_invalid_utf8",
        }
    }
}

/// 项目指令错误及其关联路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInstructionError {
    code: ProjectInstructionErrorCode,
    path: Option<PathBuf>,
    io_kind: Option<io::ErrorKind>,
}

impl ProjectInstructionError {
    fn new(code: ProjectInstructionErrorCode) -> Self {
        Self {
            code,
            path: None,
            io_kind: None,
        }
    }

    fn at_path(code: ProjectInstructionErrorCode, path: PathBuf) -> Self {
        Self {
            code,
            path: Some(path),
            io_kind: None,
        }
    }

    fn with_io_kind(
        code: ProjectInstructionErrorCode,
        path: Option<PathBuf>,
        error: &io::Error,
    ) -> Self {
        Self {
            code,
            path,
            io_kind: Some(error.kind()),
        }
    }
}

impl Display for ProjectInstructionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code.as_str())?;
        if let Some(path) = &self.path {
            write!(formatter, ":{}", path.display())?;
        }
        if let Some(io_kind) = self.io_kind {
            write!(formatter, ":{io_kind:?}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ProjectInstructionError {}

/// 从 cwd 向上查找 workspace 并加载项目指令。
pub fn load_project_instructions_from_cwd(
    cwd: impl AsRef<Path>,
) -> Result<Option<ProjectInstructions>, ProjectInstructionError> {
    let cwd = canonicalize_directory(
        cwd.as_ref(),
        ProjectInstructionErrorCode::WorkingDirectoryUnavailable,
        ProjectInstructionErrorCode::WorkingDirectoryNotDirectory,
    )?;
    let workspace_root = find_workspace_root(&cwd)?;
    load_project_instructions(&workspace_root, &cwd)
}

/// 在给定 workspace 与 cwd 边界内加载项目指令。
///
/// 唯一入口 [`load_project_instructions_from_cwd`] 的 workspace root 取自
/// cwd 的祖先链，cwd 必在 root 之下。
fn load_project_instructions(
    workspace_root: impl AsRef<Path>,
    cwd: impl AsRef<Path>,
) -> Result<Option<ProjectInstructions>, ProjectInstructionError> {
    let workspace_root = canonicalize_directory(
        workspace_root.as_ref(),
        ProjectInstructionErrorCode::WorkspaceRootUnavailable,
        ProjectInstructionErrorCode::WorkspaceRootNotDirectory,
    )?;
    let cwd = canonicalize_directory(
        cwd.as_ref(),
        ProjectInstructionErrorCode::WorkingDirectoryUnavailable,
        ProjectInstructionErrorCode::WorkingDirectoryNotDirectory,
    )?;

    let mut content = String::new();
    let mut truncated = false;
    for directory in instruction_directories(&workspace_root, &cwd) {
        // 不变量：workspace root 取自 cwd 的祖先链。
        #[allow(clippy::expect_used)]
        let ordinary_relative = directory
            .strip_prefix(&workspace_root)
            .expect("instruction directory 必在 workspace root 之下")
            .join(PROJECT_INSTRUCTIONS_FILE_NAME);
        let instruction_file = read_project_instruction_file(&directory, &ordinary_relative)?;
        let Some(instruction_file) = instruction_file else {
            continue;
        };
        if instruction_file.truncated {
            truncated = true;
        }
        // 空文件不消耗预算，也不标记截断。
        if instruction_file.text.trim().is_empty() {
            continue;
        }
        let byte_len = instruction_file.text.len();
        let remaining = PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES.saturating_sub(content.len());
        // 分隔符与正文同样占用合并预算；截断只在预算耗尽且确有内容被
        // 放弃时标记，恰好填满预算不误报。
        let separator_len = if content.is_empty() {
            0
        } else {
            PROJECT_INSTRUCTIONS_SEPARATOR.len()
        };
        if byte_len + separator_len > remaining {
            // 该文件只能纳入剩余预算内的有效 UTF-8 前缀。
            let (take, _) = crate::utf8_prefix(
                &instruction_file.text,
                remaining.saturating_sub(separator_len),
            );
            if !take.trim().is_empty() {
                if !content.is_empty() {
                    content.push_str(PROJECT_INSTRUCTIONS_SEPARATOR);
                }
                content.push_str(take);
            }
            truncated = true;
            break;
        }
        if !content.is_empty() {
            content.push_str(PROJECT_INSTRUCTIONS_SEPARATOR);
        }
        content.push_str(&instruction_file.text);
    }

    if content.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ProjectInstructions { content, truncated }))
    }
}

struct ProjectInstructionFile {
    /// 纳入模型视图的文件文本（已按文件预算截断为有效 UTF-8 前缀）。
    text: String,
    /// 该文件是否因超过文件预算而被截断。
    truncated: bool,
}

/// 返回 workspace root 到 cwd 之间需要检查指令的目录（含两端）。
fn instruction_directories(workspace_root: &Path, cwd: &Path) -> Vec<PathBuf> {
    // 不变量：workspace root 取自 cwd 的祖先链，strip_prefix 必成功。
    #[allow(clippy::expect_used)]
    let depth = cwd
        .strip_prefix(workspace_root)
        .expect("cwd 必在 workspace root 之下")
        .components()
        .count();
    let mut directories = cwd
        .ancestors()
        .take(depth + 1)
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    directories.reverse();
    directories
}

fn read_project_instruction_file(
    directory: &Path,
    relative_path: &Path,
) -> Result<Option<ProjectInstructionFile>, ProjectInstructionError> {
    let path = directory.join(PROJECT_INSTRUCTIONS_FILE_NAME);
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ProjectInstructionError::with_io_kind(
                ProjectInstructionErrorCode::MetadataReadFailed,
                Some(relative_path.to_path_buf()),
                &error,
            ));
        }
    };
    if !metadata.is_file() {
        return Err(ProjectInstructionError::at_path(
            ProjectInstructionErrorCode::UnsupportedFileType,
            relative_path.to_path_buf(),
        ));
    }
    let bytes = std::fs::read(&path).map_err(|error| {
        ProjectInstructionError::with_io_kind(
            ProjectInstructionErrorCode::FileReadFailed,
            Some(relative_path.to_path_buf()),
            &error,
        )
    })?;
    let full_text = String::from_utf8(bytes).map_err(|_| {
        ProjectInstructionError::at_path(
            ProjectInstructionErrorCode::InvalidUtf8,
            relative_path.to_path_buf(),
        )
    })?;
    let (text, truncated) = crate::utf8_prefix(&full_text, PROJECT_INSTRUCTIONS_MAX_FILE_BYTES);
    Ok(Some(ProjectInstructionFile {
        text: text.to_string(),
        truncated,
    }))
}

fn canonicalize_directory(
    path: &Path,
    unavailable_code: ProjectInstructionErrorCode,
    not_directory_code: ProjectInstructionErrorCode,
) -> Result<PathBuf, ProjectInstructionError> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| ProjectInstructionError::with_io_kind(unavailable_code, None, &error))?;
    let metadata = std::fs::metadata(&canonical)
        .map_err(|error| ProjectInstructionError::with_io_kind(unavailable_code, None, &error))?;
    if !metadata.is_dir() {
        return Err(ProjectInstructionError::new(not_directory_code));
    }
    Ok(canonical)
}

/// 从 cwd 向上查找 workspace 根（以 `.git` 标记），找不到时以 cwd 为边界。
fn find_workspace_root(cwd: &Path) -> Result<PathBuf, ProjectInstructionError> {
    for ancestor in cwd.ancestors() {
        match std::fs::symlink_metadata(ancestor.join(PROJECT_ROOT_MARKER)) {
            Ok(_) => return Ok(ancestor.to_path_buf()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ProjectInstructionError::with_io_kind(
                    ProjectInstructionErrorCode::MetadataReadFailed,
                    None,
                    &error,
                ));
            }
        }
    }
    Ok(cwd.to_path_buf())
}

#[cfg(test)]
#[path = "project_instructions_tests.rs"]
mod tests;
