//! Evaluation evidence 的 schema、逐 trial 原始脱敏证据和结果绑定规则。

use std::collections::BTreeSet;

use crate::{
    EVIDENCE_SCHEMA_VERSION, EvaluationError, EvaluationResult, GitCommit, Result, RunId, TaskId,
    require_schema_version, validation_error,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Evaluation evidence 的 schema 版本。
pub enum EvaluationEvidenceSchemaVersion {
    #[serde(rename = "evaluation.evidence/v3")]
    V3,
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
        let valid = if self.expectation_known {
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
        if !valid {
            return Err(validation_error(format!(
                "{context} required_scopes_satisfied must match known expected and observed scopes"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 真实 prompt 的安全结构投影；不包含 prompt 文本。
pub struct EvaluationPromptStructure {
    pub contract: String,
    pub model_message_roles: Vec<String>,
    pub section_kinds: Vec<String>,
    pub allowed_path_count: u32,
    pub resolved_tool_count: u32,
    pub smoke_command_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_instructions_fingerprint: Option<String>,
}

impl EvaluationPromptStructure {
    fn validate(&self, context: &str) -> Result<()> {
        if self.contract != "evaluation.agent_prompt/v1"
            || self.model_message_roles != ["developer", "user"]
            || self.section_kinds.is_empty()
            || self.section_kinds.iter().any(|kind| kind.trim().is_empty())
        {
            return Err(validation_error(format!(
                "{context} prompt structure is invalid"
            )));
        }
        if let Some(fingerprint) = &self.project_instructions_fingerprint {
            validate_sha256_digest(fingerprint, context)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// provider/model 身份和最终协议协商的脱敏原始证据。
pub struct EvaluationProviderEvidence {
    pub provider_fingerprint: String,
    pub model_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negotiation_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_contract_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_metadata_fingerprint: Option<String>,
}

impl EvaluationProviderEvidence {
    fn validate(&self, context: &str) -> Result<()> {
        validate_sha256_digest(&self.provider_fingerprint, context)?;
        validate_sha256_digest(&self.model_fingerprint, context)?;
        if let Some(fingerprint) = &self.negotiation_fingerprint {
            validate_sha256_digest(fingerprint, context)?;
        }
        for fingerprint in [
            self.protocol_contract_fingerprint.as_deref(),
            self.capability_metadata_fingerprint.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            validate_sha256_digest(fingerprint, context)?;
        }
        let negotiation_present = self.negotiation_fingerprint.is_some();
        if [
            self.api_protocol.is_some(),
            self.protocol_contract_fingerprint.is_some(),
            self.capability_metadata_fingerprint.is_some(),
        ]
        .into_iter()
        .any(|present| present != negotiation_present)
        {
            return Err(validation_error(format!(
                "{context} negotiation fingerprint, protocol, and contract metadata must be present together"
            )));
        }
        if self
            .api_protocol
            .as_ref()
            .is_some_and(|protocol| protocol.trim().is_empty())
        {
            return Err(validation_error(format!(
                "{context} api_protocol must not be empty"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 单个 trial 的原始脱敏来源、prompt、tool schema、trace 和阶段证据。
pub struct EvaluationTrialEvidence {
    pub trial: u32,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_structure: Option<EvaluationPromptStructure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_schema_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<EvaluationProviderEvidence>,
    pub local_process_fallback_count: u32,
    pub local_process_fallback_unknown_count: u32,
}

impl EvaluationTrialEvidence {
    fn validate(&self, task_id: &TaskId) -> Result<()> {
        let context = format!("evaluation evidence task {task_id} trial {}", self.trial);
        if self.trial == 0 {
            return Err(validation_error(format!(
                "{context} trial must be positive"
            )));
        }
        for digest in [
            self.changed_paths_digest.as_deref(),
            self.trace_digest.as_deref(),
            self.patch_digest.as_deref(),
            self.prompt_fingerprint.as_deref(),
            self.tool_schema_fingerprint.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            validate_sha256_digest(digest, &context)?;
        }
        if self.prompt_structure.is_some() != self.prompt_fingerprint.is_some()
            || self.prompt_structure.is_some() != self.tool_schema_fingerprint.is_some()
        {
            return Err(validation_error(format!(
                "{context} prompt structure and prompt/tool fingerprints must be present together"
            )));
        }
        if let Some(prompt_structure) = &self.prompt_structure {
            prompt_structure.validate(&context)?;
        }
        if let Some(provider) = &self.provider {
            provider.validate(&context)?;
        }
        self.smoke.validate(&format!("{context} smoke"))?;
        self.baseline.validate(&format!("{context} baseline"))?;
        self.public.validate(&format!("{context} public"))?;
        self.hidden.validate(&format!("{context} hidden"))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 单个任务的只读 prepared source 身份和逐 trial 原始证据。
pub struct EvaluationTaskEvidence {
    pub task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_tree_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<GitCommit>,
    pub allowed_paths_digest: String,
    pub tool_capability_requirements_digest: String,
    pub trials: Vec<EvaluationTrialEvidence>,
}

impl EvaluationTaskEvidence {
    fn validate_identity(&self) -> Result<()> {
        let context = format!("evaluation evidence task {}", self.task_id);
        for digest in [
            self.source_tree_digest.as_deref(),
            Some(self.allowed_paths_digest.as_str()),
            Some(self.tool_capability_requirements_digest.as_str()),
        ]
        .into_iter()
        .flatten()
        {
            validate_sha256_digest(digest, &context)?;
        }
        Ok(())
    }

    fn validate(&self, trials_per_task: u32) -> Result<()> {
        self.validate_identity()?;
        let context = format!("evaluation evidence task {}", self.task_id);
        if self.trials.len() != usize::try_from(trials_per_task).unwrap_or(usize::MAX) {
            return Err(validation_error(format!(
                "{context} trial evidence count must match trials_per_task"
            )));
        }
        for (index, trial) in self.trials.iter().enumerate() {
            trial.validate(&self.task_id)?;
            if trial.trial != u32::try_from(index + 1).unwrap_or(u32::MAX) {
                return Err(validation_error(format!(
                    "{context} trial evidence must be ordered contiguously from one"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// 与 EvaluationResult/v8 逐 trial 或 run-level preflight blocker 绑定的脱敏 evidence。
pub struct EvaluationEvidence {
    pub schema_version: EvaluationEvidenceSchemaVersion,
    pub run_id: RunId,
    pub manifest_digest: String,
    pub task_selection_digest: String,
    pub denominator_task_count: u32,
    pub trials_per_task: u32,
    pub denominator_trial_count: u32,
    pub configured_trial_count: u32,
    pub sampled_trial_count: u32,
    pub tasks: Vec<EvaluationTaskEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_preflight: Option<crate::EvaluationSandboxPreflight>,
}

impl EvaluationEvidence {
    pub fn from_json_str(json: &str) -> Result<Self> {
        require_schema_version(json, EVIDENCE_SCHEMA_VERSION)?;
        let evidence: Self = serde_json::from_str(json)?;
        evidence.validate()?;
        Ok(evidence)
    }

    pub fn validate(&self) -> Result<()> {
        validate_sha256_digest(&self.manifest_digest, "evaluation evidence manifest")?;
        validate_sha256_digest(
            &self.task_selection_digest,
            "evaluation evidence task selection",
        )?;
        if self.trials_per_task == 0 {
            return Err(validation_error(
                "evaluation evidence requires tasks and a positive trials_per_task",
            ));
        }
        let Some(preflight) = self.sandbox_preflight.as_ref() else {
            return Err(validation_error(
                "evaluation evidence requires sandbox preflight evidence",
            ));
        };
        validate_sandbox_preflight(preflight, "evaluation evidence sandbox_preflight")?;
        let preflight_blocked =
            preflight.outcome == crate::EvaluationSandboxPreflightOutcome::Unsupported;
        let zero_sampling = self.denominator_trial_count == 0 && self.sampled_trial_count == 0;
        let zero_sampling_allowed = preflight_blocked || zero_sampling;
        if self.denominator_task_count == 0
            || self.configured_trial_count
                != self
                    .denominator_task_count
                    .saturating_mul(self.trials_per_task)
            || (zero_sampling_allowed
                && (self.denominator_trial_count != 0 || self.sampled_trial_count != 0))
            || (!zero_sampling_allowed
                && (self.denominator_trial_count != self.configured_trial_count
                    || self.sampled_trial_count != self.denominator_trial_count))
        {
            return Err(validation_error(
                "evaluation evidence denominators must match selected tasks and trials",
            ));
        }
        if self.denominator_task_count != u32::try_from(self.tasks.len()).unwrap_or(u32::MAX) {
            return Err(validation_error(
                "evaluation evidence task count must match selected tasks",
            ));
        }
        let mut task_ids = BTreeSet::new();
        for task in &self.tasks {
            if !zero_sampling_allowed {
                task.validate(self.trials_per_task)?;
            } else {
                task.validate_identity()?;
                if !task.trials.is_empty() {
                    return Err(validation_error(
                        "zero-sampling evidence must not contain sampled trial evidence",
                    ));
                }
            }
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
                "evaluation evidence task_selection_digest must match selected task identities",
            ));
        }
        Ok(())
    }

    pub fn validate_against_result(&self, result: &EvaluationResult) -> Result<()> {
        result.validate()?;
        let zero_sampling = result.is_blocked_before_sampling();
        self.validate()?;
        if self.run_id != result.run_id
            || self.denominator_task_count != result.summary.task_count
            || self.trials_per_task != result.summary.trials_per_task
            || self.denominator_trial_count != result.summary.trial_count
            || (!zero_sampling && self.tasks.len() != result.tasks.len())
            || (zero_sampling
                && (self.sampled_trial_count != 0
                    || self.configured_trial_count != result.summary.configured_trial_count))
            || self.sandbox_preflight != result.sandbox_preflight
        {
            return Err(validation_error(
                "evaluation evidence task/trial denominators must match the stable result",
            ));
        }
        if zero_sampling {
            if self.tasks.iter().any(|task| !task.trials.is_empty()) {
                return Err(validation_error(
                    "zero-sampling result cannot bind trial evidence",
                ));
            }
            return Ok(());
        }
        for (task_evidence, task_result) in self.tasks.iter().zip(&result.tasks) {
            if task_evidence.task_id != task_result.task_id
                || task_evidence.trials.len() != task_result.trials.len()
            {
                return Err(validation_error(
                    "evaluation evidence task identities must match the stable result",
                ));
            }
            for (evidence, trial) in task_evidence.trials.iter().zip(&task_result.trials) {
                if evidence.trial != trial.trial
                    || evidence.patch_digest != trial.evidence.patch_digest
                    || evidence.local_process_fallback_count
                        != trial.evidence.local_process_fallback_count
                    || evidence.local_process_fallback_unknown_count
                        != trial.evidence.local_process_fallback_unknown_count
                {
                    return Err(validation_error(format!(
                        "evaluation evidence task {} trial {} must match stable evidence summary",
                        task_result.task_id, trial.trial
                    )));
                }
                if trial.evaluation_passed
                    && (evidence.allowlist != EvidenceVerdict::Passed
                        || evidence.smoke.required_scopes_satisfied != EvidenceVerdict::Passed
                        || evidence.baseline.required_scopes_satisfied != EvidenceVerdict::Passed
                        || evidence.public.required_scopes_satisfied != EvidenceVerdict::Passed
                        || evidence.hidden.required_scopes_satisfied != EvidenceVerdict::Passed
                        || evidence.trace_digest.is_none()
                        || task_evidence.source_tree_digest.is_none()
                        || evidence.prompt_structure.is_none()
                        || evidence
                            .provider
                            .as_ref()
                            .is_none_or(|provider| provider.negotiation_fingerprint.is_none())
                        || evidence.local_process_fallback_unknown_count != 0)
                {
                    return Err(validation_error(format!(
                        "evaluation evidence task {} trial {} is incomplete for a passed evaluation",
                        task_result.task_id, trial.trial
                    )));
                }
            }
        }
        Ok(())
    }
}

fn validate_sandbox_preflight(
    preflight: &crate::EvaluationSandboxPreflight,
    context: &str,
) -> Result<()> {
    crate::result::validate_sandbox_preflight(preflight, context)
}

/// 计算固定顺序的任务选择摘要。
pub fn task_selection_digest(task_ids: &[TaskId]) -> String {
    let values = task_ids
        .iter()
        .map(|task_id| task_id.as_str())
        .collect::<Vec<_>>();
    ordered_strings_digest("evaluation.task_selection/v2", &values)
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
