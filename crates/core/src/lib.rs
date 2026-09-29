#![deny(unsafe_code)]

//! 各 crate 共享的文件权限与 workspace 规则。

mod project_instructions;
pub mod skills;
mod user_home;
pub mod workspace;

pub use project_instructions::{ProjectInstructions, load_agent_instructions};
pub use user_home::{HomeOrigin, ResolvedHome, resolve_home};
pub use workspace::{CanonicalWorkspacePath, canonicalize_workspace, saved_directory_matches};

/// 项目根标记：从工作目录向上找到的第一个带该标记的目录就是项目根。指令加载与技能发现
/// 共用这个标记，但读标记失败时的策略不同（指令加载直接报错，技能发现退回当前目录），
/// 所以这里只共享标记本身，查找策略留在各自调用方。
pub(crate) const PROJECT_ROOT_MARKER: &str = ".git";

/// 把时长换算成毫秒；超出协议整数范围时截到 u64 能表示的最大值。
pub fn duration_millis(duration: std::time::Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

/// 当前 UTC 时间，ISO 8601 格式、毫秒精度；会话记录与实时快照共用这个写法。
pub fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .expect("utc timestamp always formats")
}

/// 协议与界面使用的路径文本：统一成正斜杠，并改写 Windows 的 verbatim 前缀。
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

/// 返回不超过 max_bytes 字节的有效 UTF-8 前缀；text 超长时截到字符边界并
/// 返回 true。按字节预算截断前缀的地方都走这一个实现。
pub fn utf8_prefix(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let end = text.floor_char_boundary(max_bytes);
    (&text[..end], true)
}

/// 创建新的数据文件；访问权限沿用 Windows 目录继承下来的 ACL。
pub fn create_new_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    options.open(path)
}

/// 通过系统文件 API 创建应用数据目录，允许目录链接和 junction。
pub fn create_data_dir(path: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))
}

/// 用「临时文件 + 原子替换」把字节写入目标路径：先在同一个目录下写临时文件并 sync_all，再做
/// 原子替换，读者看到的要么是完整的旧内容、要么是完整的新内容；写入或替换失败时删掉临时文件。
pub fn atomic_replace_bytes(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    atomic_write(path, bytes, create_new_file, true)
}

/// 原子创建新的数据文件：完整内容写入同目录临时文件后才公开目标路径；目标已存在时失败。
pub fn atomic_create_bytes(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    atomic_write(path, bytes, create_new_file, false)
}

/// 原子写入 workspace 文件，并保留文件原有权限；文件还不存在时使用 Windows
/// 的创建默认权限。凭据等应用状态必须改用 `atomic_replace_bytes`。
pub fn atomic_replace_workspace_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let permissions = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    atomic_write(
        path,
        bytes,
        |temporary| {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(temporary)?;
            if let Some(permissions) = &permissions {
                file.set_permissions(permissions.clone())?;
            }
            Ok(file)
        },
        true,
    )
}

fn atomic_write(
    path: &std::path::Path,
    bytes: &[u8],
    create: impl FnOnce(&std::path::Path) -> std::io::Result<std::fs::File>,
    replace_existing: bool,
) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    // 临时名只需在目标目录中唯一，不依赖目标文件名的字符编码。
    let temporary = parent.join(format!(
        ".singularity-tmp-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut handle = create(&temporary)?;
        handle.write_all(bytes)?;
        handle.flush()?;
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
fn atomic_move(
    from: &std::path::Path,
    to: &std::path::Path,
    replace_existing: bool,
) -> std::io::Result<()> {
    {
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
}
