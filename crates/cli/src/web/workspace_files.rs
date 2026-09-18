//! 已登记 Workspace 内的有界文件候选搜索。
//!
//! 只做目录遍历与过滤；原生文件夹选择窗口在 `directory_picker`。
//! 扫描失败按已有 Result 上报，只有「条目在扫描期间消失」这一瞬时事实按无
//! 候选跳过——调用方据此区分「没有匹配」与「读取失败导致的漏项」。

use singularity_core::workspace::is_ignored_directory;
use singularity_protocol::FileCandidate;

const MAX_SCANNED_DIRECTORIES: usize = 2_000;

/// 候选上限由调用入口校验（RPC 只接受 1..=100），此处不再静默修改入参。
pub(crate) fn search_files(
    directory: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<FileCandidate>, String> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let root = singularity_core::canonicalize_workspace(directory)?;
    let mut pending = vec![root.as_path().to_path_buf()];
    let mut scanned = 0;
    let mut candidates = Vec::new();
    while let Some(directory) = pending.pop() {
        if scanned >= MAX_SCANNED_DIRECTORIES || candidates.len() >= limit {
            break;
        }
        scanned += 1;
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&directory)
            .map_err(|error| format!("workspace directory could not be read: {error}"))?
        {
            match entry {
                Ok(entry) => entries.push(entry),
                // 遍历期间条目消失是可接受的瞬时事实，按无候选跳过；
                // 其他读取失败必须上报，否则调用方会把漏项当成完整搜索。
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "workspace directory entry could not be read: {error}"
                    ));
                }
            }
        }
        entries.sort_by_key(|entry| entry.file_name().to_string_lossy().to_lowercase());
        for entry in entries {
            if candidates.len() >= limit {
                break;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                // 与目录项消失同理：条目已不存在时跳过；权限等其余错误上报。
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!("workspace entry type could not be read: {error}"));
                }
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                if !is_ignored_directory(&entry.file_name().to_string_lossy()) {
                    pending.push(path);
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Ok(relative) = path.strip_prefix(root.as_path()) else {
                continue;
            };
            let relative = singularity_core::display_path(relative);
            if relative.to_lowercase().contains(&query) {
                candidates.push(FileCandidate { path: relative });
            }
        }
    }
    candidates.sort_by(|left, right| {
        left.path
            .to_lowercase()
            .cmp(&right.path.to_lowercase())
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // 测试断言惯例

    use super::*;

    /// 搜索输入：临时 Workspace 的字符串形式（与 RPC 入口传参一致）。
    fn search(
        directory: &std::path::Path,
        query: &str,
        limit: usize,
    ) -> Result<Vec<FileCandidate>, String> {
        search_files(&directory.to_string_lossy(), query, limit)
    }

    #[test]
    fn no_match_returns_an_empty_list_without_a_failure() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir_all(workspace.path().join("nested")).expect("nested directory");
        std::fs::write(workspace.path().join("nested/notes.txt"), "x").expect("fixture file");
        assert!(
            search(workspace.path(), "missing", 10)
                .expect("no candidate is not a scan failure")
                .is_empty()
        );
    }

    #[test]
    fn empty_query_returns_no_candidates() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("notes.txt"), "x").expect("fixture file");
        assert!(
            search(workspace.path(), "   ", 10)
                .expect("empty query")
                .is_empty()
        );
    }

    /// 扫描期无法读取的目录必须上报：调用方不能把读取失败当成「没有候选」。
    /// 独占打开的目录使同目录的列举失败，这是 Windows 上确定可注入的扫描故障。
    #[cfg(windows)]
    #[test]
    fn unreadable_directory_is_reported_instead_of_an_empty_list() {
        use std::os::windows::fs::OpenOptionsExt;

        /// FILE_FLAG_BACKUP_SEMANTICS：允许打开目录本身（否则只会得到访问拒绝）。
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

        let workspace = tempfile::tempdir().expect("workspace");
        let locked = workspace.path().join("locked");
        std::fs::create_dir_all(&locked).expect("locked directory");
        std::fs::write(locked.join("needle.txt"), "x").expect("fixture file");
        // 声明顺序决定释放顺序：锁先于临时目录释放，临时目录才能被删除。
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&locked)
            .expect("lock directory");

        let error = search(workspace.path(), "needle", 10)
            .expect_err("a failed scan is not an empty result");
        assert!(
            error.contains("workspace directory could not be read"),
            "{error}"
        );
    }
}
