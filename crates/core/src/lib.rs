#![deny(unsafe_code)]

//! 跨 crate 共享的取消、文件权限和 workspace 规则。

mod cancellation;
mod project_instructions;
pub mod skills;
mod user_home;
pub mod workspace;

pub use cancellation::CancellationToken;
pub use project_instructions::{ProjectInstructions, load_agent_instructions};
pub use user_home::{
    SINGULARITY_DIR_NAME, user_home_base_from_env, user_singularity_home,
    user_singularity_home_result,
};
pub use workspace::{CanonicalWorkspacePath, canonicalize_workspace};

/// 将时长转换为毫秒，超出协议整数范围时饱和到 u64。
pub fn duration_millis(duration: std::time::Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

/// 当前 UTC 时间，使用毫秒精度的 ISO 8601 格式；会话记录与实时快照共用。
#[allow(clippy::expect_used)]
pub fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .expect("utc timestamp always formats")
}

/// 协议与界面使用的路径文本；转换 Windows 分隔符和 verbatim 前缀。
pub fn display_path(path: &std::path::Path) -> String {
    {
        let text = path.to_string_lossy().replace('\\', "/");
        if let Some(rest) = text.strip_prefix("//?/UNC/") {
            format!("//{rest}")
        } else if let Some(rest) = text.strip_prefix("//?/") {
            rest.to_owned()
        } else {
            text
        }
    }
}

/// 返回不超过 max_bytes 字节的有效 UTF-8 文本前缀；text 超长则截到
/// 字符边界并返回 true（全仓字节预算截断的唯一实现）。
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

/// 创建新数据文件；访问权限沿用 Windows 目录继承的 ACL。
pub fn create_new_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    options.open(path)
}

/// 创建应用数据目录，并拒绝被非目录对象或符号链接替代的路径。
pub fn create_data_dir(path: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    if !std::fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect directory {}: {error}", path.display()))?
        .is_dir()
    {
        return Err(format!("data path is not a directory: {}", path.display()));
    }
    Ok(())
}

/// 校验数据路径是普通文件；访问权限由 Windows ACL 决定。
pub fn ensure_regular_file(path: &std::path::Path) -> Result<(), String> {
    if !std::fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect file {}: {error}", path.display()))?
        .is_file()
    {
        return Err(format!("data path is not a file: {}", path.display()));
    }
    Ok(())
}

/// 把字节以临时文件 + 原子替换方式写入目标路径。
///
/// 先写同目录临时文件并 sync_all，再原子替换，使读者只能看到完整旧内容或
/// 完整新内容。写入失败或替换失败时清理临时文件。
pub fn atomic_replace_bytes(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    atomic_write(path, bytes, create_new_file)
}

/// Atomically write a workspace file, preserving existing permissions. New
/// files use the Windows creation defaults. Application state
/// such as credentials must use `atomic_replace_bytes` instead.
pub fn atomic_replace_workspace_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let permissions = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    atomic_write(path, bytes, |temporary| {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temporary)?;
        if let Some(permissions) = &permissions {
            file.set_permissions(permissions.clone())?;
        }
        Ok(file)
    })
}

fn atomic_write(
    path: &std::path::Path,
    bytes: &[u8],
    create: impl FnOnce(&std::path::Path) -> std::io::Result<std::fs::File>,
) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("output");
    // UUID 临时名：同一进程内并发替换同一目标（或近似名）不会互相覆盖。
    let temporary = parent.join(format!(".{name}.tmp-{}", uuid::Uuid::new_v4().simple()));
    let write_result = (|| -> std::io::Result<()> {
        let mut handle = create(&temporary)?;
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

/// 用 MoveFileExW 原子替换同卷文件；替换失败时目标保持原状。
#[allow(unsafe_code)]
pub(crate) fn atomic_replace(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
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
