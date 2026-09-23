#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! Thread 目录管理测试：验证会话列表恢复、分页查询、归档与重命名。
//!
//! 全部目录事实均从会话 ledger 派生：列表、摘要与分页采用只读投影，
//! 重命名与归档通过写者锁进行并发保护；活动写者占用时拒绝归档；非法游标锚点显式报错，零 limit 返回空页。

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
        let outcome = conversation
            .run_turn(&format!("question {index}"), &mut sink)
            .expect("turn completes");
        assert_eq!(outcome.turn_status, TurnStatus::Completed);
    }
}

#[test]
fn listing_rename_and_summary_project_ledger_facts() {
    let (fixture, catalog) = catalog_fixture();
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

    run_turns(&fixture, &thread, 2);
    let summary = catalog
        .read_thread_summary(&thread_id)
        .expect("summary after turns");
    assert_eq!(summary.turn_count, 2, "run operations count as turns");
    assert_eq!(summary.status, Some(TurnStatus::Completed));
    assert_eq!(
        summary.title.as_deref(),
        Some("release checklist"),
        "the explicit name wins over the first-message fallback"
    );
}

/// 回合事实只有一个来源：目录摘要与历史分页从同一索引得到轮数、终态与手动
/// 停止。前导组、已完成回合、其后的独立压缩与用户显式停止在两个表面上必须
/// 给出一致解读；未闭合 run 与遗弃 run 的读写者区分见
/// `read_only_status_distinguishes_a_local_writer_from_a_stale_open_run`。
#[test]
fn summary_and_paging_share_one_run_index() {
    let (fixture, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).expect("create");
    let thread_id = thread.thread_id;

    // 创建后没有任何条目：既没有回合，也不产生空的投影组。
    let mut writer = open_writer(&fixture, &thread_id);
    let (summary, turns) = read_facts(&catalog, &thread_id);
    assert_eq!(summary.turn_count, 0);
    assert_eq!(summary.status, None);
    assert!(turns.is_empty());

    // 首个 run 之前落盘的条目构成前导组：成组展示，但不算回合也没有终态。
    writer
        .append_metadata(singularity_agent::session::SessionMetadata::ThreadName {
            name: "leading".to_string(),
        })
        .expect("append metadata");
    let (summary, turns) = read_facts(&catalog, &thread_id);
    assert_eq!(summary.turn_count, 0);
    assert_eq!(summary.status, None);
    assert_eq!(summary.title.as_deref(), Some("leading"));
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].turn_id, None);
    assert_eq!(turns[0].status, None);

    // 正常完成一轮。
    append(&mut writer, run_operation("op-1", "turn-1"));
    append(
        &mut writer,
        finished_operation("op-1", "turn-1", TurnStatus::Completed, false),
    );
    let (summary, turns) = read_facts(&catalog, &thread_id);
    assert_eq!(summary.turn_count, 1);
    assert_eq!(summary.status, Some(TurnStatus::Completed));
    assert!(!summary.manually_stopped);
    assert_eq!(last_turn(&turns).status, Some(TurnStatus::Completed));
    assert_eq!(last_turn(&turns).turn_id.as_deref(), Some("turn-1"));
    drop(writer);

    // 独立压缩 operation 既不是回合，也不覆盖普通回合的终态。
    let mut writer = open_writer(&fixture, &thread_id);
    append(
        &mut writer,
        LedgerRecord::OperationStarted {
            operation_id: "op-compact".to_string(),
            kind: OperationKind::Compaction,
            turn_id: None,
        },
    );
    append(
        &mut writer,
        finished_operation("op-compact", "", TurnStatus::Failed, false),
    );
    let (summary, turns) = read_facts(&catalog, &thread_id);
    assert_eq!(summary.turn_count, 1);
    assert_eq!(summary.status, Some(TurnStatus::Completed));
    assert_eq!(last_turn(&turns).status, Some(TurnStatus::Completed));

    // 用户停止：轮数、终态与手动停止标记同时出现在两个表面上。
    append(&mut writer, run_operation("op-2", "turn-2"));
    append(
        &mut writer,
        finished_operation("op-2", "turn-2", TurnStatus::Interrupted, true),
    );
    let (summary, turns) = read_facts(&catalog, &thread_id);
    assert_eq!(summary.turn_count, 2);
    assert_eq!(summary.status, Some(TurnStatus::Interrupted));
    assert!(summary.manually_stopped);
    assert_eq!(last_turn(&turns).status, Some(TurnStatus::Interrupted));
    assert_eq!(last_turn(&turns).turn_id.as_deref(), Some("turn-2"));
    drop(writer);
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
    // 请求条目的公开身份就是其观测的 request id，不再是第二个可独立构造的来源。
    let snapshot = catalog.read_snapshot(&thread.thread_id).unwrap();
    let request_item = snapshot
        .page(10, None)
        .unwrap()
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .find(|item| matches!(item, HistoryItem::Request { .. }))
        .cloned()
        .unwrap();
    let HistoryItem::Request { observation, .. } = &request_item else {
        unreachable!()
    };
    assert_eq!(request_item.id(), observation.request_id);

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

fn finished_operation(
    operation_id: &str,
    turn_id: &str,
    outcome: TurnStatus,
    user_stopped: bool,
) -> LedgerRecord {
    LedgerRecord::OperationFinished {
        operation_id: operation_id.to_string(),
        turn_id: (!turn_id.is_empty()).then(|| turn_id.to_string()),
        outcome,
        usage: None,
        error: None,
        truncated: false,
        user_stopped,
    }
}

#[test]
fn history_snapshot_pages_by_turns_and_rejects_bad_requests() {
    let (fixture, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).expect("create");
    let thread_id = thread.thread_id.clone();
    run_turns(&fixture, &thread, 3);

    // 单向往回分页：默认返回最新 limit 轮（旧→新）。
    let history = catalog.read_snapshot(&thread_id).expect("snapshot");
    let page = history.page(2, None).expect("latest page");
    assert_eq!(page.turns.len(), 2, "the page holds the newest two turns");
    assert_eq!(
        page.summary.turn_count, 3,
        "the summary carries the whole-thread fact: more turns exist"
    );
    let anchor = page.next_cursor.expect("older page cursor");

    let older = history.page(2, Some(&anchor)).expect("older page");
    assert_eq!(older.turns.len(), 1, "the remaining turn arrives");
    assert_ne!(
        older.turns[0].items[0].id(),
        page.turns[0].items[0].id(),
        "the anchor's own turn is excluded (before semantics)"
    );

    assert!(matches!(
        history.page(2, Some("missing-anchor")),
        Err(CatalogError::AnchorNotFound(_))
    ));
    let empty = history
        .page(0, None)
        .expect("limit 0 is the degenerate empty window");
    assert!(
        empty.turns.is_empty(),
        "a zero-size window returns no turns, never a full page"
    );
    assert!(matches!(
        catalog.read_snapshot("01914f6b-0000-7000-8000-00000000dead"),
        Err(CatalogError::NotFound(_))
    ));
}

#[test]
fn resume_projects_the_thread_and_rejects_unknown_ids() {
    let (fixture, catalog) = catalog_fixture();
    let thread = catalog
        .create_thread(&cwd(), Some("openai_compatible/base-model".to_string()))
        .expect("create");
    let thread_id = thread.thread_id.clone();
    run_turns(&fixture, &thread, 1);

    let resumed = catalog
        .resume_thread(&thread_id, &thread.cwd)
        .expect("resume");
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
        catalog.resume_thread("01914f6b-0000-7000-8000-00000000dead", &cwd()),
        Err(CatalogError::NotFound(_))
    ));
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

/// 目录列表区分「确认文件不在」与「本次读不出」：活动日志的尾部尚未稳定时
/// 只读扫描会拒绝它，但该会话仍是目录成员——已确认有效的旧摘要就是依据。
#[test]
fn a_failed_directory_read_is_not_reported_as_a_missing_thread() {
    let (fixture, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).expect("create");
    let thread_id = thread.thread_id;
    // 先成功读一次：这份已提交摘要代表该会话确实存在。
    let known = catalog
        .read_thread_summary(&thread_id)
        .expect("first read succeeds");

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

/// 没有任何可信旧摘要时，读失败必须让整次列表如实报错：返回一份缺项却看似
/// 完整的成功快照，等于把后端的不确定性交给读侧当作删除。
#[test]
fn a_directory_read_without_a_trustworthy_summary_reports_failure() {
    let (fixture, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).expect("create");
    let path = session_path(&fixture, &thread.thread_id);
    let mut bytes = std::fs::read(&path).expect("session file");
    bytes.extend_from_slice(br#"{"id":"half-written","timestamp":"#);
    std::fs::write(&path, bytes).expect("torn tail");

    // 本进程从未成功读过这份会话：没有可复用的已提交事实。
    assert!(matches!(
        catalog.list_threads(),
        Err(CatalogError::Session { .. })
    ));
}

#[test]
fn archive_hides_the_thread_and_respects_the_active_writer() {
    let (fixture, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).expect("create");
    let thread_id = thread.thread_id;
    let sessions = fixture.dir.clone();

    // 活动写者占用：归档拒绝，文件仍在。
    let writer = open_writer(&fixture, &thread_id);
    assert!(matches!(
        catalog.archive(&thread_id),
        Err(CatalogError::WriterActive)
    ));
    assert!(matches!(
        catalog.rename(&thread_id, "busy"),
        Err(CatalogError::WriterActive)
    ));
    drop(writer);

    catalog.archive(&thread_id).expect("archive");
    assert!(
        sessions
            .join(ARCHIVED_SESSIONS_DIR_NAME)
            .join(session_file_name(&thread_id))
            .exists(),
        "the archived session file is preserved"
    );
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
        Err(CatalogError::NotFound(_))
    ));
    assert!(matches!(
        catalog.archive(&thread_id),
        Err(CatalogError::NotFound(_))
    ));
    assert!(matches!(
        catalog.rename(&thread_id, "missing"),
        Err(CatalogError::NotFound(_))
    ));
}

/// Thread 的工作目录是一个事实：它必须在创建、恢复、列表与会话头四个表面上呈现
/// 同一个字面值，与调用方的拼法无关，不带 Windows verbatim 前缀，并且被系统
/// 提示词逐字承载。该字符串会原样交给模型，模型会把它抄进命令，\\?\C:\… 与
/// //?/C:/… 两种形状在 shell 里都不可用。
fn assert_thread_cwd_shape(
    fixture: &SessionsFixture,
    catalog: &ThreadCatalog,
    spelled: &std::path::Path,
) -> Thread {
    let thread = catalog
        .create_thread(spelled.to_str().expect("utf-8 workspace"), None)
        .expect("create");
    let resumed = catalog
        .resume_thread(&thread.thread_id, &thread.cwd)
        .expect("resume thread");
    let listed = catalog
        .list_threads()
        .expect("list")
        .into_iter()
        .find(|entry| entry.thread_id == thread.thread_id)
        .expect("listed thread");

    assert_eq!(thread.cwd, resumed.cwd, "resume rewrites the cwd");
    assert_eq!(thread.cwd, listed.cwd, "listing rewrites the cwd");

    let header =
        std::fs::read_to_string(session_path(fixture, &thread.thread_id)).expect("session file");
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

    let prompt = singularity_agent::prompts::assemble_developer_instructions(
        &thread.cwd,
        &singularity_agent::tools::ToolRegistrySnapshot::default(),
    );
    assert!(
        prompt.ends_with(&format!("\n\nCurrent working directory: {}", thread.cwd)),
        "the prompt does not carry the thread cwd verbatim"
    );
    thread
}

#[test]
fn thread_cwd_projects_one_usable_shape_across_every_surface() {
    let (fixture, catalog) = catalog_fixture();
    let workspace = std::env::current_dir().expect("workspace");
    // 冗余组件的拼法：投影结果与调用方怎么写无关。
    assert_thread_cwd_shape(&fixture, &catalog, &workspace.join(".").join("."));
    // Windows 上 canonicalize 返回带扩展前缀的规范路径。
    let canonical = std::fs::canonicalize(&workspace).expect("canonical workspace");
    let seeded = assert_thread_cwd_shape(&fixture, &catalog, &canonical);

    // 会话头可以包含 //?/ 前缀。header 只在创建时写出、之后不
    // 重写，因此在解析侧归一化路径。该形状只可能
    // 在 Windows 上产生，其余平台跳过这一段。
    if cfg!(windows) {
        let file = session_path(&fixture, &seeded.thread_id);
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
            .resume_thread(&seeded.thread_id, &seeded.cwd)
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

#[test]
fn missing_workspace_keeps_registry_and_history_readable_but_blocks_execution() {
    let (fixture, catalog) = catalog_fixture();
    let registry = crate::WorkspaceStore::open(fixture.home()).unwrap();
    let project = tempfile::tempdir().unwrap();
    let workspace = registry.add(project.path()).unwrap();
    let thread = catalog.create_thread(&workspace.root, None).unwrap();
    run_turns(&fixture, &thread, 1);
    drop(project);

    let error = catalog
        .create_thread(&workspace.root, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains(&workspace.root));
    assert!(error.contains("unavailable"));

    let registry = crate::WorkspaceStore::open(fixture.home()).unwrap();
    // 归属由会话持久化的规范 cwd 决定；registry 只登记项目本身。
    let listed = catalog.list_threads().unwrap();
    assert_eq!(
        listed
            .iter()
            .find(|entry| entry.thread_id == thread.thread_id)
            .unwrap()
            .cwd,
        workspace.root
    );
    let resumed = catalog
        .resume_thread(&thread.thread_id, &thread.cwd)
        .unwrap();
    let page = catalog
        .read_snapshot(&thread.thread_id)
        .unwrap()
        .page(100, None)
        .unwrap();
    assert_eq!(page.turns.len(), 1);
    assert!(fixture.runner(None).open_turn_writer(&resumed).is_err());
    let other = tempfile::tempdir().unwrap();
    registry
        .add(other.path())
        .expect("a missing root must not block other projects");
    registry.remove(&workspace.workspace_id).unwrap();
    assert_eq!(
        catalog
            .read_snapshot(&thread.thread_id)
            .unwrap()
            .page(100, None)
            .unwrap()
            .turns
            .len(),
        1
    );
}

/// 目录顺序契约：最近更新时间降序，同一时间按任务 ID 升序。
#[test]
fn listing_order_is_recency_then_thread_id() {
    let (fixture, catalog) = catalog_fixture();
    let workspace = cwd();
    let threads: Vec<_> = (0..4)
        .map(|_| catalog.create_thread(&workspace, None).expect("thread"))
        .collect();
    let ids: Vec<_> = {
        let mut ids: Vec<_> = threads
            .iter()
            .map(|thread| thread.thread_id.clone())
            .collect();
        ids.sort();
        ids
    };
    let listed_ids = |catalog: &ThreadCatalog| -> Vec<String> {
        catalog
            .list_threads()
            .expect("list")
            .into_iter()
            .filter(|thread| ids.contains(&thread.thread_id))
            .map(|thread| thread.thread_id)
            .collect()
    };
    // 会话由头部与一条设置 metadata 组成，两处时间都要钉住才能固定摘要的
    // 创建与更新时间。
    let pin = |thread_id: &str, stamp: &str| {
        let file = session_path(&fixture, thread_id);
        let mut patched = std::fs::read_to_string(&file).expect("session file");
        let key = "\"timestamp\":\"";
        let mut search = 0;
        while let Some(found) = patched[search..].find(key) {
            let value_start = search + found + key.len();
            let value_end = value_start + patched[value_start..].find('"').expect("timestamp end");
            patched.replace_range(value_start..value_end, stamp);
            search = value_start + stamp.len();
        }
        std::fs::write(&file, patched).expect("pin timestamps");
    };

    // 时间的先后与任务 ID 的升序相反：列表必须由最近更新决定。
    // 摘要缓存以（长度, mtime）判断文件版本，两轮钉时间都是整文件重写；第二轮
    // 与第一轮间隔极短，若长度不变就可能落在同一时间刻度上而沿用上一轮摘要。
    // 两轮分别用纳秒与毫秒精度，长度必然不同，缓存失效不再依赖时间戳分辨率。
    for (rank, thread_id) in ids.iter().enumerate() {
        pin(
            thread_id,
            &format!("2026-01-0{}T00:00:00.000000000Z", rank + 1),
        );
    }
    let newest_first: Vec<_> = ids.iter().rev().cloned().collect();
    assert_eq!(listed_ids(&catalog), newest_first, "recency decides order");

    // 创建时间相同时，唯一可用的顺序依据是任务 ID。
    for thread_id in &ids {
        pin(thread_id, "2026-01-01T00:00:00.000Z");
    }
    assert_eq!(
        listed_ids(&catalog),
        ids,
        "equal timestamps order by thread id"
    );
}

#[test]
fn request_headers_match_live_events_without_recording_full_context() {
    use singularity_protocol::{HistoryItem, ProviderAttemptStatus, TurnEvent};
    let (fixture, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).unwrap();
    let runner = fixture.runner(Some(Arc::new(ScriptedProvider::ok("answer"))));
    let conversation = Conversation::new(runner, thread.clone());
    let input = "distinct user history ".repeat(200);
    let mut observed = None;
    let mut sink = |event: TurnEvent| {
        let wire = serde_json::to_value(&event).unwrap()["params"].clone();
        if let TurnEvent::ProviderAttempt { observation, .. } = event
            && observation.status == ProviderAttemptStatus::Started
        {
            assert!(
                wire.get("request").is_none(),
                "the stream must not duplicate conversation history"
            );
            let head = observation.request_head.unwrap();
            assert!(
                !serde_json::to_string(&head)
                    .unwrap()
                    .contains("distinct user history")
            );
            observed = Some((observation.request_id, head));
        }
    };
    conversation.run_turn(&input, &mut sink).unwrap();
    let (request_id, details) = observed.unwrap();
    let snapshot = catalog.read_snapshot(&thread.thread_id).unwrap();
    let page = snapshot.page(100, None).unwrap();
    let requests: Vec<_> = page
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .filter_map(|item| {
            if let HistoryItem::Request { observation, .. } = item {
                Some(observation)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(requests.len(), 1, "start and finish project as one request");
    assert_eq!(requests[0].request_id, request_id);
    assert_eq!(requests[0].status, ProviderAttemptStatus::Ok);
    assert!(requests[0].request_head.is_some());
    assert_eq!(requests[0].request_head.as_deref(), Some(details.as_ref()));
}

/// 会话累计用量汇总整份账本：按 requestId 取末次观测（与逐请求展示同一规则），
/// 未报告 usage 的请求只把合计降为下界，不计入任何计数。
#[test]
fn summary_usage_sums_reported_requests_and_marks_the_rest_as_lower_bound() {
    use singularity_protocol::HistoryItem;
    let (fixture, catalog) = catalog_fixture();
    let thread = catalog.create_thread(&cwd(), None).expect("create");
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::success_with_usage(
            "answer",
            singularity_model::ModelUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 18,
                cached_input_tokens: 4,
                cached_input_tokens_present: true,
                reasoning_tokens: 0,
                usage_present: true,
            },
        ),
        ScriptedAttempt::success("answer without usage"),
    ]));
    let runner = fixture.runner(Some(provider as Arc<dyn Provider + Send + Sync>));
    let conversation = Conversation::new(runner, thread.clone());
    let mut sink = |_event| {};
    for index in 0..2 {
        conversation
            .run_turn(&format!("question {index}"), &mut sink)
            .expect("turn completes");
    }

    let snapshot = catalog.read_snapshot(&thread.thread_id).unwrap();
    let usage = &snapshot.summary.usage;
    assert_eq!(usage.input_tokens, 10, "只汇总报告了 usage 的请求");
    assert_eq!(usage.cached_input_tokens, 4);
    assert_eq!(usage.output_tokens, 5);
    assert_eq!(usage.total_tokens, 18, "采用已记录的供应商总量");
    assert!(!usage.cache_usage_complete, "缺失缓存用量不能展示成零命中");
    assert_eq!(usage.decode_ms, 0, "没有生成计时的记录不进入 TPS 样本");
    assert_eq!(usage.decode_tokens, 0);
    assert!(usage.usage_present);
    assert!(!usage.usage_complete, "有请求未报告用量时合计是下界");

    // 耗时与逐请求展示取自同一集合：页面里的请求观测已按 requestId 归并。
    let page = snapshot.page(100, None).unwrap();
    let durations: u64 = page
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .filter_map(|item| match item {
            HistoryItem::Request { observation, .. } if observation.input_tokens.is_some() => {
                Some(observation.duration_ms)
            }
            _ => None,
        })
        .sum();
    assert_eq!(usage.generation_ms, durations);
}
