//! 从 workspace 层级读取并限制项目指令的安全实现。

use std::fmt::{Display, Formatter};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, OpenOptions};
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInstructions {
    /// 按 workspace root 到 cwd 顺序合并、且唯一发送给模型的正文。
    pub content: String,
    /// 按正文合并顺序排列的来源 provenance。
    pub sources: Vec<ProjectInstructionSource>,
    /// 对合并正文和有序 `sources` 列表计算的稳定 SHA-256 摘要。
    pub aggregate_digest: String,
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
    UnsupportedFileType,
    UnsupportedFileIdentity,
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
            Self::UnsupportedFileType => "project_instruction_unsupported_file_type",
            Self::UnsupportedFileIdentity => "project_instruction_unsupported_file_identity",
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
    load_project_instructions_with_hook(workspace_root.as_ref(), cwd.as_ref(), || {})
}

fn load_project_instructions_with_hook(
    workspace_root: &Path,
    cwd: &Path,
    after_path_resolution: impl FnOnce(),
) -> Result<Option<ProjectInstructions>, ProjectInstructionError> {
    let workspace_root = canonicalize_directory(
        workspace_root,
        ProjectInstructionErrorCode::WorkspaceRootUnavailable,
        ProjectInstructionErrorCode::WorkspaceRootNotDirectory,
    )?;
    let cwd = canonicalize_directory(
        cwd,
        ProjectInstructionErrorCode::WorkingDirectoryUnavailable,
        ProjectInstructionErrorCode::WorkingDirectoryNotDirectory,
    )?;
    if !cwd.starts_with(&workspace_root) {
        return Err(ProjectInstructionError::new(
            ProjectInstructionErrorCode::WorkingDirectoryOutsideWorkspace,
        ));
    }

    after_path_resolution();
    let directories = open_instruction_directories(&workspace_root, &cwd)?;
    let mut content = String::new();
    let mut sources = Vec::new();
    let mut total_bytes = 0usize;
    for directory in directories {
        let override_relative = directory
            .relative_path
            .join(PROJECT_INSTRUCTIONS_OVERRIDE_FILE_NAME);
        let ordinary_relative = directory.relative_path.join(PROJECT_INSTRUCTIONS_FILE_NAME);
        let instruction_file = match read_project_instruction_file(
            &directory.dir,
            Path::new(PROJECT_INSTRUCTIONS_OVERRIDE_FILE_NAME),
            &override_relative,
        )? {
            Some(instruction_file) => Some(instruction_file),
            None => read_project_instruction_file(
                &directory.dir,
                Path::new(PROJECT_INSTRUCTIONS_FILE_NAME),
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

struct InstructionDirectory {
    dir: Dir,
    relative_path: PathBuf,
}

/// Opens the workspace and every cwd component as capabilities without following directory links.
fn open_instruction_directories(
    workspace_root: &Path,
    cwd: &Path,
) -> Result<Vec<InstructionDirectory>, ProjectInstructionError> {
    let mut current = open_absolute_directory_nofollow(
        workspace_root,
        ProjectInstructionErrorCode::WorkspaceRootUnavailable,
    )?;

    let mut directories = vec![InstructionDirectory {
        dir: current.try_clone().map_err(|error| {
            ProjectInstructionError::with_io_kind(
                ProjectInstructionErrorCode::PathResolutionFailed,
                Some(PathBuf::new()),
                &error,
            )
        })?,
        relative_path: PathBuf::new(),
    }];
    let relative_cwd = cwd
        .strip_prefix(workspace_root)
        .expect("cwd boundary checked before capability traversal");
    let mut relative_path = PathBuf::new();
    for component in relative_cwd.components() {
        let Component::Normal(component) = component else {
            return Err(ProjectInstructionError::at_path(
                ProjectInstructionErrorCode::PathResolutionFailed,
                relative_path,
            ));
        };
        relative_path.push(component);
        current = current.open_dir_nofollow(component).map_err(|error| {
            ProjectInstructionError::with_io_kind(
                ProjectInstructionErrorCode::PathResolutionFailed,
                Some(relative_path.clone()),
                &error,
            )
        })?;
        directories.push(InstructionDirectory {
            dir: current.try_clone().map_err(|error| {
                ProjectInstructionError::with_io_kind(
                    ProjectInstructionErrorCode::PathResolutionFailed,
                    Some(relative_path.clone()),
                    &error,
                )
            })?,
            relative_path: relative_path.clone(),
        });
    }
    Ok(directories)
}

/// Opens an absolute directory through a stable filesystem-root capability.
///
/// The input is already canonical, so every named component must be an actual
/// directory when opened. A symlink or reparse point inserted after
/// canonicalization is rejected instead of becoming a new ambient escape.
fn open_absolute_directory_nofollow(
    path: &Path,
    error_code: ProjectInstructionErrorCode,
) -> Result<Dir, ProjectInstructionError> {
    let mut anchor = PathBuf::new();
    let mut descendants = Vec::new();
    let mut rooted = false;
    for component in path.components() {
        match component {
            Component::Prefix(_) if !rooted && descendants.is_empty() => {
                anchor.push(component.as_os_str());
            }
            Component::RootDir if !rooted && descendants.is_empty() => {
                anchor.push(component.as_os_str());
                rooted = true;
            }
            Component::Normal(component) if rooted => descendants.push(component.to_os_string()),
            _ => {
                return Err(ProjectInstructionError::new(
                    ProjectInstructionErrorCode::PathResolutionFailed,
                ));
            }
        }
    }
    if !rooted {
        return Err(ProjectInstructionError::new(
            ProjectInstructionErrorCode::PathResolutionFailed,
        ));
    }

    let mut current = Dir::open_ambient_dir(&anchor, ambient_authority())
        .map_err(|error| ProjectInstructionError::with_io_kind(error_code, None, &error))?;
    for component in descendants {
        current = current
            .open_dir_nofollow(&component)
            .map_err(|error| ProjectInstructionError::with_io_kind(error_code, None, &error))?;
    }
    Ok(current)
}

fn read_project_instruction_file(
    directory: &Dir,
    candidate_name: &Path,
    relative_path: &Path,
) -> Result<Option<ProjectInstructionFile>, ProjectInstructionError> {
    let metadata = match directory.symlink_metadata(candidate_name) {
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
    if metadata.is_symlink() || !metadata.is_file() {
        return Err(ProjectInstructionError::at_path(
            ProjectInstructionErrorCode::UnsupportedFileType,
            relative_path.to_path_buf(),
        ));
    }
    read_project_instruction_file_with_hook(directory, candidate_name, relative_path, || {})
        .map(Some)
}

fn read_project_instruction_file_with_hook(
    directory: &Dir,
    candidate_name: &Path,
    relative_path: &Path,
    after_open: impl FnOnce(),
) -> Result<ProjectInstructionFile, ProjectInstructionError> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.follow(FollowSymlinks::No);
    let mut file = directory
        .open_with(candidate_name, &options)
        .map_err(|error| {
            ProjectInstructionError::with_io_kind(
                ProjectInstructionErrorCode::FileReadFailed,
                Some(relative_path.to_path_buf()),
                &error,
            )
        })?;
    let metadata = file.metadata().map_err(|error| {
        ProjectInstructionError::with_io_kind(
            ProjectInstructionErrorCode::MetadataReadFailed,
            Some(relative_path.to_path_buf()),
            &error,
        )
    })?;
    if !metadata.is_file() {
        return Err(ProjectInstructionError::at_path(
            ProjectInstructionErrorCode::UnsupportedFileType,
            relative_path.to_path_buf(),
        ));
    }
    if metadata.nlink() != 1 {
        return Err(ProjectInstructionError::at_path(
            ProjectInstructionErrorCode::UnsupportedFileIdentity,
            relative_path.to_path_buf(),
        ));
    }
    if metadata.len() > PROJECT_INSTRUCTIONS_MAX_FILE_BYTES as u64 {
        return Err(ProjectInstructionError::at_path(
            ProjectInstructionErrorCode::FileTooLarge,
            relative_path.to_path_buf(),
        ));
    }

    after_open();
    let bytes = read_bounded_file(&mut file, relative_path)?;
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
    Ok(ProjectInstructionFile {
        relative_path: relative_path.to_path_buf(),
        text,
        byte_len,
        content_digest,
    })
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

fn read_bounded_file(
    file: &mut File,
    relative_path: &Path,
) -> Result<Vec<u8>, ProjectInstructionError> {
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

#[cfg(test)]
mod tests {
    use super::{
        ProjectInstructionErrorCode, load_project_instructions_with_hook,
        read_project_instruction_file_with_hook,
    };
    use cap_std::ambient_authority;
    use cap_std::fs::Dir;
    use std::path::Path;

    #[test]
    fn verified_handle_does_not_follow_a_replaced_instruction_path() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        let outside = temp.path().join("outside.txt");
        std::fs::create_dir(&workspace).expect("workspace");
        let candidate = workspace.join("AGENTS.md");
        let original = workspace.join("original-agents.md");
        std::fs::write(&candidate, "trusted instructions").expect("candidate");
        std::fs::write(&outside, "outside secret").expect("outside");
        let directory =
            Dir::open_ambient_dir(&workspace, ambient_authority()).expect("workspace capability");

        let loaded = read_project_instruction_file_with_hook(
            &directory,
            Path::new("AGENTS.md"),
            Path::new("AGENTS.md"),
            || {
                std::fs::rename(&candidate, &original).expect("move original path");
                std::fs::hard_link(&outside, &candidate).expect("replace with outside hardlink");
            },
        )
        .expect("read opened instruction object");

        assert_eq!(loaded.text, "trusted instructions");
        assert_eq!(
            std::fs::read_to_string(&candidate).expect("replacement contents"),
            "outside secret"
        );
    }

    #[test]
    fn resolved_workspace_cannot_be_replaced_with_a_directory_link_before_open() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        let original = temp.path().join("original-workspace");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::create_dir(&outside).expect("outside");
        std::fs::write(outside.join("AGENTS.md"), "outside secret").expect("outside agents");

        let error = load_project_instructions_with_hook(&workspace, &workspace, || {
            std::fs::rename(&workspace, &original).expect("move workspace");
            create_directory_link(&outside, &workspace);
        })
        .expect_err("replacement directory link must not be followed");

        assert_eq!(
            error.code,
            ProjectInstructionErrorCode::WorkspaceRootUnavailable
        );
        remove_directory_link(&workspace);
    }

    #[cfg(unix)]
    fn create_directory_link(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).expect("workspace symlink");
    }

    #[cfg(unix)]
    fn remove_directory_link(link: &Path) {
        std::fs::remove_file(link).expect("remove workspace symlink");
    }

    #[cfg(windows)]
    fn create_directory_link(target: &Path, link: &Path) {
        let output = std::process::Command::new("cmd.exe")
            .args([
                "/C",
                "mklink",
                "/J",
                link.to_str().expect("workspace link path"),
                target.to_str().expect("workspace link target"),
            ])
            .output()
            .expect("create workspace junction");
        assert!(
            output.status.success(),
            "mklink /J failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(windows)]
    fn remove_directory_link(link: &Path) {
        std::fs::remove_dir(link).expect("remove workspace junction");
    }
}
