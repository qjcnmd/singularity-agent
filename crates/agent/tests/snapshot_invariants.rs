use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;

use agent::session::snapshot;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("singularity-snap-{tag}-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = snapshot::remove(&self.0);
    }
}

fn dir_entry_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .expect("read dir")
        .map(|e| e.expect("dir entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn snapshot_case_only_rename_restore_and_rollback() {
    let tmp = TempDir::new("case-rename");
    let ws = tmp.0.join("ws");
    let snap = tmp.0.join("home").join("sessions").join("snapshots").join("s1.git");
    fs::create_dir_all(&ws).expect("create ws");
    fs::write(ws.join("hello.txt"), "v1").expect("write v1");

    let tree1 = snapshot::capture(&snap, &ws).expect("capture v1");
    fs::rename(ws.join("hello.txt"), ws.join("tmp_case.txt")).expect("rename step 1");
    fs::rename(ws.join("tmp_case.txt"), ws.join("Hello.txt")).expect("rename step 2");
    fs::write(ws.join("Hello.txt"), "v2").expect("write v2");

    let changed = snapshot::diff_summary(&snap, &ws, &tree1).expect("diff summary");
    assert!(!changed.is_empty(), "case-only rename must be detected");

    let backup = snapshot::restore(&snap, &ws, &tree1).expect("restore v1");
    assert_eq!(dir_entry_names(&ws), vec!["hello.txt"]);
    assert_eq!(fs::read_to_string(ws.join("hello.txt")).expect("read v1"), "v1");

    backup.rollback().expect("rollback to v2");
    assert_eq!(dir_entry_names(&ws), vec!["Hello.txt"]);
    assert_eq!(fs::read_to_string(ws.join("Hello.txt")).expect("read v2"), "v2");
}

#[expect(
    clippy::permissions_set_readonly_false,
    reason = "test cleanup clears readonly attribute on temporary test files"
)]
#[test]
fn snapshot_readonly_file_restore_and_rollback() {
    let tmp = TempDir::new("readonly");
    let ws = tmp.0.join("ws");
    let snap = tmp.0.join("home").join("sessions").join("snapshots").join("s1.git");
    fs::create_dir_all(&ws).expect("create ws");
    let locked = ws.join("locked.txt");
    fs::write(&locked, "original").expect("write original");

    let tree1 = snapshot::capture(&snap, &ws).expect("capture");
    fs::write(&locked, "modified").expect("write modified");
    let mut perms = fs::metadata(&locked).expect("meta").permissions();
    perms.set_readonly(true);
    fs::set_permissions(&locked, perms).expect("set readonly");

    let backup = snapshot::restore(&snap, &ws, &tree1).expect("restore over readonly");
    assert_eq!(fs::read_to_string(&locked).expect("read restored"), "original");

    backup.rollback().expect("rollback readonly");
    assert_eq!(fs::read_to_string(&locked).expect("read rolled back"), "modified");
    assert!(fs::metadata(&locked).expect("meta after rollback").permissions().readonly());

    let mut clear = fs::metadata(&locked).expect("meta").permissions();
    clear.set_readonly(false);
    fs::set_permissions(&locked, clear).expect("clear readonly");
}

#[test]
fn snapshot_rejects_singularity_home_and_subdirectories() {
    let tmp = TempDir::new("home-guard");
    let home = tmp.0.join("singularity-home");
    let sessions = home.join("sessions");
    let snap = snapshot::session_snapshot_directory(&sessions, "test-session");
    fs::create_dir_all(&sessions).expect("create sessions");

    for forbidden in [&home, &sessions, &snap] {
        let err = snapshot::capture(&snap, forbidden).expect_err("must reject app data dir");
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "path: {}", forbidden.display());
    }
}

#[test]
fn snapshot_readonly_diff_on_missing_dir_returns_not_found_without_creating() {
    let tmp = TempDir::new("missing-diff");
    let ws = tmp.0.join("ws");
    let snap = tmp.0.join("home").join("sessions").join("snapshots").join("gone.git");
    fs::create_dir_all(&ws).expect("create ws");
    let dummy_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    let summary_err = snapshot::diff_summary(&snap, &ws, dummy_tree).expect_err("missing snapshot");
    assert_eq!(summary_err.kind(), ErrorKind::NotFound);
    assert!(!snap.exists(), "diff_summary must not create snapshot dir");

    let file_err = snapshot::file_diff(&snap, &ws, dummy_tree, "a.txt").expect_err("missing snapshot");
    assert_eq!(file_err.kind(), ErrorKind::NotFound);
    assert!(!snap.exists(), "file_diff must not create snapshot dir");
}

#[test]
fn snapshot_remove_clears_readonly_git_objects_and_is_idempotent() {
    let tmp = TempDir::new("remove");
    let ws = tmp.0.join("ws");
    let snap = tmp.0.join("home").join("sessions").join("snapshots").join("rm.git");
    fs::create_dir_all(&ws).expect("create ws");
    fs::write(ws.join("a.txt"), "hello world").expect("write");

    let _tree = snapshot::capture(&snap, &ws).expect("capture");
    assert!(snap.join("HEAD").exists());

    snapshot::remove(&snap).expect("remove bare repo");
    assert!(!snap.exists());
    snapshot::remove(&snap).expect("remove is idempotent");
}

#[test]
fn snapshot_rollback_reports_errors_instead_of_swallowing() {
    let tmp = TempDir::new("rollback-err");
    let ws = tmp.0.join("ws");
    let snap = tmp.0.join("home").join("sessions").join("snapshots").join("rb.git");
    fs::create_dir_all(&ws).expect("create ws");
    fs::write(ws.join("a.txt"), "v1").expect("write v1");

    let tree1 = snapshot::capture(&snap, &ws).expect("capture");
    fs::write(ws.join("a.txt"), "v2").expect("write v2");
    let backup = snapshot::restore(&snap, &ws, &tree1).expect("restore");

    let target = ws.join("a.txt");
    let mut perms = fs::metadata(&target).expect("meta").permissions();
    perms.set_readonly(true);
    fs::set_permissions(&target, perms).expect("set readonly before failed rollback");

    // Deleting the bare repo makes rollback's `git cat-file` fail; readonly bit must stay intact.
    snapshot::remove(&snap).expect("delete snapshot repo");
    let err = backup.rollback().expect_err("rollback must surface git/io error");
    assert!(!err.to_string().is_empty());
    assert!(
        fs::metadata(&target).expect("meta after failed rollback").permissions().readonly(),
        "failed rollback must not clear target file readonly attribute"
    );
}

#[cfg(windows)]
#[test]
fn snapshot_excludes_windows_directory_junction_and_protects_external_target() {
    let tmp = TempDir::new("junction");
    let ws = tmp.0.join("ws");
    let external = tmp.0.join("external");
    let snap = tmp.0.join("home").join("sessions").join("snapshots").join("junc.git");
    fs::create_dir_all(&ws).expect("create ws");
    fs::create_dir_all(&external).expect("create external");
    fs::create_dir_all(ws.join("node_modules")).expect("create node_modules");
    fs::write(ws.join(".gitignore"), "node_modules/\n*.tmp\n!/ext-link/\n!ext-link/\n!ext-link/**\n")
        .expect("write gitignore");
    fs::write(ws.join("node_modules").join("dep.js"), "ignored").expect("write dep");
    fs::write(ws.join("skip.tmp"), "ignored-tmp").expect("write tmp");
    fs::write(ws.join("inside.txt"), "inside").expect("write inside");
    fs::write(external.join("secret.txt"), "external-secret").expect("write external");

    let junction = ws.join("ext-link");
    let status = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&external)
        .status()
        .expect("run mklink /J");
    assert!(status.success(), "mklink /J must succeed on Windows");

    let tree1 = snapshot::capture(&snap, &ws).expect("capture with junction");
    fs::write(external.join("secret.txt"), "changed-external").expect("mutate external");
    fs::write(ws.join("inside.txt"), "inside-v2").expect("mutate inside");

    let changed = snapshot::diff_summary(&snap, &ws, &tree1).expect("diff summary");
    let paths: Vec<_> = changed.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["inside.txt"], "junction contents must never enter snapshot: {paths:?}");

    let backup = snapshot::restore(&snap, &ws, &tree1).expect("restore with junction present");
    assert_eq!(fs::read_to_string(ws.join("inside.txt")).expect("inside"), "inside");
    assert_eq!(
        fs::read_to_string(external.join("secret.txt")).expect("external untouched"),
        "changed-external"
    );
    backup.rollback().expect("rollback with junction");

    // Restoring a tree that tries to write inside the junction path must fail and leave target untouched.
    fs::remove_dir(&junction).expect("remove junction temporarily");
    fs::create_dir_all(ws.join("ext-link")).expect("create real dir");
    fs::write(ws.join("ext-link").join("secret.txt"), "real-dir-data").expect("write");
    let real_dir_tree = snapshot::capture(&snap, &ws).expect("capture real dir");
    fs::remove_dir_all(ws.join("ext-link")).expect("remove real dir");
    let status2 = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&external)
        .status()
        .expect("recreate mklink /J");
    assert!(status2.success());
    let conflict_err = snapshot::restore(&snap, &ws, &real_dir_tree)
        .expect_err("restore into junction path must be rejected");
    assert_eq!(conflict_err.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        fs::read_to_string(external.join("secret.txt")).expect("external still untouched"),
        "changed-external"
    );

    // Restoring a tree that tries to write a file at the exact junction path must also be rejected.
    fs::remove_dir(&junction).expect("remove junction for file capture");
    fs::write(&junction, "regular-file-data").expect("write regular file");
    let real_file_tree = snapshot::capture(&snap, &ws).expect("capture regular file");
    fs::remove_file(&junction).expect("remove regular file");
    let status3 = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&external)
        .status()
        .expect("recreate mklink /J again");
    assert!(status3.success());
    let exact_conflict = snapshot::restore(&snap, &ws, &real_file_tree)
        .expect_err("restore file over junction path must be rejected");
    assert_eq!(exact_conflict.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        fs::read_to_string(external.join("secret.txt")).expect("external still untouched"),
        "changed-external"
    );

    // Replacing the workspace root itself with a junction must reject both restore and rollback.
    fs::remove_dir(&junction).expect("remove inner junction before root swap");
    fs::write(external.join("inside.txt"), "external-inside").expect("write external inside");
    fs::write(ws.join("inside.txt"), "inside-before-swap").expect("write inside before swap");
    let rollback_backup = snapshot::restore(&snap, &ws, &tree1).expect("restore before root swap");
    let ws_moved = tmp.0.join("ws-moved");
    fs::rename(&ws, &ws_moved).expect("move real ws away");
    let status4 = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&ws)
        .arg(&external)
        .status()
        .expect("create root junction");
    assert!(status4.success());
    let root_restore_err = snapshot::restore(&snap, &ws, &tree1)
        .expect_err("restore must reject workspace root replaced by junction");
    assert_eq!(root_restore_err.kind(), ErrorKind::InvalidInput);
    let root_rollback_err = rollback_backup
        .rollback()
        .expect_err("rollback must reject workspace root replaced by junction");
    assert_eq!(root_rollback_err.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        fs::read_to_string(external.join("inside.txt")).expect("external inside untouched"),
        "external-inside"
    );
    let _ = fs::remove_dir(&ws);
}

#[test]
fn snapshot_rollback_preserves_empty_directories() {
    let tmp = TempDir::new("empty-dirs");
    let ws = tmp.0.join("ws");
    let snap = tmp.0.join("home").join("sessions").join("snapshots").join("ed.git");
    fs::create_dir_all(&ws).expect("create ws");
    fs::write(ws.join("shape"), "as-file").expect("write shape file");
    fs::create_dir_all(ws.join("keep_empty")).expect("create keep_empty");
    fs::write(ws.join("keep_empty").join("nested").join("x"), "x")
        .or_else(|_| {
            fs::create_dir_all(ws.join("keep_empty").join("nested"))?;
            fs::write(ws.join("keep_empty").join("nested").join("added.txt"), "added")
        })
        .expect("write nested");

    let tree1 = snapshot::capture(&snap, &ws).expect("capture tree1");

    // Replace file `shape` with a dir holding BOTH a tracked file and a nested empty dir,
    // and clear `keep_empty/nested` leaving `keep_empty/` as a pre-existing empty dir.
    fs::remove_file(ws.join("shape")).expect("rm shape file");
    fs::create_dir_all(ws.join("shape").join("empty").join("nested")).expect("create empty nested");
    fs::write(ws.join("shape").join("tracked.txt"), "tracked").expect("write tracked");
    fs::remove_dir_all(ws.join("keep_empty").join("nested")).expect("rm nested");

    let backup = snapshot::restore(&snap, &ws, &tree1).expect("restore tree1");
    assert!(ws.join("shape").is_file());
    assert!(ws.join("keep_empty").join("nested").join("added.txt").is_file());

    backup.rollback().expect("rollback restores empty dirs");
    assert!(ws.join("shape").join("tracked.txt").is_file());
    assert!(ws.join("shape").join("empty").join("nested").is_dir());
    assert!(ws.join("keep_empty").is_dir(), "pre-existing empty dir must survive rollback");
    assert!(!ws.join("keep_empty").join("nested").exists());
}

#[test]
fn snapshot_handles_many_protected_nested_repos_without_argv_overflow() {
    let tmp = TempDir::new("many-protected");
    let ws = tmp.0.join("ws");
    let snap = tmp.0.join("home").join("sessions").join("snapshots").join("many.git");
    fs::create_dir_all(&ws).expect("create ws");
    fs::write(ws.join("root.txt"), "hello").expect("write root");
    for i in 0..450 {
        let dir = ws.join(format!("nested_protected_repo_long_name_padding_segment_{i:04}"));
        fs::create_dir_all(dir.join(".git")).expect("create nested .git");
        fs::write(dir.join("inner.txt"), "nested").expect("write inner");
    }

    let tree = snapshot::capture(&snap, &ws).expect("capture succeeds with 450 nested repos");
    fs::write(ws.join("root.txt"), "hello-2").expect("modify root");
    let changed = snapshot::diff_summary(&snap, &ws, &tree).expect("diff summary");
    let paths: Vec<_> = changed.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["root.txt"]);
}

#[cfg(windows)]
#[test]
fn snapshot_case_only_parent_dir_conflict_with_ignored_file_fails_before_mutating() {
    let tmp = TempDir::new("case-parent-conflict");
    let ws = tmp.0.join("ws");
    let snap = tmp.0.join("home").join("sessions").join("snapshots").join("cpc.git");
    fs::create_dir_all(ws.join("Lower")).expect("create Lower");
    fs::write(ws.join(".gitignore"), "*.local\n").expect("write gitignore");
    fs::write(ws.join("Lower").join("a.txt"), "v1").expect("write a.txt");
    fs::write(ws.join("sibling.txt"), "s1").expect("write sibling");

    let tree1 = snapshot::capture(&snap, &ws).expect("capture v1");

    // Rename Lower -> lower with an ignored file inside; also mutate sibling.txt.
    fs::rename(ws.join("Lower"), ws.join("tmp_lower")).expect("rename 1");
    fs::rename(ws.join("tmp_lower"), ws.join("lower")).expect("rename 2");
    fs::write(ws.join("lower").join("user.local"), "ignored-data").expect("write ignored");
    fs::write(ws.join("sibling.txt"), "s2").expect("write sibling v2");

    let err = snapshot::restore(&snap, &ws, &tree1)
        .expect_err("case-only parent dir with ignored file must fail loudly");
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert_eq!(
        fs::read_to_string(ws.join("sibling.txt")).expect("sibling untouched"),
        "s2",
        "must abort before mutating any workspace files"
    );
    assert_eq!(fs::read_to_string(ws.join("lower").join("user.local")).expect("ignored"), "ignored-data");

    // Once ignored file is removed, pure case-only directory rename (even with empty subdir) restores cleanly.
    fs::remove_file(ws.join("lower").join("user.local")).expect("rm ignored");
    fs::create_dir_all(ws.join("lower").join("empty")).expect("create empty subdir");
    let backup = snapshot::restore(&snap, &ws, &tree1).expect("pure case dir rename restores");
    assert!(snapshot::diff_summary(&snap, &ws, &tree1).expect("diff").is_empty());
    assert!(dir_entry_names(&ws).contains(&"Lower".to_owned()));

    backup.rollback().expect("rollback case dir rename");
    assert!(dir_entry_names(&ws).contains(&"lower".to_owned()));
    assert!(ws.join("lower").join("empty").is_dir());
}
