//! `sg` 入口：无参数进入长驻交互式 TUI；`--print`/`--json` 进行单次无交互
//! 执行。两种入口在进程内直接复用同一个 Agent/Session/Provider 运行时。

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use clap::Parser;
use singularity_runtime::events::{TurnEvent, TurnEventSink};
use singularity_runtime::objects::TurnStatus;

mod jsonl_mode;
mod print_mode;
mod session_options;
mod signal;
mod tui;

use jsonl_mode::{JsonlRenderer, exit_code_for};
use session_options::SessionSetup;

/// 无交互执行模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Print,
    Json,
}

/// 第二次 Ctrl+C 的强制退出码（与优雅中断共用 130 语义）。
const FORCE_EXIT_CODE: i32 = 130;
const INTERRUPT_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Parser)]
#[command(name = "sg", about = "Singularity coding agent")]
struct Cli {
    /// 只运行一次，仅打印最终 assistant 文本。
    #[arg(long)]
    print: bool,

    /// 只运行一次，流式输出 JSONL 事件并带终态 summary 行。
    #[arg(long)]
    json: bool,

    /// 无交互模式的目标（与 --print/--json 一起必需）。
    goal: Option<String>,

    /// 仅本次执行覆盖模型选择。
    #[arg(long)]
    model: Option<String>,

    /// 按 id 恢复既有 thread。
    #[arg(long)]
    session: Option<String>,

    /// 本次执行禁用持久化。
    #[arg(long, conflicts_with = "session")]
    no_session: bool,
}

impl Cli {
    fn mode(&self) -> Result<Option<Mode>, String> {
        match (self.print, self.json) {
            (true, true) => Err("--print and --json are mutually exclusive".to_string()),
            (true, false) => Ok(Some(Mode::Print)),
            (false, true) => Ok(Some(Mode::Json)),
            (false, false) => {
                if self.goal.is_some() {
                    return Err(
                        "a positional goal is only valid together with --print or --json"
                            .to_string(),
                    );
                }
                Ok(None)
            }
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let exit_code = match run(cli) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("sg: {message}");
            1
        }
    };
    std::process::exit(exit_code);
}

fn run(cli: Cli) -> Result<i32, String> {
    let mode = cli.mode()?;
    if let Err(error) = singularity_runtime::ensure_bash_available() {
        if mode == Some(Mode::Json) {
            emit_failed_json_summary(&fallback_thread_id(cli.session.as_deref()));
        }
        return Err(error);
    }
    let Some(mode) = mode else {
        if let Err(message) = tui::ensure_terminal() {
            eprintln!("sg: {message}");
            return Ok(2);
        }
        let setup =
            session_options::prepare(cli.model.as_deref(), cli.session.as_deref(), cli.no_session)?;
        return Ok(tui::run(setup.conversation).exit_code);
    };

    let goal = cli
        .goal
        .clone()
        .ok_or_else(|| "a goal is required: sg --print <goal> | sg --json <goal>".to_string())?;
    let setup = match session_options::prepare(
        cli.model.as_deref(),
        cli.session.as_deref(),
        cli.no_session,
    ) {
        Ok(setup) => setup,
        Err(error) => {
            // 准备阶段失败也必须有终态形态：--json 输出 failed summary 行，
            // 保证机器解析方总能看到终态；--print 只向 stderr 报告。
            if mode == Mode::Json {
                emit_failed_json_summary(&fallback_thread_id(cli.session.as_deref()));
            }
            return Err(error.message);
        }
    };
    signal::ensure_installed().map_err(|message| message.to_string())?;
    run_headless(setup, &goal, mode)
}

fn fallback_thread_id(session: Option<&str>) -> String {
    session.unwrap_or("unresolved").to_string()
}

/// `--json` 失败路径的统一终态形态：机器解析方必须总能看到 failed summary
/// 行（评估器依赖此契约），故准备/执行/工作线程中断各路径共用这一个出口。
fn emit_failed_json_summary(thread_id: &str) {
    JsonlRenderer::new(thread_id).emit_summary(TurnStatus::Failed, None, false);
}

/// turn 执行线程向主循环投递的进度。
enum TurnProgress {
    Event(TurnEvent),
    Done(Result<singularity_runtime::TurnOutcome, String>),
}

fn run_headless(setup: SessionSetup, goal: &str, mode: Mode) -> Result<i32, String> {
    signal::reset();
    let conversation = Arc::clone(&setup.conversation);
    let (progress_tx, progress_rx) = mpsc::channel::<TurnProgress>();
    let event_tx = progress_tx.clone();
    let goal = goal.to_string();
    let worker = std::thread::spawn(move || {
        struct Forward {
            tx: mpsc::Sender<TurnProgress>,
        }
        impl TurnEventSink for Forward {
            fn emit(&mut self, event: TurnEvent) {
                let _ = self.tx.send(TurnProgress::Event(event));
            }
        }
        let mut sink = Forward { tx: event_tx };
        let result = conversation.run_turn(&goal, &mut sink);
        drop(sink);
        let _ = progress_tx.send(TurnProgress::Done(match result {
            Ok(outcome) => Ok(outcome),
            Err(error) => Err(error.to_string()),
        }));
    });

    match mode {
        Mode::Print => drain_print(&setup, progress_rx, worker),
        Mode::Json => drain_json(setup, progress_rx, worker),
    }
}

/// 主循环：渲染事件、轮询 Ctrl+C 计数并驱动两级取消语义。
fn drain_loop(
    conversation: &Arc<singularity_runtime::Conversation>,
    progress_rx: mpsc::Receiver<TurnProgress>,
    mut on_event: impl FnMut(&TurnEvent),
) -> Result<singularity_runtime::TurnOutcome, String> {
    let mut interrupted = false;
    loop {
        match progress_rx.recv_timeout(INTERRUPT_POLL) {
            Ok(TurnProgress::Event(event)) => on_event(&event),
            Ok(TurnProgress::Done(result)) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => match signal::count() {
                count if count >= 2 => {
                    // 第二次 Ctrl+C：强制退出，不再等待排空。
                    std::process::exit(FORCE_EXIT_CODE);
                }
                count if count == 1 && !interrupted => {
                    interrupted = true;
                    conversation.interrupt();
                }
                _ => {}
            },
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // 执行线程意外消失且未发送结果：按失败处理，不留悬空流。
                return Err("turn worker exited without a terminal result".to_string());
            }
        }
    }
}

fn drain_print(
    setup: &SessionSetup,
    progress_rx: mpsc::Receiver<TurnProgress>,
    worker: std::thread::JoinHandle<()>,
) -> Result<i32, String> {
    let mut renderer = print_mode::PrintRenderer::new();
    let conversation = Arc::clone(&setup.conversation);
    let outcome = drain_loop(&conversation, progress_rx, |event| renderer.emit(event));
    let _ = worker.join();
    match outcome {
        Ok(view) => {
            if view.turn_status == TurnStatus::Completed {
                renderer.write_final_text(view.final_text.trim_end());
                if view.truncated {
                    renderer.warn_truncated();
                }
            }
            Ok(exit_code_for(view.turn_status))
        }
        Err(message) => Err(message),
    }
}

fn drain_json(
    setup: SessionSetup,
    progress_rx: mpsc::Receiver<TurnProgress>,
    worker: std::thread::JoinHandle<()>,
) -> Result<i32, String> {
    let mut renderer = JsonlRenderer::new(setup.thread_id.clone());
    let conversation = Arc::clone(&setup.conversation);
    let outcome = drain_loop(&conversation, progress_rx, |event| renderer.emit(event));
    let _ = worker.join();
    match outcome {
        Ok(outcome) => {
            let usage = serde_json::to_value(outcome.usage).unwrap_or(serde_json::Value::Null);
            renderer.emit_summary(outcome.turn_status, Some(usage), outcome.truncated);
            Ok(exit_code_for(outcome.turn_status))
        }
        Err(message) => {
            // 失败也必须以终态 summary 收尾，保证机器解析总能看到终态行。
            emit_failed_json_summary(&setup.thread_id);
            Err(message)
        }
    }
}
