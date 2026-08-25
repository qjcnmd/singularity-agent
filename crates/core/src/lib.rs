#![deny(unsafe_code)]

//! 跨 crate 共享的取消、文件权限和 workspace 规则。

mod cancellation;
mod fs_owner;
mod project_instructions;
mod user_home;

pub use cancellation::CancellationToken;
pub use fs_owner::{create_owner_only_dir, ensure_owner_only_dir, ensure_owner_only_file};
pub use project_instructions::{
    PROJECT_INSTRUCTIONS_FILE_NAME, PROJECT_INSTRUCTIONS_MAX_FILE_BYTES,
    PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES, ProjectInstructionError, ProjectInstructionErrorCode,
    ProjectInstructions, find_workspace_root, load_project_instructions,
    load_project_instructions_from_cwd,
};
pub use user_home::{ensure_singularity_home_outside_workspace, user_singularity_home};

/// 创建仅属主可访问的新文件（在 Unix 系统上以 0600 权限创建）。
pub fn create_owner_only_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        let file = options.open(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        options.open(path)
    }
}
