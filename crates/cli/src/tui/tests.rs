#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! T019 / T025 [US1]：TUI 中断恢复与编辑合同。
//!
//! 完整 read-modify-validate 旅程的三面一致（渲染/ledger/文件效果）由
//! `tests/entrypoints.rs` 的跨入口矩阵覆盖，此处不重复；本文件保留 TUI
//! 自有状态机证据：中断后应用立即接受下一条输入（与 `tui.rs` 事件循环
//! 同构的 worker+通道驱动），以及编辑器视口/光标可预测性与提交文本保真
//! （视口行为不改变提交文本或会话状态）。

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use singularity_agent::session::test_support::WorkspaceFixture;
use singularity_model::Provider;
use singularity_runtime::events::TurnEvent;
use singularity_runtime::objects::TurnStatus;
use singularity_runtime::test_support::{GatedProvider, provider_snapshot, temp_sessions};
use singularity_runtime::{Conversation, ThreadCatalog, TurnRunner};

use super::app::{Phase, TuiApp};
use super::commands::Action;
use super::editor::Editor;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// 隔离的 TUI 应用：临时 sessions 目录 + 固定 workspace + 注入 provider。
fn app_with(
    provider: Arc<dyn Provider + Send + Sync>,
) -> (tempfile::TempDir, WorkspaceFixture, TuiApp) {
    let home = temp_sessions();
    let workspace = WorkspaceFixture::new();
    workspace.write_file("notes.txt", "alpha\n");
    let runner = Arc::new(
        TurnRunner::new(home.path().join("sessions"), provider_snapshot())
            .with_provider_override(provider),
    );
    let catalog = ThreadCatalog::new(&runner);
    let thread = catalog
        .create_thread(&workspace.path().to_string_lossy(), None)
        .expect("create thread");
    let conversation = Conversation::new(runner, thread);
    let app = TuiApp::new(conversation);
    (home, workspace, app)
}

/// 整帧渲染为纯文本（投影断言的唯一观察面）。
fn render_text(app: &mut TuiApp) -> String {
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| app.draw(frame))
        .expect("frame renders from events alone");
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<Vec<_>>()
        .join("")
}

/// T019 / US1 验收场景 2：模型等待中 Esc 中断——当前 turn 收敛为
/// interrupted，界面回到 Idle 并立即接受、完成下一条输入。
#[test]
fn interruption_leaves_the_tui_able_to_accept_the_next_input() {
    let (gate, started_rx) = GatedProvider::stop_gate();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    gate.with_release(release_rx);
    let (_home, _workspace, mut app) = app_with(gate);

    app.editor.set_text("long running task");
    let Action::Submit(goal) = app.handle_key(key(KeyCode::Enter)) else {
        panic!("Enter must submit");
    };
    // 与 tui.rs 事件循环同构：turn 在工作线程执行，事件经通道回灌。
    let (event_tx, event_rx) = std::sync::mpsc::channel::<TurnEvent>();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker_conversation = app.conversation.clone();
    std::thread::spawn(move || {
        let mut sink = move |event: TurnEvent| {
            let _ = event_tx.send(event);
        };
        let outcome = worker_conversation.run_turn(&goal, &mut sink);
        let _ = done_tx.send(outcome);
    });

    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("turn reaches the provider");
    app.handle_key(key(KeyCode::Esc));
    release_tx.send(()).expect("release the gate");

    while let Ok(event) = event_rx.recv() {
        app.on_turn_event(&event);
    }
    let outcome = done_rx
        .recv()
        .expect("chain finished")
        .expect("interruption converges as an Ok outcome");
    app.on_chain_finished(&Ok(()));
    assert_eq!(outcome.turn_status, TurnStatus::Interrupted);
    assert!(
        render_text(&mut app).contains("interrupted"),
        "the interrupted terminal state is rendered from the terminal event"
    );
    assert_eq!(
        app.phase,
        Phase::Idle,
        "the UI returns to an input-ready state"
    );

    // 下一条输入走同一条链正常完成。
    app.editor.set_text("next task");
    let Action::Submit(next) = app.handle_key(key(KeyCode::Enter)) else {
        panic!("Enter must submit after an interruption");
    };
    let conversation = app.conversation.clone();
    let mut sink = |event: TurnEvent| app.on_turn_event(&event);
    let second = conversation
        .run_turn(&next, &mut sink)
        .expect("the next input runs");
    app.on_chain_finished(&Ok(()));
    assert_eq!(second.turn_status, TurnStatus::Completed);
    assert_eq!(app.phase, Phase::Idle);
}

/// T025：编辑器视口与光标可预测——滚轮覆盖被任何光标移动清除回到跟随；
/// 视口/光标操作绝不改变提交文本。
#[test]
fn editor_viewport_is_predictable_and_never_mutates_the_submitted_text() {
    let mut editor = Editor::new();
    editor.insert_str("first line\nsecond line\nthird");
    assert_eq!(editor.row(), 2, "insert_str leaves the cursor at the end");

    // 滚轮把视口移离光标（覆盖态），光标一动立即回到跟随。
    let (visual_row, _) = editor.cursor_visual(40);
    editor.scroll_by(-2, visual_row);
    let overridden = editor.effective_scroll_top(visual_row, 2);
    editor.move_left();
    let (visual_row, _) = editor.cursor_visual(40);
    let refollowed = editor.effective_scroll_top(visual_row, 2);
    assert_ne!(
        overridden, refollowed,
        "cursor movement must clear the wheel override and re-follow"
    );

    // 可视坐标往返：向下移动一个可视行不丢内容。
    editor.set_cursor_visual(visual_row + 1, 3, 40);
    assert_eq!(
        editor.text(),
        "first line\nsecond line\nthird",
        "viewport and cursor operations never mutate the submitted text"
    );
    assert_eq!(editor.take(), "first line\nsecond line\nthird");
    assert!(editor.is_empty(), "take() drains the draft");
}
