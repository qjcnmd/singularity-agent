//! 已登记 Workspace 内的有界文件候选搜索。
//!
//! 这里只做目录遍历和过滤；原生文件夹选择窗口在 `apps/desktop/desktop/main.ts`。扫描失败照常返回
//! Err，只有「条目在扫描期间消失」这种瞬时情况按没有候选跳过。

use singularity_core::workspace::is_ignored_directory;
use singularity_protocol::FileCandidate;

/// 查找路径中包含 query 的候选，取得调用者所需数量后停止遍历。
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
    let mut candidates = Vec::new();
    while let Some(directory) = pending.pop() {
        if candidates.len() >= limit {
            break;
        }
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&directory)
            .map_err(|error| format!("workspace directory could not be read: {error}"))?
        {
            match entry {
                Ok(entry) => entries.push(entry),
                // 遍历期间条目消失属于可接受的瞬时情况，按没有候选跳过；
                // 其他读取失败必须上报，否则调用方会把漏项当成一次完整搜索。
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
                // 条目已经不存在就跳过；权限之类的错误仍然上报。
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!("workspace entry type could not be read: {error}"));
                }
            };
            // 符号链接既不算候选也不跟着进去，免得绕圈或重复扫描。
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
            let relative = path
                .strip_prefix(root.as_path())
                .expect("directory entries retain the workspace root prefix");
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
