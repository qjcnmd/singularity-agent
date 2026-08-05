//! Persistent remote source-template cache for Evaluation.
//!
//! Each task/repository identity owns one fixed template directory. The first fetch is staged,
//! checked, and atomically renamed into place while a per-key lock is held. Later runs copy the
//! existing directory directly.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};
use singularity_core::CancellationToken;

use super::workspace::{copy_tree_checked, snapshot_workspace};

const TEMPLATE_DIR: &str = "template";
const LOCK_FILE: &str = "lock";
const LOCK_RETRY_MS: u64 = 10;

/// Cache result observed for one remote source preparation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceTemplateCacheStatus {
    Hit,
    Miss,
}

/// Stable Evaluation-owned failure codes for source-template operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceCacheErrorCode {
    RootFailed,
    LookupFailed,
    LockFailed,
    FetchFailed,
    ValidateFailed,
    PublishFailed,
    MaterializeFailed,
    Cancelled,
}

impl SourceCacheErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::RootFailed => "source_cache_root_failed",
            Self::LookupFailed => "source_cache_lookup_failed",
            Self::LockFailed => "source_cache_lock_failed",
            Self::FetchFailed => "source_cache_fetch_failed",
            Self::ValidateFailed => "source_cache_validate_failed",
            Self::PublishFailed => "source_cache_publish_failed",
            Self::MaterializeFailed => "source_cache_materialize_failed",
            Self::Cancelled => "source_cache_cancelled",
        }
    }
}

/// Typed observation of a failed source-template operation.
#[derive(Debug, Clone)]
pub(crate) struct SourceCacheError {
    pub(crate) code: SourceCacheErrorCode,
    pub(crate) detail: String,
}

impl SourceCacheError {
    fn new(code: SourceCacheErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    pub(crate) fn stable_code(&self) -> &'static str {
        self.code.as_str()
    }
}

impl std::fmt::Display for SourceCacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.detail)
    }
}

/// Cache status and materialization timing returned after source-template preparation.
#[derive(Debug, Clone)]
pub(crate) struct SourceTemplatePreparation {
    pub(crate) status: SourceTemplateCacheStatus,
    pub(crate) materialization_ms: u64,
}

#[derive(Debug)]
struct CacheLock {
    _file: File,
}

/// Persistent source-template cache rooted outside each run directory.
#[derive(Debug, Clone)]
pub(crate) struct SourceTemplateCache {
    root: PathBuf,
}

impl SourceTemplateCache {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Return whether a fixed, materializable template directory exists.
    pub(crate) fn entry_available(
        &self,
        task_id: &str,
        repository: &str,
    ) -> Result<bool, SourceCacheError> {
        let key = Self::cache_key(task_id, repository);
        let key_dir = self.key_dir(&key);
        let path = key_dir.join(TEMPLATE_DIR);
        match fs::metadata(&path) {
            Ok(metadata) => {
                if metadata.is_dir() {
                    Ok(true)
                } else {
                    Err(SourceCacheError::new(
                        SourceCacheErrorCode::LookupFailed,
                        "source-template cache entry is not a directory",
                    ))
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(SourceCacheError::new(
                SourceCacheErrorCode::LookupFailed,
                format!("failed to inspect source-template cache entry: {error}"),
            )),
        }
    }

    /// Prepare a remote source from a cache hit or one controlled first fetch.
    pub(crate) fn prepare_remote<F>(
        &self,
        task_id: &str,
        repository: &str,
        workspace_dir: &Path,
        cancellation: &CancellationToken,
        fetch: F,
    ) -> Result<SourceTemplatePreparation, SourceCacheError>
    where
        F: FnOnce(&Path) -> Result<(), String>,
    {
        if cancellation.is_cancelled() {
            return Err(SourceCacheError::new(
                SourceCacheErrorCode::Cancelled,
                "evaluation cancelled before source-template cache preparation",
            ));
        }
        fs::create_dir_all(&self.root).map_err(|error| {
            SourceCacheError::new(
                SourceCacheErrorCode::RootFailed,
                format!("failed to create source-template cache root: {error}"),
            )
        })?;
        let key = Self::cache_key(task_id, repository);
        let key_dir = self.key_dir(&key);
        fs::create_dir_all(&key_dir).map_err(|error| {
            SourceCacheError::new(
                SourceCacheErrorCode::RootFailed,
                format!("failed to create source-template cache key directory: {error}"),
            )
        })?;
        let _lock = acquire_lock(&key_dir.join(LOCK_FILE), cancellation)?;
        let template = key_dir.join(TEMPLATE_DIR);
        let template_is_available = match fs::metadata(&template) {
            Ok(metadata) if metadata.is_dir() => true,
            Ok(_) => {
                return Err(SourceCacheError::new(
                    SourceCacheErrorCode::LookupFailed,
                    "source-template cache entry is not a directory",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(SourceCacheError::new(
                    SourceCacheErrorCode::LookupFailed,
                    format!("failed to inspect source-template cache entry: {error}"),
                ));
            }
        };
        if template_is_available {
            let materialization_started = Instant::now();
            copy_tree_checked(&template, workspace_dir).map_err(|error| {
                SourceCacheError::new(
                    SourceCacheErrorCode::MaterializeFailed,
                    format!("source-template cache materialization failed: {error}"),
                )
            })?;
            return Ok(SourceTemplatePreparation {
                status: SourceTemplateCacheStatus::Hit,
                materialization_ms: elapsed_ms(materialization_started),
            });
        }
        if cancellation.is_cancelled() {
            return Err(SourceCacheError::new(
                SourceCacheErrorCode::Cancelled,
                "evaluation cancelled before source-template fetch",
            ));
        }

        let staging = key_dir.join(".download");
        remove_tree(&staging)
            .map_err(|error| SourceCacheError::new(SourceCacheErrorCode::RootFailed, error))?;
        fetch(&staging)
            .map_err(|error| SourceCacheError::new(SourceCacheErrorCode::FetchFailed, error))?;
        if cancellation.is_cancelled() {
            let _ = remove_tree(&staging);
            return Err(SourceCacheError::new(
                SourceCacheErrorCode::Cancelled,
                "evaluation cancelled after source-template fetch",
            ));
        }
        snapshot_workspace(&staging)
            .map_err(|error| SourceCacheError::new(SourceCacheErrorCode::ValidateFailed, error))?;
        // Validation above is intentionally a single first-fetch check.  Reuse does not compare
        // a stored digest or metadata summary; the fixed directory is the cache fact.
        fs::rename(&staging, &template).map_err(|error| {
            SourceCacheError::new(
                SourceCacheErrorCode::PublishFailed,
                format!("failed to atomically publish source-template: {error}"),
            )
        })?;
        let materialization_started = Instant::now();
        copy_tree_checked(&template, workspace_dir).map_err(|error| {
            SourceCacheError::new(
                SourceCacheErrorCode::MaterializeFailed,
                format!("source-template cache materialization failed: {error}"),
            )
        })?;
        Ok(SourceTemplatePreparation {
            status: SourceTemplateCacheStatus::Miss,
            materialization_ms: elapsed_ms(materialization_started),
        })
    }

    pub(crate) fn cache_key(task_id: &str, repository: &str) -> String {
        let mut digest = Sha256::new();
        digest.update(b"evaluation.source-template\0");
        update_digest_value(&mut digest, task_id);
        update_digest_value(&mut digest, repository);
        format!("sha256:{:x}", digest.finalize())
    }

    fn key_dir(&self, key: &str) -> PathBuf {
        self.root.join(key.strip_prefix("sha256:").unwrap_or(key))
    }
}

fn acquire_lock(
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<CacheLock, SourceCacheError> {
    loop {
        if cancellation.is_cancelled() {
            return Err(SourceCacheError::new(
                SourceCacheErrorCode::Cancelled,
                "evaluation cancelled while waiting for source-template lock",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| {
                SourceCacheError::new(
                    SourceCacheErrorCode::LockFailed,
                    format!("failed to open source-template lock: {error}"),
                )
            })?;
        match file.try_lock() {
            Ok(()) => return Ok(CacheLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                thread::sleep(Duration::from_millis(LOCK_RETRY_MS));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(SourceCacheError::new(
                    SourceCacheErrorCode::LockFailed,
                    format!("failed to acquire source-template lock: {error}"),
                ));
            }
        }
    }
}

fn update_digest_value(digest: &mut Sha256, value: &str) {
    let length = u64::try_from(value.len()).unwrap_or(u64::MAX);
    digest.update(length.to_le_bytes());
    digest.update(value.as_bytes());
}

fn remove_tree(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("failed to remove source-template tree: {error}")),
    }
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_with_temp() -> (tempfile::TempDir, SourceTemplateCache) {
        let temp = tempfile::tempdir().expect("cache directory");
        let cache = SourceTemplateCache::new(temp.path().join("cache"));
        (temp, cache)
    }

    #[test]
    fn first_fetch_publishes_fixed_template_atomically_and_hit_reuses_it() {
        let (temp, cache) = cache_with_temp();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("README.md"), b"source").expect("source file");
        let destination = temp.path().join("destination");
        let first = cache
            .prepare_remote(
                "task",
                "https://example.invalid/repo.git",
                &destination,
                &CancellationToken::new(),
                |staging| {
                    copy_tree_checked(&source, staging).map_err(|error| error.to_string())?;
                    Ok(())
                },
            )
            .expect("first preparation");
        assert_eq!(first.status, SourceTemplateCacheStatus::Miss);
        assert_eq!(
            fs::read_to_string(destination.join("README.md")).unwrap(),
            "source"
        );
        assert!(
            cache
                .entry_available("task", "https://example.invalid/repo.git")
                .expect("entry availability")
        );

        let second_destination = temp.path().join("second");
        let second = cache
            .prepare_remote(
                "task",
                "https://example.invalid/repo.git",
                &second_destination,
                &CancellationToken::new(),
                |_| panic!("cache hit must not fetch"),
            )
            .expect("cache hit");
        assert_eq!(second.status, SourceTemplateCacheStatus::Hit);
        assert_eq!(
            fs::read_to_string(second_destination.join("README.md")).unwrap(),
            "source"
        );
    }

    #[test]
    fn failed_fetch_does_not_publish_partial_template() {
        let (_temp, cache) = cache_with_temp();
        let destination = _temp.path().join("destination");
        let error = cache
            .prepare_remote(
                "task",
                "repo",
                &destination,
                &CancellationToken::new(),
                |_| Err("network failed".to_string()),
            )
            .expect_err("fetch failure");
        assert_eq!(error.code, SourceCacheErrorCode::FetchFailed);
        assert!(!cache.entry_available("task", "repo").expect("lookup"));
    }

    #[test]
    fn invalid_template_entry_fails_closed_without_fetching_again() {
        let (temp, cache) = cache_with_temp();
        let key = SourceTemplateCache::cache_key("task", "repo");
        let key_dir = cache.key_dir(&key);
        fs::create_dir_all(&key_dir).expect("cache key directory");
        fs::write(key_dir.join(TEMPLATE_DIR), b"not a directory").expect("invalid entry");

        let error = cache
            .prepare_remote(
                "task",
                "repo",
                &temp.path().join("destination"),
                &CancellationToken::new(),
                |_| panic!("invalid cache entry must not trigger a new fetch"),
            )
            .expect_err("invalid template entry must fail closed");
        assert_eq!(error.code, SourceCacheErrorCode::LookupFailed);
    }
}
