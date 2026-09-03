//! 交互式 TUI：主会话流 + 底部多行编辑器 + 状态/提示双行 footer + 临时选择
//! 菜单。TUI 只依赖 [`singularity_runtime`] 的 `Conversation` 与 [`TurnEvent`]：
//! turn 在工作线程上执行，事件经通道驱动渲染；运行中 Enter 注入当前 turn，
//! Alt+Enter 提交到后续队列，Alt+Up 撤回最近一条排队消息。
//!
//! Ctrl+C 由 crossterm `KeyEvent` 驱动（raw mode 下不依赖操作系统信号）：
//! 第一次清空非空输入并进入退出确认，第二次正常退出。Esc 在运行中
//! 中断当前轮，空闲时依次回底跟随、清空草稿或 no-op；输入为空时 Ctrl+D 退出。
//!
//! Ctrl+T 折叠思考块；Ctrl+O（兼容 Alt+O）循环工具块的折叠、截断、完整
//! 三档；Ctrl+J 强制换行；End 回到最新内容，PageUp/PageDown 翻页。鼠标滚轮
//! 固定步长并按指针位置路由：输入框内滚动
//! 编辑器视口（光标一动即回跟随），其余滚动会话流；点击输入框定位光标；
//! 运行中状态行右侧的 [stop] 可点击（与 Esc 同一中断路径）。提交新消息后
//! 视口回底跟随。`/model`、
//! `/settings`、`/resume`、`/new`、`/session`、`/compact` 与 `/name` 通过
//! 共用的临时菜单范式处理。
//!
//! 终端生命周期：进入 alternate screen + raw mode + 鼠标捕获 + 键盘增强
//! （CSI-u 修饰键）；所有退出路径（正常、错误、panic）统一恢复终端状态。

mod app;
mod commands;
mod editor;
mod flow_select;
mod history;
mod modals;
mod mouse;
mod paste_burst;
mod scroll;
mod session_actions;
mod transcript;
mod view;

#[cfg(test)]
#[path = "tui/tests.rs"]
mod tests;

/// T028：入口等价测试需要同时触达 TUI 内部（`pub(in tui)` 字段）与 crate
/// 根的无交互执行 seam，故挂载在本模块下。
#[cfg(test)]
#[path = "../tests/entrypoints.rs"]
mod entrypoints;

use std::io::{IsTerminal, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use unicode_width::UnicodeWidthChar;

use crate::PROGRAM_NAME;
use app::{Phase, TuiApp};
use commands::Action;
use singularity_runtime::CompactionOutcome;
use singularity_runtime::events::TurnEvent;

pub(crate) fn char_display_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// 单个逻辑行按显示宽度贪心折行后，每折行在其字符序列中的起始偏移。
///
/// transcript 行物化与 editor 光标映射共享的折行核心：偏移即折行事实，
/// 需要行的文本时按偏移切片，需要行内字符数时取相邻偏移差。
pub(crate) fn wrap_offsets(line: &str, width: usize) -> Vec<usize> {
    let width = width.max(1);
    let mut offsets = vec![0usize];
    let mut current_width = 0usize;
    for (index, ch) in line.chars().enumerate() {
        let ch_width = char_display_width(ch);
        // 不变量：offsets 初始化为 [0]，恒非空。
        #[allow(clippy::expect_used)]
        if current_width + ch_width > width && index > *offsets.last().expect("non-empty") {
            offsets.push(index);
            current_width = 0;
        }
        current_width += ch_width;
    }
    offsets
}

/// transcript 与 editor 渲染共享的贪心显示宽度换行。
///
/// 单次遍历：每个字符只访问一次，行满即产出一行；长单行保持线性，
/// 不随行数二次增长（字节上限之外不再需要粘贴截断之类的护栏）。
pub(crate) fn wrapped_lines(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for logical in text.split('\n') {
        let mut current = String::new();
        let mut current_width = 0usize;
        for ch in logical.chars() {
            let ch_width = char_display_width(ch);
            // 与 wrap_offsets 同一条贪心规则：行非空且放不下时另起一行；
            // 单个超宽字符独占一行，不产生空行。
            if current_width > 0 && current_width + ch_width > width {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current_width += ch_width;
            current.push(ch);
        }
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

const SPINNER_TICK: Duration = Duration::from_millis(120);
/// 空闲（无运行中 turn）时事件泵单次阻塞等待的上限：无 spinner 节拍驱动，
/// 放宽唤醒频率以省 CPU；运行中由下一次 spinner 节拍约束。
const IDLE_POLL_TIMEOUT: Duration = Duration::from_millis(250);

/// 交互模式要求真实终端；不满足时返回面向用户的诊断。
pub fn ensure_terminal() -> Result<(), String> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Ok(())
    } else {
        Err(format!(
            "interactive mode requires a terminal; use `{PROGRAM_NAME} --print <goal>` or `{PROGRAM_NAME} --json <goal>` for non-interactive execution"
        ))
    }
}

/// 进入长驻交互模式。调用方必须先通过 [`ensure_terminal`]。返回进程退出码。
pub fn run(conversation: std::sync::Arc<singularity_runtime::Conversation>) -> i32 {
    match run_inner(conversation) {
        Ok(code) => code,
        Err(error) => {
            restore_terminal();
            eprintln!("{PROGRAM_NAME}: {error}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// 终端生命周期
// ---------------------------------------------------------------------------

fn enter_terminal() -> std::io::Result<()> {
    use crossterm::ExecutableCommand;
    use crossterm::event::{
        EnableBracketedPaste, EnableMouseCapture, KeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    };
    use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(EnableMouseCapture)?;
    // 括号粘贴：支持方把粘贴作为单一 Paste 事件送达。尽力而为。
    let _ = stdout.execute(EnableBracketedPaste);
    // 键盘增强：让 Shift+Enter / Ctrl+J 等在真实终端以带修饰符的 CSI-u 序列
    // 到达。尽力而为：Windows 控制台键记录天然携带修饰键，不受影响；不支持
    // 的主机只退回无增强模式，不阻断终端启动。
    let _ = stdout.execute(PushKeyboardEnhancementFlags(
        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
    ));
    Ok(())
}

fn restore_terminal() {
    use crossterm::ExecutableCommand;
    use crossterm::event::{
        DisableBracketedPaste, DisableMouseCapture, PopKeyboardEnhancementFlags,
    };
    use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};
    // 幂等：任一步失败继续其余步骤，保证退出路径尽量恢复。
    let _ = disable_raw_mode();
    let mut stdout = std::io::stdout();
    let _ = stdout.execute(PopKeyboardEnhancementFlags);
    let _ = stdout.execute(DisableBracketedPaste);
    let _ = stdout.execute(LeaveAlternateScreen);
    let _ = stdout.execute(DisableMouseCapture);
    let _ = stdout.flush();
}

/// panic 路径也恢复终端；恢复逻辑与正常退出共用同一实现。
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

// ---------------------------------------------------------------------------
// UI 事件通道
// ---------------------------------------------------------------------------

enum UiEvent {
    FromTurn(Box<TurnEvent>),
    /// 链窗口的生命周期回执：`Ok(())` 表示已落盘可信终态并由终态事件完成
    /// 投影（终局状态只来自事件，UI 不再复制一份状态机）；`Err` 表示
    /// 无可信终态的链中止（准备失败、终态化失败、并发占用）。
    ChainFinished(Result<(), String>),
    /// 中断/失败时未交付的转向输入，退还编辑器。
    UndeliveredInputs(Vec<String>),
    /// /compact 后台压缩线程的结果，携带 spawn 时的会话世代号。
    CompactFinished(Result<CompactionOutcome, String>, u64),
}

/// TUI worker 以完成事件作为生命周期回执；事件循环不得同步 join 而阻塞绘制。
fn spawn_ui_worker(task: impl FnOnce() + Send + 'static) {
    drop(std::thread::spawn(task));
}

fn spawn_turn(
    conversation: &std::sync::Arc<singularity_runtime::Conversation>,
    goal: String,
    tx: mpsc::Sender<UiEvent>,
) {
    let conversation = std::sync::Arc::clone(conversation);
    spawn_ui_worker(move || {
        let mut sink = |event| {
            let _ = tx.send(UiEvent::FromTurn(Box::new(event)));
        };
        let result = conversation.run_turn(&goal, &mut sink);
        let (finished, undelivered) = match result {
            Ok(outcome) => (Ok(()), outcome.undelivered_inputs),
            Err(error) => (Err(error.to_string()), Vec::new()),
        };
        let _ = tx.send(UiEvent::ChainFinished(finished));
        if !undelivered.is_empty() {
            let _ = tx.send(UiEvent::UndeliveredInputs(undelivered));
        }
    });
}

/// 后台执行 /compact：不阻塞事件循环，结果经 `UiEvent::CompactFinished`
/// 连同 spawn 时的会话世代回送；`cancellation` 由调用方持有（TUI 中 Esc
/// 取消本次压缩）。
fn spawn_compact(
    conversation: &std::sync::Arc<singularity_runtime::Conversation>,
    cancellation: singularity_core::CancellationToken,
    epoch: u64,
    tx: mpsc::Sender<UiEvent>,
) {
    let conversation = std::sync::Arc::clone(conversation);
    spawn_ui_worker(move || {
        let result = conversation
            .compact(&cancellation)
            .map_err(|error| error.to_string());
        let _ = tx.send(UiEvent::CompactFinished(result, epoch));
    });
}

// ---------------------------------------------------------------------------
// 主循环
// ---------------------------------------------------------------------------

fn run_inner(
    conversation: std::sync::Arc<singularity_runtime::Conversation>,
) -> Result<i32, String> {
    install_panic_hook();
    enter_terminal().map_err(|error| format!("terminal setup failed: {error}"))?;
    let outcome = event_loop(conversation);
    restore_terminal();
    outcome
}

fn event_loop(
    conversation: std::sync::Arc<singularity_runtime::Conversation>,
) -> Result<i32, String> {
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))
        .map_err(|error| format!("terminal backend failed: {error}"))?;
    let (tx, rx) = mpsc::channel::<UiEvent>();
    let mut app = TuiApp::new(std::sync::Arc::clone(&conversation));
    let mut last_spinner_tick = Instant::now();

    // 先渲染一帧：用真实终端尺寸填充帧缓存（会话流宽/视口），使首个事件
    // 处理时的滚动度量取自真实布局而非默认值，同时消除启动空屏闪烁。
    terminal
        .draw(|frame| app.draw(frame))
        .map_err(|error| format!("draw failed: {error}"))?;

    loop {
        // 阻塞等待键盘事件：键事件即时唤醒，避免固定轮询间隔引入的按键
        // 延迟：有界阻塞 poll，事件到达即唤醒。运行中 deadline
        // 取下一次 spinner 节拍，空闲放宽到 IDLE_POLL_TIMEOUT；turn 事件
        // 经通道送达，最迟等一个 deadline 后处理，与现状轮询节奏持平。
        let now = Instant::now();
        let mut poll_timeout = if app.phase == Phase::Idle {
            IDLE_POLL_TIMEOUT
        } else {
            // 距下一次 spinner 节拍的时间；若已过点则立即返回。
            last_spinner_tick
                .checked_add(SPINNER_TICK)
                .unwrap_or_else(|| now + SPINNER_TICK)
                .saturating_duration_since(now)
        };
        // 突发暂存待落定时缩短等待：hold 单字 9ms、突发停顿按平台超时落定，
        // 无待定内容时不影响原有节奏。
        if let Some(grace) = app.burst.poll_grace() {
            poll_timeout = poll_timeout.min(grace);
        }
        let key_ready = crossterm::event::poll(poll_timeout).unwrap_or(false);

        // 本轮是否有可见变化：turn 事件、终端输入或 spinner 节拍任一到达
        // 才重绘。空闲无变化时跳过整帧构建（ratatui 的 diff 只省终端 IO，
        // 不省帧构建 CPU）；任何终端事件都视为变化（含 resize：新尺寸只在
        // draw 时向后端确认）。
        let mut frame_changed = false;

        // 排空本轮 turn 事件。
        while let Ok(event) = rx.try_recv() {
            frame_changed = true;
            match event {
                UiEvent::FromTurn(turn_event) => app.on_turn_event(turn_event.as_ref()),
                UiEvent::ChainFinished(result) => {
                    // 终态后可能带出排队中的 /compact（Action::Compact），
                    // 与键盘触发的压缩走同一 spawn 路径。
                    if let Action::Compact(cancellation, epoch) = app.on_chain_finished(&result) {
                        spawn_compact(&app.conversation_handle(), cancellation, epoch, tx.clone());
                    }
                }
                UiEvent::UndeliveredInputs(inputs) => app.return_undelivered(inputs),
                UiEvent::CompactFinished(result, epoch) => {
                    // 压缩结束可能带出排队输入的回合（Action::Submit），
                    // 与键盘提交走同一 spawn 路径。
                    if let Action::Submit(goal) = app.on_compact_finished(epoch, result) {
                        spawn_turn(&app.conversation_handle(), goal, tx.clone());
                    }
                }
            }
        }

        // 键盘、鼠标与 resize 事件（有就绪时才排空，否则保持阻塞等待）。
        // 先整批取出：同批纯文本突发（含 Enter）按单次粘贴整体消费，
        // 分块投递的粘贴至此不再有机会误提交；其余逐个按原路径处理。
        // 送达应用的终端事件一律按界面变化登记（含 resize：新尺寸只在 draw
        // 时向后端确认）：一帧究竟向终端写多少由 ratatui 的缓冲差决定，内容
        // 未变时只发游标与样式复位（实测 27 字节、不重写任何格子），所以
        // "猜这一下有没有变化"省不到东西，只会把单键编辑（退格、方向键、
        // Delete）留在屏外直到别的事件才落屏。
        if key_ready {
            let mut batch = Vec::new();
            while crossterm::event::poll(Duration::ZERO).unwrap_or(false) {
                match crossterm::event::read() {
                    Ok(event) => batch.push(event),
                    Err(_) => break,
                }
            }
            frame_changed = !batch.is_empty();
            let at = std::time::Instant::now();
            if !app.apply_key_burst(&batch, at) {
                for event in batch {
                    match event {
                        crossterm::event::Event::Key(key) => match app.handle_key_at(key, at) {
                            Action::Continue => {}
                            Action::Submit(goal) => {
                                spawn_turn(&app.conversation_handle(), goal, tx.clone())
                            }
                            Action::Compact(cancellation, epoch) => spawn_compact(
                                &app.conversation_handle(),
                                cancellation,
                                epoch,
                                tx.clone(),
                            ),
                            Action::Exit => return Ok(0),
                        },
                        // 括号粘贴事件按焦点路由：设置/命名菜单打开时落入
                        // 当前字段，否则整段进入编辑器。
                        crossterm::event::Event::Paste(text) => {
                            if app.settings.is_some() {
                                app.handle_settings_paste(text);
                            } else {
                                app.handle_paste(text, at);
                            }
                        }
                        crossterm::event::Event::Mouse(mouse) => match mouse.kind {
                            crossterm::event::MouseEventKind::ScrollUp => {
                                app.handle_wheel(true, mouse.column, mouse.row)
                            }
                            crossterm::event::MouseEventKind::ScrollDown => {
                                app.handle_wheel(false, mouse.column, mouse.row)
                            }
                            crossterm::event::MouseEventKind::Down(
                                crossterm::event::MouseButton::Left,
                            ) => app.handle_click(mouse.column, mouse.row),
                            // 拖选：按下已起选，拖拽只扩展光标端；松开无位移即散选。
                            crossterm::event::MouseEventKind::Drag(
                                crossterm::event::MouseButton::Left,
                            ) => app.handle_drag(mouse.column, mouse.row),
                            crossterm::event::MouseEventKind::Up(
                                crossterm::event::MouseButton::Left,
                            ) => app.handle_release(),
                            _ => {}
                        },
                        _ => {}
                    }
                }
            }
        }

        // 突发暂存到期落定（hold 单字超时按打字吐出，突发停顿整串走粘贴）。
        if app.flush_burst_if_due(std::time::Instant::now()) {
            frame_changed = true;
        }

        if last_spinner_tick.elapsed() >= SPINNER_TICK {
            last_spinner_tick = Instant::now();
            // 空闲时 spinner 以空白渲染（见 draw），推进节拍无可见变化。
            if app.phase != Phase::Idle {
                app.tick();
                frame_changed = true;
            }
        }

        // 运行中（spinner/计时持续动画）或工具完成闪烁窗口内每轮绘制，
        // 其余只在有变化时绘制。
        if frame_changed || app.phase != Phase::Idle || app.transcript.completion_flash_active() {
            terminal
                .draw(|frame| app.draw(frame))
                .map_err(|error| format!("draw failed: {error}"))?;
        }
    }
}

#[cfg(test)]
mod wrap_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::transcript::Transcript;
    use super::{wrap_offsets, wrapped_lines};

    /// 宽字符折行：显示宽度为 2 的字符放不下时整字换行；单字符宽于视口
    /// 时强制独占一行（不死循环、不丢字）。
    #[test]
    fn wide_chars_wrap_on_display_width() {
        // "你好" 各宽 2，视口 3：第二字溢出 → 两行。
        assert_eq!(wrap_offsets("你好", 3), vec![0, 1]);
        assert_eq!(wrapped_lines("你好", 3), vec!["你", "好"]);
        // 单字宽于视口：仍占一行，偏移表只有 [0]。
        assert_eq!(wrap_offsets("你", 1), vec![0]);
        assert_eq!(wrapped_lines("你", 1), vec!["你"]);
    }

    /// 逻辑行保真：空行折成一行空串，多行按 `\n` 拆分后逐行折行，
    /// 拼接可还原原文。
    #[test]
    fn wrapped_lines_preserve_logical_line_count() {
        assert_eq!(wrapped_lines("", 10), vec![""]);
        assert_eq!(wrapped_lines("a\n\nb", 10), vec!["a", "", "b"]);
        // 每折行首偏移严格递增且以 0 开头。
        let offsets = wrap_offsets("abcdef", 2);
        assert_eq!(offsets.first(), Some(&0));
        assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]));
    }

    /// 流式增量折行等价性：分多次、含换行与宽字符的增量投喂，与全量重包
    /// 逐段一致；宽度变化与落定后新段落走全量重算路径，结果仍一致。
    #[test]
    fn live_wrap_matches_full_rewrap_on_append() {
        let mut transcript = Transcript::new();
        assert_eq!(transcript.live_row_count(10), 0);
        let mut full = String::new();
        for chunk in [
            "Hello ",
            "世界迎",
            "接\nsecond ",
            "line… ",
            "continues a lot to force wrapping behaviour xyz",
        ] {
            transcript.assistant_delta(chunk);
            full.push_str(chunk);
            let expected = wrapped_lines(&full, 10);
            assert_eq!(transcript.live_row_count(10), expected.len());
            let rendered: Vec<String> = transcript
                .live_rows(10)
                .into_iter()
                .map(|line| line.to_string())
                .collect();
            assert_eq!(rendered, expected);
        }
        // 宽度变化走全量重算路径，结果仍一致。
        let expected = wrapped_lines(&full, 4);
        assert_eq!(transcript.live_row_count(4), expected.len());
        // 落定后 live 行归零；新段落（缓冲收缩）同样全量重算。
        transcript.flush_assistant();
        assert_eq!(transcript.live_row_count(10), 0);
        transcript.assistant_delta("new para");
        assert_eq!(transcript.live_row_count(10), 1);
    }
}
