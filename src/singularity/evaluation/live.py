from singularity.evaluation.runner import (
    EVALUATION_RESULT_SCHEMA_VERSION,
    EVALUATION_TASK_SET_SCHEMA_VERSION,
    LEGACY_LIVE_RESULT_SCHEMA_VERSION,
    LEGACY_LIVE_TASK_SET_SCHEMA_VERSION,
    CommandEvalResult,
    EvaluationRunner,
    EvaluationTask,
    EvaluationTaskResult,
    EvaluationTaskSet,
    EvaluationWorkspace,
    SingularityPrivateBenchmarkAdapter,
    SweBenchAdapter,
    TerminalBenchAdapter,
    compare_evaluation_results,
    evaluation_report_markdown,
    evaluation_regression_markdown,
    load_evaluation_task_set,
    summarize_evaluation_results,
)

# Compatibility aliases. New code must import the neutral evaluation names from
# ``singularity.evaluation.runner``; the old live names exist only so historical
# manifests, tests, and downstream scripts remain readable during migration.
LIVE_TASK_SET_SCHEMA_VERSION = LEGACY_LIVE_TASK_SET_SCHEMA_VERSION
LIVE_RESULT_SCHEMA_VERSION = LEGACY_LIVE_RESULT_SCHEMA_VERSION
LiveAgentEvalRunner = EvaluationRunner
LiveEvalManifest = EvaluationTaskSet
LiveEvalTask = EvaluationTask
LiveEvalTaskResult = EvaluationTaskResult
LiveEvalWorkspace = EvaluationWorkspace
load_live_eval_manifest = load_evaluation_task_set
summarize_live_results = summarize_evaluation_results
compare_live_eval_results = compare_evaluation_results
live_eval_report_markdown = evaluation_report_markdown
live_eval_regression_markdown = evaluation_regression_markdown

__all__ = [
    "EVALUATION_RESULT_SCHEMA_VERSION",
    "EVALUATION_TASK_SET_SCHEMA_VERSION",
    "LIVE_RESULT_SCHEMA_VERSION",
    "LIVE_TASK_SET_SCHEMA_VERSION",
    "CommandEvalResult",
    "EvaluationRunner",
    "EvaluationTask",
    "EvaluationTaskResult",
    "EvaluationTaskSet",
    "EvaluationWorkspace",
    "LiveAgentEvalRunner",
    "LiveEvalManifest",
    "LiveEvalTask",
    "LiveEvalTaskResult",
    "LiveEvalWorkspace",
    "SingularityPrivateBenchmarkAdapter",
    "SweBenchAdapter",
    "TerminalBenchAdapter",
    "compare_evaluation_results",
    "compare_live_eval_results",
    "evaluation_report_markdown",
    "evaluation_regression_markdown",
    "live_eval_report_markdown",
    "live_eval_regression_markdown",
    "load_evaluation_task_set",
    "load_live_eval_manifest",
    "summarize_evaluation_results",
    "summarize_live_results",
]
