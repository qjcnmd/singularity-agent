//! Session deletion：JSONL 是唯一持久权威，删除会话即移除其 rollout 文件。

use std::io;
use std::path::Path;

use super::*;

pub(super) fn delete_session(rollout_path: &Path) -> AppServerResult<()> {
    remove_rollout(rollout_path)
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
