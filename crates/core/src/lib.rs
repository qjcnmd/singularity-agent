#![deny(unsafe_code)]

//! 各 crate 共用的数据目录、项目路径、AGENTS.md 加载、技能发现与基础工具函数。

mod project_instructions;
pub mod skills;
mod user_home;
pub mod workspace;

pub use project_instructions::{InstructionFile, load_agent_instructions};
pub use user_home::resolve_home;
pub use workspace::{CanonicalWorkspacePath, canonicalize_workspace, same_saved_path};

/// 向上查找项目根目录（AGENTS.md 与项目技能的查找上界）时识别的 `.git` 标记。
pub(crate) const PROJECT_ROOT_MARKER: &str = ".git";

/// 将 Duration 转换为毫秒数，溢出时封顶 u64::MAX。
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

/// 把 `now_iso` 格式的时间往前推若干毫秒；无法解析时返回 None。
pub fn iso_minus_millis(timestamp: &str, millis: u64) -> Option<String> {
    let format = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    let parsed = time::PrimitiveDateTime::parse(timestamp, &format).ok()?;
    let shifted = parsed.checked_sub(time::Duration::milliseconds(i64::try_from(millis).ok()?))?;
    shifted.format(&format).ok()
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

/// 按字节预算截取 UTF-8 字符串前缀，按字符边界对齐；返回切片与是否截断。
pub fn utf8_prefix(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let end = text.floor_char_boundary(max_bytes);
    (&text[..end], true)
}

/// 递归创建目录，失败时在错误中附带目录路径。
pub fn create_data_dir(path: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|error| format!("failed to create {}: {error}", path.display()))
}

/// 将值写成缩进 JSON 并追加一个 LF，刷盘后原子替换目标文件。
/// 调用方负责创建父目录；序列化或文件操作失败时保留原错误原因。
pub fn write_json_atomic<T: serde::Serialize + ?Sized>(
    path: &std::path::Path,
    value: &T,
) -> std::io::Result<()> {
    use std::io::Write;

    let mut bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    // 临时名只需在目标目录中唯一，不依赖目标文件名的字符编码。
    let temporary = parent.join(format!(".singularity-tmp-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| -> std::io::Result<()> {
        let mut handle = std::fs::File::create_new(&temporary)?;
        handle.write_all(&bytes)?;
        handle.sync_all()?;
        Ok(())
    })()
    .and_then(|()| std::fs::rename(&temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
