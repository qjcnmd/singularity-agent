use crate::deny_read_state::lock_state;
use crate::path_normalization::canonical_path_key;
use crate::path_normalization::canonicalize_path;
use crate::path_safety::ensure_case_insensitive_acl_path;
use crate::product_identity::CAPABILITY_SID_FILE_NAME;
use anyhow::Context;
use anyhow::Result;
use rand::RngCore;
use rand::SeedableRng;
use rand::rngs::SmallRng;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CapSids {
    pub workspace: String,
    pub readonly: String,
    /// Per-workspace capability SIDs keyed by canonicalized CWD string.
    ///
    /// This is used to isolate workspaces from other workspace sandbox writes and to
    /// apply per-workspace denies (e.g. protect `CWD/.singularity`)
    /// without permanently affecting other workspaces.
    #[serde(default)]
    pub workspace_by_cwd: HashMap<String, String>,
    /// Per-write-root capability SIDs keyed by canonicalized write-root path.
    ///
    /// These are included in a workspace-write token only when the root is
    /// currently allowed, so stale ACLs from earlier extra roots do not expand
    /// later workspace sandboxes.
    #[serde(default)]
    pub writable_root_by_path: HashMap<String, String>,
}

pub fn cap_sid_file(sandbox_home: &Path) -> PathBuf {
    sandbox_home.join(CAPABILITY_SID_FILE_NAME)
}

fn make_random_cap_sid_string() -> String {
    let mut rng = SmallRng::from_entropy();
    let a = rng.next_u32();
    let b = rng.next_u32();
    let c = rng.next_u32();
    let d = rng.next_u32();
    format!("S-1-5-21-{a}-{b}-{c}-{d}")
}

fn persist_caps(path: &Path, caps: &CapSids) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create cap sid dir {}", dir.display()))?;
    }
    let json = serde_json::to_string(caps)?;
    crate::deny_read_state::atomic_store(path, json.as_bytes())?;
    Ok(())
}

fn load_or_create_cap_sids_unlocked(sandbox_home: &Path) -> Result<CapSids> {
    let path = cap_sid_file(sandbox_home);
    if path.exists() {
        let txt = fs::read_to_string(&path)
            .with_context(|| format!("read cap sid file {}", path.display()))?;
        let t = txt.trim();
        if t.starts_with('{') || t.ends_with('}') {
            return serde_json::from_str::<CapSids>(t)
                .with_context(|| format!("parse cap sid file {}", path.display()));
        }
        if !t.is_empty() {
            // Keep the existing single-SID input readable, but do not treat arbitrary
            // corruption as a capability. A malformed state file must stop setup rather than
            // silently rotate the SID set and make the on-disk ACL ownership ambiguous.
            if !t.starts_with("S-") {
                anyhow::bail!("invalid cap sid file {}", path.display());
            }
            let caps = CapSids {
                workspace: t.to_string(),
                readonly: make_random_cap_sid_string(),
                workspace_by_cwd: HashMap::new(),
                writable_root_by_path: HashMap::new(),
            };
            persist_caps(&path, &caps)?;
            return Ok(caps);
        }
        anyhow::bail!("empty cap sid file {}", path.display());
    }
    let caps = CapSids {
        workspace: make_random_cap_sid_string(),
        readonly: make_random_cap_sid_string(),
        workspace_by_cwd: HashMap::new(),
        writable_root_by_path: HashMap::new(),
    };
    persist_caps(&path, &caps)?;
    Ok(caps)
}

pub fn load_or_create_cap_sids(sandbox_home: &Path) -> Result<CapSids> {
    let _lock = lock_state(&cap_sid_file(sandbox_home))?;
    load_or_create_cap_sids_unlocked(sandbox_home)
}

/// Returns the workspace-specific capability SID for `cwd`, creating and persisting it if missing.
pub fn workspace_cap_sid_for_cwd(sandbox_home: &Path, cwd: &Path) -> Result<String> {
    ensure_case_insensitive_acl_path(cwd)?;
    let path = cap_sid_file(sandbox_home);
    let _lock = lock_state(&path)?;
    let mut caps = load_or_create_cap_sids_unlocked(sandbox_home)?;
    let key = canonical_path_key(cwd);
    if let Some(sid) = caps.workspace_by_cwd.get(&key) {
        return Ok(sid.clone());
    }
    let sid = make_random_cap_sid_string();
    caps.workspace_by_cwd.insert(key, sid.clone());
    persist_caps(&path, &caps)?;
    Ok(sid)
}

/// Returns the capability SID for an additional writable root, creating and persisting it if missing.
pub fn writable_root_cap_sid_for_path(sandbox_home: &Path, root: &Path) -> Result<String> {
    ensure_case_insensitive_acl_path(root)?;
    let path = cap_sid_file(sandbox_home);
    let _lock = lock_state(&path)?;
    let mut caps = load_or_create_cap_sids_unlocked(sandbox_home)?;
    let key = canonical_path_key(root);
    if let Some(sid) = caps.writable_root_by_path.get(&key) {
        return Ok(sid.clone());
    }
    let sid = make_random_cap_sid_string();
    caps.writable_root_by_path.insert(key, sid.clone());
    persist_caps(&path, &caps)?;
    Ok(sid)
}

pub fn workspace_write_cap_sid_for_root(
    sandbox_home: &Path,
    cwd: &Path,
    root: &Path,
) -> Result<String> {
    ensure_case_insensitive_acl_path(cwd)?;
    ensure_case_insensitive_acl_path(root)?;
    if canonical_path_key(root) == canonical_path_key(cwd) {
        workspace_cap_sid_for_cwd(sandbox_home, cwd)
    } else {
        writable_root_cap_sid_for_path(sandbox_home, root)
    }
}

pub fn workspace_write_root_contains_path(root: &Path, path: &Path) -> bool {
    canonicalize_path(path).starts_with(canonicalize_path(root))
}

pub fn workspace_write_root_overlaps_path(root: &Path, path: &Path) -> bool {
    workspace_write_root_contains_path(root, path) || workspace_write_root_contains_path(path, root)
}

pub fn workspace_write_root_specificity(root: &Path) -> usize {
    canonicalize_path(root).components().count()
}

#[cfg(test)]
mod tests {
    use super::load_or_create_cap_sids;
    use super::workspace_cap_sid_for_cwd;
    use super::workspace_write_cap_sid_for_root;
    use super::writable_root_cap_sid_for_path;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    #[test]
    fn equivalent_cwd_spellings_share_workspace_sid_key() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        std::fs::create_dir_all(&sandbox_home).expect("create singularity home");

        let workspace = temp.path().join("WorkspaceRoot");
        std::fs::create_dir_all(&workspace).expect("create workspace root");

        let canonical = dunce::canonicalize(&workspace).expect("canonical workspace root");
        let alt_spelling = PathBuf::from(
            canonical
                .to_string_lossy()
                .replace('\\', "/")
                .to_ascii_uppercase(),
        );

        let first_sid =
            workspace_cap_sid_for_cwd(&sandbox_home, canonical.as_path()).expect("first sid");
        let second_sid =
            workspace_cap_sid_for_cwd(&sandbox_home, alt_spelling.as_path()).expect("second sid");

        assert_eq!(first_sid, second_sid);

        let caps = load_or_create_cap_sids(&sandbox_home).expect("load caps");
        assert_eq!(caps.workspace_by_cwd.len(), 1);
    }

    #[test]
    fn write_roots_get_path_scoped_sids() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        std::fs::create_dir_all(&sandbox_home).expect("create singularity home");

        let workspace = temp.path().join("workspace");
        let extra_root = temp.path().join("extra-root");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(&extra_root).expect("create extra root");

        let workspace_sid = workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &workspace)
            .expect("workspace sid");
        let extra_sid = workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &extra_root)
            .expect("extra root sid");

        assert_ne!(workspace_sid, extra_sid);
        assert_eq!(
            extra_sid,
            writable_root_cap_sid_for_path(&sandbox_home, &extra_root)
                .expect("extra root sid again")
        );

        let caps = load_or_create_cap_sids(&sandbox_home).expect("load caps");
        assert_eq!(caps.workspace_by_cwd.len(), 1);
        assert_eq!(caps.writable_root_by_path.len(), 1);
    }

    #[test]
    fn concurrent_workspace_sid_updates_preserve_both_entries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let first_workspace = temp.path().join("first-workspace");
        let second_workspace = temp.path().join("second-workspace");
        std::fs::create_dir_all(&sandbox_home).expect("create singularity home");
        std::fs::create_dir_all(&first_workspace).expect("create first workspace");
        std::fs::create_dir_all(&second_workspace).expect("create second workspace");

        std::thread::scope(|scope| {
            let first = scope.spawn(|| workspace_cap_sid_for_cwd(&sandbox_home, &first_workspace));
            let second =
                scope.spawn(|| workspace_cap_sid_for_cwd(&sandbox_home, &second_workspace));
            first.join().expect("first SID update").expect("first SID");
            second
                .join()
                .expect("second SID update")
                .expect("second SID");
        });

        let caps = load_or_create_cap_sids(&sandbox_home).expect("load caps");
        assert_eq!(caps.workspace_by_cwd.len(), 2);
    }

    #[test]
    fn malformed_cap_state_fails_closed() {
        for contents in ["{not-json", ""] {
            let temp = tempfile::tempdir().expect("tempdir");
            let sandbox_home = temp.path().join("singularity-home");
            std::fs::create_dir_all(&sandbox_home).expect("create singularity home");
            std::fs::write(super::cap_sid_file(&sandbox_home), contents)
                .expect("write malformed cap state");

            assert!(load_or_create_cap_sids(&sandbox_home).is_err());
        }
    }
}
