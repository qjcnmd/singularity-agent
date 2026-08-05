//! 开发期 Evaluation 命令行入口。

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use singularity_core::CancellationToken;
use singularity_evaluation::runner::{
    EvaluationRunMode, EvaluationRunParams, EvaluationRunResult, run_evaluation_with_mode,
};
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
        #[arg(long, value_parser = parse_max_workers)]
        max_workers: Option<usize>,
        /// Execute every manifest task for its configured trial count and publish gate artifacts.
        #[arg(long)]
        full: bool,
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

/// Resolve the task worker count, capping Full at two and falling back to one on query failure.
fn resolve_max_workers<F>(
    mode: &EvaluationRunMode,
    requested: Option<usize>,
    available_parallelism: F,
) -> usize
where
    F: FnOnce() -> std::io::Result<NonZeroUsize>,
{
    if let Some(requested) = requested {
        return requested;
    }

    match mode {
        EvaluationRunMode::Full => available_parallelism()
            .map(|parallelism| if parallelism.get() >= 2 { 2 } else { 1 })
            .unwrap_or(1),
        EvaluationRunMode::Feedback => 1,
    }
}

fn default_max_workers(mode: &EvaluationRunMode, requested: Option<usize>) -> usize {
    resolve_max_workers(mode, requested, std::thread::available_parallelism)
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
            full,
        } => run_manifest(manifest, run_id, json, max_workers, full),
    }
}

fn run_mode_from_flags(full: bool) -> EvaluationRunMode {
    if full {
        EvaluationRunMode::Full
    } else {
        EvaluationRunMode::Feedback
    }
}

fn run_manifest(
    manifest: PathBuf,
    run_id: String,
    json_output: bool,
    requested_max_workers: Option<usize>,
    full: bool,
) -> Result<(), String> {
    let mode = run_mode_from_flags(full);
    let max_workers = default_max_workers(&mode, requested_max_workers);
    if !manifest.is_file() {
        return Err(format!(
            "evaluation manifest not found: {}",
            manifest.display()
        ));
    }
    let provider_snapshot =
        ProviderConfigSnapshot::capture(|name| std::env::var(name).ok(), None, None);
    let mut trace_store = SessionStore::open(":memory:")
        .map_err(|error| format!("failed to open evaluation trace store: {error}"))?;
    let result = run_evaluation_with_mode(
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
        mode,
    )
    .map_err(|error| {
        if let Some(partial) = error.partial_result() {
            let _ = print_result(partial, json_output);
        }
        error.to_string()
    })?;
    print_result(&result, json_output)?;
    if !result.gate_applicable {
        // Feedback is observational and non-gating; a blocked run is still an execution error.
        if result.status != "blocked" {
            return Ok(());
        }
    } else if result.evaluation_passed {
        return Ok(());
    } else {
        return Err(result
            .blocker
            .unwrap_or_else(|| "evaluation_failed".to_string()));
    }
    Err(result
        .blocker
        .unwrap_or_else(|| "evaluation_failed".to_string()))
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
    format!(
        "evaluation {} {} gate_applicable={} runner={} max_workers={}",
        result.run_id, result.status, result.gate_applicable, result.runner, result.max_workers
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_workers_cli_distinguishes_omitted_and_explicit_values() {
        let cli = Cli::try_parse_from([
            "singularity-evaluation",
            "run",
            "manifest.json",
            "--run-id",
            "run",
        ])
        .expect("default max-workers parses");
        let Command::Run { max_workers, .. } = cli.command;
        assert_eq!(max_workers, None);

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
        assert_eq!(max_workers, Some(2));

        let cli = Cli::try_parse_from([
            "singularity-evaluation",
            "run",
            "manifest.json",
            "--run-id",
            "run",
            "--max-workers",
            "1",
        ])
        .expect("max-workers=1 parses");
        let Command::Run { max_workers, .. } = cli.command;
        assert_eq!(max_workers, Some(1));
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
    fn run_mode_defaults_to_feedback_and_supports_full() {
        assert!(matches!(
            run_mode_from_flags(false),
            EvaluationRunMode::Feedback
        ));
        assert!(matches!(run_mode_from_flags(true), EvaluationRunMode::Full));
    }

    #[test]
    fn default_max_workers_is_mode_aware_and_fails_closed() {
        let full = EvaluationRunMode::Full;
        assert_eq!(
            resolve_max_workers(&full, None, || {
                Ok(NonZeroUsize::new(1).expect("one is non-zero"))
            }),
            1
        );
        assert_eq!(
            resolve_max_workers(&full, None, || {
                Ok(NonZeroUsize::new(2).expect("two is non-zero"))
            }),
            2
        );
        assert_eq!(
            resolve_max_workers(&full, None, || {
                Ok(NonZeroUsize::new(16).expect("sixteen is non-zero"))
            }),
            2
        );
        assert_eq!(
            resolve_max_workers(&full, None, || {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "parallelism unavailable",
                ))
            }),
            1
        );

        let feedback = EvaluationRunMode::Feedback;
        assert_eq!(
            resolve_max_workers(&feedback, None, || {
                panic!("feedback must not query host parallelism")
            }),
            1
        );
        assert_eq!(
            resolve_max_workers(&full, Some(1), || {
                panic!("explicit worker count must not query host parallelism")
            }),
            1
        );
        assert_eq!(
            resolve_max_workers(&full, Some(2), || {
                panic!("explicit worker count must not query host parallelism")
            }),
            2
        );
    }

    #[test]
    fn feedback_text_result_is_explicitly_non_gating() {
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
            gate_applicable: false,
        };
        let text = result_text(&result);
        assert!(text.contains("evaluation run completed"));
        assert!(text.contains("gate_applicable=false"));
        assert!(!result.gate_applicable);
    }
}
