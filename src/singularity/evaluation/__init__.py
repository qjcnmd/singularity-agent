from singularity.evaluation.models import (
    BenchmarkAdapterKind,
    BenchmarkTask,
    BenchmarkTaskKind,
    BenchmarkVisibility,
    EvaluationHook,
    EvaluationProfile,
    ExpectedOutcome,
    ExpectedOutcomeKind,
    FailureCaseRecord,
    GoldenTaskContract,
    PatchQualityResult,
    ScoringResult,
    TaskDifficulty,
    TaskInput,
    TraceReplayResult,
    WorkspaceSnapshot,
    WorkspaceSnapshotKind,
)
from singularity.evaluation.failure_case_replay import FailureCaseReplayRunner
from singularity.evaluation.targeted_replay import (
    TargetedFailureReplayResult,
    TargetedFailureReplayRunner,
)
from singularity.evaluation.patch_quality import PatchQualityEvaluator
from singularity.evaluation.replay import TraceReplayHarness
from singularity.evaluation.reports import (
    EvaluationReport,
    ProfileEvaluationReport,
    RegressionReport,
    TaskEvaluationResult,
)
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
from singularity.evaluation.harness import EvaluationHarness, RegressionDetector
from singularity.evaluation.scoring import ScoringEngine
from singularity.evaluation.store import GoldenTaskStore

LIVE_RESULT_SCHEMA_VERSION = LEGACY_LIVE_RESULT_SCHEMA_VERSION
LIVE_TASK_SET_SCHEMA_VERSION = LEGACY_LIVE_TASK_SET_SCHEMA_VERSION
LiveAgentEvalRunner = EvaluationRunner
LiveEvalManifest = EvaluationTaskSet
LiveEvalTask = EvaluationTask
LiveEvalTaskResult = EvaluationTaskResult
LiveEvalWorkspace = EvaluationWorkspace
compare_live_eval_results = compare_evaluation_results
live_eval_report_markdown = evaluation_report_markdown
live_eval_regression_markdown = evaluation_regression_markdown
load_live_eval_manifest = load_evaluation_task_set
summarize_live_results = summarize_evaluation_results

__all__ = [
    "BenchmarkTask",
    "BenchmarkAdapterKind",
    "BenchmarkTaskKind",
    "BenchmarkVisibility",
    "EvaluationHook",
    "EvaluationProfile",
    "FailureCaseRecord",
    "FailureCaseReplayRunner",
    "EvaluationReport",
    "EvaluationHarness",
    "ExpectedOutcome",
    "ExpectedOutcomeKind",
    "GoldenTaskContract",
    "GoldenTaskStore",
    "CommandEvalResult",
    "EVALUATION_RESULT_SCHEMA_VERSION",
    "EVALUATION_TASK_SET_SCHEMA_VERSION",
    "EvaluationRunner",
    "EvaluationTask",
    "EvaluationTaskResult",
    "EvaluationTaskSet",
    "EvaluationWorkspace",
    "LIVE_RESULT_SCHEMA_VERSION",
    "LIVE_TASK_SET_SCHEMA_VERSION",
    "LiveAgentEvalRunner",
    "LiveEvalManifest",
    "LiveEvalTask",
    "LiveEvalTaskResult",
    "LiveEvalWorkspace",
    "SingularityPrivateBenchmarkAdapter",
    "SweBenchAdapter",
    "TerminalBenchAdapter",
    "PatchQualityEvaluator",
    "PatchQualityResult",
    "ProfileEvaluationReport",
    "RegressionDetector",
    "RegressionReport",
    "ScoringEngine",
    "ScoringResult",
    "TaskDifficulty",
    "TaskEvaluationResult",
    "TaskInput",
    "TraceReplayResult",
    "TraceReplayHarness",
    "TargetedFailureReplayResult",
    "TargetedFailureReplayRunner",
    "WorkspaceSnapshot",
    "WorkspaceSnapshotKind",
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
