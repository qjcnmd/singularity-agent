#![cfg(windows)]

//! Windows-only owner-only ACL contract tests. These tests are skipped on
//! non-Windows targets and must be executed by native Windows `cargo test`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;

use singularity_store::{ensure_owner_only_dir, ensure_owner_only_file};

#[test]
fn inherited_acl_file_is_tightened_and_remains_owner_only_after_reopen() {
    let dir = tempfile::tempdir().expect("temp dir");
    ensure_owner_only_dir(dir.path()).expect("tighten parent dir first");
    let path = dir.path().join("session.jsonl");
    std::fs::write(&path, "{}").expect("write file");

    ensure_owner_only_file(&path).expect("tighten file ACL");
    let handle = File::open(&path).expect("open file");
    singularity_core::ensure_owner_only_handle(&handle)
        .expect("owner + protected DACL verification");

    drop(handle);
    ensure_owner_only_file(&path).expect("reopen still passes");
}

#[test]
fn backup_file_is_explicitly_owner_only() {
    let dir = tempfile::tempdir().expect("temp dir");
    ensure_owner_only_dir(dir.path()).expect("tighten backup parent");
    let path = dir.path().join("pre-migration-session.jsonl");
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .expect("create backup file");
    file.write_all(b"{\"type\":\"session\"}\n")
        .expect("write backup");
    drop(file);

    ensure_owner_only_file(&path).expect("backup owner-only");
    let handle = File::open(&path).expect("reopen backup");
    singularity_core::ensure_owner_only_handle(&handle).expect("backup ACL verification");
}

#[test]
fn session_directory_is_owner_only() {
    let dir = tempfile::tempdir().expect("temp dir");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir(&sessions).expect("create sessions dir");
    ensure_owner_only_dir(&sessions).expect("tighten dir ACL");
    ensure_owner_only_dir(&sessions).expect("dir reopen still passes");
}

#[test]
fn unreachable_path_fails_closed_instead_of_silently_passing() {
    let dir = tempfile::tempdir().expect("temp dir");
    ensure_owner_only_dir(dir.path()).expect("tighten parent dir");
    let path = dir.path().join("locked.jsonl");
    std::fs::write(&path, "{}").expect("write file");

    // Hold the path with share_mode(0): any repair/reopen attempt must fail
    // closed rather than pretend owner-only was established.
    let holder = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&path)
        .expect("hold exclusive handle");
    assert!(ensure_owner_only_file(&path).is_err());
    drop(holder);
    ensure_owner_only_file(&path).expect("repair succeeds after release");
}
