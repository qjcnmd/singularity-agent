//! Electron 的 stdio AppServer 与 --json 评估入口共用 Conversation、Agent、
//! 会话持久化和模型执行；评估任务的超时与终止由评估器负责，评估入口输出事件与终态 summary。

use std::sync::Arc;

use clap::Parser;
use singularity_protocol::TurnStatus;
use singularity_runtime::{Conversation, ConversationError, TurnOutcome};

mod desktop;
mod jsonl_mode;
mod session_options;

use jsonl_mode::JsonlRenderer;

#[cfg(test)]
mod tests;

pub(crate) const PROGRAM_NAME: &str = env!("CARGO_BIN_NAME");

#[derive(Debug, Parser)]
#[command(name = PROGRAM_NAME, about = "Singularity coding agent")]
struct Arguments {
    /// 运行一次评估任务，输出 JSONL 事件和终态 summary。
    #[arg(long, requires = "goal")]
    json: bool,

    /// 评估任务的输入。
    #[arg(requires = "json")]
    goal: Option<String>,

    /// 本次评估使用的模型；省略时使用已配置的默认模型。
    #[arg(long, requires = "json")]
    model: Option<String>,

    /// Electron 管理的标准输入/输出 RPC 服务。
    #[arg(long, conflicts_with = "json")]
    app_server: bool,
}

/// 无交互执行返回成功或失败；进程终止由调用方负责。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProcessOutcome {
    Completed,
    Failed(String),
}

impl ProcessOutcome {
    fn finish(&self) -> (i32, Option<&str>) {
        match self {
            Self::Completed => (0, None),
            Self::Failed(message) => (1, Some(message)),
        }
    }

    /// 把 stdout 输出故障并进任务结果，让进程只有一个出口：任务（或准备）原因
    /// 和输出故障各自留档，互不覆盖。任务本来就失败时，原因是主、输出故障在后；
    /// 任务成功时，输出故障单独让进程失败（执行事实已经落盘，不改任务的终态）；
    fn with_output_failure(self, failure: Option<&str>) -> Self {
        let Some(error) = failure else {
            return self;
        };
        match self {
            Self::Failed(message) => Self::Failed(format!(
                "{message}; also failed to write stdout output: {error}"
            )),
            Self::Completed => {
                Self::Failed(format!("failed to write JSON output to stdout: {error}"))
            }
        }
    }
}

fn main() {
    let outcome = run(Arguments::parse());
    let (code, message) = outcome.finish();
    if let Some(message) = message {
        eprintln!("{PROGRAM_NAME}: {message}");
    }
    std::process::exit(code);
}

fn run(cli: Arguments) -> ProcessOutcome {
    if !cli.json && !cli.app_server {
        return ProcessOutcome::Failed("请启动 Singularity 桌面应用；评估任务使用 --json。".into());
    }
    let (home, _data_lock) = match session_options::lock_data_directory() {
        Ok(lock) => lock,
        Err(error) => {
            // 评估入口要把准备失败写成 summary 行，桌面入口只报进程错误。
            return if cli.json {
                preparation_failure(error)
            } else {
                ProcessOutcome::Failed(error)
            };
        }
    };
    if !cli.json {
        let setup = match session_options::prepare_desktop(&home) {
            Ok(setup) => setup,
            Err(error) => return ProcessOutcome::Failed(error),
        };
        let runtime = Arc::clone(&setup.runtime);
        return match runtime.block_on(desktop::run(setup)) {
            Ok(()) => ProcessOutcome::Completed,
            Err(message) => ProcessOutcome::Failed(message),
        };
    }
    if let Err(error) = singularity_runtime::ensure_bash_available() {
        return preparation_failure(error);
    }
    // clap 的 requires 约束保证带 --json 时一定带着目标。
    let goal = cli.goal.expect("--json requires a goal");
    let setup = match session_options::prepare(&home, cli.model.as_deref()) {
        Ok(setup) => setup,
        Err(error) => return preparation_failure(error),
    };
    let renderer = JsonlRenderer::stdout(Some(setup.conversation.thread().thread_id));
    setup
        .runtime
        .block_on(execute_headless(&setup.conversation, &goal, renderer))
}

fn preparation_failure(message: String) -> ProcessOutcome {
    let mut renderer = JsonlRenderer::stdout(None);
    renderer.emit_summary(TurnStatus::Failed, None, false);
    ProcessOutcome::Failed(message).with_output_failure(renderer.output_failure())
}

/// 直接转发共享执行层的事件，不另外建 worker 或事件队列。
async fn execute_headless(
    conversation: &Arc<Conversation>,
    goal: &str,
    mut renderer: JsonlRenderer,
) -> ProcessOutcome {
    let result = conversation
        .run_turn(goal, &mut |event| renderer.on_event(&event))
        .await;
    let (status, usage, truncated) = match &result {
        Ok(outcome) => (
            outcome.turn_status,
            Some(outcome.usage.clone()),
            outcome.truncated,
        ),
        Err(_) => (TurnStatus::Failed, None, false),
    };
    renderer.emit_summary(status, usage, truncated);
    classify_headless(result).with_output_failure(renderer.output_failure())
}

fn classify_headless(result: Result<TurnOutcome, ConversationError>) -> ProcessOutcome {
    match result {
        Ok(outcome) => match outcome.turn_status {
            TurnStatus::Completed => ProcessOutcome::Completed,
            TurnStatus::Failed => ProcessOutcome::Failed(match outcome.error {
                Some(error) => format!("turn failed {error}"),
                None => "turn failed (no error detail)".to_string(),
            }),
            TurnStatus::Running | TurnStatus::Interrupted => {
                unreachable!("headless execution completes without a cancellation source")
            }
        },
        Err(error) => ProcessOutcome::Failed(error.to_string()),
    }
}
