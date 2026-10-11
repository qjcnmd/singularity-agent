#![deny(unsafe_code)]

//! 各 crate 共享的文件权限与 workspace 规则。

mod project_instructions;
pub mod skills;
mod user_home;
pub mod workspace;

pub use project_instructions::{ProjectInstructions, load_agent_instructions};
pub use user_home::{HomeOrigin, ResolvedHome, resolve_home};
pub use workspace::{CanonicalWorkspacePath, canonicalize_workspace, saved_directory_matches};

/// 项目根目录标记文件名（`.git`）。
pub(crate) const PROJECT_ROOT_MARKER: &str = ".git";

/// 将 Duration 转换为毫秒数（防止超出 u64 范围）。
pub fn duration_millis(duration: std::time::Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

/// 获取当前 UTC 时间的 ISO 8601 字符串（毫秒精度，形如 `2026-10-11T08:00:00.000Z`）。
pub fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .expect("utc timestamp always formats")
}

/// 规范化路径显示：替换为正斜杠，并处理 Windows 的 UNC 与 verbatim 扩展前缀。
pub fn display_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if let Some(rest) = text.strip_prefix("//?/UNC/") {
        format!("//{rest}")
    } else if let Some(rest) = text.strip_prefix("//?/") {
        rest.to_owned()
    } else {
        text
    }
}

/// 按字节预算截取有效的 UTF-8 字符串前缀（保证字符完整性），返回切片与是否截断。
pub fn utf8_prefix(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let end = text.floor_char_boundary(max_bytes);
    (&text[..end], true)
}

/// 创建新文件（若文件已存在则返回错误）。
pub fn create_new_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    options.open(path)
}

/// 递归创建应用数据目录。
pub fn create_data_dir(path: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|error| format!("failed to create {}: {error}", path.display()))
}

/// 原子覆盖写入：先写入同目录临时文件并刷盘，再原子替换目标文件。
pub fn atomic_replace_bytes(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    atomic_write(path, bytes, true)
}

/// 原子创建文件：先写入同目录临时文件，成功后再原子移动至目标位置（目标已存在时失败）。
pub fn atomic_create_bytes(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    atomic_write(path, bytes, false)
}

fn atomic_write(path: &std::path::Path, bytes: &[u8], replace_existing: bool) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    // 临时名只需在目标目录中唯一，不依赖目标文件名的字符编码。
    let temporary = parent.join(format!(".singularity-tmp-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| -> std::io::Result<()> {
        let mut handle = create_new_file(&temporary)?;
        handle.write_all(bytes)?;
        handle.sync_all()?;
        Ok(())
    })()
    .and_then(|()| atomic_move(&temporary, path, replace_existing));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// 用 MoveFileExW 提交同卷临时文件；创建模式不允许覆盖目标。
#[allow(unsafe_code)]
fn atomic_move(from: &std::path::Path, to: &std::path::Path, replace_existing: bool) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let mut from_wide = from.as_os_str().encode_wide().collect::<Vec<_>>();
    from_wide.push(0);
    let mut to_wide = to.as_os_str().encode_wide().collect::<Vec<_>>();
    to_wide.push(0);
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace_existing {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    if unsafe { MoveFileExW(from_wide.as_ptr(), to_wide.as_ptr(), flags) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
