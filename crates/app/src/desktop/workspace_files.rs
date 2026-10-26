//! 已添加项目目录内的文件候选搜索（最多返回 12 条）。
//!
//! 这里只做目录遍历和过滤；原生文件夹选择窗口在 `apps/desktop/desktop/main.ts`。扫描失败直接报错，
//! 扫描期间文件被删除则跳过。

use singularity_protocol::FileCandidate;

const MAX_FILE_CANDIDATES: usize = 12;

/// 查找路径中包含 query 的候选，找到 12 项后停止遍历。
pub(crate) fn search_files(directory: &str, query: &str) -> Result<Vec<FileCandidate>, String> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let root = singularity_core::canonicalize_workspace(directory)?;
    let mut pending = vec![root.as_path().to_path_buf()];
    let mut candidates = Vec::new();
    'search: while let Some(directory) = pending.pop() {
        let directory_entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && directory != root.as_path() => {
                continue;
            }
            Err(error) => return Err(format!("workspace directory could not be read: {error}")),
        };
        let mut entries = Vec::new();
        for entry in directory_entries {
            match entry {
                Ok(entry) => entries.push(entry),
                // 遍历期间条目消失是正常瞬时情况，按没有候选跳过；
                // 其他读取失败要上报：静默跳过会让调用方把漏项当成一次完整搜索。
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!("workspace directory entry could not be read: {error}"));
                }
            }
        }
        entries.sort_by_key(|entry| entry.file_name().to_string_lossy().to_lowercase());
        for entry in entries {
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
                if !matches!(entry.file_name().to_string_lossy().as_ref(), ".git" | "node_modules" | "target")
                {
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
                if candidates.len() == MAX_FILE_CANDIDATES {
                    break 'search;
                }
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
