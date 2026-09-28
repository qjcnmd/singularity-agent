#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! Thread 目录故障与账本投影测试。
//!
//! 列表、摘要与分页从同一会话账本派生；故障注入覆盖损坏记录、
//! 未闭合操作与活动写者占用时的目录行为。

use std::sync::Arc;

use crate::Conversation;
use crate::ThreadCatalog;
use crate::test_support::{SessionsFixture, cwd};
use crate::thread_catalog::{ARCHIVED_SESSIONS_DIR_NAME, CatalogError};
use singularity_agent::session::{LedgerRecord, SessionAccess, SessionManager, session_file_name};
use singularity_model::Provider;
use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};
use singularity_protocol::Thread;
use singularity_protocol::TurnStatus;

fn catalog_fixture() -> (SessionsFixture, ThreadCatalog) {
    let fixture = SessionsFixture::new();
    let catalog = fixture.catalog();
    (fixture, catalog)
}

/// 以固定脚本 provider 在同一 sessions 目录上跑 count 个成功 turn。
fn run_turns(fixture: &SessionsFixture, thread: &Thread, count: usize) {
    let attempts = (0..count).map(|index| {
        ScriptedAttempt::success_with_usage(
            format!("answer {index}"),
            singularity_model::ModelUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                cached_input_tokens: Some(0),
                reasoning_tokens: 0,
                usage_present: true,
            },
        )
    });
    let provider = Arc::new(ScriptedProvider::new(attempts));
    let runner = fixture.runner(Some(provider as Arc<dyn Provider + Send + Sync>));
    let conversation = Conversation::new(runner, thread.clone());
    let mut sink = |_event| {};
    for index in 0..count {
        let outcome = crate::test_support::run_async(
            conversation.run_turn(&format!("question {index}"), &mut sink),
        )
        .expect("turn completes");
        assert_eq!(outcome.turn_status, TurnStatus::Completed);
    }
}

/// 请求的开始时间是开始观测的事实：终态观测只更新观测载荷，不覆盖它；
#[test]
fn request_start_time_survives_the_terminal_merge() {
    use singularity_agent::session::SessionEntry;
    use singularity_protocol::{HistoryItem, ProviderAttemptStatus};

    let (fixture, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).unwrap();
    run_turns(&fixture, &thread, 1);
    let path = session_path(&fixture, &thread.thread_id);

    // 重写两条 model_request 记录的时间戳，让开始与终态在时间上可区分。
    let original = std::fs::read_to_string(&path).unwrap();
    let mut lines = original.lines();
    let mut rewritten = format!("{}\n", lines.next().unwrap());
    let mut observations = 0;
    for line in lines {
        let entry: SessionEntry = serde_json::from_str(line).unwrap();
        let rewritten_line = match entry {
            SessionEntry::Record {
                id,
                record:
                    LedgerRecord::ModelRequest {
                        observation,
                        context,
                    },
                ..
            } => {
                observations += 1;
                let timestamp = match observation.status {
                    ProviderAttemptStatus::Started => "2026-01-01T00:00:01.000Z",
                    _ => "2026-01-01T00:00:09.000Z",
                };
                serde_json::to_string(&SessionEntry::Record {
                    id,
                    timestamp: timestamp.to_string(),
                    record: LedgerRecord::ModelRequest {
                        observation,
                        context,
                    },
                })
                .unwrap()
            }
            other => serde_json::to_string(&other).unwrap(),
        };
        rewritten.push_str(&rewritten_line);
        rewritten.push('\n');
    }
    assert_eq!(
        observations, 2,
        "one request records a started and a terminal observation"
    );
    std::fs::write(&path, &rewritten).unwrap();

    let requests = |catalog: &ThreadCatalog| {
        catalog
            .read_snapshot(&thread.thread_id)
            .unwrap()
            .page(10, None)
            .unwrap()
            .turns
            .iter()
            .flat_map(|turn| &turn.items)
            .filter_map(|item| match item {
                HistoryItem::Request {
                    started_at,
                    observation,
                } => Some((started_at.clone(), observation.status)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        requests(&catalog),
        vec![(
            Some("2026-01-01T00:00:01.000Z".to_string()),
            ProviderAttemptStatus::Ok
        )],
        "the two observations merge into one request that keeps its start time"
    );
}

/// 会话文件路径：文件名规则仍只在 `session::session_file_name` 一处维护。
fn session_path(fixture: &SessionsFixture, thread_id: &str) -> std::path::PathBuf {
    fixture.dir.join(session_file_name(thread_id))
}

/// 以 Append 意图打开会话写者；未闭合 operation 不被修复重写。
fn open_writer(fixture: &SessionsFixture, thread_id: &str) -> SessionManager {
    SessionManager::open_existing_with_access(
        &session_path(fixture, thread_id),
        &fixture.coordinator,
        thread_id,
        SessionAccess::Append,
    )
    .expect("writer open")
}

/// 完整记录投影不依赖旧缓存或写者是否仍活动。
#[test]
fn a_torn_tail_keeps_complete_records_readable() {
    let (fixture, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).expect("create");
    let thread_id = thread.thread_id;
    // 先成功读一次：这份已提交摘要代表该会话确实存在。
    let known = catalog
        .read_snapshot(&thread_id)
        .map(|snapshot| snapshot.summary.clone())
        .expect("first read succeeds");
    let writer = open_writer(&fixture, &thread_id);

    // 末行只写了一半（JSON 未闭合、没有结尾换行）：与 append 中途被扫描到的
    // 形状一致，只读扫描忽略这一行。
    let path = session_path(&fixture, &thread_id);
    let mut bytes = std::fs::read(&path).expect("session file");
    bytes.extend_from_slice(br#"{"id":"half-written","timestamp":"#);
    std::fs::write(&path, bytes).expect("torn tail");
    assert!(
        catalog.read_snapshot(&thread_id).is_ok(),
        "complete records remain readable"
    );

    let listed = catalog
        .list_threads()
        .expect("listing keeps the known thread");
    let entry = listed
        .iter()
        .find(|entry| entry.thread_id == thread_id)
        .expect("a read failure is not a deletion");
    assert_eq!(entry.created_at, known.created_at);
    assert_eq!(entry.turn_count, known.turn_count);

    drop(writer);
    assert_eq!(fixture.catalog().list_threads().unwrap().len(), 1);

    // 真正离开目录的会话仍然不再出现：归档把文件移出顶层。
    catalog.archive(&thread_id).expect("archive");
    assert!(
        !catalog
            .list_threads()
            .expect("list")
            .iter()
            .any(|entry| entry.thread_id == thread_id),
        "an archived thread leaves the active listing"
    );
}

#[test]
fn cached_summary_does_not_hide_a_persistent_read_error() {
    let (fixture, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).expect("create");
    let path = session_path(&fixture, &thread.thread_id);
    catalog
        .read_snapshot(&thread.thread_id)
        .map(|snapshot| snapshot.summary.clone())
        .expect("cache summary");
    let mut bytes = std::fs::read(&path).expect("session file");
    bytes.extend_from_slice(b"not json\n");
    std::fs::write(&path, bytes).expect("corrupt session");

    assert!(matches!(
        catalog.list_threads(),
        Err(CatalogError::Session {
            source: singularity_agent::session::SessionError::MalformedLine { .. },
            ..
        })
    ));
}

mod archive_and_projection;
