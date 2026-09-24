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
use singularity_agent::session::{
    ExpectedSession, LedgerRecord, OperationKind, SessionAccess, SessionManager, session_file_name,
};
use singularity_model::Provider;
use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};
use singularity_protocol::Thread;
use singularity_protocol::TurnStatus;

fn catalog_fixture() -> (SessionsFixture, ThreadCatalog) {
    let fixture = SessionsFixture::new();
    let catalog = fixture.catalog();
    (fixture, catalog)
}

#[test]
fn broken_request_details_do_not_hide_history_or_prevent_continuation() {
    use singularity_agent::session::SessionEntry;
    use singularity_protocol::HistoryItem;
    let (fixture, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).unwrap();
    run_turns(&fixture, &thread, 1);
    let path = session_path(&fixture, &thread.thread_id);
    let original = std::fs::read_to_string(&path).unwrap();
    let mut lines = original.lines();
    let mut changed = format!("{}\n", lines.next().unwrap());
    let mut removed = false;
    for line in lines {
        let entry: SessionEntry = serde_json::from_str(line).unwrap();
        if !removed
            && matches!(
                entry,
                SessionEntry::Record {
                    record: LedgerRecord::RequestDefinitions { .. },
                    ..
                }
            )
        {
            removed = true;
            continue;
        }
        changed.push_str(line);
        changed.push('\n');
    }
    assert!(removed);
    std::fs::write(&path, changed).unwrap();
    let page = catalog
        .read_snapshot(&thread.thread_id)
        .unwrap()
        .page(100, None)
        .unwrap();
    assert!(
        page.turns
            .iter()
            .flat_map(|t| &t.items)
            .any(|item| matches!(item, HistoryItem::Message { text, .. } if text == "answer 0"))
    );
    assert!(page.turns.iter().flat_map(|t| &t.items).any(|item| matches!(item, HistoryItem::Request { observation, .. } if observation.request_head.is_none() && observation.request_error.is_some())));
    let resumed = catalog
        .resume_thread(&thread.thread_id, &thread.cwd)
        .unwrap();
    run_turns(&fixture, &resumed, 1);
    assert_eq!(
        catalog
            .read_thread_summary(&thread.thread_id)
            .unwrap()
            .turn_count,
        2
    );
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
                cached_input_tokens: 0,
                cached_input_tokens_present: true,
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

/// 同一份快照同时提供列表摘要与整页历史，两个表面必须解读出相同的回合事实。
fn read_facts(
    catalog: &ThreadCatalog,
    thread_id: &str,
) -> (
    singularity_protocol::ThreadSummary,
    Vec<singularity_protocol::ThreadTurn>,
) {
    let summary = catalog
        .read_thread_summary(thread_id)
        .expect("summary projection");
    let page = catalog
        .read_snapshot(thread_id)
        .expect("snapshot")
        .page(100, None)
        .expect("page");
    assert_eq!(page.summary, summary, "one snapshot serves both surfaces");
    assert_eq!(
        summary.turn_count,
        page.turns
            .iter()
            .filter(|turn| turn.status.is_some())
            .count(),
        "the listing and the page count runs the same way"
    );
    (summary, page.turns)
}

fn last_turn(turns: &[singularity_protocol::ThreadTurn]) -> &singularity_protocol::ThreadTurn {
    turns.last().expect("at least one turn")
}

/// 请求的开始时间是开始观测的事实：终态观测只更新观测载荷，不覆盖它；
/// 只有终态观测（缺开始记录）时保持未知，不用结束记录时间冒充开始时间。
#[test]
fn request_start_time_survives_the_terminal_merge_and_stays_unknown_without_a_start() {
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
    // 缺开始记录的旧日志保持未知：不把结束记录时间当作开始时间。
    let without_start = rewritten
        .lines()
        .filter(|line| {
            !serde_json::from_str::<SessionEntry>(line).is_ok_and(|entry| {
                matches!(
                    entry,
                    SessionEntry::Record {
                        record: LedgerRecord::ModelRequest {
                            observation: singularity_protocol::RequestObservation {
                                status: ProviderAttemptStatus::Started,
                                ..
                            },
                            ..
                        },
                        ..
                    }
                )
            })
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, format!("{without_start}\n")).unwrap();
    let unknown = requests(&catalog);
    assert_eq!(unknown.len(), 1);
    assert_eq!(
        unknown[0].0, None,
        "a request without a started record has no known start time"
    );
    assert_eq!(unknown[0].1, ProviderAttemptStatus::Ok);
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
        ExpectedSession {
            id: thread_id,
            cwd: None,
        },
        SessionAccess::Append,
    )
    .expect("writer open")
}

fn append(writer: &mut SessionManager, record: LedgerRecord) {
    writer.append_record(record).expect("append record");
}

fn run_operation(operation_id: &str, turn_id: &str) -> LedgerRecord {
    LedgerRecord::OperationStarted {
        operation_id: operation_id.to_string(),
        kind: OperationKind::Run,
        turn_id: Some(turn_id.to_string()),
    }
}

#[test]
fn read_only_status_distinguishes_a_local_writer_from_a_stale_open_run() {
    let (fixture, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).expect("create");
    let thread_id = thread.thread_id;
    let mut writer = open_writer(&fixture, &thread_id);
    append(&mut writer, run_operation("op-live", "turn-live"));

    // 本进程仍持有写者：未闭合 run 在摘要与分页上都是 running，也不算手动停止。
    let (summary, turns) = read_facts(&catalog, &thread_id);
    assert_eq!(summary.turn_count, 1);
    assert_eq!(summary.status, Some(TurnStatus::Running));
    assert_eq!(
        catalog.list_threads().unwrap()[0].status,
        Some(TurnStatus::Running)
    );
    assert!(!summary.manually_stopped);
    assert_eq!(last_turn(&turns).status, Some(TurnStatus::Running));
    assert_eq!(last_turn(&turns).turn_id.as_deref(), Some("turn-live"));
    drop(writer);

    // 写者退出后同一份日志是 interrupted：被遗弃的 run 同样不是手动停止。
    let (summary, turns) = read_facts(&catalog, &thread_id);
    assert_eq!(summary.status, Some(TurnStatus::Interrupted));
    assert_eq!(
        catalog.list_threads().unwrap()[0].status,
        Some(TurnStatus::Interrupted)
    );
    assert!(!summary.manually_stopped);
    assert_eq!(last_turn(&turns).status, Some(TurnStatus::Interrupted));
}

/// 目录列表只在本进程写者仍活动、尾部暂未写完时沿用已确认的旧摘要。
#[test]
fn a_live_torn_tail_uses_cached_summary_until_the_writer_closes() {
    let (fixture, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).expect("create");
    let thread_id = thread.thread_id;
    // 先成功读一次：这份已提交摘要代表该会话确实存在。
    let known = catalog
        .read_thread_summary(&thread_id)
        .expect("first read succeeds");
    let writer = open_writer(&fixture, &thread_id);

    // 末行只写了一半（JSON 未闭合、没有结尾换行）：与 append 中途被扫描到的
    // 形状一致，只读扫描必然拒绝。
    let path = session_path(&fixture, &thread_id);
    let mut bytes = std::fs::read(&path).expect("session file");
    bytes.extend_from_slice(br#"{"id":"half-written","timestamp":"#);
    std::fs::write(&path, bytes).expect("torn tail");
    assert!(
        catalog.read_thread_summary(&thread_id).is_err(),
        "a torn tail cannot be read back"
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
    assert!(matches!(
        catalog.list_threads(),
        Err(CatalogError::Session {
            source: singularity_agent::session::SessionError::TailRepairRequired,
            ..
        })
    ));

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
        .read_thread_summary(&thread.thread_id)
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
