#![deny(unsafe_code)]

//! 各 crate 共用的数据目录、项目路径、AGENTS.md 加载、技能发现与基础工具函数。

mod project_instructions;
pub mod skills;
mod user_home;
pub mod workspace;

pub use project_instructions::{InstructionFile, load_agent_instructions};
pub use user_home::resolve_home;
pub use workspace::{
    CanonicalWorkspacePath, canonicalize_workspace, same_saved_path, saved_path_key, workspace_key,
};

/// 向上查找项目根目录（AGENTS.md 与项目技能的查找上界）时识别的 `.git` 标记。
pub(crate) const PROJECT_ROOT_MARKER: &str = ".git";

/// 启发式 Token 估算：按 UTF-16 字符数除以 4 向上取整。上下文预算和胶囊上的估算共用这条规则。
pub fn estimate_tokens_of(text: &str) -> u64 {
    (text.encode_utf16().count() as u64).div_ceil(4)
}

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
    let mut bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    write_bytes_atomic(path, &bytes)
}

/// 同目录写入临时文件并刷盘，成功后替换目标；失败保留目标并清理临时文件。
pub fn write_bytes_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    // 临时名只需在目标目录中唯一，不依赖目标文件名的字符编码。
    let temporary = parent.join(format!(".singularity-tmp-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| -> std::io::Result<()> {
        let mut handle = std::fs::File::create_new(&temporary)?;
        handle.write_all(bytes)?;
        handle.sync_all()?;
        Ok(())
    })()
    .and_then(|()| std::fs::rename(&temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
