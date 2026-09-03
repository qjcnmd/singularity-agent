#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! T019 / T025 [US1]：TUI 中断恢复与编辑合同。
//!
//! 完整 read-modify-validate 旅程的三面一致（渲染/ledger/文件效果）由
//! `tests/entrypoints.rs` 的跨入口矩阵覆盖，此处不重复；本文件保留 TUI
//! 自有状态机证据：中断后应用立即接受下一条输入（与 `tui.rs` 事件循环
//! 同构的 worker+通道驱动），以及编辑器视口/光标可预测性与提交文本保真
//! （视口行为不改变提交文本或会话状态）。

use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
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
use super::editor::{Editor, LARGE_PASTE_CHAR_THRESHOLD};

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
    let Action::Submit(goal) = app.handle_key_at(key(KeyCode::Enter), std::time::Instant::now())
    else {
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
    app.handle_key_at(key(KeyCode::Esc), std::time::Instant::now());
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
    let Action::Submit(next) = app.handle_key_at(key(KeyCode::Enter), std::time::Instant::now())
    else {
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
    assert_eq!(editor.take_expanded(), "first line\nsecond line\nthird");
    assert!(editor.is_empty(), "take_expanded() drains the draft");
}

/// A方案：超限粘贴以原子占位块入列——展示只见一行标签，提交展开全文；
/// 光标上下越过块，行首退格整体删除块，内容不丢。
#[test]
fn large_paste_is_atomic_and_expands_on_submit() {
    let mut editor = Editor::new();
    editor.insert_str("before");
    let big = "y".repeat(LARGE_PASTE_CHAR_THRESHOLD + 5);
    editor.insert_paste(big.clone(), std::time::Instant::now());
    assert!(
        !editor.is_empty(),
        "a pending paste keeps the editor non-empty"
    );

    // 展示面：原文行 + 一行标签 + 空尾行。
    let pieces = editor.wrapped_pieces(40);
    assert_eq!(pieces.len(), 3);
    assert_eq!(
        pieces[1],
        format!(
            "[pasted text · {} chars · expands on submit]",
            big.chars().count()
        )
    );
    assert!(editor.text().contains("pasted"));

    // 光标落在块后文本行，上下移动越过块。
    assert_eq!((editor.row(), editor.col()), (2, 0));
    editor.move_up();
    assert_eq!((editor.row(), editor.col()), (0, 0));
    editor.move_down();
    assert_eq!((editor.row(), editor.col()), (2, 0));

    // 行首退格整体删除块。
    editor.backspace();
    assert_eq!(editor.take_expanded(), "before\n");

    // 提交展开全文。
    let mut editor = Editor::new();
    editor.insert_str("before");
    editor.insert_paste(big.clone(), std::time::Instant::now());
    assert_eq!(editor.take_expanded(), format!("before\n{big}\n"));
    assert!(editor.is_empty());
}

/// 粘贴会话：同一逻辑粘贴被拆成多次投递时，窗内连续投递并入同一块，
/// 直接拼接不加分隔符，等价于整包一次到达。
#[test]
fn paste_session_merges_split_deliveries_into_one_block() {
    use std::time::{Duration, Instant};
    let (gate, _started) = GatedProvider::stop_gate();
    let (_home, _workspace, mut app) = app_with(gate);
    let start = Instant::now();
    let at = |ms: u64| start + Duration::from_millis(ms);

    // 短分块先行（内联暂存）+ 长分块随后：吸收合并为一块，无内联残留。
    let short = "ab\ncd\n";
    app.handle_paste(short.to_string(), at(0));
    assert_eq!(app.editor.text(), short);
    let big = "x".repeat(LARGE_PASTE_CHAR_THRESHOLD + 10);
    app.handle_paste(big.clone(), at(100));
    assert_eq!(app.editor.text().matches("[pasted").count(), 1);
    assert_eq!(app.editor.take_expanded(), format!("{short}{big}\n"));

    // 长分块 + 长分块：同样并入同一块。
    let head = "y".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1);
    let tail = "z".repeat(LARGE_PASTE_CHAR_THRESHOLD + 2);
    app.handle_paste(head.clone(), at(2000));
    app.handle_paste(tail.clone(), at(2100));
    assert_eq!(app.editor.text().matches("[pasted").count(), 1);
    assert_eq!(app.editor.take_expanded(), format!("{head}{tail}\n"));
}

/// 会话终结：窗口外重粘起新块；中间插过打字也起新块（不吞可见文本）。
#[test]
fn paste_session_ends_on_timeout_or_typing() {
    use std::time::{Duration, Instant};
    let (gate, _started) = GatedProvider::stop_gate();
    let (_home, _workspace, mut app) = app_with(gate);
    let start = Instant::now();
    let at = |ms: u64| start + Duration::from_millis(ms);
    let first = "a".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1);
    let second = "b".repeat(LARGE_PASTE_CHAR_THRESHOLD + 2);

    // 窗口外：两个块。
    app.handle_paste(first.clone(), at(0));
    app.handle_paste(second.clone(), at(5000));
    assert_eq!(app.editor.text().matches("[pasted").count(), 2);
    assert_eq!(app.editor.take_expanded(), format!("{first}\n{second}\n"));

    // 中间打字：两个块，字留在外面不进块。
    app.handle_paste(first.clone(), at(9000));
    app.editor.insert_char('Q');
    app.handle_paste(second.clone(), at(9100));
    assert_eq!(app.editor.text().matches("[pasted").count(), 2);
    assert_eq!(
        app.editor.take_expanded(),
        format!("{first}\nQ\n{second}\n")
    );
}

/// 回归（粘贴即发送）：非括号粘贴的高速按键流不再自动发送——多行内容
/// 拼回单个输入，全程无提交；慢速打字仍按 Enter 照常提交。
#[test]
fn burst_paste_keys_do_not_submit() {
    use std::time::{Duration, Instant};
    let (gate, _started) = GatedProvider::stop_gate();
    let (_home, _workspace, mut app) = app_with(gate);
    let start = Instant::now();
    let at = |ms: u64| start + Duration::from_millis(ms);
    let char_key = |ch: char| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);

    // 高速按键流 "ab\ncd"：全程不得提交，落定后拼回单个多行输入。
    for (index, ch) in "ab".chars().enumerate() {
        assert!(matches!(
            app.handle_key_at(char_key(ch), at(index as u64)),
            Action::Continue
        ));
    }
    assert!(matches!(
        app.handle_key_at(key(KeyCode::Enter), at(2)),
        Action::Continue
    ));
    for (index, ch) in "cd".chars().enumerate() {
        assert!(matches!(
            app.handle_key_at(char_key(ch), at(3 + index as u64)),
            Action::Continue
        ));
    }
    assert_eq!(app.phase, Phase::Idle);
    assert!(app.flush_burst_if_due(at(1000)));
    assert_eq!(app.editor.take_expanded(), "ab\ncd");

    // 慢速打字对照：冷字直通立即落屏，无暂存可冲刷；按 Enter 照常提交。
    for (index, ch) in "hi".chars().enumerate() {
        let base = 2000 + index as u64 * 100;
        assert!(matches!(
            app.handle_key_at(char_key(ch), at(base)),
            Action::Continue
        ));
        assert!(!app.flush_burst_if_due(at(base + 9)));
    }
    match app.handle_key_at(key(KeyCode::Enter), at(2300)) {
        Action::Submit(goal) => assert_eq!(goal, "hi"),
        _ => panic!("slow typing must still submit on Enter"),
    }
}
/// 同批次防线：整批纯文本按键（含 Enter）按单次粘贴消费，不提交；
/// 不足批量或含修饰 Enter 时走原路径。
#[test]
fn key_burst_batch_is_consumed_as_single_paste() {
    let (gate, _started) = GatedProvider::stop_gate();
    let (_home, _workspace, mut app) = app_with(gate);
    let key = |code: KeyCode| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
    // "ab\ncd" 同批到达：整体消费，进编辑器，无提交。
    let batch = vec![
        key(KeyCode::Char('a')),
        key(KeyCode::Char('b')),
        key(KeyCode::Enter),
        key(KeyCode::Char('c')),
        key(KeyCode::Char('d')),
    ];
    assert!(app.apply_key_burst(&batch, std::time::Instant::now()));
    assert_eq!(app.editor.take_expanded(), "ab\ncd");
    // 两键不成批。
    let small = vec![key(KeyCode::Char('a')), key(KeyCode::Enter)];
    assert!(!app.apply_key_burst(&small, std::time::Instant::now()));
    // 修饰 Enter 不参与成批（保留原提交语义）。
    let modified = vec![
        key(KeyCode::Char('a')),
        key(KeyCode::Char('b')),
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
    ];
    assert!(!app.apply_key_burst(&modified, std::time::Instant::now()));
}

/// 鼠标选中替换编辑：拖选区间反白，打字顶掉、退格删掉，块整体删除。
#[test]
fn mouse_selection_replaces_on_edit() {
    let mut editor = Editor::new();
    editor.insert_str("hello world");
    // 选中 "hello"：行首起选，移到第 5 列。
    editor.move_home();
    editor.begin_selection();
    editor.set_cursor_visual(0, 5, 80);
    assert_eq!(editor.selection_spans(80), vec![(0, 0, 5)]);
    editor.insert_char('X');
    assert_eq!(editor.text(), "X world");
    assert!(editor.selection_spans(80).is_empty());

    // 区间跨块：退格整体删块。
    editor.clear();
    editor.insert_str("ab");
    let big = "q".repeat(LARGE_PASTE_CHAR_THRESHOLD + 3);
    editor.insert_paste(big, std::time::Instant::now());
    editor.set_cursor_visual(0, 0, 80);
    editor.begin_selection();
    editor.set_cursor_visual(2, 0, 80);
    editor.backspace();
    assert!(!editor.text().contains("pasted"));
    assert_eq!(editor.take_expanded(), "");
}

/// 散选路径：纯点击（零宽）松开即散；键盘移动取消选择；无选区退格照常。
#[test]
fn empty_selection_collapses_and_moves_cancel() {
    let mut editor = Editor::new();
    editor.insert_str("ab");
    // 纯点击：起选落同一点，松开即散。
    editor.move_home();
    editor.begin_selection();
    editor.end_selection();
    assert!(editor.selection_spans(80).is_empty());
    // 有宽度的选中被键盘移动取消。
    editor.begin_selection();
    editor.set_cursor_visual(0, 2, 80);
    assert_eq!(editor.selection_spans(80).len(), 1);
    editor.move_right();
    assert!(editor.selection_spans(80).is_empty());
    // 无选区退格照常删一字。
    editor.backspace();
    assert_eq!(editor.text(), "a");
}

/// 历史回溯的草稿存展开文本：带占位块时上下箭头往返不丢粘贴内容，
/// 占位标签不漏进恢复文本。
#[test]
fn history_draft_round_trip_preserves_paste_content() {
    let (gate, _started) = GatedProvider::stop_gate();
    let (_home, _workspace, mut app) = app_with(gate);
    let big = "q".repeat(LARGE_PASTE_CHAR_THRESHOLD + 3);
    // 块前垫一字：回溯要求光标能到首行起始，块行不可停留。
    app.editor.insert_char('>');
    app.handle_paste(big.clone(), std::time::Instant::now());
    app.record_history("previous");
    // 光标回到首行起始，进入回溯后再退出。
    app.editor.move_up();
    app.editor.move_up();
    assert_eq!((app.editor.row(), app.editor.col()), (0, 0));
    app.handle_key_at(key(KeyCode::Up), std::time::Instant::now());
    assert_eq!(app.editor.expanded_text(), "previous");
    app.handle_key_at(key(KeyCode::Down), std::time::Instant::now());
    let restored = app.editor.take_expanded();
    assert!(
        restored.contains(&big),
        "the draft must keep the paste content"
    );
    assert!(
        !restored.contains("pasted text"),
        "no placeholder label may leak into restored text"
    );
}
