use std::io::Read;
use std::path::{Path, PathBuf};

use protocol::{RpcError, TextDocument};

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

pub(crate) fn read_text_file(resolved: &Path) -> Result<TextDocument, RpcError> {
    let shown = app_core::display_path(resolved);
    let file = std::fs::File::open(resolved).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => RpcError::new(format!("文件不存在：{shown}")),
        std::io::ErrorKind::PermissionDenied
            if std::fs::metadata(resolved).is_ok_and(|metadata| !metadata.is_file()) =>
        {
            RpcError::new(format!("不是文件：{shown}"))
        }
        _ => RpcError::new(format!("{shown}: {error}")),
    })?;
    let metadata = file.metadata().map_err(|error| RpcError::new(format!("{shown}: {error}")))?;
    if !metadata.is_file() {
        return Err(RpcError::new(format!("不是文件：{shown}")));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(RpcError::new(format!("文件超过 2 MB，请用编辑器打开：{shown}")));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).expect("file size is at most 2 MiB"));
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| RpcError::new(format!("{shown}: {error}")))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(RpcError::new(format!("文件超过 2 MB，请用编辑器打开：{shown}")));
    }
    if bytes.contains(&0) {
        return Err(RpcError::new(format!("不是文本文件：{shown}")));
    }
    let content = String::from_utf8(bytes).map_err(|_| RpcError::new(format!("不是 UTF-8 文本：{shown}")))?;
    Ok(TextDocument { path: shown, content })
}

pub(crate) fn resolve_path(cwd: &str, path: &str) -> PathBuf {
    let path = path.trim();
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"))
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(cwd).join(path)
    }
}
