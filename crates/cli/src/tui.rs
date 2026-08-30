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
//! 按事件间隔归一化加速（滚轮/触控板）并按指针位置路由：输入框内滚动
//! 编辑器视口（光标一动即回跟随），其余滚动会话流；点击输入框定位光标；
//! 运行中状态行右侧的 [stop] 可点击（与 Esc 同一中断路径）。提交新消息后
//! 视口钉在新内容首行（page-flip），填满一屏后自动回底跟随。`/model`、
//! `/settings`、`/resume`、`/new`、`/session`、`/compact` 与 `/name` 通过
//! 共用的临时菜单范式处理。
//!
//! 终端生命周期：进入 alternate screen + raw mode + 鼠标捕获 + 键盘增强
//! （CSI-u 修饰键）；所有退出路径（正常、错误、panic）统一恢复终端状态。

mod app;
mod commands;
mod editor;
mod history;
mod modals;
mod mouse;
mod paste_burst;
mod scroll;
mod session_actions;
mod transcript;
mod view;

use std::io::{IsTerminal, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use unicode_width::UnicodeWidthStr;

use crate::forward::EventForward;
use app::{Phase, TuiApp};
use commands::Action;
use singularity_runtime::CompactionOutcome;
use singularity_runtime::events::TurnEvent;
use singularity_runtime::objects::TurnStatus;

pub(crate) fn char_display_width(ch: char) -> usize {
    UnicodeWidthStr::width(ch.to_string().as_str())
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
pub(crate) fn wrapped_lines(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for logical in text.split('\n') {
        let offsets = wrap_offsets(logical, width);
        let char_count = logical.chars().count();
        for (index, &start) in offsets.iter().enumerate() {
            let end = offsets.get(index + 1).copied().unwrap_or(char_count);
            lines.push(logical.chars().skip(start).take(end - start).collect());
        }
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

pub struct InteractiveOutcome {
    pub exit_code: i32,
}

/// 交互模式要求真实终端；不满足时返回面向用户的诊断。
pub fn ensure_terminal() -> Result<(), &'static str> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Ok(())
    } else {
        Err(
            "interactive mode requires a terminal; use `sg --print <goal>` or `sg --json <goal>` for non-interactive execution",
        )
    }
}

/// 进入长驻交互模式。调用方必须先通过 [`ensure_terminal`]。
pub fn run(conversation: std::sync::Arc<singularity_runtime::Conversation>) -> InteractiveOutcome {
    match run_inner(conversation) {
        Ok(code) => InteractiveOutcome { exit_code: code },
        Err(error) => {
            let _ = restore_terminal();
            eprintln!("sg: {error}");
            InteractiveOutcome { exit_code: 1 }
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
    // 括号粘贴：支持方把粘贴作为单一 Paste 事件送达，Windows 控制台等
    // 不支持方退回 burst 检测兜底（见 paste_burst 模块）。尽力而为。
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

fn restore_terminal() -> std::io::Result<()> {
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
    Ok(())
}

/// panic 路径也恢复终端；恢复逻辑与正常退出共用同一实现。
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        previous(info);
    }));
}

// ---------------------------------------------------------------------------
// UI 事件通道
// ---------------------------------------------------------------------------

enum UiEvent {
    FromTurn(Box<TurnEvent>),
    ChainFinished(Result<TurnStatus, String>),
    /// 中断/失败时未交付的转向输入，退还编辑器。
    UndeliveredInputs(Vec<String>),
    /// /compact 后台压缩线程的结果，携带 spawn 时的会话世代号。
    CompactFinished(Result<CompactionOutcome, String>, u64),
}

fn from_turn(event: TurnEvent) -> UiEvent {
    UiEvent::FromTurn(Box::new(event))
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
        let mut sink = EventForward::new(tx.clone(), from_turn);
        let result = conversation.run_turn(&goal, &mut sink);
        drop(sink);
        let (finished, undelivered) = match result {
            Ok(outcome) => (Ok(outcome.turn_status), outcome.undelivered_inputs),
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
    let _ = restore_terminal();
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

    loop {
        // 阻塞等待键盘事件：键事件即时唤醒，避免固定轮询间隔引入的按键
        // 延迟：有界阻塞 poll，事件到达即唤醒。运行中 deadline
        // 取下一次 spinner 节拍，空闲放宽到 IDLE_POLL_TIMEOUT；turn 事件
        // 经通道送达，最迟等一个 deadline 后处理，与现状轮询节奏持平。
        let now = Instant::now();
        let poll_timeout = if app.phase == Phase::Idle {
            if app.paste_burst.is_active() {
                // 有未落地的 burst 缓冲时缩短等待，确保静默超时及时 flush。
                Duration::from_millis(50)
            } else {
                IDLE_POLL_TIMEOUT
            }
        } else {
            // 距下一次 spinner 节拍的时间；若已过点则立即返回。
            last_spinner_tick
                .checked_add(SPINNER_TICK)
                .unwrap_or_else(|| now + SPINNER_TICK)
                .saturating_duration_since(now)
        };
        let key_ready = crossterm::event::poll(poll_timeout).unwrap_or(false);

        // 排空本轮 turn 事件。
        while let Ok(event) = rx.try_recv() {
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
        if key_ready {
            while crossterm::event::poll(Duration::ZERO).unwrap_or(false) {
                match crossterm::event::read() {
                    Ok(crossterm::event::Event::Key(key)) => match app.handle_key(key) {
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
                        Action::Exit(code) => return Ok(code),
                    },
                    Ok(crossterm::event::Event::Paste(text)) => {
                        // 括号粘贴事件：CRLF/CR 归一并整段插入（burst 状态清空）。
                        app.handle_paste(text);
                    }
                    Ok(crossterm::event::Event::Mouse(mouse)) => match mouse.kind {
                        crossterm::event::MouseEventKind::ScrollUp => {
                            app.handle_wheel(true, mouse.column, mouse.row)
                        }
                        crossterm::event::MouseEventKind::ScrollDown => {
                            app.handle_wheel(false, mouse.column, mouse.row)
                        }
                        crossterm::event::MouseEventKind::Down(
                            crossterm::event::MouseButton::Left,
                        ) => app.handle_click(mouse.column, mouse.row),
                        _ => {}
                    },
                    Ok(crossterm::event::Event::Resize(_, _)) => {
                        // 布局按帧重算；滚动位置由 ScrollState 钳制。
                    }
                    _ => {}
                }
            }
        }

        // 每帧推进 burst 静默超时：把已缓冲的粘贴整体落地。
        app.flush_paste_burst_if_due(Instant::now());

        if last_spinner_tick.elapsed() >= SPINNER_TICK {
            app.tick();
            last_spinner_tick = Instant::now();
        }

        terminal
            .draw(|frame| app.draw(frame))
            .map_err(|error| format!("draw failed: {error}"))?;
    }
}
