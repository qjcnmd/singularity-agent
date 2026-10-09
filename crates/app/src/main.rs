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

fn main() {
    if let Err(message) = run(Arguments::parse()) {
        eprintln!("{PROGRAM_NAME}: {message}");
        std::process::exit(1);
    }
}

fn run(cli: Arguments) -> Result<(), String> {
    if !cli.json && !cli.app_server {
        return Err("请启动 Singularity 桌面应用；评估任务使用 --json。".into());
    }
    let (home, _data_lock) = match session_options::lock_data_directory() {
        Ok(lock) => lock,
        Err(error) => {
            // 评估入口要把准备失败写成 summary 行，桌面入口只报进程错误。
            return if cli.json {
                preparation_failure(error)
            } else {
                Err(error)
            };
        }
    };
    if !cli.json {
        let setup = session_options::prepare_desktop(&home)?;
        let runtime = Arc::clone(&setup.runtime);
        return runtime.block_on(desktop::run(setup));
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
    setup.runtime.block_on(async {
        let result = execute_headless(&setup.conversation, &goal, renderer).await;
        setup.mcp.shutdown().await;
        result
    })
}

fn preparation_failure(message: String) -> Result<(), String> {
    let mut renderer = JsonlRenderer::stdout(None);
    renderer.emit_summary(TurnStatus::Failed, None, false);
    with_output_failure(Err(message), renderer.output_failure())
}

async fn execute_headless(
    conversation: &Arc<Conversation>,
    goal: &str,
    mut renderer: JsonlRenderer,
) -> Result<(), String> {
    let result = conversation.run_turn(goal, &mut |event| renderer.on_event(&event)).await;
    let (status, usage, truncated) = match &result {
        Ok(outcome) => (outcome.turn_status, Some(outcome.usage.clone()), outcome.truncated),
        Err(_) => (TurnStatus::Failed, None, false),
    };
    renderer.emit_summary(status, usage, truncated);
    with_output_failure(classify_headless(result), renderer.output_failure())
}

fn classify_headless(result: Result<TurnOutcome, ConversationError>) -> Result<(), String> {
    match result {
        Ok(outcome) => match outcome.turn_status {
            TurnStatus::Completed => Ok(()),
            TurnStatus::Failed => Err(match outcome.error {
                Some(error) => format!("turn failed {error}"),
                None => "turn failed (no error detail)".to_string(),
            }),
            TurnStatus::Running | TurnStatus::Interrupted => {
                unreachable!("headless execution completes without a cancellation source")
            }
        },
        Err(error) => Err(error.to_string()),
    }
}

/// stdout 故障使进程失败；已失败的任务保留原原因，并附上输出错误。
fn with_output_failure(result: Result<(), String>, failure: Option<&str>) -> Result<(), String> {
    let Some(error) = failure else {
        return result;
    };
    Err(match result {
        Err(message) => format!("{message}; also failed to write stdout output: {error}"),
        Ok(()) => format!("failed to write JSON output to stdout: {error}"),
    })
}
