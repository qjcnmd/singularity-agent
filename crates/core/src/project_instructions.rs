//! 项目级指令文件（`AGENTS.md`）加载与合并模块。
//!
//! 支持从工作区根目录（Workspace Root）逐层向下检索至当前工作目录（CWD），
//! 并按照层级顺序合并指令内容。单文件超 32KB 时按预算截断为前缀纳入；合并总
//! 预算 64KB 用尽即停止纳入后续文件。预算导致的截断通过
//! [`ProjectInstructions::truncated()`] 暴露而非报错；
//! 真正的 I/O 错误（读取失败、非法 UTF-8 等）仍 fail closed。

use std::fmt::{Display, Formatter};
use std::io;
use std::path::{Component, Path, PathBuf};

/// 项目指令文件名。
pub const PROJECT_INSTRUCTIONS_FILE_NAME: &str = "AGENTS.md";
/// 单个项目指令文件的最大字节数。
pub const PROJECT_INSTRUCTIONS_MAX_FILE_BYTES: usize = 32 * 1024;
/// 合并项目指令的最大总字节数。
pub const PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES: usize = 64 * 1024;
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
pub enum ProjectInstructionErrorCode {
    WorkspaceRootUnavailable,
    WorkspaceRootNotDirectory,
    WorkingDirectoryUnavailable,
    WorkingDirectoryNotDirectory,
    WorkingDirectoryOutsideWorkspace,
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
            Self::WorkingDirectoryOutsideWorkspace => {
                "project_instruction_working_directory_outside_workspace"
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
    pub code: ProjectInstructionErrorCode,
    pub path: Option<PathBuf>,
    pub io_kind: Option<io::ErrorKind>,
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
pub fn load_project_instructions(
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
    if !cwd.starts_with(&workspace_root) {
        return Err(ProjectInstructionError::new(
            ProjectInstructionErrorCode::WorkingDirectoryOutsideWorkspace,
        ));
    }

    let mut content = String::new();
    let mut found = false;
    let mut truncated = false;
    let mut remaining = PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES;
    for directory in instruction_directories(&workspace_root, &cwd) {
        // 预算耗尽后不再读取后续文件。
        if remaining == 0 {
            truncated = true;
            break;
        }
        let ordinary_relative = directory.relative_path.join(PROJECT_INSTRUCTIONS_FILE_NAME);
        let instruction_file = read_project_instruction_file(
            &directory.dir,
            PROJECT_INSTRUCTIONS_FILE_NAME,
            &ordinary_relative,
        )?;
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
        let byte_len = instruction_file.byte_len;
        if byte_len > remaining {
            // 该文件只能纳入剩余预算内的有效 UTF-8 前缀。
            let (take, _) = budget_prefix(&instruction_file.text, remaining);
            if !take.trim().is_empty() {
                if !content.is_empty() {
                    content.push_str(PROJECT_INSTRUCTIONS_SEPARATOR);
                }
                content.push_str(take);
                found = true;
            }
            truncated = true;
            break;
        }
        remaining -= byte_len;
        if !content.is_empty() {
            content.push_str(PROJECT_INSTRUCTIONS_SEPARATOR);
        }
        content.push_str(&instruction_file.text);
        found = true;
    }

    if !found {
        Ok(None)
    } else {
        Ok(Some(ProjectInstructions { content, truncated }))
    }
}

struct ProjectInstructionFile {
    /// 纳入模型视图的文件文本（已按文件预算截断为有效 UTF-8 前缀）。
    text: String,
    /// 纳入文本的字节长度（≤ 文件预算）。
    byte_len: usize,
    /// 该文件是否因超过文件预算而被截断。
    truncated: bool,
}

/// 待检查指令的目录：`dir` 为绝对路径（读取用），`relative_path` 为 workspace 相对路径（provenance 用）。
struct InstructionDirectory {
    dir: PathBuf,
    relative_path: PathBuf,
}

/// 返回 workspace root 到 cwd 之间需要检查指令的目录（含两端）。
fn instruction_directories(workspace_root: &Path, cwd: &Path) -> Vec<InstructionDirectory> {
    let mut directories = vec![InstructionDirectory {
        dir: workspace_root.to_path_buf(),
        relative_path: PathBuf::new(),
    }];
    let relative_cwd = cwd
        .strip_prefix(workspace_root)
        .expect("cwd boundary checked before traversal");
    let mut dir = workspace_root.to_path_buf();
    let mut relative = PathBuf::new();
    for component in relative_cwd.components() {
        if let Component::Normal(component) = component {
            dir.push(component);
            relative.push(component);
            directories.push(InstructionDirectory {
                dir: dir.clone(),
                relative_path: relative.clone(),
            });
        }
    }
    directories
}

fn read_project_instruction_file(
    directory: &Path,
    candidate_name: &str,
    relative_path: &Path,
) -> Result<Option<ProjectInstructionFile>, ProjectInstructionError> {
    let path = directory.join(candidate_name);
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
    let (text, truncated) = budget_prefix(&full_text, PROJECT_INSTRUCTIONS_MAX_FILE_BYTES);
    let byte_len = text.len();
    Ok(Some(ProjectInstructionFile {
        text: text.to_string(),
        byte_len,
        truncated,
    }))
}

/// 返回不超过 `max_bytes` 字节的有效 UTF-8 文本前缀；`text` 超长则截断并返回 `true`。
fn budget_prefix(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
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
pub fn find_workspace_root(cwd: &Path) -> Result<PathBuf, ProjectInstructionError> {
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
