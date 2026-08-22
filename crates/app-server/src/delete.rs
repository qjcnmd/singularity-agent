//! Session deletion: JSONL is the only persistent authority, so deleting a
//! session removes its rollout file and drops the in-memory index record.

use std::io;
use std::path::Path;

use super::*;

/// Delete one session: remove the JSONL rollout (missing file is an error),
/// then drop the in-memory index record.
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
