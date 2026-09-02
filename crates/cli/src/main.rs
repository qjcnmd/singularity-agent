//! `singularity` 入口：无参数进入长驻交互式 TUI；`--print`/`--json` 进行单次无交互
//! 执行。三种入口共享同一个 `Conversation` 协调器与 Agent 执行边界——参数
//! 适配、输入控制与渲染之外不存在第二份 turn 循环、重试策略、压缩调用或
//! 会话写者；差异只在各自的事件投影与 stdout 合同。
//!
//! 进程结果由 [`ProcessOutcome`] 单点分类：completed=0、interrupted=130、
//! 失败=1，且准备失败、Agent 执行失败、终态化失败与输出通道失败各自拥有
//! 可区分的报告文本。`--json` 的每条路径在终态形态可能时恰好输出一条可
//! 解析 summary 行；Thread 未解析的失败不伪造 Thread 事实。

use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use clap::Parser;
use singularity_runtime::events::TurnEvent;
use singularity_runtime::objects::TurnStatus;
use singularity_runtime::{Conversation, ConversationError, TurnOutcome, TurnRunError};

mod jsonl_mode;
mod print_mode;
mod session_options;
mod signal;
mod tui;

use jsonl_mode::JsonlRenderer;
use print_mode::PrintRenderer;
use session_options::SessionSetup;

#[cfg(test)]
#[path = "../tests/headless_support.rs"]
mod headless_support;

#[cfg(test)]
#[path = "../tests/output_failures_tests.rs"]
mod output_failures_tests;

const INTERRUPT_POLL: Duration = Duration::from_millis(100);

/// 无交互执行模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Print,
    Json,
}

/// 第二次 Ctrl+C 的强制退出码（与优雅中断共用 130 语义）。
const FORCE_EXIT_CODE: i32 = 130;

/// 命令行程序名的唯一事实源：它由 `[[bin]] name` 决定，clap 属性与所有面向用户的
/// 消息都从这里取，改名只需改 `Cargo.toml` 一处。
pub(crate) const PROGRAM_NAME: &str = env!("CARGO_BIN_NAME");

#[derive(Debug, Parser)]
#[command(name = PROGRAM_NAME, about = "Singularity coding agent")]
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

/// 精确进程结果：哪个阶段失败、如何报告、以什么退出码收敛，由此单点分类；
/// 两种无交互入口共用同一分类器，渲染差异不改变进程语义。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProcessOutcome {
    /// completed 终态且输出投影写入成功。
    Completed,
    /// interrupted 终态且输出投影写入成功（或本无 stdout 内容）。
    Interrupted,
    /// Agent 执行失败：可信失败终态已落盘（`--json` 的 failed summary 已投影）。
    TurnFailed(String),
    /// 准备阶段失败：不存在 turn 痕迹；Thread 未解析时 summary 不伪造 thread 事实。
    Preparation(String),
    /// 终态化失败：终态记录无法落盘，不存在可信终态（区别于执行失败）。
    Terminalization(String),
    /// 输出通道失败：执行事实不受影响，但 stdout 投影不完整——绝不以成功报告。
    Output(String),
    /// 进程内异常（turn worker 终态前退出或 panic）。
    Internal(String),
    /// 参数使用错误：执行开始之前失败，无 summary。
    Usage(String),
    /// 交互 TUI 按终端生命周期自行退出。
    Interactive(i32),
    /// 交互模式缺少真实终端：既有退出码语义（2），又携带 stderr 报告。
    NoTerminal(String),
}

impl ProcessOutcome {
    fn exit_code(&self) -> i32 {
        match self {
            Self::Completed => 0,
            Self::Interrupted | Self::Internal(_) => 130,
            Self::Interactive(code) => *code,
            Self::NoTerminal(_) => 2,
            Self::TurnFailed(_)
            | Self::Preparation(_)
            | Self::Terminalization(_)
            | Self::Output(_)
            | Self::Usage(_) => 1,
        }
    }

    /// 需要写入 stderr 的失败报告；成功/interrupted 终态由事件流或文本输出
    /// 自身表达，不再重复报告。
    fn stderr_message(&self) -> Option<&str> {
        match self {
            Self::TurnFailed(message)
            | Self::Preparation(message)
            | Self::Terminalization(message)
            | Self::Output(message)
            | Self::Internal(message)
            | Self::Usage(message)
            | Self::NoTerminal(message) => Some(message),
            Self::Completed | Self::Interrupted | Self::Interactive(_) => None,
        }
    }
}

fn main() {
    let outcome = run(Cli::parse());
    if let Some(message) = outcome.stderr_message() {
        eprintln!("{PROGRAM_NAME}: {message}");
    }
    std::process::exit(outcome.exit_code());
}

fn run(cli: Cli) -> ProcessOutcome {
    let mode = match cli.mode() {
        Ok(mode) => mode,
        Err(message) => return ProcessOutcome::Usage(message),
    };
    if let Err(error) = singularity_runtime::ensure_bash_available() {
        let outcome = ProcessOutcome::Preparation(error);
        if mode == Some(Mode::Json) {
            // Thread 尚未解析：summary 省略 thread 事实。
            if let Err(summary_error) = emit_threadless_failed_summary() {
                return ProcessOutcome::Output(format!(
                    "failed to write preparation summary to stdout: {summary_error}"
                ));
            }
        }
        return outcome;
    }
    let Some(mode) = mode else {
        if let Err(message) = tui::ensure_terminal() {
            return ProcessOutcome::NoTerminal(message);
        }
        return match session_options::prepare(
            cli.model.as_deref(),
            cli.session.as_deref(),
            cli.no_session,
        ) {
            Ok(setup) => ProcessOutcome::Interactive(tui::run(setup.conversation)),
            Err(error) => ProcessOutcome::Preparation(error.message),
        };
    };

    let Some(goal) = cli.goal.clone() else {
        return ProcessOutcome::Usage(format!(
            "a goal is required: {PROGRAM_NAME} --print <goal> | {PROGRAM_NAME} --json <goal>"
        ));
    };
    let setup = match session_options::prepare(
        cli.model.as_deref(),
        cli.session.as_deref(),
        cli.no_session,
    ) {
        Ok(setup) => setup,
        Err(error) => {
            let outcome = ProcessOutcome::Preparation(error.message);
            if mode == Mode::Json {
                // 准备阶段失败也必须以终态形态收尾：--json 输出 failed summary
                // 行（thread 未解析，省略 thread 事实），机器解析方总能看到终态。
                if let Err(summary_error) = emit_threadless_failed_summary() {
                    return ProcessOutcome::Output(format!(
                        "failed to write preparation summary to stdout: {summary_error}"
                    ));
                }
            }
            return outcome;
        }
    };
    if let Err(message) = signal::ensure_installed() {
        let outcome = ProcessOutcome::Preparation(message.to_string());
        if mode == Mode::Json
            && let Err(summary_error) = emit_threadless_failed_summary()
        {
            return ProcessOutcome::Output(format!(
                "failed to write preparation summary to stdout: {summary_error}"
            ));
        }
        return outcome;
    }
    run_headless(setup, goal, mode)
}

/// Thread 尚未解析的失败终态 summary（`--json` 准备阶段失败出口）：
/// `thread` 事实整体省略，不写伪造哨兵值；summary 自身写失败已无从报告。
fn emit_threadless_failed_summary() -> Result<(), String> {
    let mut renderer = JsonlRenderer::stdout(None);
    renderer.emit_summary(TurnStatus::Failed, None, false)
}

/// worker 线程送回的消息：实时事件与终局结果共用同一通道。
enum WorkerMessage {
    Event(TurnEvent),
    Done(HeadlessDone),
}

/// `Conversation::run_turn` 的四种收敛形态：`Ok` 恒为可信终态；
/// `Err` 细分到与 [`TurnRunError`] 一致的类别，CLI 不从事件重建终态事实。
enum HeadlessDone {
    Turn(Box<TurnOutcome>),
    Preparation { message: String },
    Terminalization { message: String },
    Aborted { message: String },
}

/// `--print` 与 `--json` 的共享执行 seam 入口：装配生产 writer 的 view 后
/// 交给 [`execute_headless`]（测试注入自有 view/writer）。`setup` 的临时
/// home 与 tokio runtime 守卫贯穿执行。
fn run_headless(setup: SessionSetup, goal: String, mode: Mode) -> ProcessOutcome {
    let view = match mode {
        Mode::Print => HeadlessView::Print(PrintRenderer::stdout()),
        Mode::Json => HeadlessView::Json(JsonlRenderer::stdout(Some(setup.thread_id))),
    };
    execute_headless(setup.conversation, goal, view)
}

/// `--print` 与 `--json` 的共享执行 seam：与 TUI 同一个 `Conversation`
/// 协调器、同一条 `run_turn → TurnRunner → Agent` 路径；差异只在 view 的
/// 投影。主循环只做两件事：转发事件给 view、观察 Ctrl+C（第一次优雅中断，
/// 第二次强制退出）。
fn execute_headless(
    conversation: Arc<Conversation>,
    goal: String,
    mut view: HeadlessView,
) -> ProcessOutcome {
    let (progress_tx, progress_rx) = mpsc::channel::<WorkerMessage>();
    let worker_conversation = Arc::clone(&conversation);
    signal::reset();
    let worker = std::thread::spawn(move || {
        let done = {
            let mut sink = |event| {
                let _ = progress_tx.send(WorkerMessage::Event(event));
            };
            match worker_conversation.run_turn(&goal, &mut sink) {
                Ok(outcome) => HeadlessDone::Turn(Box::new(outcome)),
                Err(ConversationError::Turn(TurnRunError::Preparation { message, .. })) => {
                    HeadlessDone::Preparation { message }
                }
                Err(ConversationError::Turn(TurnRunError::Terminalization(failure))) => {
                    HeadlessDone::Terminalization {
                        message: format!("terminalization failed: {failure:?}"),
                    }
                }
                Err(error) => HeadlessDone::Aborted {
                    message: error.to_string(),
                },
            }
            // sink 在这里 drop：事件通道随执行收敛而关闭。
        };
        // 主循环可能已提前退出（强制退出路径），发送失败无需报告。
        let _ = progress_tx.send(WorkerMessage::Done(done));
    });
    let drained = drain_headless(&conversation, &mut view, &progress_rx);
    let outcome = finish_headless(&mut view, drained);
    let _ = worker.join();
    outcome
}

/// 事件泵 + Ctrl+C 观察循环的终局：`Drained` 携带 worker 的 `HeadlessDone`，
/// 通道断开（worker panic/终态前退出）按 `WorkerLost` 收敛。
enum DrainResult {
    Done(HeadlessDone),
    WorkerLost,
}

fn drain_headless(
    conversation: &Conversation,
    view: &mut HeadlessView,
    progress_rx: &mpsc::Receiver<WorkerMessage>,
) -> DrainResult {
    loop {
        match progress_rx.recv_timeout(INTERRUPT_POLL) {
            Ok(WorkerMessage::Event(event)) => view.on_event(&event),
            Ok(WorkerMessage::Done(done)) => return DrainResult::Done(done),
            Err(RecvTimeoutError::Timeout) => match signal::count() {
                0 => {}
                1 => {
                    eprintln!(
                        "{PROGRAM_NAME}: interrupting current turn (Ctrl+C again to force quit)"
                    );
                    conversation.interrupt();
                }
                // 第二次 Ctrl+C：用户明确要求强制退出；接受 turn 的 durable
                // 事实仍由写者守卫在进程死亡时收敛。
                _ => std::process::exit(FORCE_EXIT_CODE),
            },
            Err(RecvTimeoutError::Disconnected) => return DrainResult::WorkerLost,
        }
    }
}

/// 无交互投影视图：print 与 json 共享同一事件流与同一终态分类，各自只
/// 实现自己的渲染合同。
enum HeadlessView {
    Print(PrintRenderer),
    Json(JsonlRenderer),
}

impl HeadlessView {
    fn on_event(&mut self, event: &TurnEvent) {
        match self {
            Self::Print(renderer) => renderer.on_event(event),
            Self::Json(renderer) => renderer.on_event(event),
        }
    }
}

/// 把终局结果收敛到精确进程结果。`--json` 在每种终局恰好写出一条 summary
/// （写失败降级为 Output 类别）；`--print` 只在非失败终态时写 stdout 文本。
fn finish_headless(view: &mut HeadlessView, drain: DrainResult) -> ProcessOutcome {
    let done = match drain {
        DrainResult::Done(done) => done,
        DrainResult::WorkerLost => HeadlessDone::Aborted {
            message: "turn worker exited before a terminal result".to_string(),
        },
    };
    match (view, done) {
        (HeadlessView::Print(renderer), HeadlessDone::Turn(outcome)) => match outcome.turn_status {
            TurnStatus::Completed | TurnStatus::Interrupted => {
                if outcome.truncated {
                    renderer.warn_truncated();
                }
                // stdout 合同：只有非空最终文本进入 stdout；中断且无文本时
                // 不写任何内容。
                let write = if outcome.final_text.is_empty() {
                    Ok(())
                } else {
                    renderer.write_final_text(outcome.final_text.trim_end())
                };
                match write {
                    Ok(()) if outcome.turn_status == TurnStatus::Completed => {
                        ProcessOutcome::Completed
                    }
                    Ok(()) => ProcessOutcome::Interrupted,
                    Err(message) => ProcessOutcome::Output(format!(
                        "failed to write result to stdout: {message}"
                    )),
                }
            }
            TurnStatus::Failed => ProcessOutcome::TurnFailed(turn_failed_message(&outcome)),
            // 协调器合同：run_turn 的 Ok 终态恒为终态状态；running 不可达。
            TurnStatus::Running => ProcessOutcome::Internal(
                "coordinator returned a non-terminal turn outcome".to_string(),
            ),
        },
        (HeadlessView::Json(renderer), done) => {
            let (status, usage, truncated) = match &done {
                HeadlessDone::Turn(outcome) => (
                    outcome.turn_status,
                    Some(outcome.usage.clone()),
                    outcome.truncated,
                ),
                HeadlessDone::Preparation { .. }
                | HeadlessDone::Terminalization { .. }
                | HeadlessDone::Aborted { .. } => (TurnStatus::Failed, None, false),
            };
            let summary_result = renderer.emit_summary(status, usage, truncated);
            if let Some(message) = renderer.output_failure() {
                return ProcessOutcome::Output(format!(
                    "failed to write JSON output to stdout: {message}"
                ));
            }
            if let Err(message) = summary_result {
                return ProcessOutcome::Output(format!(
                    "failed to write summary to stdout: {message}"
                ));
            }
            match done {
                HeadlessDone::Turn(outcome) => match outcome.turn_status {
                    TurnStatus::Completed => ProcessOutcome::Completed,
                    TurnStatus::Interrupted => ProcessOutcome::Interrupted,
                    TurnStatus::Failed => ProcessOutcome::TurnFailed(turn_failed_message(&outcome)),
                    TurnStatus::Running => ProcessOutcome::Internal(
                        "coordinator returned a non-terminal turn outcome".to_string(),
                    ),
                },
                HeadlessDone::Preparation { message } => ProcessOutcome::Preparation(message),
                HeadlessDone::Terminalization { message } => {
                    ProcessOutcome::Terminalization(message)
                }
                HeadlessDone::Aborted { message } => ProcessOutcome::Internal(message),
            }
        }
        // print 的非 Turn 终局不写 stdout；失败类别各自保留报告文本。
        (HeadlessView::Print(_), HeadlessDone::Preparation { message }) => {
            ProcessOutcome::Preparation(message)
        }
        (HeadlessView::Print(_), HeadlessDone::Terminalization { message }) => {
            ProcessOutcome::Terminalization(message)
        }
        (HeadlessView::Print(_), HeadlessDone::Aborted { message }) => {
            ProcessOutcome::Internal(message)
        }
    }
}

/// 可信失败终态的 stderr 报告文本：与已发布的 `turn/error` 事件同源
/// （`TurnOutcome.error`），不重建第二份事实。
fn turn_failed_message(outcome: &TurnOutcome) -> String {
    match &outcome.error {
        Some(error) => format!(
            "turn failed [{}]: {} ({})",
            error.stage, error.message, error.cause
        ),
        None => "turn failed (no error detail)".to_string(),
    }
}
