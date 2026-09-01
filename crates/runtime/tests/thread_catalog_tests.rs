#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! T039 [US3]：Thread 目录的恢复、分页、归档与重命名。
//!
//! 全部目录事实从 ledger 派生：列表/摘要/分页只读投影，重命名与归档经
//! 写者锁；活动写者占用时归档拒绝；未知锚点与零 limit 显式报错。

use std::sync::Arc;

use crate::Conversation;
use crate::ThreadCatalog;
use crate::objects::Thread;
use crate::runner::TurnRunner;
use crate::store::ResumeError;
use crate::test_support::{provider_snapshot, temp_sessions, test_model_configuration};
use singularity_agent::session::{
    LedgerRecord, OperationIntent, OperationKind, SessionAccess, SessionManager,
};
use singularity_model::Provider;
use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};
use singularity_protocol::TurnStatus;

fn catalog_fixture() -> (tempfile::TempDir, Arc<TurnRunner>, ThreadCatalog) {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let runner = Arc::new(TurnRunner::new(sessions, provider_snapshot()));
    let catalog = ThreadCatalog::new(&runner);
    (home, runner, catalog)
}

fn cwd() -> String {
    std::env::current_dir()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

/// 以固定脚本 provider 在同一 sessions 目录上跑 count 个成功 turn。
fn run_turns(runner_source: &Arc<TurnRunner>, thread: &Thread, count: usize) {
    let attempts = (0..count).map(|index| {
        ScriptedAttempt::success_with_usage(
            format!("answer {index}"),
            singularity_model::ModelUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                cached_input_tokens: 0,
                reasoning_tokens: 0,
                usage_present: true,
            },
        )
    });
    let provider = Arc::new(ScriptedProvider::new(attempts));
    let runner = Arc::new(
        TurnRunner::new(
            runner_source.sessions_dir().to_path_buf(),
            provider_snapshot(),
        )
        .with_provider_override(provider as Arc<dyn Provider + Send + Sync>),
    );
    let conversation = Conversation::new(runner, thread.clone());
    let mut sink = |_event| {};
    for index in 0..count {
        let outcome = conversation
            .run_turn(&format!("question {index}"), &mut sink)
            .expect("turn completes");
        assert_eq!(outcome.turn_status, TurnStatus::Completed);
    }
}

#[test]
fn listing_rename_and_summary_project_ledger_facts() {
    let (_home, runner, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).expect("create");
    let thread_id = thread.thread_id.clone();

    assert!(
        catalog
            .list_threads()
            .expect("list")
            .iter()
            .any(|entry| entry.thread_id == thread_id),
        "a fresh thread appears in the listing"
    );
    assert!(
        catalog.rename(&thread_id, "   ").is_err(),
        "an empty name is rejected"
    );
    catalog
        .rename(&thread_id, "release checklist")
        .expect("rename");
    let summary = catalog.read_thread_summary(&thread_id).expect("summary");
    assert_eq!(summary.title.as_deref(), Some("release checklist"));

    run_turns(&runner, &thread, 2);
    let summary = catalog
        .read_thread_summary(&thread_id)
        .expect("summary after turns");
    assert_eq!(summary.turn_count, 2, "run operations count as turns");
    assert_eq!(summary.status, Some(TurnStatus::Completed));
    assert_eq!(
        summary.total_tokens, 30,
        "observed usage aggregates from the ledger"
    );
    assert_eq!(
        summary.title.as_deref(),
        Some("release checklist"),
        "the explicit name wins over the first-message fallback"
    );
}

#[test]
fn paged_read_pages_by_turns_and_rejects_bad_requests() {
    let (_home, runner, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).expect("create");
    let thread_id = thread.thread_id.clone();
    run_turns(&runner, &thread, 3);

    // 单向往回分页：默认返回最新 limit 轮（旧→新）。
    let page = catalog
        .paged_read(&thread_id, 2, None)
        .expect("latest page");
    assert_eq!(page.turns.len(), 2, "the page holds the newest two turns");
    assert_eq!(
        page.summary.turn_count, 3,
        "the summary carries the whole-thread fact: more turns exist"
    );
    let anchor = page.turns[0].items[0].id().to_string();

    let older = catalog
        .paged_read(&thread_id, 2, Some(&anchor))
        .expect("older page");
    assert_eq!(older.turns.len(), 1, "the remaining turn arrives");
    assert_ne!(
        older.turns[0].items[0].id(),
        anchor,
        "the anchor's own turn is excluded (before semantics)"
    );

    assert!(matches!(
        catalog.paged_read(&thread_id, 2, Some("missing-anchor")),
        Err(ResumeError::AnchorNotFound(_))
    ));
    let empty = catalog
        .paged_read(&thread_id, 0, None)
        .expect("limit 0 is the degenerate empty window");
    assert!(
        empty.turns.is_empty(),
        "a zero-size window returns no turns, never a full page"
    );
    assert!(matches!(
        catalog.paged_read("01914f6b-0000-7000-8000-00000000dead", 10, None),
        Err(ResumeError::NotFound(_))
    ));
}

#[test]
fn resume_projects_the_thread_and_rejects_unknown_ids() {
    let (_home, runner, catalog) = catalog_fixture();
    let thread = catalog
        .create_thread(&cwd(), Some("openai_compatible/base-model".to_string()))
        .expect("create");
    let thread_id = thread.thread_id.clone();
    run_turns(&runner, &thread, 1);

    let resumed = catalog.resume_thread(&thread_id).expect("resume");
    assert_eq!(resumed.thread_id, thread_id);
    assert_eq!(
        catalog
            .read_thread_summary(&thread_id)
            .expect("summary projection")
            .status,
        Some(TurnStatus::Completed)
    );
    // 设置由 turn 边界的 Thread 投影落盘，resume 从同一 ledger 事实投影回来。
    assert_eq!(
        resumed.model.as_deref(),
        Some("openai_compatible/base-model"),
        "the resumed thread projects the settings recorded at the turn boundary"
    );

    assert!(matches!(
        catalog.resume_thread("01914f6b-0000-7000-8000-00000000dead"),
        Err(ResumeError::NotFound(_))
    ));
}

#[test]
fn read_only_status_distinguishes_a_local_writer_from_a_stale_open_run() {
    let (_home, runner, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).expect("create");
    let thread_id = thread.thread_id;
    let path = runner.sessions_dir().join(format!("{thread_id}.jsonl"));
    let mut writer = SessionManager::open_existing_with_access(
        &path,
        runner.coordinator(),
        &thread_id,
        SessionAccess::Append,
    )
    .expect("writer open");
    writer
        .append_record(LedgerRecord::OperationStarted {
            operation_id: "op-live".to_string(),
            kind: OperationKind::Run,
            turn_id: Some("turn-live".to_string()),
            intent: OperationIntent::Run {
                model: test_model_configuration(),
                input: "question".to_string(),
            },
        })
        .expect("operation started");

    assert_eq!(
        catalog.read_thread_summary(&thread_id).unwrap().status,
        Some(TurnStatus::Running)
    );
    assert_eq!(
        catalog.paged_read(&thread_id, 1, None).unwrap().turns[0].status,
        Some(TurnStatus::Running)
    );

    drop(writer);
    assert_eq!(
        catalog.read_thread_summary(&thread_id).unwrap().status,
        Some(TurnStatus::Interrupted)
    );
    assert_eq!(
        catalog.paged_read(&thread_id, 1, None).unwrap().turns[0].status,
        Some(TurnStatus::Interrupted)
    );
}

#[test]
fn archive_hides_the_thread_and_respects_the_active_writer() {
    let (_home, runner, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).expect("create");
    let thread_id = thread.thread_id;
    let sessions = runner.sessions_dir().to_path_buf();

    // 活动写者占用：归档拒绝，文件仍在。
    let writer = SessionManager::open_existing_with_access(
        &sessions.join(format!("{thread_id}.jsonl")),
        runner.coordinator(),
        &thread_id,
        SessionAccess::Append,
    )
    .expect("writer open");
    assert!(matches!(
        catalog.archive(&thread_id),
        Err(ResumeError::WriterActive)
    ));
    drop(writer);

    catalog.archive(&thread_id).expect("archive");
    assert!(
        !catalog
            .list_threads()
            .expect("list")
            .iter()
            .any(|entry| entry.thread_id == thread_id),
        "an archived thread leaves the active listing"
    );
    assert!(matches!(
        catalog.read_thread_summary(&thread_id),
        Err(ResumeError::NotFound(_))
    ));
    assert!(matches!(
        catalog.archive(&thread_id),
        Err(ResumeError::NotFound(_))
    ));
}
