//! Application state path validation.
//!
//! This module owns repository-home containment checks and owner/link-safe
//! canonicalization of session state directories.

/// 校验 SINGULARITY_HOME 不在当前仓库内（仓库边界以 `.git` 标记查找，找不到时
/// 以 cwd 为边界）。`home` 可能尚不存在：先对已存在前缀做 canonicalize 再比较。
pub(crate) fn ensure_home_outside_current_repo(home: &std::path::Path) -> Result<(), String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("failed to read app-server cwd: {error}"))?;
    ensure_home_outside_repo(home, &cwd)
}

pub(crate) fn ensure_home_outside_repo(
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> Result<(), String> {
    let root = singularity_core::find_workspace_root(cwd)
        .map_err(|error| format!("failed to locate repository boundary: {error}"))?;
    let canonical_home = canonicalize_existing_prefix(home)?;
    let canonical_root = canonicalize_existing_prefix(&root)?;
    if canonical_home.starts_with(&canonical_root) {
        return Err("SINGULARITY_HOME must not be inside the current repository".to_string());
    }
    Ok(())
}

/// 对路径的已存在前缀做 canonicalize，缺失的尾部组件原样保留（用于尚不存在的目录）。
pub(crate) fn canonicalize_existing_prefix(
    path: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let mut current = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(&current) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = current.file_name().ok_or_else(|| {
                    format!("cannot canonicalize path prefix: {}", path.display())
                })?;
                missing.push(component.to_os_string());
                if !current.pop() {
                    return Err(format!(
                        "cannot canonicalize path prefix: {}",
                        path.display()
                    ));
                }
            }
            Err(_) => {
                return Err(format!(
                    "cannot canonicalize path prefix: {}",
                    path.display()
                ));
            }
        }
    }
}
