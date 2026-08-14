//! 从 workspace 层级读取并限制项目指令的实现。
//!
//! 信任边界内不再防 symlink（Phase 8b 裁决 7）：使用 `std::fs` 直接读取；
//! 是否加载由信任决策（`trust.rs`）控制，本模块不重复防御。

use std::fmt::{Display, Formatter};
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 项目指令文件名。
pub const PROJECT_INSTRUCTIONS_FILE_NAME: &str = "AGENTS.md";
/// 当前层级可覆盖普通项目指令的文件名。
pub const PROJECT_INSTRUCTIONS_OVERRIDE_FILE_NAME: &str = "AGENTS.override.md";
/// 单个项目指令文件的最大字节数。
pub const PROJECT_INSTRUCTIONS_MAX_FILE_BYTES: usize = 32 * 1024;
/// 合并项目指令的最大总字节数。
pub const PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES: usize = 64 * 1024;
const PROJECT_INSTRUCTIONS_SEPARATOR: &str = "\n\n";
const PROJECT_ROOT_MARKER: &str = ".git";

/// 单个项目指令来源的 workspace-relative provenance。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInstructionSource {
    /// 相对于 workspace root 的稳定 POSIX 风格路径。
    pub path: String,
    /// 文件原始 UTF-8 字节的 SHA-256 摘要。
    pub content_digest: String,
}

/// 当前 workspace 读取到的项目指令集合及其可验证来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInstructions {
    /// 按 workspace root 到 cwd 顺序合并、且唯一发送给模型的正文。
    content: String,
    /// 按正文合并顺序排列的来源 provenance。
    sources: Vec<ProjectInstructionSource>,
    /// 对合并正文和有序 `sources` 列表计算的稳定 SHA-256 摘要。
    aggregate_digest: String,
}

impl ProjectInstructions {
    /// Returns the model-visible merged instruction text.
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Returns the ordered workspace-relative provenance records.
    pub fn sources(&self) -> &[ProjectInstructionSource] {
        &self.sources
    }

    /// Returns the aggregate digest that binds content and provenance.
    pub fn aggregate_digest(&self) -> &str {
        &self.aggregate_digest
    }

    /// Consumes the verified aggregate into the model text and its binding digest.
    pub fn into_snapshot(self) -> (String, String) {
        (self.content, self.aggregate_digest)
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
    for directory in instruction_directories(&workspace_root, &cwd) {
        let override_relative = directory.relative_path.join(PROJECT_INSTRUCTIONS_OVERRIDE_FILE_NAME);
        let ordinary_relative = directory.relative_path.join(PROJECT_INSTRUCTIONS_FILE_NAME);
        let instruction_file = match read_project_instruction_file(
            &directory.dir,
            PROJECT_INSTRUCTIONS_OVERRIDE_FILE_NAME,
            &override_relative,
        )? {
            Some(instruction_file) => Some(instruction_file),
            None => read_project_instruction_file(
                &directory.dir,
                PROJECT_INSTRUCTIONS_FILE_NAME,
                &ordinary_relative,
            )?,
        };
        let Some(instruction_file) = instruction_file else {
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
                instruction_file.relative_path.clone(),
            ));
        }
        if instruction_file.text.trim().is_empty() {
            continue;
        }
        if !content.is_empty() {
            content.push_str(PROJECT_INSTRUCTIONS_SEPARATOR);
        }
        content.push_str(&instruction_file.text);
        sources.push(ProjectInstructionSource {
            path: workspace_relative_path(&instruction_file.relative_path),
            content_digest: instruction_file.content_digest,
        });
    }

    if sources.is_empty() {
        Ok(None)
    } else {
        let aggregate_digest = sha256_digest(
            &serde_json::to_vec(&(content.as_str(), &sources))
                .expect("project instruction aggregate serialize"),
        );
        Ok(Some(ProjectInstructions {
            content,
            sources,
            aggregate_digest,
        }))
    }
}

struct ProjectInstructionFile {
    relative_path: PathBuf,
    text: String,
    byte_len: usize,
    content_digest: String,
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
    if metadata.len() > PROJECT_INSTRUCTIONS_MAX_FILE_BYTES as u64 {
        return Err(ProjectInstructionError::at_path(
            ProjectInstructionErrorCode::FileTooLarge,
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
    if bytes.len() > PROJECT_INSTRUCTIONS_MAX_FILE_BYTES {
        return Err(ProjectInstructionError::at_path(
            ProjectInstructionErrorCode::FileTooLarge,
            relative_path.to_path_buf(),
        ));
    }
    let byte_len = bytes.len();
    let content_digest = sha256_digest(&bytes);
    let text = String::from_utf8(bytes).map_err(|_| {
        ProjectInstructionError::at_path(
            ProjectInstructionErrorCode::InvalidUtf8,
            relative_path.to_path_buf(),
        )
    })?;
    Ok(Some(ProjectInstructionFile {
        relative_path: relative_path.to_path_buf(),
        text,
        byte_len,
        content_digest,
    }))
}

fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn workspace_relative_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
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
