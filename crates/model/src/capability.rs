//! provider capability probe、进程内/持久化缓存和脱敏 runtime fingerprint。
use std::fmt;

use super::openai::OpenAiCompletion;
use super::transport::{add_provider_attempt_metadata, provider_cancelled_error};
use super::{
    CAPABILITY_PROBE_ALTERNATE_LABEL, CAPABILITY_PROBE_CONTINUATION_REQUEST_ID,
    CAPABILITY_PROBE_CONTRACT_VERSION, CAPABILITY_PROBE_DEVELOPER_INSTRUCTION,
    CAPABILITY_PROBE_EXPECTED_LABEL, CAPABILITY_PROBE_EXPECTED_VALUE, CAPABILITY_PROBE_REQUEST_ID,
    CAPABILITY_PROBE_TOOL_A, CAPABILITY_PROBE_TOOL_B, DEFAULT_MAX_TOOLS_PER_REQUEST,
    HTTP_STATUS_BAD_REQUEST, HTTP_STATUS_NOT_FOUND, HTTP_STATUS_UNPROCESSABLE_ENTITY,
    MAX_CONFIGURED_CONTEXT_TOKENS, MAX_CONFIGURED_OUTPUT_TOKENS,
    MAX_PROVIDER_CAPABILITY_CACHE_BYTES, MAX_PROVIDER_CAPABILITY_CACHE_KEY_LOCK_FILES,
    MAX_PROVIDER_CAPABILITY_CACHE_RECORDS, ModelError, ModelErrorKind, ModelMessage, ModelRole,
    ModelToolParseStatus, ModelToolSchema, ModelTurnRequest, ModelTurnResponse, ModelTurnStatus,
    ModelUsage, OpenAiProvider, OpenAiProviderConfig, PROVIDER_ADAPTER_VERSION,
    PROVIDER_CANCELLATION_POLL_MS, PROVIDER_CAPABILITY_CACHE_INVALIDATION_DEADLINE_CODE,
    PROVIDER_CAPABILITY_CACHE_KEY_LOCK_PREFIX, PROVIDER_CAPABILITY_CACHE_KEY_LOCK_SUFFIX,
    PROVIDER_CAPABILITY_CACHE_LOCK_FILE_NAME, PROVIDER_CAPABILITY_CACHE_LOCK_RETRY_MS,
    PROVIDER_CAPABILITY_CACHE_SCHEMA_VERSION, PROVIDER_CAPABILITY_CACHE_TTL_SECONDS,
    ProviderApiProtocol, ProviderAttemptEvent, ProviderAttemptMetadata, ProviderCapabilityCacheKey,
    ProviderCapabilityCacheLookupResult, ProviderCapabilityCacheObservation,
    ProviderCapabilityMetadata, ProviderCapabilityProbeKey, ProviderCapabilityProfile,
    ProviderError, ProviderErrorStage, ProviderProtocolContract, ProviderProtocolNegotiation,
    ProviderRuntimeFingerprint, ProviderToolReasoningMode, ToolChoiceMode, ToolChoicePolicy,
    responses_endpoint, validate_model_request, validate_model_request_with_capabilities,
};
use cap_fs_ext::{FollowSymlinks, MetadataExt as CapMetadataExt};
use cap_fs_ext::{OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir as CapabilityDir, OpenOptions as CapabilityOpenOptions};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use singularity_core::{CancellationToken, Timestamp};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

impl ProviderCapabilityProbeKey {
    fn cache_key(&self, api_protocol: ProviderApiProtocol) -> ProviderCapabilityCacheKey {
        ProviderCapabilityCacheKey {
            provider_name: self.provider_name.clone(),
            endpoint_sha256: self.endpoint_sha256.clone(),
            model_name: self.model_name.clone(),
            api_protocol,
            adapter_version: self.adapter_version,
            probe_contract_version: self.probe_contract_version,
            max_context_tokens: self.max_context_tokens,
            max_output_tokens: self.max_output_tokens,
            reasoning_effort: self.reasoning_effort.clone(),
            reasoning_variant_enabled: self.reasoning_variant_enabled,
            wire_reasoning_effort: self.wire_reasoning_effort.clone(),
            tool_reasoning_mode: self.tool_reasoning_mode,
            supports_developer_role: self.supports_developer_role,
            supports_tool_choice: self.supports_tool_choice,
            requires_reasoning_content_for_tool_calls: self
                .requires_reasoning_content_for_tool_calls,
            requires_assistant_content_for_tool_calls: self
                .requires_assistant_content_for_tool_calls,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct ProviderCapabilityCacheRecord {
    key: ProviderCapabilityCacheKey,
    stored_at_unix_seconds: u64,
    expires_at_unix_seconds: u64,
    contract: PersistedProviderProtocolContract,
    metadata: PersistedProviderCapabilityMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct ProviderCapabilityCacheFile {
    schema_version: u32,
    records: Vec<ProviderCapabilityCacheRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct PersistedProviderProtocolContract {
    supports_tools: bool,
    supports_parallel_tool_calls: bool,
    supports_required_tool_choice: bool,
    supports_strict_tool_schema: bool,
    tool_reasoning_mode: ProviderToolReasoningMode,
    max_tools_per_request: u32,
    supports_json_mode: bool,
    supports_system_message: bool,
    supports_developer_message: bool,
    max_context_tokens: u32,
    max_output_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct PersistedProviderCapabilityMetadata {
    api_protocol: ProviderApiProtocol,
    profile: ProviderCapabilityProfile,
}

#[derive(Debug, Clone)]
pub(super) struct ProviderCapabilityCache {
    path: PathBuf,
    global_lock_path: PathBuf,
    key_lock_dir: PathBuf,
}

pub(super) struct ProviderCapabilityCacheFileLock {
    _file: std::fs::File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProviderCapabilityCacheError {
    Cancelled,
    Deadline,
    Unavailable,
    Invalid,
}

#[derive(Clone)]
struct InMemoryProviderCapabilityCacheEntry {
    negotiation: ProviderProtocolNegotiation,
    expires_at: Instant,
}

enum CapabilityCacheLookup {
    Hit(
        Box<BoundProviderProtocolNegotiation>,
        ProviderCapabilityCacheObservation,
    ),
    Miss(ProviderCapabilityCacheObservation),
}

fn cache_observation(
    api_protocol: ProviderApiProtocol,
    outcome: ProviderCapabilityCacheLookupResult,
) -> ProviderCapabilityCacheObservation {
    ProviderCapabilityCacheObservation {
        api_protocol,
        outcome,
        observed_at_unix_ms: Timestamp::now_utc().unix_ms(),
        model_turn_ordinal: None,
        parent_occurrence_id: None,
    }
}

fn cache_observation_metadata(
    observation: &ProviderCapabilityCacheObservation,
) -> ProviderCapabilityMetadata {
    ProviderCapabilityMetadata {
        api_protocol: observation.api_protocol,
        profile: ProviderCapabilityProfile::Declared,
        cache_hit: false,
        profile_attempts: 0,
        fallback_count: 0,
        probe_usage: ModelUsage::default(),
        probe_attempt_metadata: ProviderAttemptMetadata::zero(),
        cache_observations: vec![observation.clone()],
    }
}

fn prepend_cache_observations(
    metadata: &mut ProviderCapabilityMetadata,
    observations: impl IntoIterator<Item = ProviderCapabilityCacheObservation>,
) {
    let mut observations = observations.into_iter().collect::<Vec<_>>();
    observations.append(&mut metadata.cache_observations);
    metadata.cache_observations = observations;
}

fn mark_cache_observation_hit(
    observations: &mut [ProviderCapabilityCacheObservation],
    api_protocol: ProviderApiProtocol,
) {
    if let Some(observation) = observations
        .iter_mut()
        .rev()
        .find(|observation| observation.api_protocol == api_protocol)
    {
        observation.outcome = ProviderCapabilityCacheLookupResult::Hit;
    }
}

fn prepend_cache_observations_to_error(
    mut error: ProviderError,
    observations: impl IntoIterator<Item = ProviderCapabilityCacheObservation>,
) -> ProviderError {
    let observations = observations.into_iter().collect::<Vec<_>>();
    if observations.is_empty() {
        return error;
    }
    let mut metadata = error
        .capability_metadata
        .take()
        .map(|metadata| *metadata)
        .unwrap_or_else(|| cache_observation_metadata(&observations[0]));
    prepend_cache_observations(&mut metadata, observations);
    error.capability_metadata = Some(Box::new(metadata));
    error
}

fn prepend_cache_observations_to_result(
    result: Result<BoundProviderProtocolNegotiation, ProviderError>,
    observations: impl IntoIterator<Item = ProviderCapabilityCacheObservation>,
) -> Result<BoundProviderProtocolNegotiation, ProviderError> {
    let observations = observations.into_iter().collect::<Vec<_>>();
    if observations.is_empty() {
        return result;
    }
    match result {
        Ok(mut bound) => {
            prepend_cache_observations(&mut bound.negotiation.metadata, observations);
            Ok(bound)
        }
        Err(error) => Err(prepend_cache_observations_to_error(error, observations)),
    }
}

impl From<&ProviderProtocolContract> for PersistedProviderProtocolContract {
    fn from(contract: &ProviderProtocolContract) -> Self {
        Self {
            supports_tools: contract.supports_tools,
            supports_parallel_tool_calls: contract.supports_parallel_tool_calls,
            supports_required_tool_choice: contract.supports_required_tool_choice,
            supports_strict_tool_schema: contract.supports_strict_tool_schema,
            tool_reasoning_mode: contract.tool_reasoning_mode,
            max_tools_per_request: contract.max_tools_per_request,
            supports_json_mode: contract.supports_json_mode,
            supports_system_message: contract.supports_system_message,
            supports_developer_message: contract.supports_developer_message,
            max_context_tokens: contract.max_context_tokens,
            max_output_tokens: contract.max_output_tokens,
        }
    }
}

impl PersistedProviderProtocolContract {
    fn into_contract(self) -> ProviderProtocolContract {
        ProviderProtocolContract {
            supports_tools: self.supports_tools,
            supports_parallel_tool_calls: self.supports_parallel_tool_calls,
            supports_required_tool_choice: self.supports_required_tool_choice,
            supports_strict_tool_schema: self.supports_strict_tool_schema,
            tool_reasoning_mode: self.tool_reasoning_mode,
            max_tools_per_request: self.max_tools_per_request,
            supports_json_mode: self.supports_json_mode,
            supports_system_message: self.supports_system_message,
            supports_developer_message: self.supports_developer_message,
            max_context_tokens: self.max_context_tokens,
            max_output_tokens: self.max_output_tokens,
        }
    }
}

impl From<&ProviderCapabilityMetadata> for PersistedProviderCapabilityMetadata {
    fn from(metadata: &ProviderCapabilityMetadata) -> Self {
        Self {
            api_protocol: metadata.api_protocol,
            profile: metadata.profile,
        }
    }
}

impl PersistedProviderCapabilityMetadata {
    fn into_metadata(self) -> ProviderCapabilityMetadata {
        ProviderCapabilityMetadata {
            api_protocol: self.api_protocol,
            profile: self.profile,
            cache_hit: true,
            profile_attempts: 0,
            fallback_count: 0,
            probe_usage: ModelUsage::default(),
            probe_attempt_metadata: ProviderAttemptMetadata::zero(),
            cache_observations: Vec::new(),
        }
    }
}

impl ProviderCapabilityCache {
    pub(super) fn new(path: PathBuf) -> Option<Self> {
        let path_text = path.to_string_lossy();
        if path.as_os_str().is_empty()
            || path_text.eq_ignore_ascii_case(":memory:")
            || path_text.to_ascii_lowercase().starts_with("file:")
            || path_text.to_ascii_lowercase().starts_with("sqlite:")
        {
            return None;
        }
        let parent = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Some(Self {
            path,
            global_lock_path: parent.join(PROVIDER_CAPABILITY_CACHE_LOCK_FILE_NAME),
            key_lock_dir: parent,
        })
    }

    fn load(
        &self,
        key: &ProviderCapabilityCacheKey,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Option<(ProviderProtocolNegotiation, Duration)>, ProviderCapabilityCacheError> {
        let Some(now) = unix_time_seconds() else {
            return Ok(None);
        };
        let _lock = self.acquire_global_lock(false, cancellation, deadline)?;
        self.load_locked(key, now)
    }

    fn load_locked(
        &self,
        key: &ProviderCapabilityCacheKey,
        now: u64,
    ) -> Result<Option<(ProviderProtocolNegotiation, Duration)>, ProviderCapabilityCacheError> {
        let Some(file) = self.read_file()? else {
            return Ok(None);
        };
        if file.schema_version != PROVIDER_CAPABILITY_CACHE_SCHEMA_VERSION {
            return Ok(None);
        }
        Ok(file.records.into_iter().find_map(|record| {
            let negotiation = valid_cached_record(&record, key, now)?;
            let remaining = Duration::from_secs(record.expires_at_unix_seconds - now);
            Some((negotiation, remaining))
        }))
    }

    fn load_locked_with_global_lock(
        &self,
        key: &ProviderCapabilityCacheKey,
        now: u64,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Option<(ProviderProtocolNegotiation, Duration)>, ProviderCapabilityCacheError> {
        let _global_lock = self.acquire_global_lock(false, cancellation, deadline)?;
        self.load_locked(key, now)
    }

    fn store_locked(
        &self,
        key: &ProviderCapabilityCacheKey,
        negotiation: &ProviderProtocolNegotiation,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<(), ProviderCapabilityCacheError> {
        if cancellation.is_cancelled() {
            return Err(ProviderCapabilityCacheError::Cancelled);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(ProviderCapabilityCacheError::Deadline);
        }
        let Some(now) = unix_time_seconds() else {
            return Err(ProviderCapabilityCacheError::Unavailable);
        };
        let record = ProviderCapabilityCacheRecord {
            key: key.clone(),
            stored_at_unix_seconds: now,
            expires_at_unix_seconds: now.saturating_add(PROVIDER_CAPABILITY_CACHE_TTL_SECONDS),
            contract: PersistedProviderProtocolContract::from(&negotiation.contract),
            metadata: PersistedProviderCapabilityMetadata::from(&negotiation.metadata),
        };
        if valid_cached_record(&record, key, now).is_none() {
            return Err(ProviderCapabilityCacheError::Invalid);
        }
        let mut file = self
            .read_file()?
            .unwrap_or_else(empty_capability_cache_file);
        if file.schema_version != PROVIDER_CAPABILITY_CACHE_SCHEMA_VERSION {
            file = empty_capability_cache_file();
        }
        file.records
            .retain(|existing| existing.key != *key && valid_cache_record_shape_at(existing, now));
        file.records
            .sort_unstable_by_key(|existing| existing.stored_at_unix_seconds);
        if file.records.len() >= MAX_PROVIDER_CAPABILITY_CACHE_RECORDS {
            let remove_count = file.records.len() - (MAX_PROVIDER_CAPABILITY_CACHE_RECORDS - 1);
            file.records.drain(..remove_count);
        }
        file.records.push(record);
        if cancellation.is_cancelled() {
            return Err(ProviderCapabilityCacheError::Cancelled);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(ProviderCapabilityCacheError::Deadline);
        }
        self.write_file(&file)
            .map_err(|_| ProviderCapabilityCacheError::Unavailable)?;
        let key_lock_path = self.key_lock_path(key);
        self.cleanup_key_lock_files_locked(Some(&key_lock_path))
    }

    fn invalidate(
        &self,
        key: &ProviderCapabilityCacheKey,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<(), ProviderCapabilityCacheError> {
        let Some(now) = unix_time_seconds() else {
            return Err(ProviderCapabilityCacheError::Unavailable);
        };
        let _key_lock = self.acquire_key_lock(key, cancellation, deadline)?;
        let _global_lock = self.acquire_global_lock(false, cancellation, deadline)?;
        let key_lock_path = self.key_lock_path(key);
        let Some(mut file) = self.read_file()? else {
            return self.cleanup_key_lock_files_locked(Some(&key_lock_path));
        };
        if file.schema_version != PROVIDER_CAPABILITY_CACHE_SCHEMA_VERSION {
            self.write_file(&empty_capability_cache_file())
                .map_err(|_| ProviderCapabilityCacheError::Unavailable)?;
        } else {
            let original_len = file.records.len();
            file.records
                .retain(|record| record.key != *key && valid_cache_record_shape_at(record, now));
            if file.records.len() != original_len {
                self.write_file(&file)
                    .map_err(|_| ProviderCapabilityCacheError::Unavailable)?;
            }
        }
        self.cleanup_key_lock_files_locked(Some(&key_lock_path))
    }

    pub(super) fn acquire_global_lock(
        &self,
        create_parent: bool,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<ProviderCapabilityCacheFileLock, ProviderCapabilityCacheError> {
        self.acquire_lock_path(
            &self.global_lock_path,
            create_parent,
            cancellation,
            deadline,
        )
    }

    pub(super) fn acquire_key_lock(
        &self,
        key: &ProviderCapabilityCacheKey,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<ProviderCapabilityCacheFileLock, ProviderCapabilityCacheError> {
        let path = self.key_lock_path(key);
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|_| ProviderCapabilityCacheError::Unavailable)?;
        loop {
            check_cache_wait(cancellation, deadline)?;
            let global_lock = self.acquire_global_lock(true, cancellation, deadline)?;
            if let Err(error) = self.cleanup_key_lock_files_locked(Some(&path)) {
                drop(global_lock);
                return Err(error);
            }
            let file = match open_or_create_private_lock_file(&path) {
                Ok(file) => file,
                Err(error) => {
                    drop(global_lock);
                    return Err(error);
                }
            };
            match file.try_lock() {
                Ok(()) => {
                    drop(global_lock);
                    return Ok(ProviderCapabilityCacheFileLock { _file: file });
                }
                Err(error) => {
                    let error = std::io::Error::from(error);
                    drop(file);
                    drop(global_lock);
                    if error.kind() != std::io::ErrorKind::WouldBlock {
                        return Err(ProviderCapabilityCacheError::Unavailable);
                    }
                    wait_for_cache_lock_retry(cancellation, deadline)?;
                }
            }
        }
    }

    fn key_lock_path(&self, key: &ProviderCapabilityCacheKey) -> PathBuf {
        let digest = sha256_hex(
            &serde_json::to_string(key).unwrap_or_else(|_| "invalid-provider-cache-key".into()),
        );
        self.key_lock_dir.join(format!(
            "{PROVIDER_CAPABILITY_CACHE_KEY_LOCK_PREFIX}{digest}{PROVIDER_CAPABILITY_CACHE_KEY_LOCK_SUFFIX}"
        ))
    }

    fn acquire_lock_path(
        &self,
        path: &Path,
        create_parent: bool,
        cancellation: &CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<ProviderCapabilityCacheFileLock, ProviderCapabilityCacheError> {
        check_cache_wait(cancellation, deadline)?;
        if create_parent {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(parent)
                .map_err(|_| ProviderCapabilityCacheError::Unavailable)?;
        }
        let file = open_or_create_private_lock_file(path)?;
        loop {
            check_cache_wait(cancellation, deadline)?;
            match file.try_lock() {
                Ok(()) => return Ok(ProviderCapabilityCacheFileLock { _file: file }),
                Err(error) => {
                    let error = std::io::Error::from(error);
                    if error.kind() != std::io::ErrorKind::WouldBlock {
                        return Err(ProviderCapabilityCacheError::Unavailable);
                    }
                    wait_for_cache_lock_retry(cancellation, deadline)?;
                }
            }
        }
    }

    /// 在 global lock 内清理未被占用的旧 per-key lock 文件。
    ///
    /// 所有 key-lock 的打开和首次 try-lock 都在同一 global lock 内完成；因此清理不会
    /// 删除另一个进程刚打开但尚未取得 OS 锁的 inode。持有中的 lock 的 try-lock 会返回
    /// WouldBlock，保持其路径不变。
    fn cleanup_key_lock_files_locked(
        &self,
        keep: Option<&Path>,
    ) -> Result<(), ProviderCapabilityCacheError> {
        let entries = match std::fs::read_dir(&self.key_lock_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(ProviderCapabilityCacheError::Unavailable),
        };
        let mut paths = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with(PROVIDER_CAPABILITY_CACHE_KEY_LOCK_PREFIX)
                            && name.ends_with(PROVIDER_CAPABILITY_CACHE_KEY_LOCK_SUFFIX)
                    })
            })
            .collect::<Vec<_>>();
        if paths.len() <= MAX_PROVIDER_CAPABILITY_CACHE_KEY_LOCK_FILES {
            return Ok(());
        }
        paths.sort_unstable();
        let mut remaining = paths.len() - MAX_PROVIDER_CAPABILITY_CACHE_KEY_LOCK_FILES;
        for path in paths {
            if remaining == 0 || keep.is_some_and(|keep| keep == path.as_path()) {
                continue;
            }
            let (file, identity) = match open_cache_path(&path, true, true, false, true) {
                Ok((file, identity)) => {
                    validate_opened_cache_file(&path, &file, identity)?;
                    (file, identity)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(ProviderCapabilityCacheError::Unavailable),
            };
            match file.try_lock() {
                Ok(()) => {
                    drop(file);
                    if remove_owned_path(&path, identity) {
                        remaining -= 1;
                    }
                }
                Err(error) => {
                    let error = std::io::Error::from(error);
                    drop(file);
                    if error.kind() != std::io::ErrorKind::WouldBlock {
                        return Err(ProviderCapabilityCacheError::Unavailable);
                    }
                }
            }
        }
        Ok(())
    }

    fn read_file(
        &self,
    ) -> Result<Option<ProviderCapabilityCacheFile>, ProviderCapabilityCacheError> {
        let Some(mut file) = open_existing_cache_file(&self.path, false)? else {
            return Ok(None);
        };
        let length = file
            .metadata()
            .map_err(|_| ProviderCapabilityCacheError::Unavailable)?
            .len();
        if length > MAX_PROVIDER_CAPABILITY_CACHE_BYTES as u64 {
            return Ok(None);
        }
        let mut bytes = Vec::with_capacity(length as usize);
        if file.read_to_end(&mut bytes).is_err() {
            return Err(ProviderCapabilityCacheError::Unavailable);
        }
        if bytes.len() > MAX_PROVIDER_CAPABILITY_CACHE_BYTES {
            return Ok(None);
        }
        let Ok(cache) = serde_json::from_slice::<ProviderCapabilityCacheFile>(&bytes) else {
            return Ok(None);
        };
        Ok((cache.records.len() <= MAX_PROVIDER_CAPABILITY_CACHE_RECORDS).then_some(cache))
    }

    fn write_file(&self, file: &ProviderCapabilityCacheFile) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(file)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if bytes.len() > MAX_PROVIDER_CAPABILITY_CACHE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "provider capability cache serialization exceeds safety limit",
            ));
        }
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        if let Some(target) =
            open_existing_cache_file(&self.path, false).map_err(cache_error_to_io)?
        {
            drop(target);
        }
        let (temp_path, temp_identity, mut output) = self.create_temp_file(parent)?;
        let write_result = output
            .write_all(&bytes)
            .and_then(|()| output.flush())
            .and_then(|()| output.sync_all());
        drop(output);
        if let Err(error) = write_result {
            remove_owned_temp(&temp_path, temp_identity);
            return Err(error);
        }
        if let Err(error) = replace_existing_atomic(&temp_path, &self.path) {
            remove_owned_temp(&temp_path, temp_identity);
            return Err(error);
        }
        sync_cache_directory(parent)?;
        if let Some(target) =
            open_existing_cache_file(&self.path, true).map_err(cache_error_to_io)?
        {
            target.sync_all()?;
        }
        Ok(())
    }

    fn temp_file_name(&self) -> String {
        let name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("provider-capability-cache.json");
        format!(
            ".{name}.tmp-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        )
    }

    fn create_temp_file(
        &self,
        parent: &Path,
    ) -> std::io::Result<(PathBuf, CacheFileIdentity, std::fs::File)> {
        let temp_path = parent.join(self.temp_file_name());
        let (file, identity) = open_cache_path(&temp_path, true, true, true, true)?;
        if let Err(error) = make_private_file(&file).and_then(|()| {
            validate_opened_cache_file(&temp_path, &file, identity).map_err(cache_error_to_io)
        }) {
            drop(file);
            remove_owned_temp(&temp_path, identity);
            return Err(error);
        }
        Ok((temp_path, identity, file))
    }
}

fn check_cache_wait(
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<(), ProviderCapabilityCacheError> {
    if cancellation.is_cancelled() {
        return Err(ProviderCapabilityCacheError::Cancelled);
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(ProviderCapabilityCacheError::Deadline);
    }
    Ok(())
}

fn wait_for_cache_lock_retry(
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
) -> Result<(), ProviderCapabilityCacheError> {
    check_cache_wait(cancellation, deadline)?;
    let remaining = deadline
        .map(|deadline| deadline.saturating_duration_since(Instant::now()))
        .unwrap_or_else(|| Duration::from_millis(PROVIDER_CAPABILITY_CACHE_LOCK_RETRY_MS));
    if remaining.is_zero() {
        return Err(ProviderCapabilityCacheError::Deadline);
    }
    std::thread::sleep(remaining.min(Duration::from_millis(
        PROVIDER_CAPABILITY_CACHE_LOCK_RETRY_MS,
    )));
    check_cache_wait(cancellation, deadline)
}

fn open_cache_path(
    path: &Path,
    read: bool,
    write: bool,
    create_new: bool,
    synchronized: bool,
) -> std::io::Result<(std::fs::File, CacheFileIdentity)> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cache path has no file name",
        )
    })?;
    let directory = CapabilityDir::open_ambient_dir(parent, cap_std::ambient_authority())?;
    let mut options = CapabilityOpenOptions::new();
    options
        .read(read)
        .write(write)
        .create_new(create_new)
        .follow(FollowSymlinks::No)
        .sync(synchronized);
    let file = directory.open_with(name, &options)?;
    let identity = cache_file_identity(&file.metadata()?).map_err(cache_error_to_io)?;
    Ok((file.into_std(), identity))
}

fn open_existing_cache_file(
    path: &Path,
    write: bool,
) -> Result<Option<std::fs::File>, ProviderCapabilityCacheError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ProviderCapabilityCacheError::Unavailable),
    };
    validate_cache_metadata(&metadata)?;
    let (file, identity) = open_cache_path(path, true, write, false, write)
        .map_err(|_| ProviderCapabilityCacheError::Unavailable)?;
    validate_opened_cache_file(path, &file, identity)?;
    Ok(Some(file))
}

fn cache_error_to_io(error: ProviderCapabilityCacheError) -> std::io::Error {
    let kind = match error {
        ProviderCapabilityCacheError::Cancelled => std::io::ErrorKind::Interrupted,
        ProviderCapabilityCacheError::Deadline => std::io::ErrorKind::TimedOut,
        ProviderCapabilityCacheError::Unavailable => std::io::ErrorKind::PermissionDenied,
        ProviderCapabilityCacheError::Invalid => std::io::ErrorKind::InvalidData,
    };
    std::io::Error::new(kind, "provider capability cache file is unavailable")
}

fn open_or_create_private_lock_file(
    path: &Path,
) -> Result<std::fs::File, ProviderCapabilityCacheError> {
    if let Some(file) = open_existing_cache_file(path, true)? {
        make_private_file(&file).map_err(|_| ProviderCapabilityCacheError::Unavailable)?;
        return Ok(file);
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|_| ProviderCapabilityCacheError::Unavailable)?;
    match open_cache_path(path, true, true, true, true) {
        Ok((file, identity)) => {
            make_private_file(&file).map_err(|_| ProviderCapabilityCacheError::Unavailable)?;
            validate_opened_cache_file(path, &file, identity)?;
            Ok(file)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            open_existing_cache_file(path, true)?.ok_or(ProviderCapabilityCacheError::Unavailable)
        }
        Err(_) => Err(ProviderCapabilityCacheError::Unavailable),
    }
}

fn validate_opened_cache_file(
    path: &Path,
    file: &std::fs::File,
    identity: CacheFileIdentity,
) -> Result<(), ProviderCapabilityCacheError> {
    let opened = file
        .metadata()
        .map_err(|_| ProviderCapabilityCacheError::Unavailable)?;
    validate_cache_metadata(&opened)?;
    let (reopened, reopened_identity) = open_cache_path(path, true, false, false, false)
        .map_err(|_| ProviderCapabilityCacheError::Unavailable)?;
    let reopened_metadata = reopened
        .metadata()
        .map_err(|_| ProviderCapabilityCacheError::Unavailable)?;
    validate_cache_metadata(&reopened_metadata)?;
    if identity != reopened_identity {
        return Err(ProviderCapabilityCacheError::Unavailable);
    }
    Ok(())
}

fn validate_cache_metadata(
    metadata: &std::fs::Metadata,
) -> Result<(), ProviderCapabilityCacheError> {
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(ProviderCapabilityCacheError::Unavailable);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(ProviderCapabilityCacheError::Unavailable);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CacheFileIdentity {
    device: u64,
    inode: u64,
    links: u64,
}

fn cache_file_identity(
    metadata: &cap_std::fs::Metadata,
) -> Result<CacheFileIdentity, ProviderCapabilityCacheError> {
    let identity = CacheFileIdentity {
        device: CapMetadataExt::dev(metadata),
        inode: CapMetadataExt::ino(metadata),
        links: CapMetadataExt::nlink(metadata),
    };
    (identity.links == 1)
        .then_some(identity)
        .ok_or(ProviderCapabilityCacheError::Unavailable)
}

fn make_private_file(_file: &std::fs::File) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = _file.metadata()?.permissions();
        permissions.set_mode(0o600);
        _file.set_permissions(permissions)?;
    }
    Ok(())
}

fn remove_owned_temp(path: &Path, expected: CacheFileIdentity) {
    let _ = remove_owned_path(path, expected);
}

fn remove_owned_path(path: &Path, expected: CacheFileIdentity) -> bool {
    let Ok((file, identity)) = open_cache_path(path, true, true, false, true) else {
        return false;
    };
    if identity != expected || validate_opened_cache_file(path, &file, identity).is_err() {
        return false;
    }
    drop(file);
    std::fs::remove_file(path).is_ok()
}

pub(super) fn replace_existing_atomic(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        windows_file_replace::replace(from, to)
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_file_replace {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    /// Replaces the destination using the Windows operation required for an existing target.
    pub(super) fn replace(from: &Path, to: &Path) -> io::Result<()> {
        let source = encoded_path(from)?;
        let destination = encoded_path(to)?;

        // SAFETY: both vectors are owned by this function, contain no interior NULs, and remain
        // alive for the synchronous call. MoveFileExW does not retain either pointer after it
        // returns; it receives no Rust-owned handle or mutable alias that could escape.
        let moved = unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if moved == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn encoded_path(path: &Path) -> io::Result<Vec<u16>> {
        let mut encoded = Vec::new();
        for unit in path.as_os_str().encode_wide() {
            if unit == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Windows path contains an embedded NUL",
                ));
            }
            encoded.push(unit);
        }
        encoded.push(0);
        Ok(encoded)
    }
}

fn sync_cache_directory(parent: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(())
    }
}

fn empty_capability_cache_file() -> ProviderCapabilityCacheFile {
    ProviderCapabilityCacheFile {
        schema_version: PROVIDER_CAPABILITY_CACHE_SCHEMA_VERSION,
        records: Vec::new(),
    }
}

fn valid_cached_record(
    record: &ProviderCapabilityCacheRecord,
    key: &ProviderCapabilityCacheKey,
    now: u64,
) -> Option<ProviderProtocolNegotiation> {
    if record.key != *key || !valid_cache_record_shape_at(record, now) {
        return None;
    }
    Some(ProviderProtocolNegotiation {
        contract: record.contract.clone().into_contract(),
        metadata: record.metadata.clone().into_metadata(),
    })
}

fn valid_cache_record_shape(record: &ProviderCapabilityCacheRecord) -> bool {
    if record.stored_at_unix_seconds > record.expires_at_unix_seconds
        || record
            .expires_at_unix_seconds
            .saturating_sub(record.stored_at_unix_seconds)
            > PROVIDER_CAPABILITY_CACHE_TTL_SECONDS
    {
        return false;
    }
    let contract = record.contract.clone().into_contract();
    valid_persisted_cache_contract(
        &record.key,
        &contract,
        record.metadata.api_protocol,
        record.metadata.profile,
    )
}

fn valid_cache_record_shape_at(record: &ProviderCapabilityCacheRecord, now: u64) -> bool {
    valid_cache_record_shape(record)
        && record.stored_at_unix_seconds <= now
        && record.expires_at_unix_seconds > now
}

fn valid_negotiation_for_cache(
    key: &ProviderCapabilityCacheKey,
    negotiation: &ProviderProtocolNegotiation,
) -> Option<()> {
    valid_cache_key(key).then_some(())?;
    valid_cache_contract(key, &negotiation.contract, &negotiation.metadata).then_some(())
}

fn valid_cache_key(key: &ProviderCapabilityCacheKey) -> bool {
    !key.provider_name.trim().is_empty()
        && !key.model_name.trim().is_empty()
        && is_sha256_hex(&key.endpoint_sha256)
        && !matches!(key.api_protocol, ProviderApiProtocol::Declared)
        && key.adapter_version == PROVIDER_ADAPTER_VERSION
        && key.probe_contract_version == CAPABILITY_PROBE_CONTRACT_VERSION
        && key.max_context_tokens > 0
        && key.max_context_tokens <= MAX_CONFIGURED_CONTEXT_TOKENS
        && key.max_output_tokens > 0
        && key.max_output_tokens < key.max_context_tokens
        && key.max_output_tokens <= MAX_CONFIGURED_OUTPUT_TOKENS
}

fn valid_cache_contract(
    key: &ProviderCapabilityCacheKey,
    contract: &ProviderProtocolContract,
    metadata: &ProviderCapabilityMetadata,
) -> bool {
    if !valid_cache_key(key)
        || metadata.api_protocol != key.api_protocol
        || metadata.cache_hit
        || metadata.profile_attempts == 0
        || !metadata
            .probe_usage
            .cost_estimate
            .is_none_or(|cost| cost.is_finite() && cost >= 0.0)
        || contract.max_context_tokens != key.max_context_tokens
        || contract.max_output_tokens != key.max_output_tokens
        || !contract.supports_tools
        || contract.supports_required_tool_choice
        || contract.supports_json_mode
        || contract.supports_system_message
        || !contract.supports_developer_message
        || contract.max_tools_per_request == 0
        || contract.max_tools_per_request > DEFAULT_MAX_TOOLS_PER_REQUEST
        || !matches!(
            (key.api_protocol, contract.tool_reasoning_mode),
            (
                ProviderApiProtocol::OpenAiResponses,
                ProviderToolReasoningMode::DisabledForToolCalls
            ) | (
                ProviderApiProtocol::OpenAiChatCompletions,
                ProviderToolReasoningMode::Unspecified
            ) | (
                ProviderApiProtocol::OpenAiChatCompletions,
                ProviderToolReasoningMode::DisabledForToolCalls
            ) | (
                ProviderApiProtocol::OpenAiChatCompletions,
                ProviderToolReasoningMode::ReplayReasoningContent
            ) | (
                ProviderApiProtocol::OpenAiResponses,
                ProviderToolReasoningMode::ReplayResponsesItems
            )
        )
        || (contract.supports_parallel_tool_calls
            && (!contract.supports_tools || contract.max_tools_per_request < 2))
        || (contract.supports_required_tool_choice && !contract.supports_tools)
        || (contract.supports_strict_tool_schema && !contract.supports_tools)
    {
        return false;
    }
    match metadata.profile {
        ProviderCapabilityProfile::StrictParallel => {
            contract.supports_strict_tool_schema && contract.supports_parallel_tool_calls
        }
        ProviderCapabilityProfile::StrictSingle => {
            contract.supports_strict_tool_schema && !contract.supports_parallel_tool_calls
        }
        ProviderCapabilityProfile::NonStrictParallel => {
            !contract.supports_strict_tool_schema && contract.supports_parallel_tool_calls
        }
        ProviderCapabilityProfile::NonStrictSingle => {
            !contract.supports_strict_tool_schema && !contract.supports_parallel_tool_calls
        }
        ProviderCapabilityProfile::Declared => false,
    }
}

fn valid_persisted_cache_contract(
    key: &ProviderCapabilityCacheKey,
    contract: &ProviderProtocolContract,
    api_protocol: ProviderApiProtocol,
    profile: ProviderCapabilityProfile,
) -> bool {
    let metadata = ProviderCapabilityMetadata {
        api_protocol,
        profile,
        cache_hit: false,
        profile_attempts: 1,
        fallback_count: 0,
        probe_usage: ModelUsage::default(),
        probe_attempt_metadata: ProviderAttemptMetadata::zero(),
        cache_observations: Vec::new(),
    };
    valid_cache_contract(key, contract, &metadata)
}

fn normalize_endpoint(endpoint: &str) -> String {
    endpoint.trim().trim_end_matches('/').to_string()
}

fn provider_fingerprint_for_probe_key(key: &ProviderCapabilityProbeKey) -> String {
    let material = format!(
        "singularity-provider-fingerprint-v3\nprovider_name={}\nendpoint_sha256={}\nadapter_version={}\nprobe_contract_version={}\nmax_context_tokens={}\nmax_output_tokens={}\nreasoning_effort={}\nreasoning_variant_enabled={}\nwire_reasoning_effort={}\ntool_reasoning_mode={}\nsupports_developer_role={}\nsupports_tool_choice={}\nrequires_reasoning_content_for_tool_calls={}\nrequires_assistant_content_for_tool_calls={}",
        key.provider_name,
        key.endpoint_sha256,
        key.adapter_version,
        key.probe_contract_version,
        key.max_context_tokens,
        key.max_output_tokens,
        key.reasoning_effort.as_deref().unwrap_or("off"),
        key.reasoning_variant_enabled,
        key.wire_reasoning_effort.as_deref().unwrap_or("none"),
        provider_tool_reasoning_mode_name(key.tool_reasoning_mode),
        key.supports_developer_role,
        key.supports_tool_choice,
        key.requires_reasoning_content_for_tool_calls,
        key.requires_assistant_content_for_tool_calls,
    );
    format!("sha256:{}", sha256_hex(&material))
}

fn model_fingerprint_for_probe_key(key: &ProviderCapabilityProbeKey) -> String {
    let material = format!(
        "singularity-model-fingerprint-v2\neffective_model={}\nreasoning_effort={}\nreasoning_variant_enabled={}\nwire_reasoning_effort={}\nsupports_developer_role={}\nsupports_tool_choice={}\nrequires_reasoning_content_for_tool_calls={}\nrequires_assistant_content_for_tool_calls={}",
        key.model_name,
        key.reasoning_effort.as_deref().unwrap_or("off"),
        key.reasoning_variant_enabled,
        key.wire_reasoning_effort.as_deref().unwrap_or("none"),
        key.supports_developer_role,
        key.supports_tool_choice,
        key.requires_reasoning_content_for_tool_calls,
        key.requires_assistant_content_for_tool_calls,
    );
    format!("sha256:{}", sha256_hex(&material))
}

fn negotiation_fingerprint_for_probe_key_and_contract(
    probe_key: &ProviderCapabilityProbeKey,
    api_protocol: ProviderApiProtocol,
    contract: &ProviderProtocolContract,
) -> String {
    let provider_fingerprint = provider_fingerprint_for_probe_key(probe_key);
    let model_fingerprint = model_fingerprint_for_probe_key(probe_key);
    let material = format!(
        "singularity-negotiation-fingerprint-v1\nprovider_fingerprint={}\nmodel_fingerprint={}\napi_protocol={}\nsupports_tools={}\nsupports_parallel_tool_calls={}\nsupports_required_tool_choice={}\nsupports_strict_tool_schema={}\ntool_reasoning_mode={}\nmax_tools_per_request={}\nsupports_json_mode={}\nsupports_system_message={}\nsupports_developer_message={}\ncontract_max_context_tokens={}\ncontract_max_output_tokens={}",
        provider_fingerprint,
        model_fingerprint,
        provider_api_protocol_name(api_protocol),
        contract.supports_tools,
        contract.supports_parallel_tool_calls,
        contract.supports_required_tool_choice,
        contract.supports_strict_tool_schema,
        provider_tool_reasoning_mode_name(contract.tool_reasoning_mode),
        contract.max_tools_per_request,
        contract.supports_json_mode,
        contract.supports_system_message,
        contract.supports_developer_message,
        contract.max_context_tokens,
        contract.max_output_tokens,
    );
    format!("sha256:{}", sha256_hex(&material))
}

fn provider_api_protocol_name(protocol: ProviderApiProtocol) -> &'static str {
    match protocol {
        ProviderApiProtocol::Declared => "declared",
        ProviderApiProtocol::OpenAiResponses => "open_ai_responses",
        ProviderApiProtocol::OpenAiChatCompletions => "open_ai_chat_completions",
    }
}

fn provider_tool_reasoning_mode_name(mode: ProviderToolReasoningMode) -> &'static str {
    match mode {
        ProviderToolReasoningMode::Unspecified => "unspecified",
        ProviderToolReasoningMode::DisabledForToolCalls => "disabled_for_tool_calls",
        ProviderToolReasoningMode::ReplayReasoningContent => "replay_reasoning_content",
        ProviderToolReasoningMode::ReplayResponsesItems => "replay_responses_items",
    }
}

pub(super) fn sha256_hex(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn unix_time_seconds() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

pub(super) struct InMemoryProviderCapabilityCacheState {
    entries: HashMap<ProviderCapabilityCacheKey, InMemoryProviderCapabilityCacheEntry>,
    pub(super) tombstones: HashMap<ProviderCapabilityCacheKey, u64>,
    next_epoch: u64,
}

impl InMemoryProviderCapabilityCacheState {
    pub(super) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            tombstones: HashMap::new(),
            next_epoch: 0,
        }
    }

    fn epoch(&self, key: &ProviderCapabilityCacheKey) -> u64 {
        self.tombstones.get(key).copied().unwrap_or_default()
    }

    fn invalidate(&mut self, key: &ProviderCapabilityCacheKey) -> u64 {
        self.next_epoch = self.next_epoch.wrapping_add(1).max(1);
        self.entries.remove(key);
        self.tombstones.insert(key.clone(), self.next_epoch);
        self.next_epoch
    }
}

#[derive(Clone)]
pub(super) struct BoundProviderProtocolNegotiation {
    pub(super) key: ProviderCapabilityCacheKey,
    pub(super) negotiation: ProviderProtocolNegotiation,
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("config", &self.config)
            .field("client", &"[redacted]")
            .field("runtime", &"[shared]")
            .field("tool_capability_cache", &"[redacted]")
            .field("tool_capability_probe_in_flight", &"[redacted]")
            .field("persistent_capability_cache", &"[redacted]")
            .finish()
    }
}

#[derive(Clone)]
enum CapabilityProbeCompletion {
    Result(Box<Result<BoundProviderProtocolNegotiation, ProviderError>>),
    OwnerCancelled,
}

pub(super) struct CapabilityProbeState {
    completion: Mutex<Option<CapabilityProbeCompletion>>,
    participants: Mutex<usize>,
    wake: Condvar,
}

impl CapabilityProbeState {
    pub(super) fn new() -> Self {
        Self {
            completion: Mutex::new(None),
            participants: Mutex::new(1),
            wake: Condvar::new(),
        }
    }

    fn join(&self) {
        let mut participants = self
            .participants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *participants = participants.saturating_add(1);
    }

    fn leave(&self) -> bool {
        let mut participants = self
            .participants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *participants = participants.saturating_sub(1);
        *participants == 0
            && self
                .completion
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some()
    }

    fn wait(
        &self,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<CapabilityProbeCompletion, ProviderError> {
        let mut completion = self
            .completion
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if cancellation.is_cancelled() {
                return Err(capability_probe_cancelled_error());
            }
            if Instant::now() >= deadline {
                return Err(capability_probe_deadline_error());
            }
            if let Some(completion) = completion.as_ref() {
                return Ok(completion.clone());
            }
            let wait_duration = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(PROVIDER_CANCELLATION_POLL_MS));
            completion = match self.wake.wait_timeout(completion, wait_duration) {
                Ok((completion, _)) => completion,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    fn complete(&self, completion: CapabilityProbeCompletion) {
        let mut current = self
            .completion
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if current.is_none() {
            *current = Some(completion);
        }
        self.wake.notify_all();
    }
}

struct CapabilityProbeOwnerGuard {
    in_flight: Arc<Mutex<HashMap<ProviderCapabilityProbeKey, Arc<CapabilityProbeState>>>>,
    probe_key: ProviderCapabilityProbeKey,
    state: Arc<CapabilityProbeState>,
    armed: bool,
}

impl CapabilityProbeOwnerGuard {
    pub(super) fn new(
        in_flight: Arc<Mutex<HashMap<ProviderCapabilityProbeKey, Arc<CapabilityProbeState>>>>,
        probe_key: ProviderCapabilityProbeKey,
        state: Arc<CapabilityProbeState>,
    ) -> Self {
        Self {
            in_flight,
            probe_key,
            state,
            armed: true,
        }
    }

    fn finish(mut self, completion: CapabilityProbeCompletion) {
        let owner_cancelled = matches!(completion, CapabilityProbeCompletion::OwnerCancelled);
        self.state.complete(completion);
        self.state.leave();
        if owner_cancelled {
            self.remove_state_unconditionally();
        } else {
            self.remove_state();
        }
        self.armed = false;
    }

    fn remove_state(&self) {
        if !self.state_is_idle() {
            return;
        }
        self.remove_state_unconditionally();
    }

    fn remove_state_unconditionally(&self) {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if in_flight
            .get(&self.probe_key)
            .is_some_and(|current| Arc::ptr_eq(current, &self.state))
        {
            in_flight.remove(&self.probe_key);
        }
    }

    fn state_is_idle(&self) -> bool {
        let participants = self
            .state
            .participants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *participants == 0
    }
}

impl Drop for CapabilityProbeOwnerGuard {
    fn drop(&mut self) {
        if self.armed {
            self.state
                .complete(CapabilityProbeCompletion::OwnerCancelled);
            self.state.leave();
            self.remove_state_unconditionally();
        }
    }
}

impl OpenAiProvider {
    fn cached_tool_capability_negotiation(
        &self,
        key: &ProviderCapabilityCacheKey,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<CapabilityCacheLookup, ProviderError> {
        if cancellation.is_cancelled() {
            return Err(capability_probe_cancelled_error());
        }
        if Instant::now() >= deadline {
            return Err(capability_probe_deadline_error());
        }
        let now = Instant::now();
        let mut cache = self
            .tool_capability_cache
            .lock()
            .map_err(|_| provider_capability_cache_error())?;
        if let Some(entry) = cache.entries.get(key)
            && entry.expires_at > now
        {
            return Ok(CapabilityCacheLookup::Hit(
                Box::new(BoundProviderProtocolNegotiation {
                    key: key.clone(),
                    negotiation: cache_hit_negotiation(entry.negotiation.clone()),
                }),
                cache_observation(key.api_protocol, ProviderCapabilityCacheLookupResult::Hit),
            ));
        }
        cache.entries.remove(key);
        if cache.tombstones.contains_key(key) {
            return Ok(CapabilityCacheLookup::Miss(cache_observation(
                key.api_protocol,
                ProviderCapabilityCacheLookupResult::Miss,
            )));
        }
        drop(cache);

        let Some(persistent_cache) = &self.persistent_capability_cache else {
            return Ok(CapabilityCacheLookup::Miss(cache_observation(
                key.api_protocol,
                ProviderCapabilityCacheLookupResult::Miss,
            )));
        };
        let miss_observation =
            cache_observation(key.api_protocol, ProviderCapabilityCacheLookupResult::Miss);
        let loaded = match persistent_cache.load(key, cancellation, Some(deadline)) {
            Ok(loaded) => loaded,
            Err(ProviderCapabilityCacheError::Cancelled) => {
                return Err(prepend_cache_observations_to_error(
                    capability_probe_cancelled_error(),
                    [miss_observation.clone()],
                ));
            }
            Err(ProviderCapabilityCacheError::Deadline) if Instant::now() >= deadline => {
                return Err(prepend_cache_observations_to_error(
                    capability_probe_deadline_error(),
                    [miss_observation.clone()],
                ));
            }
            Err(ProviderCapabilityCacheError::Deadline)
            | Err(ProviderCapabilityCacheError::Unavailable)
            | Err(ProviderCapabilityCacheError::Invalid) => None,
        };
        let Some((negotiation, remaining)) = loaded else {
            return Ok(CapabilityCacheLookup::Miss(miss_observation));
        };
        if cancellation.is_cancelled() {
            return Err(prepend_cache_observations_to_error(
                capability_probe_cancelled_error(),
                [miss_observation.clone()],
            ));
        }
        if Instant::now() >= deadline {
            return Err(prepend_cache_observations_to_error(
                capability_probe_deadline_error(),
                [miss_observation.clone()],
            ));
        }
        let mut cache = self.tool_capability_cache.lock().map_err(|_| {
            prepend_cache_observations_to_error(
                provider_capability_cache_error(),
                [miss_observation.clone()],
            )
        })?;
        if cache.tombstones.contains_key(key) {
            return Ok(CapabilityCacheLookup::Miss(miss_observation));
        }
        cache.entries.insert(
            key.clone(),
            InMemoryProviderCapabilityCacheEntry {
                negotiation: negotiation.clone(),
                expires_at: Instant::now() + remaining,
            },
        );
        Ok(CapabilityCacheLookup::Hit(
            Box::new(BoundProviderProtocolNegotiation {
                key: key.clone(),
                negotiation: cache_hit_negotiation(negotiation),
            }),
            cache_observation(key.api_protocol, ProviderCapabilityCacheLookupResult::Hit),
        ))
    }

    fn capability_probe_key(&self, model_name: &str) -> ProviderCapabilityProbeKey {
        ProviderCapabilityProbeKey {
            provider_name: self.config.provider_name.clone(),
            endpoint_sha256: sha256_hex(&normalize_endpoint(&self.config.base_url)),
            model_name: model_name.to_string(),
            adapter_version: PROVIDER_ADAPTER_VERSION,
            probe_contract_version: CAPABILITY_PROBE_CONTRACT_VERSION,
            max_context_tokens: self.config.max_context_tokens,
            max_output_tokens: self.config.max_output_tokens,
            reasoning_effort: self
                .selected_model
                .as_ref()
                .and_then(|selection| selection.reasoning_variant.clone()),
            reasoning_variant_enabled: self
                .selected_model
                .as_ref()
                .is_some_and(|selection| selection.reasoning_enabled),
            wire_reasoning_effort: self
                .selected_model
                .as_ref()
                .and_then(|selection| selection.wire_reasoning_effort.clone()),
            tool_reasoning_mode: self
                .selected_model
                .as_ref()
                .map_or(ProviderToolReasoningMode::Unspecified, |selection| {
                    selection.tool_reasoning_mode
                }),
            supports_developer_role: self
                .selected_model
                .as_ref()
                .is_none_or(|selection| selection.supports_developer_role),
            supports_tool_choice: self
                .selected_model
                .as_ref()
                .is_none_or(|selection| selection.supports_tool_choice),
            requires_reasoning_content_for_tool_calls: self
                .selected_model
                .as_ref()
                .is_some_and(|selection| selection.requires_reasoning_content_for_tool_calls),
            requires_assistant_content_for_tool_calls: self
                .selected_model
                .as_ref()
                .is_some_and(|selection| selection.requires_assistant_content_for_tool_calls),
        }
    }

    pub(super) fn capability_cache_key(
        &self,
        model_name: &str,
        api_protocol: ProviderApiProtocol,
    ) -> ProviderCapabilityCacheKey {
        let endpoint = match api_protocol {
            ProviderApiProtocol::OpenAiResponses => responses_endpoint(&self.config.base_url),
            ProviderApiProtocol::Declared | ProviderApiProtocol::OpenAiChatCompletions => {
                self.config.endpoint()
            }
        };
        let mut key = self
            .capability_probe_key(model_name)
            .cache_key(api_protocol);
        key.endpoint_sha256 = sha256_hex(&normalize_endpoint(&endpoint));
        key
    }

    /// 返回不含原 endpoint、API key 或 probe 内容的 provider/model 稳定指纹。
    pub fn runtime_fingerprint(&self, effective_model: Option<&str>) -> ProviderRuntimeFingerprint {
        let model_name = effective_model.unwrap_or(&self.config.model_name);
        let probe_key = self.capability_probe_key(model_name);
        ProviderRuntimeFingerprint {
            provider_fingerprint: provider_fingerprint_for_probe_key(&probe_key),
            model_fingerprint: model_fingerprint_for_probe_key(&probe_key),
            negotiation_fingerprint: None,
        }
    }

    /// 将已协商协议和本地 contract 投影为稳定的脱敏 runtime 指纹。
    pub fn runtime_fingerprint_for_negotiation(
        &self,
        effective_model: Option<&str>,
        negotiation: &ProviderProtocolNegotiation,
    ) -> ProviderRuntimeFingerprint {
        let model_name = effective_model.unwrap_or(&self.config.model_name);
        let probe_key = self.capability_probe_key(model_name);
        ProviderRuntimeFingerprint {
            provider_fingerprint: provider_fingerprint_for_probe_key(&probe_key),
            model_fingerprint: model_fingerprint_for_probe_key(&probe_key),
            negotiation_fingerprint: Some(negotiation_fingerprint_for_probe_key_and_contract(
                &probe_key,
                negotiation.metadata.api_protocol,
                &negotiation.contract,
            )),
        }
    }

    fn remember_tool_capability_negotiation(
        &self,
        key: &ProviderCapabilityCacheKey,
        negotiation: &ProviderProtocolNegotiation,
        expected_epoch: u64,
    ) -> Result<bool, ProviderError> {
        if valid_negotiation_for_cache(key, negotiation).is_none() {
            return Ok(false);
        }
        let mut cache = self
            .tool_capability_cache
            .lock()
            .map_err(|_| provider_capability_cache_error())?;
        if cache.epoch(key) != expected_epoch {
            return Ok(false);
        }
        cache.entries.insert(
            key.clone(),
            InMemoryProviderCapabilityCacheEntry {
                negotiation: negotiation.clone(),
                expires_at: Instant::now()
                    + Duration::from_secs(PROVIDER_CAPABILITY_CACHE_TTL_SECONDS),
            },
        );
        Ok(true)
    }

    fn remember_cached_tool_capability_negotiation(
        &self,
        key: &ProviderCapabilityCacheKey,
        negotiation: &ProviderProtocolNegotiation,
        expected_epoch: u64,
        remaining: Duration,
    ) -> Result<bool, ProviderError> {
        let mut cache = self
            .tool_capability_cache
            .lock()
            .map_err(|_| provider_capability_cache_error())?;
        if cache.epoch(key) != expected_epoch {
            return Ok(false);
        }
        cache.entries.insert(
            key.clone(),
            InMemoryProviderCapabilityCacheEntry {
                negotiation: negotiation.clone(),
                expires_at: Instant::now() + remaining,
            },
        );
        Ok(true)
    }

    fn persist_tool_capability_negotiation(
        &self,
        key: &ProviderCapabilityCacheKey,
        negotiation: &ProviderProtocolNegotiation,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<(), ProviderCapabilityCacheError> {
        if valid_negotiation_for_cache(key, negotiation).is_none() {
            return Err(ProviderCapabilityCacheError::Invalid);
        }
        if let Some(persistent_cache) = &self.persistent_capability_cache {
            let _global_lock =
                persistent_cache.acquire_global_lock(true, cancellation, Some(deadline))?;
            persistent_cache.store_locked(key, negotiation, cancellation, Some(deadline))
        } else {
            Ok(())
        }
    }

    pub(super) fn invalidate_tool_capability_negotiation(
        &self,
        key: &ProviderCapabilityCacheKey,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<(), ProviderError> {
        {
            let mut cache = self
                .tool_capability_cache
                .lock()
                .map_err(|_| provider_capability_cache_error())?;
            cache.invalidate(key);
        }
        let Some(persistent_cache) = &self.persistent_capability_cache else {
            return Ok(());
        };
        if let Err(error) = persistent_cache.invalidate(key, cancellation, Some(deadline)) {
            self.record_cache_diagnostic(error);
            return Err(match error {
                ProviderCapabilityCacheError::Cancelled => provider_cancelled_error(),
                ProviderCapabilityCacheError::Deadline => {
                    provider_capability_cache_invalidation_deadline_error()
                }
                ProviderCapabilityCacheError::Unavailable
                | ProviderCapabilityCacheError::Invalid => {
                    provider_capability_cache_invalidation_error()
                }
            });
        }
        Ok(())
    }

    fn current_cache_epoch(&self, key: &ProviderCapabilityCacheKey) -> Result<u64, ProviderError> {
        self.tool_capability_cache
            .lock()
            .map(|cache| cache.epoch(key))
            .map_err(|_| provider_capability_cache_error())
    }

    fn remove_cached_entry(&self, key: &ProviderCapabilityCacheKey) {
        if let Ok(mut cache) = self.tool_capability_cache.lock() {
            cache.entries.remove(key);
        }
    }

    fn clear_cache_tombstone(&self, key: &ProviderCapabilityCacheKey, epoch: u64) {
        if let Ok(mut cache) = self.tool_capability_cache.lock()
            && cache.epoch(key) == epoch
        {
            cache.tombstones.remove(key);
        }
    }

    fn record_cache_diagnostic(&self, error: ProviderCapabilityCacheError) {
        let diagnostic = match error {
            ProviderCapabilityCacheError::Cancelled => "cancelled",
            ProviderCapabilityCacheError::Deadline => "deadline",
            ProviderCapabilityCacheError::Unavailable => "unavailable",
            ProviderCapabilityCacheError::Invalid => "invalid",
        };
        if let Ok(mut current) = self.capability_cache_diagnostic.lock() {
            *current = Some(diagnostic.to_string());
        }
    }

    fn probe_and_remember_as_owner(
        &self,
        model_name: &str,
        cancellation: &CancellationToken,
        epochs: &HashMap<ProviderCapabilityCacheKey, u64>,
        deadline: Instant,
        cache_observations: Vec<ProviderCapabilityCacheObservation>,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<BoundProviderProtocolNegotiation, ProviderError> {
        let result = self.probe_tool_capabilities(model_name, cancellation, deadline, on_attempt);
        let negotiation = match result {
            Ok(negotiation) => negotiation,
            Err(error) => {
                return Err(prepend_cache_observations_to_error(
                    error,
                    cache_observations,
                ));
            }
        };
        if cancellation.is_cancelled() {
            return Err(prepend_cache_observations_to_error(
                capability_probe_cancelled_error(),
                cache_observations.clone(),
            ));
        }
        if Instant::now() >= deadline {
            return Err(prepend_cache_observations_to_error(
                capability_probe_deadline_error(),
                cache_observations.clone(),
            ));
        }
        let cache_key = self.capability_cache_key(model_name, negotiation.metadata.api_protocol);
        let epoch = epochs.get(&cache_key).copied().unwrap_or_default();
        let remembered = self
            .remember_tool_capability_negotiation(&cache_key, &negotiation, epoch)
            .map_err(|error| {
                prepend_cache_observations_to_error(error, cache_observations.clone())
            })?;
        if !remembered {
            if cancellation.is_cancelled() {
                return Err(prepend_cache_observations_to_error(
                    capability_probe_cancelled_error(),
                    cache_observations.clone(),
                ));
            }
            if Instant::now() >= deadline {
                return Err(prepend_cache_observations_to_error(
                    capability_probe_deadline_error(),
                    cache_observations.clone(),
                ));
            }
            let mut bound = BoundProviderProtocolNegotiation {
                key: cache_key,
                negotiation,
            };
            prepend_cache_observations(&mut bound.negotiation.metadata, cache_observations);
            return Ok(bound);
        }
        if let Some(_persistent_cache) = &self.persistent_capability_cache {
            match self.persist_tool_capability_negotiation(
                &cache_key,
                &negotiation,
                cancellation,
                deadline,
            ) {
                Ok(()) => self.clear_cache_tombstone(&cache_key, epoch),
                Err(ProviderCapabilityCacheError::Cancelled) => {
                    self.remove_cached_entry(&cache_key);
                    return Err(prepend_cache_observations_to_error(
                        capability_probe_cancelled_error(),
                        cache_observations.clone(),
                    ));
                }
                Err(ProviderCapabilityCacheError::Deadline) => {
                    self.remove_cached_entry(&cache_key);
                    return Err(prepend_cache_observations_to_error(
                        capability_probe_deadline_error(),
                        cache_observations.clone(),
                    ));
                }
                Err(error) => self.record_cache_diagnostic(error),
            }
        } else {
            self.clear_cache_tombstone(&cache_key, epoch);
        }
        if cancellation.is_cancelled() {
            self.remove_cached_entry(&cache_key);
            let _ = self.invalidate_tool_capability_negotiation(&cache_key, cancellation, deadline);
            return Err(prepend_cache_observations_to_error(
                capability_probe_cancelled_error(),
                cache_observations.clone(),
            ));
        }
        if Instant::now() >= deadline {
            self.remove_cached_entry(&cache_key);
            let _ = self.invalidate_tool_capability_negotiation(&cache_key, cancellation, deadline);
            return Err(prepend_cache_observations_to_error(
                capability_probe_deadline_error(),
                cache_observations.clone(),
            ));
        }
        let mut bound = BoundProviderProtocolNegotiation {
            key: cache_key,
            negotiation,
        };
        prepend_cache_observations(&mut bound.negotiation.metadata, cache_observations);
        Ok(bound)
    }

    fn probe_as_persistent_owner(
        &self,
        model_name: &str,
        cancellation: &CancellationToken,
        epochs: &HashMap<ProviderCapabilityCacheKey, u64>,
        deadline: Instant,
        mut cache_observations: Vec<ProviderCapabilityCacheObservation>,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<BoundProviderProtocolNegotiation, ProviderError> {
        let Some(persistent_cache) = &self.persistent_capability_cache else {
            return self.probe_and_remember_as_owner(
                model_name,
                cancellation,
                epochs,
                deadline,
                cache_observations,
                on_attempt,
            );
        };
        let protocols = self.protocol_candidates();
        let candidate_keys = if protocols.is_empty() {
            vec![self.capability_cache_key(model_name, ProviderApiProtocol::OpenAiChatCompletions)]
        } else {
            protocols
                .into_iter()
                .map(|api_protocol| self.capability_cache_key(model_name, api_protocol))
                .collect::<Vec<_>>()
        };
        let mut lock_keys = candidate_keys.clone();
        lock_keys.sort_by_key(|key| persistent_cache.key_lock_path(key));
        lock_keys.dedup();
        let mut key_locks = Vec::with_capacity(lock_keys.len());
        for key in &lock_keys {
            match persistent_cache.acquire_key_lock(key, cancellation, Some(deadline)) {
                Ok(lock) => key_locks.push(lock),
                Err(ProviderCapabilityCacheError::Cancelled) => {
                    return Err(prepend_cache_observations_to_error(
                        capability_probe_cancelled_error(),
                        cache_observations.clone(),
                    ));
                }
                Err(ProviderCapabilityCacheError::Deadline) => {
                    return Err(prepend_cache_observations_to_error(
                        capability_probe_deadline_error(),
                        cache_observations.clone(),
                    ));
                }
                Err(ProviderCapabilityCacheError::Unavailable)
                | Err(ProviderCapabilityCacheError::Invalid) => {
                    drop(key_locks);
                    return self.probe_and_remember_as_owner(
                        model_name,
                        cancellation,
                        epochs,
                        deadline,
                        cache_observations,
                        on_attempt,
                    );
                }
            }
        }
        if let Some(now) = unix_time_seconds() {
            for cache_key in candidate_keys {
                let loaded = persistent_cache.load_locked_with_global_lock(
                    &cache_key,
                    now,
                    cancellation,
                    Some(deadline),
                );
                if let Ok(Some((negotiation, remaining))) = loaded {
                    if cancellation.is_cancelled() {
                        return Err(prepend_cache_observations_to_error(
                            capability_probe_cancelled_error(),
                            cache_observations.clone(),
                        ));
                    }
                    if Instant::now() >= deadline {
                        return Err(prepend_cache_observations_to_error(
                            capability_probe_deadline_error(),
                            cache_observations.clone(),
                        ));
                    }
                    let epoch = epochs.get(&cache_key).copied().unwrap_or_default();
                    let remembered = self
                        .remember_cached_tool_capability_negotiation(
                            &cache_key,
                            &negotiation,
                            epoch,
                            remaining,
                        )
                        .map_err(|error| {
                            prepend_cache_observations_to_error(error, cache_observations.clone())
                        })?;
                    if remembered {
                        mark_cache_observation_hit(&mut cache_observations, cache_key.api_protocol);
                        let mut negotiation = negotiation;
                        prepend_cache_observations(&mut negotiation.metadata, cache_observations);
                        return Ok(BoundProviderProtocolNegotiation {
                            key: cache_key,
                            negotiation: cache_hit_negotiation(negotiation),
                        });
                    }
                }
            }
        }
        self.probe_and_remember_as_owner(
            model_name,
            cancellation,
            epochs,
            deadline,
            cache_observations,
            on_attempt,
        )
    }

    pub(super) fn negotiate_openai_tool_capabilities_bound_observed(
        &self,
        model_name: &str,
        cancellation: &CancellationToken,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<BoundProviderProtocolNegotiation, ProviderError> {
        let probe_key = self.capability_probe_key(model_name);
        let deadline = Instant::now() + self.capability_probe_deadline;
        let mut epochs = HashMap::new();
        for api_protocol in self.protocol_candidates() {
            let key = self.capability_cache_key(model_name, api_protocol);
            epochs.insert(key.clone(), self.current_cache_epoch(&key)?);
        }
        let mut cache_observations = Vec::new();
        loop {
            if cancellation.is_cancelled() {
                return Err(prepend_cache_observations_to_error(
                    capability_probe_cancelled_error(),
                    cache_observations,
                ));
            }
            if Instant::now() >= deadline {
                return Err(prepend_cache_observations_to_error(
                    capability_probe_deadline_error(),
                    cache_observations,
                ));
            }
            for api_protocol in self.protocol_candidates() {
                let cache_key = self.capability_cache_key(model_name, api_protocol);
                match self.cached_tool_capability_negotiation(&cache_key, cancellation, deadline) {
                    Ok(CapabilityCacheLookup::Hit(cached, observation)) => {
                        cache_observations.push(observation);
                        return prepend_cache_observations_to_result(
                            Ok(*cached),
                            cache_observations,
                        );
                    }
                    Ok(CapabilityCacheLookup::Miss(observation)) => {
                        cache_observations.push(observation);
                    }
                    Err(error) => {
                        return Err(prepend_cache_observations_to_error(
                            error,
                            cache_observations,
                        ));
                    }
                }
            }
            let mut in_flight = self.tool_capability_probe_in_flight.lock().map_err(|_| {
                prepend_cache_observations_to_error(
                    provider_capability_cache_error(),
                    cache_observations.clone(),
                )
            })?;
            let (probe_state, owner) = if let Some(probe_state) = in_flight.get(&probe_key) {
                probe_state.join();
                (Arc::clone(probe_state), false)
            } else {
                let probe_state = Arc::new(CapabilityProbeState::new());
                in_flight.insert(probe_key.clone(), Arc::clone(&probe_state));
                (probe_state, true)
            };
            drop(in_flight);

            if owner {
                let owner_guard = CapabilityProbeOwnerGuard::new(
                    Arc::clone(&self.tool_capability_probe_in_flight),
                    probe_key.clone(),
                    Arc::clone(&probe_state),
                );
                let result = self.probe_as_persistent_owner(
                    model_name,
                    cancellation,
                    &epochs,
                    deadline,
                    cache_observations.clone(),
                    on_attempt,
                );
                let result = match result {
                    Err(error) => Err(self.invalidate_fresh_probe_rejection(
                        model_name,
                        cancellation,
                        deadline,
                        error,
                    )),
                    result => result,
                };
                let completion = match &result {
                    Ok(_) => CapabilityProbeCompletion::Result(Box::new(result.clone())),
                    Err(error) if capability_probe_owner_failure_requires_reselection(error) => {
                        CapabilityProbeCompletion::OwnerCancelled
                    }
                    Err(_) => CapabilityProbeCompletion::Result(Box::new(result.clone())),
                };
                owner_guard.finish(completion);
                return result;
            }

            let completion = match probe_state.wait(cancellation, deadline) {
                Ok(completion) => completion,
                Err(error) => {
                    if probe_state.leave() {
                        let mut in_flight = self
                            .tool_capability_probe_in_flight
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if in_flight
                            .get(&probe_key)
                            .is_some_and(|current| Arc::ptr_eq(current, &probe_state))
                        {
                            in_flight.remove(&probe_key);
                        }
                    }
                    return Err(prepend_cache_observations_to_error(
                        error,
                        cache_observations,
                    ));
                }
            };
            if probe_state.leave() {
                let mut in_flight = self
                    .tool_capability_probe_in_flight
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if in_flight
                    .get(&probe_key)
                    .is_some_and(|current| Arc::ptr_eq(current, &probe_state))
                {
                    in_flight.remove(&probe_key);
                }
            }
            match completion {
                // A waiter joins the owner's logical cache lookup; reuse the owner's closed
                // observation so single-flight callers retain the same typed outcome.
                CapabilityProbeCompletion::Result(result) => return *result,
                CapabilityProbeCompletion::OwnerCancelled => {
                    let mut in_flight = self
                        .tool_capability_probe_in_flight
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if in_flight
                        .get(&probe_key)
                        .is_some_and(|current| Arc::ptr_eq(current, &probe_state))
                    {
                        in_flight.remove(&probe_key);
                    }
                    continue;
                }
            }
        }
    }

    /// Fresh probe 的稳定能力拒绝也要清除对应实际 protocol key；调用点位于 key-lock
    /// owner 返回之后，避免在仍持有 probe lock 时递归获取同一锁。
    pub(super) fn invalidate_fresh_probe_rejection(
        &self,
        model_name: &str,
        cancellation: &CancellationToken,
        deadline: Instant,
        mut error: ProviderError,
    ) -> ProviderError {
        if !is_stable_capability_rejection(&error) {
            return error;
        }
        let Some(api_protocol) = error
            .capability_metadata
            .as_deref()
            .map(|metadata| metadata.api_protocol)
            .filter(|protocol| !matches!(protocol, ProviderApiProtocol::Declared))
        else {
            return error;
        };
        let key = self.capability_cache_key(model_name, api_protocol);
        if let Err(invalidation_error) =
            self.invalidate_tool_capability_negotiation(&key, cancellation, deadline)
        {
            error.error.validation_errors.push(
                invalidation_error
                    .error
                    .code
                    .unwrap_or_else(|| "provider_capability_cache_invalidation_failed".to_string()),
            );
        }
        error
    }

    fn probe_tool_capabilities(
        &self,
        model_name: &str,
        cancellation: &CancellationToken,
        deadline: Instant,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<ProviderProtocolNegotiation, ProviderError> {
        let protocols = self.protocol_candidates();
        let mut accumulated_metadata: Option<ProviderCapabilityMetadata> = None;
        for (index, api_protocol) in protocols.iter().copied().enumerate() {
            match self.probe_tool_capabilities_for_protocol(
                model_name,
                cancellation,
                api_protocol,
                deadline,
                on_attempt,
            ) {
                Ok(mut negotiation) => {
                    if let Some(metadata) = accumulated_metadata.take() {
                        merge_capability_metadata(&mut negotiation.metadata, &metadata);
                    }
                    negotiation.metadata.fallback_count = negotiation
                        .metadata
                        .fallback_count
                        .saturating_add(index as u32);
                    return Ok(negotiation);
                }
                Err(mut error)
                    if index + 1 < protocols.len()
                        && provider_protocol_fallback_allowed(&error) =>
                {
                    if let Some(metadata) = error.capability_metadata.take().map(|value| *value) {
                        match accumulated_metadata.as_mut() {
                            Some(accumulated) => merge_capability_metadata(accumulated, &metadata),
                            None => accumulated_metadata = Some(metadata),
                        }
                    }
                }
                Err(mut error) => {
                    if let Some(accumulated) = accumulated_metadata {
                        match error.capability_metadata.as_mut() {
                            Some(metadata) => merge_capability_metadata(metadata, &accumulated),
                            None => error.capability_metadata = Some(Box::new(accumulated)),
                        }
                    }
                    return Err(error);
                }
            }
        }
        Err(capability_probe_unsupported_error(ModelError::new(
            ModelErrorKind::UnsupportedCapability,
            "provider does not support native structured tool calls",
        )))
    }

    fn probe_tool_capabilities_for_protocol(
        &self,
        model_name: &str,
        cancellation: &CancellationToken,
        api_protocol: ProviderApiProtocol,
        deadline: Instant,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<ProviderProtocolNegotiation, ProviderError> {
        let mut probe_usage = ModelUsage::default();
        let mut probe_attempt_metadata = ProviderAttemptMetadata::zero();
        let profiles = capability_probe_profiles(
            &self.config,
            model_name,
            api_protocol,
            self.selected_model
                .as_ref()
                .map_or(ProviderToolReasoningMode::Unspecified, |selection| {
                    selection.tool_reasoning_mode
                }),
            self.selected_model
                .as_ref()
                .and_then(|selection| selection.reasoning_variant.as_deref()),
        );
        let profile_count = profiles.len();

        for (index, profile) in profiles.into_iter().enumerate() {
            if cancellation.is_cancelled() {
                return Err(provider_cancelled_error().with_capability_metadata(
                    capability_probe_metadata(
                        api_protocol,
                        profile.profile,
                        index as u32,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    ),
                ));
            }
            if Instant::now() >= deadline {
                return Err(capability_probe_deadline_error());
            }
            let local_validation = validate_model_request(&profile.request);
            if !local_validation.valid {
                return Err(capability_probe_definition_error(local_validation.errors)
                    .with_capability_metadata(capability_probe_metadata(
                        api_protocol,
                        profile.profile,
                        index as u32,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    )));
            }
            let mut completion = match self.complete_capability_probe(
                &profile.request,
                cancellation,
                &profile.contract,
                api_protocol,
                &mut probe_usage,
                &mut probe_attempt_metadata,
                deadline,
                on_attempt,
            ) {
                Ok(completion) => completion,
                Err(error) if is_capability_probe_profile_rejection(&error) => {
                    if index + 1 == profile_count {
                        return Err(capability_probe_failure(
                            error,
                            capability_probe_metadata(
                                api_protocol,
                                profile.profile,
                                index as u32 + 1,
                                index as u32,
                                &probe_usage,
                                &probe_attempt_metadata,
                            ),
                            "capability_profiles_exhausted",
                        ));
                    }
                    continue;
                }
                Err(error) => {
                    return Err(error.with_capability_metadata(capability_probe_metadata(
                        api_protocol,
                        profile.profile,
                        index as u32 + 1,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    )));
                }
            };
            let mut negotiated_profile =
                capability_probe_profile_match(&completion.response, &profile);
            let mut contract = profile.contract.clone();
            let replay_reasoning = matches!(
                profile.contract.tool_reasoning_mode,
                ProviderToolReasoningMode::ReplayReasoningContent
                    | ProviderToolReasoningMode::ReplayResponsesItems
            );
            if negotiated_profile.is_some()
                && completion.reasoning_content_present
                && !replay_reasoning
            {
                if self
                    .selected_model
                    .as_ref()
                    .is_some_and(|selection| selection.reasoning_enabled)
                {
                    return Err(capability_probe_tool_reasoning_error(
                        &completion.response,
                        "tool_reasoning_history_not_configured_for_selected_variant",
                    )
                    .with_capability_metadata(capability_probe_metadata(
                        api_protocol,
                        profile.profile,
                        index as u32 + 1,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    )));
                }
                contract.tool_reasoning_mode = ProviderToolReasoningMode::DisabledForToolCalls;
                completion = match self.complete_capability_probe(
                    &profile.request,
                    cancellation,
                    &contract,
                    api_protocol,
                    &mut probe_usage,
                    &mut probe_attempt_metadata,
                    deadline,
                    on_attempt,
                ) {
                    Ok(completion) => completion,
                    Err(error) => {
                        return Err(capability_probe_failure(
                            error,
                            capability_probe_metadata(
                                api_protocol,
                                profile.profile,
                                index as u32 + 1,
                                index as u32,
                                &probe_usage,
                                &probe_attempt_metadata,
                            ),
                            "tool_reasoning_disable_unsupported",
                        ));
                    }
                };
                if completion.reasoning_content_present {
                    return Err(capability_probe_tool_reasoning_error(
                        &completion.response,
                        "tool_reasoning_disable_not_honored",
                    )
                    .with_capability_metadata(capability_probe_metadata(
                        api_protocol,
                        profile.profile,
                        index as u32 + 1,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    )));
                }
                negotiated_profile = capability_probe_profile_match(&completion.response, &profile);
                if negotiated_profile.is_none() {
                    return Err(capability_probe_tool_reasoning_error(
                        &completion.response,
                        "tool_reasoning_disabled_profile_invalid",
                    )
                    .with_capability_metadata(capability_probe_metadata(
                        api_protocol,
                        profile.profile,
                        index as u32 + 1,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    )));
                }
            }
            if let Some(negotiated_profile) = negotiated_profile {
                if negotiated_profile != profile.profile {
                    contract.supports_parallel_tool_calls = false;
                }
                let continuation_request =
                    capability_probe_continuation_request(&profile, &completion.response);
                if replay_reasoning
                    && completion.reasoning_content_present
                    && continuation_request.provider_reasoning_history
                        != completion.response.provider_reasoning_history
                {
                    return Err(capability_probe_tool_reasoning_error(
                        &completion.response,
                        "tool_reasoning_history_not_bound_to_continuation",
                    )
                    .with_capability_metadata(capability_probe_metadata(
                        api_protocol,
                        profile.profile,
                        index as u32 + 1,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    )));
                }
                let continuation_validation = validate_model_request_with_capabilities(
                    &continuation_request,
                    Some(&contract),
                );
                if !continuation_validation.valid {
                    return Err(
                        capability_probe_definition_error(continuation_validation.errors)
                            .with_capability_metadata(capability_probe_metadata(
                                api_protocol,
                                profile.profile,
                                index as u32 + 1,
                                index as u32,
                                &probe_usage,
                                &probe_attempt_metadata,
                            )),
                    );
                }
                let continuation = match self.complete_capability_probe(
                    &continuation_request,
                    cancellation,
                    &contract,
                    api_protocol,
                    &mut probe_usage,
                    &mut probe_attempt_metadata,
                    deadline,
                    on_attempt,
                ) {
                    Ok(completion) => completion,
                    Err(error) if is_capability_probe_profile_rejection(&error) => {
                        if index + 1 == profile_count {
                            return Err(capability_probe_failure(
                                error,
                                capability_probe_metadata(
                                    api_protocol,
                                    profile.profile,
                                    index as u32 + 1,
                                    index as u32,
                                    &probe_usage,
                                    &probe_attempt_metadata,
                                ),
                                "capability_probe_multi_turn_tool_calls_unsupported",
                            ));
                        }
                        continue;
                    }
                    Err(error) => {
                        return Err(error.with_capability_metadata(capability_probe_metadata(
                            api_protocol,
                            profile.profile,
                            index as u32 + 1,
                            index as u32,
                            &probe_usage,
                            &probe_attempt_metadata,
                        )));
                    }
                };
                if continuation.reasoning_content_present && !replay_reasoning {
                    let error = capability_probe_tool_reasoning_error(
                        &continuation.response,
                        "tool_reasoning_content_present_after_tool_result",
                    );
                    if index + 1 == profile_count {
                        return Err(error.with_capability_metadata(capability_probe_metadata(
                            api_protocol,
                            profile.profile,
                            index as u32 + 1,
                            index as u32,
                            &probe_usage,
                            &probe_attempt_metadata,
                        )));
                    }
                    continue;
                }
                if !capability_probe_continuation_matches(&continuation.response, &profile) {
                    let error = capability_probe_continuation_error(&continuation.response);
                    if index + 1 == profile_count {
                        return Err(error.with_capability_metadata(capability_probe_metadata(
                            api_protocol,
                            profile.profile,
                            index as u32 + 1,
                            index as u32,
                            &probe_usage,
                            &probe_attempt_metadata,
                        )));
                    }
                    continue;
                }
                let negotiation = ProviderProtocolNegotiation {
                    contract,
                    metadata: capability_probe_metadata(
                        api_protocol,
                        negotiated_profile,
                        index as u32 + 1,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    ),
                };
                return Ok(negotiation);
            }
            if index + 1 == profile_count {
                return Err(capability_probe_response_error(&completion.response)
                    .with_capability_metadata(capability_probe_metadata(
                        api_protocol,
                        profile.profile,
                        index as u32 + 1,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    )));
            }
        }

        Err(capability_probe_unsupported_error(ModelError::new(
            ModelErrorKind::UnsupportedCapability,
            "provider does not support native structured tool calls",
        )))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn complete_capability_probe(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        contract: &ProviderProtocolContract,
        api_protocol: ProviderApiProtocol,
        probe_usage: &mut ModelUsage,
        probe_attempt_metadata: &mut ProviderAttemptMetadata,
        deadline: Instant,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<OpenAiCompletion, ProviderError> {
        let model_name = request
            .model_preferences
            .model_name
            .as_deref()
            .unwrap_or(&self.config.model_name);
        let result = self.complete_with_contract_details_until(
            request,
            cancellation,
            contract,
            api_protocol,
            model_name,
            Some(deadline),
            on_attempt,
        );
        match &result {
            Ok(completion) => {
                add_model_usage(probe_usage, &completion.response.usage);
                if let Some(metadata) = &completion.response.provider_attempt_metadata {
                    add_provider_attempt_metadata(probe_attempt_metadata, metadata);
                }
            }
            Err(error) => {
                if let Some(metadata) = &error.provider_attempt_metadata {
                    add_provider_attempt_metadata(probe_attempt_metadata, metadata);
                }
            }
        }
        result
    }
}

struct CapabilityProbeProfile {
    profile: ProviderCapabilityProfile,
    contract: ProviderProtocolContract,
    request: ModelTurnRequest,
    expected_calls: Vec<CapabilityProbeExpectedCall>,
    single_call_fallback: Option<ProviderCapabilityProfile>,
}

#[derive(Debug, Clone)]
struct CapabilityProbeExpectedCall {
    tool_name: &'static str,
    allowed_arguments: Vec<Value>,
}

fn capability_probe_profiles(
    config: &OpenAiProviderConfig,
    model_name: &str,
    api_protocol: ProviderApiProtocol,
    selected_tool_reasoning_mode: ProviderToolReasoningMode,
    selected_reasoning_variant: Option<&str>,
) -> Vec<CapabilityProbeProfile> {
    let base = config.protocol_contract();
    let schema_branch = |label: &str| {
        json!({
            "type": "object",
            "properties": {
                "probe": {
                    "type": "string",
                    "const": label
                },
                "values": {
                    "type": "array",
                    "minItems": 2,
                    "maxItems": 2,
                    "items": {
                        "type": "integer",
                        "const": CAPABILITY_PROBE_EXPECTED_VALUE
                    }
                }
            },
            "required": ["probe", "values"],
            "additionalProperties": false
        })
    };
    let tool_schema = json!({
        "oneOf": [
            schema_branch(CAPABILITY_PROBE_EXPECTED_LABEL),
            schema_branch(CAPABILITY_PROBE_ALTERNATE_LABEL)
        ]
    });
    let strict_arguments = json!({
        "probe": CAPABILITY_PROBE_EXPECTED_LABEL,
        "values": [CAPABILITY_PROBE_EXPECTED_VALUE, CAPABILITY_PROBE_EXPECTED_VALUE]
    });
    let alternate_strict_arguments = json!({
        "probe": CAPABILITY_PROBE_ALTERNATE_LABEL,
        "values": [CAPABILITY_PROBE_EXPECTED_VALUE, CAPABILITY_PROBE_EXPECTED_VALUE]
    });
    let tool = |name: String, parameters_schema: Value| ModelToolSchema {
        name,
        description: "Fixed capability probe tool; no external side effect.".to_string(),
        parameters_schema,
    };
    let probe_tool_name = |index: u32| match index {
        0 => CAPABILITY_PROBE_TOOL_A.to_string(),
        1 => CAPABILITY_PROBE_TOOL_B.to_string(),
        _ => format!("singularity_capability_probe_extra_{index}"),
    };
    let probe_tools = |count: u32, parameters_schema: &Value| {
        (0..count)
            .map(|index| tool(probe_tool_name(index), parameters_schema.clone()))
            .collect::<Vec<_>>()
    };
    let make_request = |tools: Vec<ModelToolSchema>,
                        mode: ToolChoiceMode,
                        max_tool_calls: u32,
                        strict: bool,
                        instruction: &str| {
        let mut request = ModelTurnRequest::new(
            CAPABILITY_PROBE_REQUEST_ID,
            vec![
                ModelMessage::text(ModelRole::Developer, CAPABILITY_PROBE_DEVELOPER_INSTRUCTION),
                ModelMessage::text(ModelRole::User, instruction),
            ],
        );
        request.model_preferences.model_name = Some(model_name.to_string());
        request.tools = tools;
        request.tool_choice = ToolChoicePolicy {
            mode,
            max_tool_calls,
            strict_tool_schema: strict,
        };
        request
    };
    let make_contract =
        |strict: bool, supports_parallel_tool_calls: bool, max_tools_per_request: u32| {
            ProviderProtocolContract {
                supports_parallel_tool_calls,
                supports_strict_tool_schema: strict,
                tool_reasoning_mode: if selected_tool_reasoning_mode
                    != ProviderToolReasoningMode::Unspecified
                {
                    selected_tool_reasoning_mode
                } else if api_protocol == ProviderApiProtocol::OpenAiResponses
                    && selected_reasoning_variant.is_none()
                {
                    ProviderToolReasoningMode::DisabledForToolCalls
                } else {
                    ProviderToolReasoningMode::Unspecified
                },
                max_tools_per_request,
                supports_json_mode: false,
                supports_system_message: false,
                supports_developer_message: true,
                ..base.clone()
            }
        };
    let parallel_expected = |allowed_arguments: Vec<Value>| {
        vec![
            CapabilityProbeExpectedCall {
                tool_name: CAPABILITY_PROBE_TOOL_A,
                allowed_arguments: allowed_arguments.clone(),
            },
            CapabilityProbeExpectedCall {
                tool_name: CAPABILITY_PROBE_TOOL_B,
                allowed_arguments,
            },
        ]
    };
    let single_expected = |tool_name, allowed_arguments| {
        vec![CapabilityProbeExpectedCall {
            tool_name,
            allowed_arguments,
        }]
    };
    let direct_tool_count = DEFAULT_MAX_TOOLS_PER_REQUEST;
    let strict_allowed_arguments =
        vec![strict_arguments.clone(), alternate_strict_arguments.clone()];
    let profiles = vec![
        CapabilityProbeProfile {
            profile: ProviderCapabilityProfile::StrictParallel,
            contract: make_contract(true, true, direct_tool_count),
            request: make_request(
                probe_tools(direct_tool_count, &tool_schema),
                ToolChoiceMode::Auto,
                2,
                true,
                "First call singularity_capability_probe_a and singularity_capability_probe_b once each. After both tool results, call singularity_capability_probe_a once more.",
            ),
            expected_calls: parallel_expected(strict_allowed_arguments.clone()),
            single_call_fallback: Some(ProviderCapabilityProfile::StrictSingle),
        },
        CapabilityProbeProfile {
            profile: ProviderCapabilityProfile::StrictSingle,
            contract: make_contract(true, false, direct_tool_count),
            request: make_request(
                probe_tools(direct_tool_count, &tool_schema),
                ToolChoiceMode::Auto,
                1,
                true,
                "First call singularity_capability_probe_a once. After its tool result, call singularity_capability_probe_a once more.",
            ),
            expected_calls: single_expected(CAPABILITY_PROBE_TOOL_A, strict_allowed_arguments),
            single_call_fallback: None,
        },
        CapabilityProbeProfile {
            profile: ProviderCapabilityProfile::NonStrictParallel,
            contract: make_contract(false, true, direct_tool_count),
            request: make_request(
                probe_tools(direct_tool_count, &tool_schema),
                ToolChoiceMode::Auto,
                2,
                false,
                "First call singularity_capability_probe_a and singularity_capability_probe_b once each with exactly {\"probe\":\"schema_sentinel_alpha\",\"values\":[7,7]} as each arguments object. After both tool results, call singularity_capability_probe_a once more with the same arguments.",
            ),
            expected_calls: parallel_expected(Vec::new()),
            single_call_fallback: Some(ProviderCapabilityProfile::NonStrictSingle),
        },
        CapabilityProbeProfile {
            profile: ProviderCapabilityProfile::NonStrictSingle,
            contract: make_contract(false, false, direct_tool_count),
            request: make_request(
                probe_tools(direct_tool_count, &tool_schema),
                ToolChoiceMode::Auto,
                1,
                false,
                "First call singularity_capability_probe_a once with arguments {\"probe\":\"schema_sentinel_alpha\",\"values\":[7,7]}. After its tool result, call singularity_capability_probe_a once more with the same arguments.",
            ),
            expected_calls: single_expected(CAPABILITY_PROBE_TOOL_A, Vec::new()),
            single_call_fallback: None,
        },
    ];
    profiles
}

fn capability_probe_profile_match(
    response: &ModelTurnResponse,
    profile: &CapabilityProbeProfile,
) -> Option<ProviderCapabilityProfile> {
    profile
        .single_call_fallback
        .filter(|_| capability_probe_single_call_matches(response, &profile.expected_calls))
        .or_else(|| {
            capability_probe_response_matches(response, &profile.expected_calls)
                .then_some(profile.profile)
        })
}

fn capability_probe_continuation_request(
    profile: &CapabilityProbeProfile,
    response: &ModelTurnResponse,
) -> ModelTurnRequest {
    let mut request = profile.request.clone();
    request.request_id = CAPABILITY_PROBE_CONTINUATION_REQUEST_ID.to_string();
    request.provider_reasoning_history = response.provider_reasoning_history.clone();
    request.messages.push(ModelMessage::assistant_tool_calls(
        response.tool_calls.clone(),
    ));
    for call in &response.tool_calls {
        let mut message = ModelMessage::text(
            ModelRole::Tool,
            json!({
                "ok": true,
                "tool_name": call.tool_name,
                "tool_call_id": call.tool_call_id,
                "truncated": false,
                "content": {"probe": "completed"}
            })
            .to_string(),
        );
        message.tool_call_id = Some(call.tool_call_id.clone());
        request.messages.push(message);
    }
    request.tool_choice.max_tool_calls = 1;
    request
}

fn capability_probe_continuation_matches(
    response: &ModelTurnResponse,
    profile: &CapabilityProbeProfile,
) -> bool {
    profile.expected_calls.first().is_some_and(|expected| {
        capability_probe_response_matches(response, std::slice::from_ref(expected))
    })
}

fn capability_probe_response_matches(
    response: &ModelTurnResponse,
    expected_calls: &[CapabilityProbeExpectedCall],
) -> bool {
    if response.status != ModelTurnStatus::Success
        || response.tool_calls.len() != expected_calls.len()
    {
        return false;
    }

    let mut matched = vec![false; expected_calls.len()];
    for call in &response.tool_calls {
        if call.parse_status != ModelToolParseStatus::Valid {
            return false;
        }
        let Some(index) = expected_calls
            .iter()
            .enumerate()
            .find_map(|(index, expected)| {
                (!matched[index]
                    && call.tool_name == expected.tool_name
                    && (expected.allowed_arguments.is_empty()
                        || expected.allowed_arguments.contains(&call.arguments)))
                .then_some(index)
            })
        else {
            return false;
        };
        matched[index] = true;
    }
    true
}

fn capability_probe_single_call_matches(
    response: &ModelTurnResponse,
    expected_calls: &[CapabilityProbeExpectedCall],
) -> bool {
    let Some(call) = response.tool_calls.first() else {
        return false;
    };
    response.status == ModelTurnStatus::Success
        && response.tool_calls.len() == 1
        && call.parse_status == ModelToolParseStatus::Valid
        && expected_calls.iter().any(|expected| {
            call.tool_name == expected.tool_name
                && (expected.allowed_arguments.is_empty()
                    || expected.allowed_arguments.contains(&call.arguments))
        })
}

fn add_model_usage(total: &mut ModelUsage, usage: &ModelUsage) {
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
    total.cached_input_tokens = total
        .cached_input_tokens
        .saturating_add(usage.cached_input_tokens);
    total.reasoning_tokens = total
        .reasoning_tokens
        .saturating_add(usage.reasoning_tokens);
    if let Some(cost) = usage.cost_estimate {
        total.cost_estimate = Some(total.cost_estimate.unwrap_or_default() + cost);
    }
}

pub(super) fn capability_probe_cancelled_error() -> ProviderError {
    provider_cancelled_error().with_capability_metadata(capability_probe_metadata(
        ProviderApiProtocol::Declared,
        ProviderCapabilityProfile::Declared,
        0,
        0,
        &ModelUsage::default(),
        &ProviderAttemptMetadata::zero(),
    ))
}

pub(super) fn capability_probe_deadline_error() -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::Timeout,
            "provider capability probe deadline exceeded",
        )
        .with_provider_diagnostic(
            "provider_capability_probe_deadline_exceeded",
            ProviderErrorStage::RequestSend,
        ),
    )
    .with_capability_metadata(capability_probe_metadata(
        ProviderApiProtocol::Declared,
        ProviderCapabilityProfile::Declared,
        0,
        0,
        &ModelUsage::default(),
        &ProviderAttemptMetadata::zero(),
    ))
}

fn capability_probe_owner_failure_requires_reselection(error: &ProviderError) -> bool {
    matches!(
        error.error.kind,
        ModelErrorKind::Cancelled | ModelErrorKind::Timeout | ModelErrorKind::NetworkError
    ) || matches!(
        error.error.code.as_deref(),
        Some(
            "provider_capability_cache_unavailable"
                | "provider_capability_cache_invalidation_failed"
                | "provider_capability_probe_deadline_exceeded"
                | "provider_request_cancelled"
        )
    )
}

fn provider_capability_cache_error() -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::UnknownProviderError,
            "provider capability cache is unavailable",
        )
        .with_provider_diagnostic(
            "provider_capability_cache_unavailable",
            ProviderErrorStage::ClientInitialization,
        ),
    )
}

fn provider_capability_cache_invalidation_error() -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::UnknownProviderError,
            "provider capability cache invalidation failed",
        )
        .with_provider_diagnostic(
            "provider_capability_cache_invalidation_failed",
            ProviderErrorStage::ClientInitialization,
        ),
    )
}

fn provider_capability_cache_invalidation_deadline_error() -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::Timeout,
            "provider capability cache invalidation deadline exceeded",
        )
        .with_provider_diagnostic(
            PROVIDER_CAPABILITY_CACHE_INVALIDATION_DEADLINE_CODE,
            ProviderErrorStage::ClientInitialization,
        ),
    )
}

fn capability_probe_definition_error(errors: Vec<String>) -> ProviderError {
    let mut error = ModelError::new(
        ModelErrorKind::UnknownProviderError,
        "provider capability probe definition is invalid",
    )
    .with_provider_diagnostic(
        "provider_capability_probe_definition_invalid",
        ProviderErrorStage::RequestSend,
    );
    error.validation_errors = errors;
    ProviderError::from_model_error(error)
}

pub(super) fn capability_probe_metadata(
    api_protocol: ProviderApiProtocol,
    profile: ProviderCapabilityProfile,
    profile_attempts: u32,
    fallback_count: u32,
    probe_usage: &ModelUsage,
    probe_attempt_metadata: &ProviderAttemptMetadata,
) -> ProviderCapabilityMetadata {
    ProviderCapabilityMetadata {
        api_protocol,
        profile,
        cache_hit: false,
        profile_attempts,
        fallback_count,
        probe_usage: probe_usage.clone(),
        probe_attempt_metadata: probe_attempt_metadata.clone(),
        cache_observations: Vec::new(),
    }
}

fn capability_probe_failure(
    error: ProviderError,
    metadata: ProviderCapabilityMetadata,
    evidence: &str,
) -> ProviderError {
    let provider_attempt_metadata = error.provider_attempt_metadata.clone();
    let mut model_error = *error.error;
    if !model_error
        .validation_errors
        .iter()
        .any(|existing| existing == evidence)
    {
        model_error.validation_errors.push(evidence.to_string());
    }
    let provider_error = ProviderError::from_model_error(model_error);
    let provider_error = if let Some(metadata) = provider_attempt_metadata {
        provider_error.with_provider_attempt_metadata(metadata)
    } else {
        provider_error
    };
    provider_error.with_capability_metadata(metadata)
}

fn cache_hit_negotiation(
    mut negotiation: ProviderProtocolNegotiation,
) -> ProviderProtocolNegotiation {
    negotiation.metadata.cache_hit = true;
    negotiation.metadata.profile_attempts = 0;
    negotiation.metadata.fallback_count = 0;
    negotiation.metadata.probe_usage = ModelUsage::default();
    negotiation.metadata.probe_attempt_metadata = ProviderAttemptMetadata::zero();
    negotiation
}

fn merge_capability_metadata(
    target: &mut ProviderCapabilityMetadata,
    previous: &ProviderCapabilityMetadata,
) {
    target.profile_attempts = target
        .profile_attempts
        .saturating_add(previous.profile_attempts);
    target.fallback_count = target
        .fallback_count
        .saturating_add(previous.fallback_count);
    add_model_usage(&mut target.probe_usage, &previous.probe_usage);
    add_provider_attempt_metadata(
        &mut target.probe_attempt_metadata,
        &previous.probe_attempt_metadata,
    );
}

fn provider_protocol_fallback_allowed(error: &ProviderError) -> bool {
    error.error.kind == ModelErrorKind::UnsupportedCapability
        || (error.error.stage == Some(ProviderErrorStage::ResponseStatus)
            && matches!(
                error.error.http_status,
                Some(
                    HTTP_STATUS_BAD_REQUEST
                        | HTTP_STATUS_NOT_FOUND
                        | HTTP_STATUS_UNPROCESSABLE_ENTITY
                )
            ))
}

fn capability_probe_unsupported_error(mut error: ModelError) -> ProviderError {
    error.kind = ModelErrorKind::UnsupportedCapability;
    error.message = "provider does not support native structured tool calls".to_string();
    if error.code.is_none() {
        error.code = Some("provider_native_structured_tool_calls_unsupported".to_string());
    }
    if error.stage.is_none() {
        error.stage = Some(ProviderErrorStage::ResponseValidation);
    }
    ProviderError::from_model_error(error)
}

fn capability_probe_response_error(response: &ModelTurnResponse) -> ProviderError {
    let mut error = response.error.as_ref().cloned().unwrap_or_else(|| {
        ModelError::new(
            ModelErrorKind::UnsupportedCapability,
            "provider capability probe did not return native structured tool calls",
        )
        .with_provider_diagnostic(
            "provider_native_structured_tool_calls_unsupported",
            ProviderErrorStage::ResponseValidation,
        )
    });
    if let Some(validation) = &response.validation {
        error.validation_errors = validation.errors.clone();
    }
    let explicit_capability_violation = error.kind == ModelErrorKind::UnsupportedCapability
        || error.validation_errors.iter().any(|validation_error| {
            matches!(
                validation_error.as_str(),
                "provider_does_not_support_tools"
                    | "provider_does_not_support_strict_tool_schema"
                    | "provider_does_not_support_parallel_tool_calls"
                    | "max_tool_calls_exceeded"
            )
        });
    if !response.tool_calls.is_empty() && !explicit_capability_violation {
        return ProviderError::from_model_error(error);
    }
    if response.tool_calls.is_empty()
        && !error
            .validation_errors
            .iter()
            .any(|error| error == "capability_probe_native_tool_calls_missing")
    {
        error
            .validation_errors
            .push("capability_probe_native_tool_calls_missing".to_string());
    }
    capability_probe_unsupported_error(error)
}

fn capability_probe_continuation_error(response: &ModelTurnResponse) -> ProviderError {
    let mut error = capability_probe_response_error(response);
    if !error
        .error
        .validation_errors
        .iter()
        .any(|existing| existing == "capability_probe_multi_turn_tool_calls_missing")
    {
        error
            .error
            .validation_errors
            .push("capability_probe_multi_turn_tool_calls_missing".to_string());
    }
    error
}

pub(super) fn capability_probe_tool_reasoning_error(
    response: &ModelTurnResponse,
    evidence: &str,
) -> ProviderError {
    let mut error = response.error.as_ref().cloned().unwrap_or_else(|| {
        ModelError::new(
            ModelErrorKind::UnsupportedCapability,
            "provider cannot stabilize native tool calls with reasoning disabled",
        )
        .with_provider_diagnostic(
            "provider_tool_reasoning_mode_unsupported",
            ProviderErrorStage::ResponseValidation,
        )
    });
    if let Some(validation) = &response.validation {
        error.validation_errors = validation.errors.clone();
    }
    if !error
        .validation_errors
        .iter()
        .any(|existing| existing == evidence)
    {
        error.validation_errors.push(evidence.to_string());
    }
    let provider_error = ProviderError::from_model_error(error);
    if let Some(metadata) = &response.provider_attempt_metadata {
        provider_error.with_provider_attempt_metadata(metadata.clone())
    } else {
        provider_error
    }
}

fn is_capability_probe_profile_rejection(error: &ProviderError) -> bool {
    error.error.stage == Some(ProviderErrorStage::ResponseStatus)
        && matches!(
            error.error.http_status,
            Some(HTTP_STATUS_BAD_REQUEST | HTTP_STATUS_UNPROCESSABLE_ENTITY)
        )
}

pub(super) fn is_stable_capability_rejection(error: &ProviderError) -> bool {
    matches!(
        error.error.code.as_deref(),
        Some(
            "provider_native_structured_tool_calls_unsupported"
                | "provider_tool_reasoning_mode_unsupported"
                | "provider_tool_reasoning_mode_not_honored"
                | "provider_tool_reasoning_history_unsupported"
        )
    ) || error
        .error
        .validation_errors
        .iter()
        .any(|validation_error| {
            matches!(
                validation_error.as_str(),
                "provider_does_not_support_tools"
                    | "provider_does_not_support_required_tool_choice"
                    | "provider_does_not_support_parallel_tool_calls"
                    | "provider_does_not_support_strict_tool_schema"
                    | "provider_does_not_support_json_mode"
                    | "provider_does_not_support_system_messages"
                    | "provider_does_not_support_developer_messages"
                    | "requested_tools_exceed_provider_limit"
                    | "requested_output_tokens_exceed_provider_limit"
                    | "tool_reasoning_disable_not_honored"
                    | "tool_reasoning_content_requires_adapter_history_support"
            )
        })
}
