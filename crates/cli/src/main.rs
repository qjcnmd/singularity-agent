//! 默认启动本地 Web 工作台；--json 提供单次评估入口。
//! 两个入口共用 Conversation、Agent、会话持久化与模型执行。
//! 评估器负责进程超时与终止，本入口只输出执行事件和终态 summary。

use std::sync::Arc;

use clap::Parser;
use singularity_protocol::TurnStatus;
use singularity_runtime::{Conversation, ConversationError, TurnOutcome};

mod jsonl_mode;
mod session_options;
mod web;

use jsonl_mode::JsonlRenderer;

#[cfg(test)]
mod tests;

/// 命令行程序名来自 Cargo 的二进制目标名称。
pub(crate) const PROGRAM_NAME: &str = env!("CARGO_BIN_NAME");

#[derive(Debug, Parser)]
#[command(name = PROGRAM_NAME, about = "Singularity coding agent")]
struct Cli {
    /// 运行一次评估任务，输出 JSONL 事件和终态 summary。
    #[arg(long, requires = "goal")]
    json: bool,

    /// 评估任务的输入。
    #[arg(requires = "json")]
    goal: Option<String>,

    /// 本次评估使用的模型；省略时使用已配置的默认模型。
    #[arg(long, requires = "json")]
    model: Option<String>,

    /// 本地 Web 工作台监听端口；0 表示由系统选择空闲端口。
    #[arg(long, default_value_t = 3080, conflicts_with = "json")]
    port: u16,

    /// 启动 Web 工作台但不打开默认浏览器。
    #[arg(long, conflicts_with = "json")]
    no_open: bool,
}

/// 进程结果保留成功、失败和用户中断的退出码。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProcessOutcome {
    Completed,
    Interrupted,
    Failed(String),
}

impl ProcessOutcome {
    fn finish(&self) -> (i32, Option<&str>) {
        match self {
            Self::Completed => (0, None),
            Self::Interrupted => (130, None),
            Self::Failed(message) => (1, Some(message)),
        }
    }
}

fn main() {
    let outcome = run(Cli::parse());
    let (code, message) = outcome.finish();
    if let Some(message) = message {
        eprintln!("{PROGRAM_NAME}: {message}");
    }
    std::process::exit(code);
}

fn run(cli: Cli) -> ProcessOutcome {
    let _data_lock = match session_options::lock_data_directory() {
        Ok(lock) => lock,
        Err(error) => {
            return if cli.json {
                preparation_failure(error)
            } else {
                ProcessOutcome::Failed(error)
            };
        }
    };
    if !cli.json {
        let setup = match session_options::prepare_web() {
            Ok(setup) => setup,
            Err(error) => return ProcessOutcome::Failed(error),
        };
        let runtime = Arc::clone(&setup.runtime);
        return match runtime.block_on(web::run(setup, cli.port, cli.no_open)) {
            Ok(()) => ProcessOutcome::Completed,
            Err(message) => ProcessOutcome::Failed(message),
        };
    }
    if let Err(error) = singularity_runtime::ensure_bash_available() {
        return preparation_failure(error);
    }
    // clap 的 requires 约束保证 --json 必须携带目标。
    #[allow(clippy::expect_used)]
    let goal = cli.goal.expect("--json requires a goal");
    let setup = match session_options::prepare(cli.model.as_deref()) {
        Ok(setup) => setup,
        Err(error) => return preparation_failure(error),
    };
    let renderer = JsonlRenderer::stdout(Some(setup.thread_id.clone()));
    execute_headless(&setup.conversation, &goal, renderer)
}

fn preparation_failure(message: String) -> ProcessOutcome {
    let mut renderer = JsonlRenderer::stdout(None);
    renderer.emit_summary(TurnStatus::Failed, None, false);
    if let Some(error) = renderer.output_failure() {
        return ProcessOutcome::Failed(format!(
            "failed to write preparation summary to stdout: {error}"
        ));
    }
    ProcessOutcome::Failed(message)
}

/// 直接转发共享执行层的事件，不另建 worker 或事件队列。
fn execute_headless(
    conversation: &Arc<Conversation>,
    goal: &str,
    mut renderer: JsonlRenderer,
) -> ProcessOutcome {
    let result = conversation.run_turn(goal, &mut |event| renderer.on_event(&event));
    let (status, usage, truncated) = match &result {
        Ok(outcome) => (
            outcome.turn_status,
            Some(outcome.usage.clone()),
            outcome.truncated,
        ),
        Err(_) => (TurnStatus::Failed, None, false),
    };
    renderer.emit_summary(status, usage, truncated);
    if let Some(message) = renderer.output_failure() {
        return ProcessOutcome::Failed(format!("failed to write JSON output to stdout: {message}"));
    }
    classify_headless(result)
}

fn classify_headless(result: Result<TurnOutcome, ConversationError>) -> ProcessOutcome {
    match result {
        Ok(outcome) => match outcome.turn_status {
            TurnStatus::Completed => ProcessOutcome::Completed,
            TurnStatus::Interrupted => ProcessOutcome::Interrupted,
            TurnStatus::Failed => ProcessOutcome::Failed(turn_failed_message(&outcome)),
            TurnStatus::Running => ProcessOutcome::Failed(
                "coordinator returned a non-terminal turn outcome".to_string(),
            ),
        },
        Err(error) => ProcessOutcome::Failed(error.to_string()),
    }
}

/// 失败报告与已发布的 turn/error 事件同源。
fn turn_failed_message(outcome: &TurnOutcome) -> String {
    match &outcome.error {
        Some(error) => format!("turn failed {error}"),
        None => "turn failed (no error detail)".to_string(),
    }
}
