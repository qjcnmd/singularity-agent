#![deny(unsafe_code)]

//! 跨 crate 共享的取消、文件权限和 workspace 规则。

mod cancellation;
mod fs_owner;
mod project_instructions;
mod user_home;
pub mod workspace;

pub use cancellation::CancellationToken;
pub use fs_owner::{create_owner_only_dir, ensure_owner_only_file};
pub use project_instructions::{
    ProjectInstructionError, ProjectInstructions, load_project_instructions_from_cwd,
};
pub use user_home::{SINGULARITY_DIR_NAME, user_home_base_from_env, user_singularity_home};
pub use workspace::{CanonicalWorkspacePath, WorkspacePathError, canonicalize_workspace};

/// 返回不超过 `max_bytes` 字节的有效 UTF-8 文本前缀；`text` 超长则截到
/// 字符边界并返回 `true`（全仓字节预算截断的唯一实现）。
pub fn utf8_prefix(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

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

/// 把字节以临时文件 + 原子替换方式写入目标路径。
///
/// 先写同目录临时文件并 `sync_all`，再经跨平台原子替换落盘：崩溃/断电时
/// 目标文件要么是旧内容要么是新内容，绝不出现半写撕裂。临时文件按属主
/// 专用权限创建，写入失败或替换失败时清理。
#[cfg_attr(windows, allow(unsafe_code))]
pub fn atomic_replace_bytes(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("output");
    // UUID 临时名：同一进程内并发替换同一目标（或近似名）不会互相覆盖。
    let temporary = parent.join(format!(".{name}.tmp-{}", uuid::Uuid::new_v4().simple()));
    let write_result = (|| -> std::io::Result<()> {
        let mut handle = create_owner_only_file(&temporary)?;
        handle.write_all(bytes)?;
        handle.flush()?;
        handle.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = atomic_replace(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

/// 跨平台原子替换：Windows 用 `MoveFileExW`（同一卷内可覆盖），其余平台
/// 用 `rename`。替换失败时目标保持原状。
#[cfg_attr(windows, allow(unsafe_code))]
pub(crate) fn atomic_replace(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        let mut from_wide = from.as_os_str().encode_wide().collect::<Vec<_>>();
        from_wide.push(0);
        let mut to_wide = to.as_os_str().encode_wide().collect::<Vec<_>>();
        to_wide.push(0);
        if unsafe {
            MoveFileExW(
                from_wide.as_ptr(),
                to_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::atomic_replace_bytes;

    #[test]
    fn atomic_replace_bytes_writes_and_overwrites() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("data.txt");
        atomic_replace_bytes(&target, b"first").expect("first write");
        assert_eq!(
            std::fs::read_to_string(&target).expect("read back"),
            "first"
        );
        atomic_replace_bytes(&target, b"second").expect("overwrite");
        assert_eq!(
            std::fs::read_to_string(&target).expect("read back"),
            "second"
        );
        // 临时文件不应残留。
        let leftovers = std::fs::read_dir(dir.path())
            .expect("list dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(leftovers, 0, "temporary files must be cleaned up");
    }
}
