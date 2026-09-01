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

/// Thread 的工作目录是一个事实：它必须在创建、恢复、列表与会话头四个表面上呈现
/// 同一个字面值，与调用方的拼法无关，不带 Windows verbatim 前缀，并且被系统
/// 提示词逐字承载。该字符串会原样交给模型，模型会把它抄进命令，`\\?\C:\…` 与
/// `//?/C:/…` 两种形状在 shell 里都不可用。
fn assert_thread_cwd_shape(
    runner: &TurnRunner,
    catalog: &ThreadCatalog,
    spelled: &std::path::Path,
) -> Thread {
    let thread = catalog
        .create_thread(spelled.to_str().expect("utf-8 workspace"), None)
        .expect("create");
    let resumed = catalog
        .resume_thread(&thread.thread_id)
        .expect("resume thread");
    let listed = catalog
        .list_threads()
        .expect("list")
        .into_iter()
        .find(|entry| entry.thread_id == thread.thread_id)
        .expect("listed thread");

    assert_eq!(thread.cwd, resumed.cwd, "resume rewrites the cwd");
    assert_eq!(thread.cwd, listed.cwd, "listing rewrites the cwd");

    let header = std::fs::read_to_string(
        runner
            .sessions_dir()
            .join(format!("{}.jsonl", thread.thread_id)),
    )
    .expect("session file");
    let stored = header
        .split_once("\"cwd\":\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(value, _)| value.to_owned())
        .expect("header cwd");
    assert_eq!(
        thread.cwd, stored,
        "the durable cwd differs from the projected one"
    );

    assert!(
        std::path::Path::new(&thread.cwd).is_absolute(),
        "thread cwd is not absolute: {}",
        thread.cwd
    );
    assert!(
        !thread.cwd.contains(r"\\?\") && !thread.cwd.contains("//?/"),
        "thread cwd carries a Windows verbatim prefix: {}",
        thread.cwd
    );

    let prompt = singularity_agent::prompts::PromptAssembly::assemble(
        &thread.cwd,
        &singularity_agent::tools::ToolRegistrySnapshot::new(),
        None,
    );
    assert!(
        prompt
            .system_prompt
            .ends_with(&format!("\n\nCurrent working directory: {}", thread.cwd)),
        "the prompt does not carry the thread cwd verbatim"
    );
    thread
}

#[test]
fn thread_cwd_projects_one_usable_shape_across_every_surface() {
    let (_home, runner, catalog) = catalog_fixture();
    let workspace = std::env::current_dir().expect("workspace");
    // 冗余组件的拼法：投影结果与调用方怎么写无关。
    assert_thread_cwd_shape(&runner, &catalog, &workspace.join(".").join("."));
    // Windows 上 canonicalize 返回的 verbatim 形状（修复前新建的会话就是它）。
    let canonical = std::fs::canonicalize(&workspace).expect("canonical workspace");
    let seeded = assert_thread_cwd_shape(&runner, &catalog, &canonical);

    // 存量形状：修复前落盘的头里带 `//?/` 前缀。header 只在创建时写出、之后不
    // 重写，所以归一必须发生在解析侧，否则旧会话永久携带坏形状。该形状只可能
    // 在 Windows 上产生，其余平台跳过这一段。
    if cfg!(windows) {
        let file = runner
            .sessions_dir()
            .join(format!("{}.jsonl", seeded.thread_id));
        let text = std::fs::read_to_string(&file).expect("session file");
        let patched = text.replace(
            &format!("\"cwd\":\"{}\"", seeded.cwd),
            &format!("\"cwd\":\"//?/{}\"", seeded.cwd),
        );
        assert_ne!(
            patched, text,
            "the fixture does not store the projected cwd"
        );
        std::fs::write(&file, patched).expect("write legacy-shaped header");
        let resumed = catalog
            .resume_thread(&seeded.thread_id)
            .expect("resume legacy-shaped session");
        assert_eq!(
            resumed.cwd, seeded.cwd,
            "a stored verbatim cwd reaches the Thread projection unchanged"
        );
        let listed = catalog
            .list_threads()
            .expect("list")
            .into_iter()
            .find(|entry| entry.thread_id == seeded.thread_id)
            .expect("listed thread");
        assert_eq!(
            listed.cwd, seeded.cwd,
            "a stored verbatim cwd reaches the listing"
        );
    }
}
