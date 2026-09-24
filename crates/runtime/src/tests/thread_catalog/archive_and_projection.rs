use super::*;

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
            assert!(!head.definitions_id.is_empty());
            assert!(
                !serde_json::to_string(&head)
                    .unwrap()
                    .contains("distinct user history")
            );
            observed = Some((observation.request_id, head));
        }
    };
    crate::test_support::run_async(conversation.run_turn(&input, &mut sink)).unwrap();
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
        crate::test_support::run_async(
            conversation.run_turn(&format!("question {index}"), &mut sink),
        )
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
