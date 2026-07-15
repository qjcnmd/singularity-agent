//! 从 workspace 层级读取并限制项目指令的安全实现。

use std::fmt::{Display, Formatter};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// 项目指令文件名。
pub const PROJECT_INSTRUCTIONS_FILE_NAME: &str = "AGENTS.md";
/// 单个项目指令文件的最大字节数。
pub const PROJECT_INSTRUCTIONS_MAX_FILE_BYTES: usize = 32 * 1024;
/// 合并项目指令的最大总字节数。
pub const PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES: usize = 64 * 1024;
const PROJECT_INSTRUCTIONS_SEPARATOR: &str = "\n\n";
const PROJECT_ROOT_MARKER: &str = ".git";

/// 当前 workspace 读取到的项目指令集合。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInstructions {
    pub content: String,
    pub sources: Vec<PathBuf>,
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
    PathResolutionFailed,
    PathOutsideWorkspace,
    UnsupportedFileType,
    FileTooLarge,
    TotalTooLarge,
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
            Self::PathResolutionFailed => "project_instruction_path_resolution_failed",
            Self::PathOutsideWorkspace => "project_instruction_path_outside_workspace",
            Self::UnsupportedFileType => "project_instruction_unsupported_file_type",
            Self::FileTooLarge => "project_instruction_file_too_large",
            Self::TotalTooLarge => "project_instruction_total_too_large",
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
    let mut sources = Vec::new();
    let mut total_bytes = 0usize;
    for directory in instruction_search_directories(&workspace_root, &cwd) {
        let candidate = directory.join(PROJECT_INSTRUCTIONS_FILE_NAME);
        let Some(instruction_file) = read_project_instruction_file(&workspace_root, &candidate)?
        else {
            continue;
        };
        total_bytes = total_bytes
            .checked_add(instruction_file.byte_len)
            .ok_or_else(|| {
                ProjectInstructionError::at_path(
                    ProjectInstructionErrorCode::TotalTooLarge,
                    instruction_file.relative_path.clone(),
                )
            })?;
        if total_bytes > PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES {
            return Err(ProjectInstructionError::at_path(
                ProjectInstructionErrorCode::TotalTooLarge,
                instruction_file.relative_path,
            ));
        }
        if instruction_file.text.trim().is_empty() {
            continue;
        }
        if !content.is_empty() {
            content.push_str(PROJECT_INSTRUCTIONS_SEPARATOR);
        }
        content.push_str(&instruction_file.text);
        sources.push(instruction_file.relative_path);
    }

    if sources.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ProjectInstructions { content, sources }))
    }
}

struct ProjectInstructionFile {
    relative_path: PathBuf,
    text: String,
    byte_len: usize,
}

fn read_project_instruction_file(
    workspace_root: &Path,
    candidate: &Path,
) -> Result<Option<ProjectInstructionFile>, ProjectInstructionError> {
    let relative_path = candidate
        .strip_prefix(workspace_root)
        .expect("instruction candidate is within workspace")
        .to_path_buf();
    match std::fs::symlink_metadata(candidate) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ProjectInstructionError::with_io_kind(
                ProjectInstructionErrorCode::MetadataReadFailed,
                Some(relative_path),
                &error,
            ));
        }
    }

    let resolved_path = std::fs::canonicalize(candidate).map_err(|error| {
        ProjectInstructionError::with_io_kind(
            ProjectInstructionErrorCode::PathResolutionFailed,
            Some(relative_path.clone()),
            &error,
        )
    })?;
    if !resolved_path.starts_with(workspace_root) {
        return Err(ProjectInstructionError::at_path(
            ProjectInstructionErrorCode::PathOutsideWorkspace,
            relative_path,
        ));
    }
    let metadata = std::fs::metadata(&resolved_path).map_err(|error| {
        ProjectInstructionError::with_io_kind(
            ProjectInstructionErrorCode::MetadataReadFailed,
            Some(relative_path.clone()),
            &error,
        )
    })?;
    if !metadata.is_file() {
        return Err(ProjectInstructionError::at_path(
            ProjectInstructionErrorCode::UnsupportedFileType,
            relative_path,
        ));
    }
    if metadata.len() > PROJECT_INSTRUCTIONS_MAX_FILE_BYTES as u64 {
        return Err(ProjectInstructionError::at_path(
            ProjectInstructionErrorCode::FileTooLarge,
            relative_path,
        ));
    }

    let bytes = read_bounded_file(&resolved_path, &relative_path)?;
    if bytes.len() > PROJECT_INSTRUCTIONS_MAX_FILE_BYTES {
        return Err(ProjectInstructionError::at_path(
            ProjectInstructionErrorCode::FileTooLarge,
            relative_path,
        ));
    }
    let byte_len = bytes.len();
    let text = String::from_utf8(bytes).map_err(|_| {
        ProjectInstructionError::at_path(
            ProjectInstructionErrorCode::InvalidUtf8,
            relative_path.clone(),
        )
    })?;
    Ok(Some(ProjectInstructionFile {
        relative_path,
        text,
        byte_len,
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

fn instruction_search_directories(workspace_root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let relative_cwd = cwd
        .strip_prefix(workspace_root)
        .expect("cwd boundary checked before directory construction");
    let mut current = workspace_root.to_path_buf();
    let mut directories = vec![current.clone()];
    for component in relative_cwd.components() {
        current.push(component);
        directories.push(current.clone());
    }
    directories
}

fn read_bounded_file(
    resolved_path: &Path,
    relative_path: &Path,
) -> Result<Vec<u8>, ProjectInstructionError> {
    let file = File::open(resolved_path).map_err(|error| {
        ProjectInstructionError::with_io_kind(
            ProjectInstructionErrorCode::FileReadFailed,
            Some(relative_path.to_path_buf()),
            &error,
        )
    })?;
    let mut bytes = Vec::new();
    file.take((PROJECT_INSTRUCTIONS_MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            ProjectInstructionError::with_io_kind(
                ProjectInstructionErrorCode::FileReadFailed,
                Some(relative_path.to_path_buf()),
                &error,
            )
        })?;
    Ok(bytes)
}
