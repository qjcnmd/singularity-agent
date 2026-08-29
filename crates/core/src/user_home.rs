//! 用户主目录与数据根目录解析模块。

use std::path::PathBuf;

/// 未显式设置 `SINGULARITY_HOME` 时使用的用户级数据子目录名。
const USER_SINGULARITY_DIR_NAME: &str = ".singularity";

/// 解析用户级 singularity 数据目录：显式 `SINGULARITY_HOME`，否则 `$HOME/.singularity`
/// （与 model crate 的用户配置目录语义一致）。
pub fn user_singularity_home() -> Option<PathBuf> {
    let explicit_home = std::env::var_os("SINGULARITY_HOME");
    let home = explicit_home
        .clone()
        .or_else(|| std::env::var_os("USERPROFILE"))
        .or_else(|| std::env::var_os("HOME"))?;
    let home = PathBuf::from(home);
    if home.as_os_str().is_empty() || !home.is_absolute() {
        return None;
    }
    if explicit_home.is_some() {
        Some(home)
    } else {
        Some(home.join(USER_SINGULARITY_DIR_NAME))
    }
}

/// 校验数据目录不位于当前 workspace 内。workspace 边界以 `cwd` 向上的
/// `.git` 标记为准，找不到标记时以 `cwd` 为边界；`home` 可以尚不存在。
pub fn ensure_singularity_home_outside_workspace(
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> Result<(), String> {
    let root = crate::find_workspace_root(cwd)
        .map_err(|error| format!("failed to locate repository boundary: {error}"))?;
    let canonical_home = canonicalize_existing_prefix(home)?;
    let canonical_root = canonicalize_existing_prefix(&root)?;
    if path_starts_with(&canonical_home, &canonical_root) {
        return Err("SINGULARITY_HOME must not be inside the current repository".to_string());
    }
    Ok(())
}

fn path_starts_with(path: &std::path::Path, prefix: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        let mut path_components = path.components();
        for prefix_component in prefix.components() {
            let Some(path_component) = path_components.next() else {
                return false;
            };
            if !path_component
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&prefix_component.as_os_str().to_string_lossy())
            {
                return false;
            }
        }
        true
    }
    #[cfg(not(windows))]
    {
        path.starts_with(prefix)
    }
}

/// 对路径的已存在前缀做 canonicalize，缺失的尾部组件原样保留。
fn canonicalize_existing_prefix(path: &std::path::Path) -> Result<PathBuf, String> {
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::ensure_singularity_home_outside_workspace;

    #[test]
    fn singularity_home_boundary_uses_nearest_repository_and_missing_tail() {
        let directory = tempfile::tempdir().expect("repository boundary directory");
        let workspace = directory.path().join("workspace");
        let repository = workspace.join("repository");
        let nested = repository.join("nested");
        std::fs::create_dir_all(&nested).expect("create repository tree");
        std::fs::write(repository.join(".git"), b"gitdir: test").expect("create worktree marker");

        ensure_singularity_home_outside_workspace(&workspace, &nested)
            .expect("repository ancestor remains usable");
        let inside = repository.join("missing-home");
        let error = ensure_singularity_home_outside_workspace(&inside, &nested)
            .expect_err("repository descendant rejected");
        assert!(error.contains("current repository"));
    }

    #[cfg(windows)]
    #[test]
    fn singularity_home_boundary_is_case_insensitive_with_missing_tail() {
        let directory = tempfile::tempdir().expect("repository boundary directory");
        let repository = directory.path().join("CaseSensitiveRepo");
        let nested = repository.join("nested");
        std::fs::create_dir_all(&nested).expect("create repository tree");
        std::fs::create_dir(repository.join(".git")).expect("create repository marker");
        let case_variant =
            std::path::PathBuf::from(repository.to_string_lossy().to_ascii_lowercase())
                .join("missing-home");

        assert!(
            ensure_singularity_home_outside_workspace(&case_variant, &nested).is_err(),
            "case variants of repository descendants must be rejected"
        );
    }
}
