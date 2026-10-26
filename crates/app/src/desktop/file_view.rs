//! 工作台右侧文件栏的只读文件读取：只读 UTF-8 文本，过大或二进制文件直接说明原因。

use std::path::{Path, PathBuf};

use singularity_protocol::{RpcError, TextDocument};

use super::app_server::invalid_request;

/// 文件栏只用来查看，超过这个大小的文件交给编辑器打开。
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// `path` 可以是绝对路径、`~/` 开头或相对 `cwd`；行号后缀（`file.rs:12`）由前端去掉。
pub(crate) fn read_text_file(cwd: &str, path: &str) -> Result<TextDocument, RpcError> {
    let resolved = resolve(cwd, path.trim());
    let shown = singularity_core::display_path(&resolved);
    let metadata = std::fs::metadata(&resolved).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => invalid_request(format!("文件不存在：{shown}")),
        _ => invalid_request(format!("{shown}: {error}")),
    })?;
    if !metadata.is_file() {
        return Err(invalid_request(format!("不是文件：{shown}")));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(invalid_request(format!("文件超过 2 MB，请用编辑器打开：{shown}")));
    }
    let bytes = std::fs::read(&resolved).map_err(|error| invalid_request(format!("{shown}: {error}")))?;
    if bytes.contains(&0) {
        return Err(invalid_request(format!("不是文本文件：{shown}")));
    }
    let content =
        String::from_utf8(bytes).map_err(|_| invalid_request(format!("不是 UTF-8 文本：{shown}")))?;
    Ok(TextDocument { path: shown, content })
}

fn resolve(cwd: &str, path: &str) -> PathBuf {
    let home = std::env::home_dir();
    if let (Some(rest), Some(home)) = (path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")), home) {
        return home.join(rest);
    }
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(cwd).join(path)
    }
}
