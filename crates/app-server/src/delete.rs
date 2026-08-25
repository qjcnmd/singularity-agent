//! Session deletion：JSONL 是唯一持久权威，删除会话即移除其 rollout 文件
//! 并丢弃内存索引记录。

use std::io;
use std::path::Path;

use super::*;

/// 删除一个会话：移除 JSONL rollout（文件缺失是错误），随后丢弃内存索引记录。
pub(super) fn delete_session(record: &SessionRecord, store: &SessionIndex) -> AppServerResult<()> {
    let rollout = Path::new(&record.rollout_path);
    let identity = std::fs::symlink_metadata(rollout).map_err(|error| {
        AppServerError::Workspace(format!(
            "failed to inspect session rollout {}: {error}",
            rollout.display()
        ))
    })?;
    if !identity.is_file() {
        return Err(AppServerError::Workspace(format!(
            "session rollout is not a regular file: {}",
            rollout.display()
        )));
    }
    std::fs::remove_file(rollout).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            AppServerError::Workspace(format!("session rollout not found: {}", rollout.display()))
        } else {
            AppServerError::Workspace(format!(
                "failed to remove session rollout {}: {error}",
                rollout.display()
            ))
        }
    })?;
    store.delete_session(&record.session_id)?;
    Ok(())
}
