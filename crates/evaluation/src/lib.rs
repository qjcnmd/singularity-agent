#![forbid(unsafe_code)]

//! Evaluation manifest、任务投影、结果和安全 evidence 的公共领域模型。

mod evidence;
mod manifest;
mod result;
mod value;

use std::path::PathBuf;

pub use evidence::{
    EvaluationEvidence, EvaluationEvidenceSchemaVersion, EvaluationPromptStructure,
    EvaluationProviderEvidence, EvaluationScopeEvidence, EvaluationTaskEvidence,
    EvaluationTrialEvidence, EvidenceVerdict, task_selection_digest,
};
pub use manifest::{
    AgentStagePlan, AgentTaskProjection, AgentTaskSpec, BaselineStagePlan, CommandExpectation,
    CommandSpec, EvaluationCapability, EvaluationManifest, EvaluationStage, EvaluationTask,
    EvaluationTaskSet, EvaluatorSpec, EvaluatorStageSpec, EvaluatorTestPatch, PatchFormat,
    PlannedWorkspaceSource, TaskSetSchemaVersion, ToolCapabilityRequirement, VerificationStagePlan,
    WorkspacePlan, WorkspaceSeed, WorkspaceSource, WorkspaceSpec,
};
pub use result::{
    BlockerKind, EvaluationBlocker, EvaluationEvidenceSummary, EvaluationResult,
    EvaluationResultSchemaVersion, EvaluationRunSummary, EvaluationStabilitySummary,
    EvaluationStageResults, EvaluationStatus, EvaluationTaskResult, EvaluationTaskSummary,
    EvaluationTrialResult, FiniteStatistics, StageResult, StageStatus,
};
pub use value::{
    Argv, GitCommit, RelativePath, RemoteRepository, RunId, TaskId, ToolCapabilityName,
};

/// 当前 task set schema 版本。
pub const TASK_SET_SCHEMA_VERSION: &str = "evaluation.task_set/v5";
/// 当前稳定 result schema 版本。
pub const RESULT_SCHEMA_VERSION: &str = "evaluation.result/v6";
/// 当前 evidence schema 版本。
pub const EVIDENCE_SCHEMA_VERSION: &str = "evaluation.evidence/v2";
/// 核心任务成功率门禁的 basis points 阈值。
pub const CORE_TASK_SUCCESS_THRESHOLD_BASIS_POINTS: u32 = 8_000;

#[derive(Debug, thiserror::Error)]
/// Evaluation 输入、执行和结果校验错误。
pub enum EvaluationError {
    #[error("invalid evaluation JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to access evaluation path {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("unsupported evaluation schema version {actual:?}; expected {expected}")]
    UnsupportedSchemaVersion {
        expected: &'static str,
        actual: String,
    },
    #[error("invalid evaluation contract: {0}")]
    Validation(String),
    #[error("duplicate evaluation task id: {0}")]
    DuplicateTaskId(TaskId),
    #[error("evaluation task not found: {0}")]
    TaskNotFound(TaskId),
}

/// Evaluation crate 的统一结果类型。
pub type Result<T> = std::result::Result<T, EvaluationError>;

pub(crate) fn require_schema_version(json: &str, expected: &'static str) -> Result<()> {
    let payload: serde_json::Value = serde_json::from_str(json)?;
    let actual = payload
        .get("schema_version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if actual == expected {
        Ok(())
    } else {
        Err(EvaluationError::UnsupportedSchemaVersion {
            expected,
            actual: actual.to_string(),
        })
    }
}

pub(crate) fn validation_error(message: impl Into<String>) -> EvaluationError {
    EvaluationError::Validation(message.into())
}
