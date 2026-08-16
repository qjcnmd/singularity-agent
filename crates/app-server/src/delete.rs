//! Recoverable session deletion: JSONL is first atomically renamed to a
//! tombstone on the same filesystem, then the SQLite index row is removed.

use std::path::{Path, PathBuf};

use super::*;

/// Test-only fault points; production uses `DeleteFaults::default()`.
#[derive(Debug, Default)]
pub(super) struct DeleteFaults {
    pub(super) fail_rename: bool,
    pub(super) fail_index_delete: bool,
    pub(super) leave_tombstone: bool,
}

/// Delete one session with a recoverable two-phase protocol.
///
/// Returns `Some(tombstone_path)` when logical deletion succeeded but final
/// tombstone cleanup failed; the tombstone is recognizable and never restored
/// to a visible session.
pub(super) fn delete_session_with_faults(
    record: &SessionRecord,
    store: &SessionStore,
    faults: DeleteFaults,
) -> AppServerResult<Option<PathBuf>> {
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
    let parent = rollout.parent().ok_or_else(|| {
        AppServerError::Workspace(format!(
            "session rollout has no parent directory: {}",
            rollout.display()
        ))
    })?;
    let file_name = rollout
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            AppServerError::Workspace(format!(
                "session rollout name is not UTF-8: {}",
                rollout.display()
            ))
        })?;
    let tombstone = parent.join(format!(
        ".{file_name}.{}.tombstone",
        Uuid::new_v4().simple()
    ));

    if faults.fail_rename {
        return Err(AppServerError::Workspace(
            "injected session rename failure".to_string(),
        ));
    }
    std::fs::rename(rollout, &tombstone).map_err(|error| {
        AppServerError::Workspace(format!(
            "failed to rename session rollout {} to tombstone: {error}",
            rollout.display()
        ))
    })?;

    let index_result = if faults.fail_index_delete {
        Err(StoreError::InvalidState(
            "injected session index delete failure".to_string(),
        ))
    } else {
        store.delete_session(&record.session_id)
    };
    if let Err(error) = index_result {
        // Roll back: restore the original visible path, then return the index
        // failure. If rollback itself fails, keep the tombstone and report both.
        return match std::fs::rename(&tombstone, rollout) {
            Ok(()) => Err(AppServerError::Store(error)),
            Err(restore_error) => Err(AppServerError::Workspace(format!(
                "session index delete failed ({error}) and tombstone restore failed ({restore_error}); tombstone={}",
                tombstone.display()
            ))),
        };
    }

    if faults.leave_tombstone {
        return Ok(Some(tombstone));
    }
    match std::fs::remove_file(&tombstone) {
        Ok(()) => Ok(None),
        Err(cleanup_error) => {
            eprintln!(
                "session {} logically deleted; tombstone cleanup failed: {cleanup_error}; tombstone={}",
                record.session_id,
                tombstone.display()
            );
            Ok(Some(tombstone))
        }
    }
}
