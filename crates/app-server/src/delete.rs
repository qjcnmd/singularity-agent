//! Session deletion：JSONL 是唯一持久权威，删除会话即移除其 rollout 文件
//! 并丢弃内存索引记录。

use std::io;
use std::path::Path;

use super::*;

/// 删除一个会话：先丢弃内存索引记录（进程内、可重试），最后才执行不可逆的
/// 文件删除。索引锁失败时 rollout 保持原样，不会出现「文件已删但缓存仍
/// 残留」的不可恢复状态；文件删除失败时 JSONL 仍是权威，可由重新发现重建
/// 索引记录。
pub(super) fn delete_session(record: &SessionRecord, store: &SessionIndex) -> AppServerResult<()> {
    store.delete_session(&record.session_id)?;
    remove_rollout(Path::new(&record.rollout_path))
}

fn remove_rollout(rollout: &Path) -> AppServerResult<()> {
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
    })
}
