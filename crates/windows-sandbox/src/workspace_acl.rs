use crate::acl::add_deny_write_ace;
use crate::deny_read_acl::cleanup_empty_runtime_sentinel;
use crate::deny_read_acl::ensure_directory_materialized;
use crate::path_normalization::canonicalize_path;
use crate::product_identity::PROTECTED_METADATA_DIR_NAME;
use anyhow::Result;
use singularity_core::PROTECTED_AGENTS_DIR_NAME;
use std::ffi::c_void;
use std::path::Path;

pub fn is_command_cwd_root(root: &Path, canonical_command_cwd: &Path) -> bool {
    canonicalize_path(root) == canonical_command_cwd
}

/// # Safety
/// Caller must ensure `psid` is a valid SID pointer.
pub unsafe fn protect_workspace_singularity_dir(cwd: &Path, psid: *mut c_void) -> Result<bool> {
    protect_workspace_subdir(cwd, psid, PROTECTED_METADATA_DIR_NAME)
}

/// # Safety
/// Caller must ensure `psid` is a valid SID pointer.
pub unsafe fn protect_workspace_agents_dir(cwd: &Path, psid: *mut c_void) -> Result<bool> {
    protect_workspace_subdir(cwd, psid, PROTECTED_AGENTS_DIR_NAME)
}

unsafe fn protect_workspace_subdir(cwd: &Path, psid: *mut c_void, subdir: &str) -> Result<bool> {
    let path = cwd.join(subdir);
    let created = match std::fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_dir() => {
            // The generic deny-path pass protects an existing file with the reserved name.
            return Ok(false);
        }
        Ok(_) => false,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            ensure_directory_materialized(&path)?
        }
        Err(err) => return Err(err.into()),
    };
    match add_deny_write_ace(&path, psid) {
        Ok(added) => Ok(added),
        Err(err) => {
            if created && let Err(cleanup) = cleanup_empty_runtime_sentinel(&path, cwd, true) {
                return Err(err.context(format!("cleanup failed: {cleanup}")));
            }
            Err(err)
        }
    }
}
