//! 开发期 Evaluation 命令行入口。

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use singularity_core::CancellationToken;
use singularity_evaluation::runner::{
    EvaluationRunParams, EvaluationRunResult, run_evaluation_with_selection,
};
use singularity_evaluation::{EvaluationSelection, TaskId};
use singularity_model::ProviderConfigSnapshot;
use singularity_sandbox::PlatformSandboxBackend;
use singularity_store::SessionStore;

#[derive(Debug, Parser)]
#[command(name = "singularity-evaluation")]
#[command(about = "Development evaluator for the Singularity agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate and run an Evaluation manifest.
    Run {
        manifest: PathBuf,
        #[arg(long)]
        run_id: String,
        #[arg(long)]
        json: bool,
        /// Maximum number of independent tasks to execute concurrently (1-2).
        #[arg(long, default_value_t = 1, value_parser = parse_max_workers)]
        max_workers: usize,
        /// Select one task for an Evaluation-only diagnostic run (must be paired with --trial).
        #[arg(long, value_parser = parse_task_id, requires = "trial")]
        task_id: Option<TaskId>,
        /// Select the manifest's one-based trial ordinal (must be paired with --task-id).
        #[arg(long, requires = "task_id")]
        trial: Option<u32>,
    },
}

fn parse_max_workers(value: &str) -> Result<usize, String> {
    let workers = value
        .parse::<usize>()
        .map_err(|_| "max-workers must be an integer between 1 and 2".to_string())?;
    if (1..=2).contains(&workers) {
        Ok(workers)
    } else {
        Err("max-workers must be between 1 and 2".to_string())
    }
}

fn parse_task_id(value: &str) -> Result<TaskId, String> {
    TaskId::new(value.to_string())
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Run {
            manifest,
            run_id,
            json,
            max_workers,
            task_id,
            trial,
        } => run_manifest(manifest, run_id, json, max_workers, task_id, trial),
    }
}

fn run_manifest(
    manifest: PathBuf,
    run_id: String,
    json_output: bool,
    max_workers: usize,
    task_id: Option<TaskId>,
    trial: Option<u32>,
) -> Result<(), String> {
    let selection = match (task_id, trial) {
        (Some(task_id), Some(trial)) => Some(
            EvaluationSelection::new(task_id, trial)
                .map_err(|error| format!("invalid diagnostic selection: {error}"))?,
        ),
        (None, None) => None,
        _ => {
            return Err("--task-id and --trial must be provided together".to_string());
        }
    };
    if !manifest.is_file() {
        return Err(format!(
            "evaluation manifest not found: {}",
            manifest.display()
        ));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("failed to build provider runtime: {error}"))?;
    let provider_snapshot = ProviderConfigSnapshot::capture_with_runtime_handle(
        |name| std::env::var(name).ok(),
        runtime.handle().clone(),
    );
    let mut trace_store = SessionStore::open(":memory:")
        .map_err(|error| format!("failed to open evaluation trace store: {error}"))?;
    let result = run_evaluation_with_selection(
        &EvaluationRunParams {
            manifest: manifest.to_string_lossy().into_owned(),
            run_id,
            output_root: std::env::var("SINGULARITY_EVAL_OUTPUT_DIR").ok(),
            max_workers,
        },
        Arc::new(PlatformSandboxBackend::new()),
        &provider_snapshot,
        &CancellationToken::new(),
        &mut trace_store,
        selection,
    )
    .map_err(|error| {
        if let Some(partial) = error.partial_result() {
            let _ = print_result(partial, json_output);
        }
        error.to_string()
    })?;
    print_result(&result, json_output)?;
    if result.diagnostic_passed.unwrap_or(result.evaluation_passed) {
        Ok(())
    } else {
        Err(result
            .blocker
            .unwrap_or_else(|| "evaluation_failed".to_string()))
    }
}

fn print_result(result: &EvaluationRunResult, json_output: bool) -> Result<(), String> {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(result).map_err(|error| error.to_string())?
        );
    } else {
        println!("{}", result_text(result));
    }
    Ok(())
}

fn result_text(result: &EvaluationRunResult) -> String {
    if let Some(selection) = &result.selection {
        format!(
            "evaluation diagnostic {} {} task={} trial={} gate_applicable=false diagnostic_passed={} evaluation_passed=false runner={} max_workers={}",
            result.run_id,
            result.status,
            selection.task_id,
            selection.trial,
            result.diagnostic_passed.unwrap_or(false),
            result.runner,
            result.max_workers
        )
    } else {
        format!(
            "evaluation {} {} runner={} max_workers={}",
            result.run_id, result.status, result.runner, result.max_workers
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_workers_cli_defaults_to_one_and_accepts_two() {
        let cli = Cli::try_parse_from([
            "singularity-evaluation",
            "run",
            "manifest.json",
            "--run-id",
            "run",
        ])
        .expect("default max-workers parses");
        let Command::Run { max_workers, .. } = cli.command;
        assert_eq!(max_workers, 1);

        let cli = Cli::try_parse_from([
            "singularity-evaluation",
            "run",
            "manifest.json",
            "--run-id",
            "run",
            "--max-workers",
            "2",
        ])
        .expect("max-workers=2 parses");
        let Command::Run { max_workers, .. } = cli.command;
        assert_eq!(max_workers, 2);
    }

    #[test]
    fn max_workers_cli_rejects_zero_and_values_above_two() {
        for value in ["0", "3"] {
            let error = Cli::try_parse_from([
                "singularity-evaluation",
                "run",
                "manifest.json",
                "--run-id",
                "run",
                "--max-workers",
                value,
            ])
            .expect_err("invalid max-workers must be rejected");
            assert!(error.to_string().contains("max-workers"));
        }
    }

    #[test]
    fn diagnostic_selector_flags_are_typed_and_must_be_paired() {
        let cli = Cli::try_parse_from([
            "singularity-evaluation",
            "run",
            "manifest.json",
            "--run-id",
            "run",
            "--task-id",
            "task-a",
            "--trial",
            "2",
        ])
        .expect("paired diagnostic selector parses");
        let Command::Run { task_id, trial, .. } = cli.command;
        assert_eq!(task_id.expect("task id").as_str(), "task-a");
        assert_eq!(trial, Some(2));

        let cases: &[&[&str]] = &[
            &[
                "singularity-evaluation",
                "run",
                "manifest.json",
                "--run-id",
                "run",
                "--task-id",
                "task-a",
            ],
            &[
                "singularity-evaluation",
                "run",
                "manifest.json",
                "--run-id",
                "run",
                "--trial",
                "2",
            ],
        ];
        for args in cases {
            assert!(Cli::try_parse_from(*args).is_err());
        }
    }

    #[test]
    fn diagnostic_text_result_is_explicitly_non_gating() {
        let result = EvaluationRunResult {
            run_id: "run".to_string(),
            manifest: "manifest.json".to_string(),
            runner: "agent_loop".to_string(),
            max_workers: 1,
            status: "completed".to_string(),
            blocker: None,
            tasks: Vec::new(),
            result_path: None,
            report_path: None,
            evidence_path: None,
            evaluation_passed: false,
            gate_applicable: Some(false),
            selection: Some(
                EvaluationSelection::new(TaskId::new("task-a").expect("task id"), 2)
                    .expect("selection"),
            ),
            diagnostic_passed: Some(true),
        };
        let text = result_text(&result);
        assert!(text.contains("evaluation diagnostic"));
        assert!(text.contains("task=task-a trial=2"));
        assert!(text.contains("gate_applicable=false"));
        assert!(text.contains("diagnostic_passed=true"));
        assert!(text.contains("evaluation_passed=false"));
        assert_eq!(result.gate_applicable, Some(false));
    }
}
