//! Evaluation evidence 的 schema、摘要校验和结果绑定规则。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    EVIDENCE_SCHEMA_VERSION, EvaluationError, EvaluationResult, GitCommit, Result, RunId, TaskId,
    require_schema_version, validation_error,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Evaluation evidence 的 schema 版本。
pub enum EvaluationEvidenceSchemaVersion {
    #[serde(rename = "evaluation.evidence/v1")]
    V1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// 单项 evidence 的判定结果。
pub enum EvidenceVerdict {
    Passed,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 一组作用域摘要及其满足状态。
pub struct EvaluationScopeEvidence {
    pub expectation_known: bool,
    pub expected_scope_digests: Vec<String>,
    pub observed_scope_digests: Vec<String>,
    pub required_scopes_satisfied: EvidenceVerdict,
}

impl EvaluationScopeEvidence {
    fn validate(&self, context: &str) -> Result<()> {
        for digest in self
            .expected_scope_digests
            .iter()
            .chain(&self.observed_scope_digests)
        {
            validate_sha256_digest(digest, context)?;
        }
        let satisfied =
            multiset_contains(&self.observed_scope_digests, &self.expected_scope_digests);
        let verdict_is_valid = if self.expectation_known {
            self.required_scopes_satisfied
                == if satisfied {
                    EvidenceVerdict::Passed
                } else {
                    EvidenceVerdict::Failed
                }
        } else {
            self.expected_scope_digests.is_empty()
                && self.required_scopes_satisfied == EvidenceVerdict::Unknown
        };
        if !verdict_is_valid {
            return Err(validation_error(format!(
                "{context} required_scopes_satisfied must match known expected and observed scopes"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 单个任务的来源、路径、trace 和阶段 evidence。
pub struct EvaluationTaskEvidence {
    pub task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_tree_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<GitCommit>,
    pub allowed_paths_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_paths_digest: Option<String>,
    pub allowlist: EvidenceVerdict,
    pub smoke: EvaluationScopeEvidence,
    pub baseline: EvaluationScopeEvidence,
    pub public: EvaluationScopeEvidence,
    pub hidden: EvaluationScopeEvidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_digest: Option<String>,
    pub local_process_fallback_count: u32,
    pub local_process_fallback_unknown_count: u32,
}

impl EvaluationTaskEvidence {
    fn validate(&self) -> Result<()> {
        let context = format!("evaluation evidence task {}", self.task_id);
        for digest in [
            self.source_tree_digest.as_deref(),
            Some(self.allowed_paths_digest.as_str()),
            self.changed_paths_digest.as_deref(),
            self.trace_digest.as_deref(),
            self.patch_digest.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            validate_sha256_digest(digest, &context)?;
        }
        self.smoke.validate(&format!("{context} smoke"))?;
        self.baseline.validate(&format!("{context} baseline"))?;
        self.public.validate(&format!("{context} public"))?;
        self.hidden.validate(&format!("{context} hidden"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 与稳定 EvaluationResult 绑定的脱敏 evidence。
pub struct EvaluationEvidence {
    pub schema_version: EvaluationEvidenceSchemaVersion,
    pub run_id: RunId,
    pub manifest_digest: String,
    pub task_selection_digest: String,
    pub denominator_task_count: u32,
    pub tasks: Vec<EvaluationTaskEvidence>,
}

impl EvaluationEvidence {
    /// 从 JSON 读取并校验证据。
    pub fn from_json_str(json: &str) -> Result<Self> {
        require_schema_version(json, EVIDENCE_SCHEMA_VERSION)?;
        let evidence: Self = serde_json::from_str(json)?;
        evidence.validate()?;
        Ok(evidence)
    }

    /// 校验证据自身的闭集约束和任务选择摘要。
    pub fn validate(&self) -> Result<()> {
        validate_sha256_digest(&self.manifest_digest, "evaluation evidence manifest")?;
        validate_sha256_digest(
            &self.task_selection_digest,
            "evaluation evidence task selection",
        )?;
        if self.tasks.is_empty() {
            return Err(validation_error(
                "evaluation evidence requires at least one task",
            ));
        }
        if self.denominator_task_count != u32::try_from(self.tasks.len()).unwrap_or(u32::MAX) {
            return Err(validation_error(
                "evaluation evidence denominator_task_count must match the selected tasks",
            ));
        }
        let mut task_ids = BTreeSet::new();
        for task in &self.tasks {
            task.validate()?;
            if !task_ids.insert(task.task_id.clone()) {
                return Err(EvaluationError::DuplicateTaskId(task.task_id.clone()));
            }
        }
        if self.task_selection_digest
            != task_selection_digest(
                &self
                    .tasks
                    .iter()
                    .map(|task| task.task_id.clone())
                    .collect::<Vec<_>>(),
            )
        {
            return Err(validation_error(
                "evaluation evidence task_selection_digest must match the selected task identities",
            ));
        }
        Ok(())
    }

    /// 校验证据与稳定 EvaluationResult 的逐任务一致性。
    pub fn validate_against_result(&self, result: &EvaluationResult) -> Result<()> {
        self.validate()?;
        result.validate()?;
        if self.run_id != result.run_id {
            return Err(validation_error(
                "evaluation evidence run_id must match the stable result",
            ));
        }
        if self.denominator_task_count != result.summary.scored_task_count
            || self.denominator_task_count != result.summary.task_count
            || self.tasks.len() != result.tasks.len()
        {
            return Err(validation_error(
                "evaluation evidence task denominator must match the stable result",
            ));
        }
        for (evidence, task) in self.tasks.iter().zip(&result.tasks) {
            if evidence.task_id != task.task_id {
                return Err(validation_error(
                    "evaluation evidence task identities must match the stable result",
                ));
            }
            if evidence.patch_digest != task.evidence.patch_digest
                || evidence.local_process_fallback_count
                    != task.evidence.local_process_fallback_count
                || evidence.local_process_fallback_unknown_count
                    != task.evidence.local_process_fallback_unknown_count
            {
                return Err(validation_error(format!(
                    "evaluation evidence task {} must match stable patch and fallback evidence",
                    task.task_id
                )));
            }
            if task.evaluation_passed
                && (evidence.allowlist != EvidenceVerdict::Passed
                    || evidence.smoke.required_scopes_satisfied != EvidenceVerdict::Passed
                    || evidence.baseline.required_scopes_satisfied != EvidenceVerdict::Passed
                    || evidence.public.required_scopes_satisfied != EvidenceVerdict::Passed
                    || evidence.hidden.required_scopes_satisfied != EvidenceVerdict::Passed
                    || evidence.trace_digest.is_none()
                    || evidence.source_tree_digest.is_none()
                    || evidence.local_process_fallback_unknown_count != 0)
            {
                return Err(validation_error(format!(
                    "evaluation evidence task {} is incomplete for a passed evaluation",
                    task.task_id
                )));
            }
        }
        Ok(())
    }
}

/// 计算固定顺序的任务选择摘要。
pub fn task_selection_digest(task_ids: &[TaskId]) -> String {
    let values = task_ids
        .iter()
        .map(|task_id| task_id.as_str())
        .collect::<Vec<_>>();
    ordered_strings_digest("evaluation.task_selection/v1", &values)
}

fn ordered_strings_digest(domain: &str, values: &[&str]) -> String {
    let mut digest = Sha256::new();
    update_digest_value(&mut digest, domain);
    for value in values {
        update_digest_value(&mut digest, value);
    }
    format!("sha256:{:x}", digest.finalize())
}

fn update_digest_value(digest: &mut Sha256, value: &str) {
    let length = u64::try_from(value.len()).unwrap_or(u64::MAX);
    digest.update(length.to_le_bytes());
    digest.update(value.as_bytes());
}

fn multiset_contains(observed: &[String], required: &[String]) -> bool {
    let mut remaining = observed.to_vec();
    required.iter().all(|required| {
        let Some(index) = remaining.iter().position(|observed| observed == required) else {
            return false;
        };
        remaining.remove(index);
        true
    })
}

fn validate_sha256_digest(digest: &str, context: &str) -> Result<()> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(validation_error(format!(
            "{context} digest must use sha256"
        )));
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(validation_error(format!(
            "{context} digest must contain 64 hexadecimal characters"
        )));
    }
    Ok(())
}
