#![forbid(unsafe_code)]

mod evidence;
mod manifest;
mod result;
mod value;

use std::path::PathBuf;

pub use evidence::{
    EvaluationEvidence, EvaluationEvidenceSchemaVersion, EvaluationScopeEvidence,
    EvaluationTaskEvidence, EvidenceVerdict, task_selection_digest,
};
pub use manifest::{
    AgentStagePlan, AgentTaskProjection, AgentTaskSpec, BaselineStagePlan, CommandExpectation,
    CommandSpec, EvaluationCapability, EvaluationManifest, EvaluationStage, EvaluationTask,
    EvaluationTaskSet, EvaluatorSpec, EvaluatorStageSpec, EvaluatorTestPatch, PatchFormat,
    PlannedWorkspaceSource, TaskSetSchemaVersion, VerificationStagePlan, WorkspacePlan,
    WorkspaceSeed, WorkspaceSource, WorkspaceSpec,
};
pub use result::{
    BlockerKind, EvaluationBlocker, EvaluationEvidenceSummary, EvaluationResult,
    EvaluationResultSchemaVersion, EvaluationRunSummary, EvaluationStageResults, EvaluationStatus,
    EvaluationTaskResult, StageResult, StageStatus,
};
pub use value::{Argv, GitCommit, RelativePath, RemoteRepository, RunId, TaskId, ToolName};

pub const TASK_SET_SCHEMA_VERSION: &str = "evaluation.task_set/v4";
pub const RESULT_SCHEMA_VERSION: &str = "evaluation.result/v5";
pub const EVIDENCE_SCHEMA_VERSION: &str = "evaluation.evidence/v1";
pub const CORE_TASK_SUCCESS_THRESHOLD_BASIS_POINTS: u32 = 8_000;

#[derive(Debug, thiserror::Error)]
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
