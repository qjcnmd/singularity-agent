//! TuiApp 行为测试（经 `app.rs` 的 `#[path]` 内联进 `mod tests`）。

use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use singularity_runtime::ReasoningPatch;
use singularity_runtime::TurnRunner;
use std::sync::mpsc;

// -- 夹具 ----------------------------------------------------------------

struct NeverCalledProvider;

impl singularity_model::Provider for NeverCalledProvider {
    fn protocol_contract(&self) -> singularity_model::ProviderProtocolContract {
        singularity_model::ProviderProtocolContract::default()
    }
    fn complete(
        &self,
        _request: &singularity_model::ModelTurnRequest,
        _cancellation: &singularity_core::CancellationToken,
    ) -> Result<singularity_model::ModelTurnResponse, singularity_model::ProviderError> {
        panic!("provider must not be called in this test")
    }
}

/// 在 provider 内部挂起直到外部放行（用于制造活动 turn 窗口）。
struct GatedOnceProvider {
    release: std::sync::Mutex<mpsc::Receiver<()>>,
}

impl singularity_model::Provider for GatedOnceProvider {
    fn protocol_contract(&self) -> singularity_model::ProviderProtocolContract {
        singularity_model::ProviderProtocolContract::default()
    }
    fn complete(
        &self,
        request: &singularity_model::ModelTurnRequest,
        _cancellation: &singularity_core::CancellationToken,
    ) -> Result<singularity_model::ModelTurnResponse, singularity_model::ProviderError> {
        let _ = self
            .release
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(3));
        Ok(singularity_model::ModelTurnResponse::completed(
            request.request_id.clone(),
            "r",
            "ok",
        ))
    }
}

fn test_home() -> (&'static tempfile::TempDir, std::path::PathBuf) {
    let dir = Box::leak(Box::new(tempfile::TempDir::new().expect("temp home")));
    let sessions = dir.path().join("sessions");
    (dir, sessions)
}

fn test_conversation(
    sessions: &std::path::Path,
    provider: std::sync::Arc<dyn singularity_model::Provider + Send + Sync>,
) -> Arc<Conversation> {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = runtime.handle().clone();
    std::mem::forget(runtime);
    let snapshot = singularity_model::ProviderConfigSnapshot::capture(
        |name| match name {
            "SINGULARITY_MODEL" => Some("base-model".to_string()),
            "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:9/v1".to_string()),
            "SINGULARITY_API_KEY" => Some("test-key-placeholder".to_string()),
            _ => None,
        },
        handle,
    );
    let runner = Arc::new(
        TurnRunner::new(sessions.to_path_buf(), snapshot).with_provider_override(provider),
    );
    let thread = singularity_runtime::store::create_thread(
        sessions,
        std::env::current_dir().unwrap().to_str().unwrap(),
        Some("openai_compatible/base-model".to_string()),
    )
    .expect("create thread");
    Conversation::new(runner, thread)
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn draw_at(app: &mut TuiApp, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| app.draw(frame))
        .expect("draw succeeds");
    terminal
}

/// 收集指定行范围内的可见文本。
fn row_text(terminal: &Terminal<TestBackend>, y: u16, width: u16) -> String {
    let buffer = terminal.backend().buffer();
    let mut line = String::new();
    for x in 0..width {
        line.push_str(buffer[(x, y)].symbol());
    }
    line.trim_end().to_string()
}

// -- 滚动与渲染 ----------------------------------------------------------

#[test]
fn following_stays_pinned_to_latest_output() {
    let (_home, sessions) = test_home();
    let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
    for index in 1..=40 {
        app.push_test_note(&format!("note-{index}"));
    }
    let terminal = draw_at(&mut app, 80, 24);
    let (follow, _top, pending) = app.scroll_snapshot();
    assert!(follow, "fresh content keeps following");
    assert_eq!(pending, 0);
    // 视口底行显示最新条目。
    let bottom = row_text(&terminal, 18, 80);
    assert!(
        bottom.contains("note-40"),
        "latest output must be visible, got: {bottom:?}"
    );
}

// -- 设置模态 ------------------------------------------------------------

#[test]
fn settings_modal_preserves_scroll_and_composes_patch() {
    let menu = SettingsMenu::open(Some("prov/m#high"));
    let patch = menu.patch();
    assert_eq!(patch.provider.as_deref(), Some("prov"));
    assert_eq!(patch.model.as_deref(), Some("m"));
    assert_eq!(patch.reasoning, ReasoningPatch::Set("high".to_string()));

    let (_home, sessions) = test_home();
    let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
    for index in 1..=30 {
        app.push_test_note(&format!("note-{index}"));
    }
    draw_at(&mut app, 80, 24);
    app.handle_key(key(KeyCode::PageUp, KeyModifiers::NONE));
    let snapshot_before = app.scroll_snapshot();

    for ch in "/settings".chars() {
        app.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
    draw_at(&mut app, 80, 24);
    app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));

    assert_eq!(app.scroll_snapshot(), snapshot_before);
    assert!(
        app.editor_text().is_empty(),
        "modal never steals the editor buffer"
    );
}

#[test]
fn clearing_reasoning_in_settings_removes_the_selector_effort() {
    let mut menu = SettingsMenu::open(Some("prov/m#high"));
    menu.reasoning.clear();

    assert_eq!(
        singularity_runtime::compose_merged_selector(Some("prov/m#high"), &menu.patch()),
        "prov/m"
    );
}

#[test]
fn esc_staircase_returns_to_follow_then_clears_draft_without_dead_end() {
    let (_home, sessions) = test_home();
    let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
    for index in 1..=40 {
        app.push_test_note(&format!("note-{index}"));
    }
    draw_at(&mut app, 80, 24);
    app.handle_key(key(KeyCode::PageUp, KeyModifiers::NONE));
    let (follow, top, _) = app.scroll_snapshot();
    assert!(!follow);
    assert!(top > 0);

    // 第一级：浏览态 Esc → 回底跟随。
    app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
    let (follow, _, pending) = app.scroll_snapshot();
    assert!(follow, "browsing Esc returns to follow");
    assert_eq!(pending, 0);

    // 第二级：非空草稿 Esc → 清空（pi/Codex 习惯）。
    for ch in "draft".chars() {
        app.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(app.editor_text(), "draft");
    app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.editor_text().is_empty(), "Esc clears the draft");

    // 第三级：空输入 + 跟随态 Esc → no-op，状态保持。
    app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
    let (follow, _, _) = app.scroll_snapshot();
    assert!(follow);
    assert!(app.editor_text().is_empty());
}

// -- 输入路由：followUp 单队列 -------------------------------------------

#[test]
fn follow_up_submission_enqueues_exactly_once_in_conversation() {
    let (_home, sessions) = test_home();
    let (release_tx, release_rx) = mpsc::channel();
    let conversation = test_conversation(
        &sessions,
        Arc::new(GatedOnceProvider {
            release: std::sync::Mutex::new(release_rx),
        }),
    );
    let mut app = TuiApp::new(Arc::clone(&conversation));

    let (tx, rx) = mpsc::channel::<crate::tui::UiEvent>();
    // 生产路径：编辑器输入 + Enter 提交初始轮（事件循环随后 spawn）。
    for ch in "initial goal".chars() {
        app.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
        Action::Submit(goal) => crate::tui::spawn_turn(&conversation, goal, tx.clone()),
        other => panic!("expected Submit action, got {other:?}"),
    }
    for _ in 0..400 {
        if conversation.has_active_turn() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(conversation.has_active_turn(), "gated turn must be active");
    assert_eq!(app.phase(), Phase::Running);
    assert!(
        app.waiting_since.is_some(),
        "submit must arm the waiting timer via set_waiting"
    );

    // 与生产路径一致：编辑器输入 + Alt+Enter 提交 followUp。
    for ch in "second".chars() {
        app.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(app.editor_text(), "second");
    app.handle_key(key(KeyCode::Enter, KeyModifiers::ALT));
    assert!(
        app.editor_text().is_empty(),
        "submitted input leaves the editor"
    );
    assert_eq!(
        conversation.pending_follow_ups(),
        vec!["second"],
        "the queue lives in Conversation and accepts the entry once"
    );

    release_tx.send(()).expect("release");
    // 等待链条结束（初始轮 + followUp 轮），保证夹具线程干净退出。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(16)) {
            Ok(crate::tui::UiEvent::ChainFinished(_)) => break,
            Ok(_) => {}
            Err(_) => panic!("chain should finish"),
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for chain finish");
        }
    }
    assert!(conversation.pending_follow_ups().is_empty());
}

// -- Ctrl+C 状态机（按键驱动） -------------------------------------------

fn ctrl_c() -> KeyEvent {
    key(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

fn hint_text(app: &TuiApp) -> String {
    let (_, hint) = app.footer_spans(100, 20);
    hint.iter().map(|span| span.content.clone()).collect()
}

#[test]
fn idle_first_ctrl_c_arms_confirm_and_second_exits_zero() {
    let (_home, sessions) = test_home();
    let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
    assert_eq!(app.handle_key(ctrl_c()), Action::Continue);
    assert_eq!(
        app.phase(),
        Phase::Idle,
        "idle Ctrl+C must not change phase"
    );
    assert!(
        hint_text(&app).contains("again to quit"),
        "idle first Ctrl+C shows the re-confirm hint"
    );
    assert_eq!(
        app.handle_key(ctrl_c()),
        Action::Exit(0),
        "idle second Ctrl+C exits with code 0"
    );
}

#[test]
fn running_ctrl_c_uses_the_normal_two_press_exit() {
    let (_home, sessions) = test_home();
    let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
    app.force_phase(Phase::Running);
    assert_eq!(app.handle_key(ctrl_c()), Action::Continue);
    assert_eq!(
        app.phase(),
        Phase::Running,
        "Ctrl+C does not interrupt the active turn"
    );
    assert!(
        hint_text(&app).contains("again to quit"),
        "the first Ctrl+C announces the normal exit confirmation"
    );
    assert_eq!(
        app.handle_key(ctrl_c()),
        Action::Exit(0),
        "the second Ctrl+C exits normally"
    );
}

#[test]
fn running_escape_delivers_interrupt_to_the_active_turn() {
    use singularity_core::CancellationToken;
    struct ProbeProvider {
        probe: Arc<std::sync::Mutex<Option<CancellationToken>>>,
    }
    impl singularity_model::Provider for ProbeProvider {
        fn protocol_contract(&self) -> singularity_model::ProviderProtocolContract {
            singularity_model::ProviderProtocolContract::default()
        }
        fn complete(
            &self,
            _request: &singularity_model::ModelTurnRequest,
            cancellation: &CancellationToken,
        ) -> Result<singularity_model::ModelTurnResponse, singularity_model::ProviderError>
        {
            self.probe.lock().unwrap().replace(cancellation.clone());
            while !cancellation.is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(singularity_model::ProviderError::from_model_error(
                singularity_model::ModelError::new(
                    singularity_model::ModelErrorKind::Cancelled,
                    "cancelled",
                ),
            ))
        }
    }

    let (_home, sessions) = test_home();
    let probe: Arc<std::sync::Mutex<Option<CancellationToken>>> = Arc::default();
    let conversation = test_conversation(
        &sessions,
        Arc::new(ProbeProvider {
            probe: Arc::clone(&probe),
        }),
    );
    let mut app = TuiApp::new(Arc::clone(&conversation));
    let (tx, rx) = mpsc::channel::<crate::tui::UiEvent>();
    for ch in "block me".chars() {
        app.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    match app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
        Action::Submit(goal) => crate::tui::spawn_turn(&conversation, goal, tx.clone()),
        other => panic!("expected Submit action, got {other:?}"),
    }
    for _ in 0..400 {
        if probe.lock().unwrap().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(probe.lock().unwrap().is_some(), "provider must be running");

    // 生产路径：运行中 Esc → Conversation::interrupt 送达当前轮。
    app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.phase(), Phase::Interrupting);
    assert!(
        probe.lock().unwrap().as_ref().unwrap().is_cancelled(),
        "Esc must cancel the active turn token"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(16)) {
            Ok(crate::tui::UiEvent::ChainFinished(Ok(TurnStatus::Interrupted))) => break,
            Ok(_) => {}
            Err(_) => panic!("chain should finish interrupted"),
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for interrupted chain finish");
        }
    }
}

// -- 鼠标与滚轮 ----------------------------------------------------------

#[test]
fn clicking_stop_interrupts_the_running_turn() {
    let (_home, sessions) = test_home();
    let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
    app.force_phase(Phase::Running);
    // 帧缓存点击矩形表：模拟 draw 注册的运行中 [stop] 命中矩形。
    app.click_targets = vec![(Rect::new(90, 28, 7, 1), ClickTarget::Stop)];
    // 命中 [stop]：中断当前轮。
    app.handle_click(93, 28);
    assert_eq!(
        app.phase(),
        Phase::Interrupting,
        "click on [stop] interrupts"
    );
}

// -- 压缩异步化 ----------------------------------------------------------

#[test]
fn compact_command_arms_state_and_returns_the_async_action() {
    let (_home, sessions) = test_home();
    let mut app = TuiApp::new(test_conversation(&sessions, Arc::new(NeverCalledProvider)));
    for ch in "/compact".chars() {
        app.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    let action = app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(action, Action::Compact, "/compact spawns the async action");
    assert!(
        app.compacting,
        "compaction flag is armed for the event loop"
    );
    assert!(
        app.compact_cancel.is_some(),
        "a cancellation token is stored for Esc cancel"
    );

    // 压缩进行中提交普通消息被护栏拒绝。
    for ch in "hello".chars() {
        app.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Continue,
        "submission is blocked while compacting"
    );

    // Esc 取消：令牌被取消，状态保持到 on_compact_finished 统一收尾。
    app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
        app.compact_cancel
            .as_ref()
            .map(|token| token.is_cancelled())
            .unwrap_or(false),
        "Esc must cancel the compaction token"
    );

    // 完成回调：无论取消与否都复位压缩状态并给出收尾 note。
    app.on_compact_finished(Err("cancelled".to_string()));
    assert!(!app.compacting, "finished compaction resets the flag");
    assert!(
        app.compact_cancel.is_none(),
        "finished compaction drops the token"
    );
}
