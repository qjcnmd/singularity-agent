//! 交互式 TUI：主会话流 + 底部多行编辑器 + 状态/提示双行 footer + 临时设置
//! 模态。TUI 只依赖 [`singularity_runtime`] 的 `Conversation` 与
//! [`TurnEvent`]：turn 在工作线程上执行，事件经通道驱动渲染；steer 注入当前
//! 活动 turn，followUp 提交给 Conversation 的后续队列并自动逐条执行。
//!
//! Ctrl+C 由 crossterm `KeyEvent` 驱动（raw mode 下不依赖操作系统信号）：
//! 运行中第一次中断当前轮（已接受 followUp 在可信终态后继续），第二次强制
//! 退出（130）；空闲第一次进入再确认提示，第二次正常退出（0）。确认状态由
//! 任何其他按键、提交输入或 turn 链结束复位，settings 模态不改变该语义。
//!
//! 其余按键语义（对照上游 Grok Build 的 cancel/clear/rewind 惯例）：
//! Esc 为无死端阶梯——浏览态先回底跟随，其次清空非空草稿，空输入为 no-op；
//! Ctrl+T 切换 steer/followUp 提交语义（运行中 Enter 注入当前轮或排队为
//! 后续 turn）；Ctrl+S 打开临时设置模态；Ctrl+J 强制插入换行（窗口/终端
//! 不转发 Shift+Enter 时的手工等价键）；Ctrl+Home/Ctrl+End 跳转记录两端，
//! PageUp/PageDown 翻页；Alt+O 折叠/展开最近工具 result。
//!
//! 终端生命周期：进入 alternate screen + raw mode + 鼠标捕获 + 键盘增强
//! （CSI-u 修饰键）；所有退出路径（正常、错误、panic）统一恢复终端状态。

mod app;
mod editor;
mod scroll;
mod transcript;

use std::io::{IsTerminal, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use unicode_width::UnicodeWidthStr;

use app::{Action, TuiApp};
use singularity_runtime::events::{TurnEvent, TurnEventSink};
use singularity_runtime::objects::TurnStatus;

pub(crate) fn char_display_width(ch: char) -> usize {
    UnicodeWidthStr::width(ch.to_string().as_str())
}

const INTERRUPT_POLL: Duration = Duration::from_millis(100);
const SPINNER_TICK: Duration = Duration::from_millis(120);

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
        EnableMouseCapture, KeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    };
    use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    stdout.execute(EnableMouseCapture)?;
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
    use crossterm::event::{DisableMouseCapture, PopKeyboardEnhancementFlags};
    use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};
    // 幂等：任一步失败继续其余步骤，保证退出路径尽量恢复。
    let _ = disable_raw_mode();
    let mut stdout = std::io::stdout();
    let _ = stdout.execute(PopKeyboardEnhancementFlags);
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
}

struct Forward {
    tx: mpsc::Sender<UiEvent>,
}

impl TurnEventSink for Forward {
    fn emit(&mut self, event: TurnEvent) {
        let _ = self.tx.send(UiEvent::FromTurn(Box::new(event)));
    }
}

fn spawn_turn(
    conversation: &std::sync::Arc<singularity_runtime::Conversation>,
    goal: String,
    tx: mpsc::Sender<UiEvent>,
) {
    let conversation = std::sync::Arc::clone(conversation);
    std::thread::spawn(move || {
        let mut sink = Forward { tx: tx.clone() };
        let result = conversation.run_turn(&goal, &mut sink);
        drop(sink);
        let finished = match result {
            Ok(outcome) => Ok(outcome.turn_status),
            Err(error) => Err(error.to_string()),
        };
        let _ = tx.send(UiEvent::ChainFinished(finished));
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
        // 排空本轮 turn 事件。
        while let Ok(event) = rx.try_recv() {
            match event {
                UiEvent::FromTurn(turn_event) => app.on_turn_event(turn_event.as_ref()),
                UiEvent::ChainFinished(result) => app.on_chain_finished(&result),
            }
        }

        // 键盘、鼠标与 resize 事件（排空至无事件为止）。
        while crossterm::event::poll(Duration::ZERO).unwrap_or(false) {
            match crossterm::event::read() {
                Ok(crossterm::event::Event::Key(key)) => match app.handle_key(key) {
                    Action::Continue => {}
                    Action::Submit(goal) => spawn_turn(&conversation, goal, tx.clone()),
                    Action::Exit(code) => return Ok(code),
                },
                Ok(crossterm::event::Event::Mouse(mouse)) => match mouse.kind {
                    crossterm::event::MouseEventKind::ScrollUp => app.handle_wheel(true),
                    crossterm::event::MouseEventKind::ScrollDown => app.handle_wheel(false),
                    _ => {}
                },
                Ok(crossterm::event::Event::Resize(_, _)) => {
                    // 布局按帧重算；滚动位置由 ScrollState 钳制。
                }
                _ => {}
            }
        }

        if last_spinner_tick.elapsed() >= SPINNER_TICK {
            app.tick();
            last_spinner_tick = Instant::now();
        }

        terminal
            .draw(|frame| app.draw(frame))
            .map_err(|error| format!("draw failed: {error}"))?;
        std::thread::sleep(INTERRUPT_POLL.min(SPINNER_TICK));
    }
}
