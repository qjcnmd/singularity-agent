#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! Thread 目录故障与账本投影测试。
//!
//! 列表、摘要与分页从同一会话账本派生；覆盖文件读取与历史投影。

use std::sync::Arc;

use crate::Conversation;
use crate::ThreadCatalog;
use crate::test_support::{SessionsFixture, cwd};
use crate::thread_catalog::ARCHIVED_SESSIONS_DIR_NAME;
use singularity_agent::session::session_file_name;
use singularity_model::Provider;
use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};
use singularity_protocol::Thread;

fn catalog_fixture() -> (SessionsFixture, ThreadCatalog) {
    let fixture = SessionsFixture::new();
    let catalog = fixture.catalog();
    (fixture, catalog)
}

/// 会话文件路径：文件名规则仍只在 `session::session_file_name` 一处维护。
fn session_path(fixture: &SessionsFixture, thread_id: &str) -> std::path::PathBuf {
    fixture.dir.join(session_file_name(thread_id))
}

/// 读取不完整尾行之前的完整记录。
#[test]
fn a_torn_tail_keeps_complete_records_readable() {
    let (fixture, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).expect("create");
    let thread_id = thread.thread_id;
    // 记录完整文件的摘要，供尾行写入后对照。
    let known = catalog
        .read_snapshot(&thread_id)
        .map(|snapshot| snapshot.summary.clone())
        .expect("first read succeeds");
    // 末行只写了一半（JSON 未闭合、没有结尾换行）：与 append 中途被扫描到的
    // 形状一致，只读扫描忽略这一行。
    let path = session_path(&fixture, &thread_id);
    let mut bytes = std::fs::read(&path).expect("session file");
    bytes.extend_from_slice(br#"{"id":"half-written","timestamp":"#);
    std::fs::write(&path, bytes).expect("torn tail");
    assert!(catalog.read_snapshot(&thread_id).is_ok(), "complete records remain readable");

    let listed = catalog.list_threads().expect("listing keeps the known thread");
    let entry = listed
        .iter()
        .find(|entry| entry.thread_id == thread_id)
        .expect("a read failure is not a deletion");
    assert_eq!(entry.created_at, known.created_at);
    assert_eq!(entry.turn_count, known.turn_count);

    assert_eq!(fixture.catalog().list_threads().unwrap().len(), 1);

    // 真正离开目录的会话仍然不再出现：归档把文件移出顶层。
    catalog.archive(&thread_id).expect("archive");
    assert!(
        !catalog.list_threads().expect("list").iter().any(|entry| entry.thread_id == thread_id),
        "an archived thread leaves the active listing"
    );
}

mod archive_and_projection;
