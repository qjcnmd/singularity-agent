use crate::identity::{DATA_DIR_NAME, HOME_ENV_VAR};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

pub fn resolve_home() -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os(HOME_ENV_VAR).filter(|value| !is_blank(value)) {
        let base = Path::new(&value);
        let path =
            validated_root(base).map_err(|reason| format!("{HOME_ENV_VAR} {reason}: {}", base.display()))?;
        return Ok(path);
    }
    let base = std::env::home_dir().ok_or_else(|| {
        "cannot resolve the default data directory: system user profile is unavailable".to_string()
    })?;
    let base = validated_root(&base).map_err(|reason| {
        format!(
            "cannot resolve the default data directory: system user profile {reason}: {}",
            base.display()
        )
    })?;
    Ok(base.join(DATA_DIR_NAME))
}

fn is_blank(value: &OsStr) -> bool {
    value.to_string_lossy().trim().is_empty()
}

fn validated_root(base: &Path) -> Result<PathBuf, String> {
    if !base.is_absolute() {
        return Err("must be an absolute path".into());
    }
    std::path::absolute(base).map_err(|error| format!("cannot resolve path: {error}"))
}
