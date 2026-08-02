//! Evaluation workspace 的安全复制、快照和变更归因。

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt as _;
#[cfg(unix)]
use std::os::unix::fs::symlink;

use cap_fs_ext::{DirExt as _, FollowSymlinks, MetadataExt as _, OpenOptionsFollowExt as _};
use cap_std::ambient_authority;
#[cfg(unix)]
use cap_std::fs::PermissionsExt as _;
use cap_std::fs::{Dir, Metadata, OpenOptions as CapOpenOptions};
use serde::Serialize;
use sha2::{Digest, Sha256};
use singularity_core::CancellationToken;
use singularity_sandbox::{
    PreparedWorkspaceObservation, PreparedWorkspaceObserver, SandboxBackend,
    SandboxPreflightReport, is_toolchain_artifact_path,
};
#[cfg(windows)]
const REPARSE_POINT_ATTRIBUTE: u32 = 0x0400;
const PREPARED_OBSERVATION_POLICY: &str = "observed_prepared_source/v1";
pub(super) type WorkspaceSnapshot = BTreeMap<String, WorkspaceSnapshotEntry>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct WorkspaceSnapshotEntry {
    kind: WorkspaceSnapshotEntryKind,
    content_digest: Option<String>,
    platform_permissions: u32,
    length: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WorkspaceSnapshotEntryKind {
    Directory,
    File,
    Symlink,
}

/// One continuously observed prepared source bound to content, root identity and backend contract.
///
/// This token proves observation continuity, not OS-enforced immutability. Its anonymous image is
/// derived from the same authoritative capture and never exposes another workspace path.
pub(super) struct ObservedPreparedSource {
    root: PathBuf,
    root_identity: WorkspaceRootIdentity,
    root_entry: WorkspaceSnapshotEntry,
    snapshot_digest: String,
    backend: String,
    contract_digest: String,
    valid: AtomicBool,
    image: Mutex<PreparedSourceImage>,
    observer: Mutex<Box<dyn PreparedWorkspaceObserver>>,
}

struct PreparedSourceImage {
    file: File,
    payloads: BTreeMap<String, ImagePayload>,
}

#[derive(Clone, Copy)]
struct ImagePayload {
    offset: u64,
    length: u64,
}

struct PreparedImageCapture {
    root_identity: WorkspaceRootIdentity,
    snapshot: WorkspaceSnapshot,
    image: PreparedSourceImage,
    work: SourceCaptureWork,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SourceCaptureWork {
    pub source_tree_entries_read: usize,
    pub source_tree_content_reads: usize,
    pub source_tree_content_bytes: u64,
    pub image_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkspaceRootIdentity {
    device: u64,
    object: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct WorkspaceObservationMetric {
    pub stage: String,
    pub contract: String,
    pub duration_ms: u64,
    pub copy_files: usize,
    pub copy_bytes: u64,
    pub source_tree_entry_reads: usize,
    pub source_tree_content_reads: usize,
    pub image_reads: usize,
    pub image_bytes: u64,
    pub observation: &'static str,
}

#[derive(Debug)]
pub(super) struct PreparedMaterialization {
    pub metric: WorkspaceObservationMetric,
}

pub(super) struct PreparedSourceCapture {
    pub snapshot: WorkspaceSnapshot,
    pub observed: Option<ObservedPreparedSource>,
    pub full_scans: u64,
    pub work: SourceCaptureWork,
}

/// Materialize one stage from the authoritative prepared-source image.
pub(super) fn materialize_prepared_workspace(
    stage: &str,
    source: &Path,
    destination: &Path,
    expected: &WorkspaceSnapshot,
    prepared: Option<&ObservedPreparedSource>,
    backend_name: &str,
    cancellation: &CancellationToken,
) -> Result<PreparedMaterialization, String> {
    let started = Instant::now();
    if cancellation.is_cancelled() {
        return Err("evaluation cancelled before workspace materialization".to_string());
    }
    let mut materialized = if let Some(prepared) = prepared {
        prepared.materialize(stage, destination, expected, backend_name, cancellation)?
    } else {
        let capture = capture_workspace_image(source)?;
        if &capture.snapshot != expected {
            return Err(
                "prepared source changed before full workspace materialization".to_string(),
            );
        }
        let (_, source_work, copy_work) =
            materialize_workspace_capture(capture, source, destination)?;
        PreparedMaterialization {
            metric: WorkspaceObservationMetric {
                stage: stage.to_string(),
                contract: backend_name.to_string(),
                duration_ms: 0,
                copy_files: copy_work.copy_files,
                copy_bytes: copy_work.copy_bytes,
                source_tree_entry_reads: source_work.source_tree_entries_read,
                source_tree_content_reads: source_work.source_tree_content_reads,
                image_reads: copy_work.image_reads,
                image_bytes: copy_work.image_bytes,
                observation: "full",
            },
        }
    };
    materialized.metric.duration_ms =
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let _ = source;
    Ok(materialized)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
/// 单个工作区变更及其前后摘要。
pub(super) struct WorkspaceChangeEvidence {
    pub path: String,
    pub change_kind: &'static str,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
}

/// 在排除 `.git` 且不跟随链接的前提下安全复制工作区。
pub(super) fn copy_tree_checked(
    source: &Path,
    destination: &Path,
) -> Result<WorkspaceSnapshot, String> {
    let capture = capture_workspace_image(source)?;
    materialize_workspace_capture(capture, source, destination).map(|(snapshot, _, _)| snapshot)
}

/// Materialize a preparation tree from the same bounded image used by later stages.
pub(super) fn copy_tree_for_preparation(source: &Path, destination: &Path) -> Result<(), String> {
    copy_tree_checked(source, destination).map(|_| ())
}

impl ObservedPreparedSource {
    /// Capture the single authoritative prepared-source snapshot after starting any available
    /// continuous observer. No earlier copy-time hash map is reused as final evidence.
    pub(super) fn capture(
        root: &Path,
        backend: &(impl SandboxBackend + ?Sized),
        preflight: &SandboxPreflightReport,
    ) -> Result<PreparedSourceCapture, String> {
        let mut observer = if preflight.proves_supported_contract_for(backend.name()) {
            backend.observe_prepared_workspace(root)?
        } else {
            None
        };
        let root = root.to_path_buf();
        let capture = capture_workspace_image(&root)?;
        let root_entry = capture
            .snapshot
            .get(".")
            .cloned()
            .ok_or_else(|| "prepared workspace snapshot has no root entry".to_string())?;
        if let Some(observer) = observer.as_mut()
            && (observer.checkpoint()? != PreparedWorkspaceObservation::Unchanged
                || workspace_root_identity(&root)? != capture.root_identity
                || prepared_root_entry(&root)? != root_entry)
        {
            return Err(
                "prepared source changed while its observation token was being published"
                    .to_string(),
            );
        }
        let contract = serde_json::to_vec(&(preflight, PREPARED_OBSERVATION_POLICY))
            .map_err(|error| format!("failed to encode prepared workspace contract: {error}"))?;
        let snapshot_digest = workspace_snapshot_digest(&capture.snapshot)?;
        let Some(observer) = observer else {
            // 无连续 observer 时不能保留 image 复用：没有任何机制能证明 capture 与后续
            // materialize 之间源树未变，复用旧 image 会漏掉带内写入（REQ-001）。
            // 此处只保留权威 snapshot 和累计 work，丢弃刚构建的 image；后续每个 stage
            // 由 materialize_prepared_workspace 的无 observer 分支从 live 源树重新全量
            // capture 并与该 snapshot 比对，从同一次新 capture 物化（PAT-002）。
            return Ok(PreparedSourceCapture {
                snapshot: capture.snapshot,
                observed: None,
                full_scans: 1,
                work: capture.work,
            });
        };
        let observed = Self {
            root,
            root_identity: capture.root_identity,
            root_entry,
            snapshot_digest,
            backend: backend.name().to_string(),
            contract_digest: format!("sha256:{:x}", Sha256::digest(contract)),
            valid: AtomicBool::new(true),
            image: Mutex::new(capture.image),
            observer: Mutex::new(observer),
        };
        let snapshot = capture.snapshot;
        let work = capture.work;
        Ok(PreparedSourceCapture {
            snapshot,
            observed: Some(observed),
            full_scans: 1,
            work,
        })
    }

    /// Materialize one isolated stage without re-hashing the complete prepared source.
    pub(super) fn materialize(
        &self,
        stage: &str,
        destination: &Path,
        expected: &WorkspaceSnapshot,
        backend_name: &str,
        cancellation: &CancellationToken,
    ) -> Result<PreparedMaterialization, String> {
        let started = Instant::now();
        if cancellation.is_cancelled() {
            self.valid.store(false, Ordering::Release);
            return Err("evaluation cancelled before prepared source materialization".to_string());
        }
        let expected_digest = match workspace_snapshot_digest(expected) {
            Ok(digest) => digest,
            Err(error) => {
                self.valid.store(false, Ordering::Release);
                return Err(error);
            }
        };
        if backend_name != self.backend || expected_digest != self.snapshot_digest {
            self.valid.store(false, Ordering::Release);
            return Err(
                "observed prepared source contract no longer matches its consumer".to_string(),
            );
        }
        self.require_unchanged("before materialization")?;
        let copy = self.materialize_image(destination, expected);
        let work = match copy {
            Ok(work) => work,
            Err(error) => {
                self.valid.store(false, Ordering::Release);
                let _ = remove_partial_tree(destination);
                return Err(error);
            }
        };
        if let Err(error) = self.require_unchanged("after materialization") {
            let cleanup = remove_partial_tree(destination);
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!("{error}; {cleanup}")),
            };
        }
        if cancellation.is_cancelled() {
            self.valid.store(false, Ordering::Release);
            let cleanup = remove_partial_tree(destination);
            return match cleanup {
                Ok(()) => {
                    Err("evaluation cancelled during prepared source materialization".to_string())
                }
                Err(cleanup) => Err(format!(
                    "evaluation cancelled during prepared source materialization; {cleanup}"
                )),
            };
        }
        Ok(PreparedMaterialization {
            metric: WorkspaceObservationMetric {
                stage: stage.to_string(),
                contract: self.contract_digest.clone(),
                duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                copy_files: work.copy_files,
                copy_bytes: work.copy_bytes,
                source_tree_entry_reads: 0,
                source_tree_content_reads: 0,
                image_reads: work.image_reads,
                image_bytes: work.image_bytes,
                observation: "reused",
            },
        })
    }

    fn require_unchanged(&self, phase: &str) -> Result<(), String> {
        if !self.valid.load(Ordering::Acquire) {
            return Err("observed prepared source token was invalidated".to_string());
        }
        self.require_root_unchanged(phase)?;
        let mut observer = self.observer.lock().map_err(|_| {
            self.valid.store(false, Ordering::Release);
            "prepared workspace observer lock is poisoned".to_string()
        })?;
        if !self.valid.load(Ordering::Acquire) {
            return Err("observed prepared source token was invalidated".to_string());
        }
        let observation = match observer.checkpoint() {
            Ok(observation) => observation,
            Err(error) => {
                self.valid.store(false, Ordering::Release);
                return Err(error);
            }
        };
        match observation {
            PreparedWorkspaceObservation::Unchanged => {}
            PreparedWorkspaceObservation::Changed(_) => {
                self.valid.store(false, Ordering::Release);
                return Err(format!("observed prepared source changed {phase}"));
            }
            PreparedWorkspaceObservation::Unknown => {
                self.valid.store(false, Ordering::Release);
                return Err(format!(
                    "observed prepared source observation is incomplete {phase}"
                ));
            }
        }
        self.require_root_unchanged(phase)
    }

    fn materialize_image(
        &self,
        destination: &Path,
        expected: &WorkspaceSnapshot,
    ) -> Result<ImageMaterializationWork, String> {
        let mut image = self.image.lock().map_err(|_| {
            self.valid.store(false, Ordering::Release);
            "prepared source image lock is poisoned".to_string()
        })?;
        materialize_prepared_image(&mut image, &self.root, destination, expected)
    }

    fn require_root_unchanged(&self, phase: &str) -> Result<(), String> {
        let unchanged = workspace_root_identity(&self.root)
            .and_then(|identity| prepared_root_entry(&self.root).map(|root| (identity, root)))
            .map(|(identity, root)| identity == self.root_identity && root == self.root_entry);
        match unchanged {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.valid.store(false, Ordering::Release);
                Err(format!(
                    "observed prepared source root identity or metadata changed {phase}"
                ))
            }
            Err(error) => {
                self.valid.store(false, Ordering::Release);
                Err(error)
            }
        }
    }
}

fn materialize_workspace_capture(
    mut capture: PreparedImageCapture,
    source: &Path,
    destination: &Path,
) -> Result<
    (
        WorkspaceSnapshot,
        SourceCaptureWork,
        ImageMaterializationWork,
    ),
    String,
> {
    let copy_work = match materialize_prepared_image(
        &mut capture.image,
        source,
        destination,
        &capture.snapshot,
    ) {
        Ok(work) => work,
        Err(error) => {
            let cleanup = remove_partial_tree(destination);
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!("{error}; {cleanup}")),
            };
        }
    };
    Ok((capture.snapshot, capture.work, copy_work))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ImageMaterializationWork {
    copy_files: usize,
    copy_bytes: u64,
    image_reads: usize,
    image_bytes: u64,
}

fn materialize_prepared_image(
    image: &mut PreparedSourceImage,
    source: &Path,
    destination: &Path,
    expected: &WorkspaceSnapshot,
) -> Result<ImageMaterializationWork, String> {
    validate_image_layout(image, expected)?;
    let source_for_overlap = canonical_or_original(source);
    if destination.exists() {
        return Err(format!(
            "evaluation workspace destination already exists: {}",
            destination.display()
        ));
    }
    let destination_parent = destination.parent().ok_or_else(|| {
        format!(
            "workspace destination has no parent: {}",
            destination.display()
        )
    })?;
    let destination_parent = fs::canonicalize(destination_parent).map_err(|error| {
        format!(
            "failed to resolve workspace destination parent {}: {error}",
            destination_parent.display()
        )
    })?;
    let destination_name = destination.file_name().ok_or_else(|| {
        format!(
            "workspace destination has no file name: {}",
            destination.display()
        )
    })?;
    let destination = destination_parent.join(destination_name);
    if destination.starts_with(&source_for_overlap) || source_for_overlap.starts_with(&destination)
    {
        return Err(format!(
            "evaluation workspace source and destination overlap: {} -> {}",
            source.display(),
            destination.display()
        ));
    }
    fs::create_dir(&destination).map_err(|error| {
        format!(
            "failed to create workspace destination {}: {error}",
            destination.display()
        )
    })?;
    let mut directories = Vec::new();
    for (relative, entry) in expected {
        if relative == "." {
            continue;
        }
        let destination_path = destination.join(relative);
        if entry.kind == WorkspaceSnapshotEntryKind::Directory {
            fs::create_dir(&destination_path).map_err(|error| {
                format!(
                    "failed to create workspace directory {}: {error}",
                    destination_path.display()
                )
            })?;
            directories.push((destination_path, entry));
        }
    }

    image
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to rewind prepared source image: {error}"))?;
    let mut payloads = image.payloads.iter().collect::<Vec<_>>();
    // 同 offset 的零长度 payload 必须先于推进位置的 payload 处理（写入顺序如此）；
    // 仅按 offset 排序时路径序会颠倒二者，导致布局校验误判。
    payloads.sort_by_key(|(_, payload)| (payload.offset, payload.length));
    let mut work = ImageMaterializationWork::default();
    let mut image_position = 0u64;
    for (relative, payload) in payloads {
        if payload.offset != image_position {
            return Err("prepared source image layout is not sequential".to_string());
        }
        let entry = expected
            .get(relative)
            .ok_or_else(|| "prepared source image contains an unknown payload".to_string())?;
        let destination_path = destination.join(relative);
        match entry.kind {
            WorkspaceSnapshotEntryKind::File => {
                let mut output = File::create(&destination_path).map_err(|error| {
                    format!(
                        "failed to create materialized workspace file {}: {error}",
                        destination_path.display()
                    )
                })?;
                let mut remaining = payload.length;
                let mut buffer = [0u8; 64 * 1024];
                while remaining > 0 {
                    let requested =
                        usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
                    let read = image.file.read(&mut buffer[..requested]).map_err(|error| {
                        format!(
                            "failed to materialize workspace file {} from image: {error}",
                            destination_path.display()
                        )
                    })?;
                    if read == 0 {
                        return Err(
                            "prepared source image ended before its declared payload".to_string()
                        );
                    }
                    output.write_all(&buffer[..read]).map_err(|error| {
                        format!(
                            "failed to materialize workspace file {}: {error}",
                            destination_path.display()
                        )
                    })?;
                    remaining = remaining.saturating_sub(u64::try_from(read).unwrap_or(u64::MAX));
                }
                output.flush().map_err(|error| {
                    format!(
                        "failed to flush materialized workspace file {}: {error}",
                        destination_path.display()
                    )
                })?;
                set_snapshot_permissions(&destination_path, entry)?;
            }
            WorkspaceSnapshotEntryKind::Symlink => {
                let length = usize::try_from(payload.length)
                    .map_err(|_| "prepared source link payload is too large".to_string())?;
                let mut target = vec![0u8; length];
                image.file.read_exact(&mut target).map_err(|error| {
                    format!("failed to read prepared source link payload: {error}")
                })?;
                create_symlink_from_payload(&target, &destination_path)?;
            }
            WorkspaceSnapshotEntryKind::Directory => {
                return Err("prepared source image contains a directory payload".to_string());
            }
        }
        image_position = image_position
            .checked_add(payload.length)
            .ok_or_else(|| "prepared source image position overflowed".to_string())?;
        work.copy_files = work.copy_files.saturating_add(1);
        work.copy_bytes = work
            .copy_bytes
            .checked_add(payload.length)
            .ok_or_else(|| "prepared workspace copy byte count overflowed".to_string())?;
        work.image_reads = work.image_reads.saturating_add(1);
        work.image_bytes = work
            .image_bytes
            .checked_add(payload.length)
            .ok_or_else(|| "prepared source image byte count overflowed".to_string())?;
    }
    directories.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (directory, entry) in directories {
        set_snapshot_permissions(&directory, entry)?;
    }
    let root = expected
        .get(".")
        .ok_or_else(|| "prepared workspace snapshot has no root entry".to_string())?;
    set_snapshot_permissions(&destination, root)?;
    Ok(work)
}

fn validate_image_layout(
    image: &PreparedSourceImage,
    expected: &WorkspaceSnapshot,
) -> Result<(), String> {
    let expected_payloads = expected
        .iter()
        .filter(|(_, entry)| entry.kind != WorkspaceSnapshotEntryKind::Directory);
    if expected_payloads.clone().count() != image.payloads.len()
        || expected_payloads.into_iter().any(|(path, entry)| {
            image
                .payloads
                .get(path)
                .is_none_or(|payload| payload.length != entry.length)
        })
    {
        return Err("prepared source image does not match its authoritative snapshot".to_string());
    }
    let mut payloads = image.payloads.values().collect::<Vec<_>>();
    // 同 offset 的零长度 payload 必须先于推进位置的 payload 处理（写入顺序如此）。
    payloads.sort_by_key(|payload| (payload.offset, payload.length));
    let mut expected_length = 0u64;
    for payload in payloads {
        if payload.offset != expected_length {
            return Err("prepared source image layout is not sequential".to_string());
        }
        expected_length = expected_length
            .checked_add(payload.length)
            .ok_or_else(|| "prepared source image length overflowed".to_string())?;
    }
    let actual_length = image
        .file
        .metadata()
        .map_err(|error| format!("failed to inspect prepared source image: {error}"))?
        .len();
    if actual_length != expected_length {
        return Err("prepared source image length does not match its layout".to_string());
    }
    Ok(())
}

fn set_snapshot_permissions(path: &Path, entry: &WorkspaceSnapshotEntry) -> Result<(), String> {
    let mut permissions = fs::metadata(path)
        .map_err(|error| format!("failed to inspect {} permissions: {error}", path.display()))?
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        permissions.set_mode(entry.platform_permissions);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(entry.platform_permissions & 0x1 != 0);
    fs::set_permissions(path, permissions)
        .map_err(|error| format!("failed to set {} permissions: {error}", path.display()))
}

fn remove_partial_tree(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "failed to clean partial workspace {}: {error}",
            path.display()
        )),
    }
}

pub(super) fn workspace_root_identity(path: &Path) -> Result<WorkspaceRootIdentity, String> {
    let root = open_workspace_root(path)?;
    let metadata = root
        .dir_metadata()
        .map_err(|error| format!("failed to inspect workspace root identity: {error}"))?;
    Ok(WorkspaceRootIdentity {
        device: metadata.dev(),
        object: metadata.ino(),
    })
}

fn prepared_root_entry(path: &Path) -> Result<WorkspaceSnapshotEntry, String> {
    let root = open_workspace_root(path)?;
    let metadata = root
        .dir_metadata()
        .map_err(|error| format!("failed to inspect prepared workspace root metadata: {error}"))?;
    snapshot_entry_from_cap(&metadata, None)
}

fn open_workspace_root(path: &Path) -> Result<Dir, String> {
    let parent_path = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| format!("workspace root has no final component: {}", path.display()))?;
    let parent = Dir::open_ambient_dir(parent_path, ambient_authority())
        .map_err(|error| format!("failed to open workspace root parent: {error}"))?;
    let path_metadata = parent.symlink_metadata(name).map_err(|error| {
        format!(
            "failed to inspect workspace root path {}: {error}",
            path.display()
        )
    })?;
    if is_reparse_point_cap(&path_metadata) || !path_metadata.is_dir() {
        return Err(format!(
            "workspace root is not a regular directory: {}",
            path.display()
        ));
    }
    let root = parent.open_dir_nofollow(name).map_err(|error| {
        format!("failed to open workspace root without following links: {error}")
    })?;
    let root_metadata = root
        .dir_metadata()
        .map_err(|error| format!("failed to inspect workspace root handle: {error}"))?;
    if !metadata_matches(&path_metadata, &root_metadata) {
        return Err("workspace root changed while it was being opened".to_string());
    }
    Ok(root)
}

fn metadata_identity(metadata: &Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn metadata_matches(before: &Metadata, after: &Metadata) -> bool {
    metadata_identity(before) == metadata_identity(after)
        && before.is_dir() == after.is_dir()
        && before.is_file() == after.is_file()
        && before.is_symlink() == after.is_symlink()
        && before.len() == after.len()
        && before.permissions().readonly() == after.permissions().readonly()
        && platform_permissions_cap(before) == platform_permissions_cap(after)
}

fn platform_permissions_cap(metadata: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        return metadata.permissions().mode();
    }
    #[cfg(not(unix))]
    {
        u32::from(metadata.permissions().readonly())
    }
}

#[cfg(windows)]
fn is_reparse_point_cap(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
        || cap_std::fs::MetadataExt::file_attributes(metadata) & REPARSE_POINT_ATTRIBUTE != 0
}

#[cfg(not(windows))]
fn is_reparse_point_cap(_metadata: &Metadata) -> bool {
    false
}

#[cfg(unix)]
fn create_symlink_from_payload(target: &[u8], destination: &Path) -> Result<(), String> {
    let target = std::ffi::OsString::from_vec(target.to_vec());
    symlink(&target, destination).map_err(|error| {
        format!(
            "failed to materialize workspace link {}: {error}",
            destination.display()
        )
    })
}

#[cfg(not(unix))]
fn create_symlink_from_payload(_target: &[u8], destination: &Path) -> Result<(), String> {
    Err(format!(
        "prepared source image contains an unsupported reparse point: {}",
        destination.display()
    ))
}

struct PreparedImageBuilder {
    file: File,
    payloads: BTreeMap<String, ImagePayload>,
    next_offset: u64,
}

fn capture_workspace_image(root: &Path) -> Result<PreparedImageCapture, String> {
    let root_dir = open_workspace_root(root)?;
    let metadata = root_dir
        .dir_metadata()
        .map_err(|error| format!("failed to inspect workspace {}: {error}", root.display()))?;
    let root_identity = WorkspaceRootIdentity {
        device: metadata.dev(),
        object: metadata.ino(),
    };
    let mut snapshot = BTreeMap::new();
    snapshot.insert(".".to_string(), snapshot_entry_from_cap(&metadata, None)?);
    let mut work = SourceCaptureWork {
        source_tree_entries_read: 1,
        ..SourceCaptureWork::default()
    };
    let mut builder = PreparedImageBuilder {
        file: anonymous_image_file()?,
        payloads: BTreeMap::new(),
        next_offset: 0,
    };
    let mut context = SourceCaptureContext {
        work: &mut work,
        builder: Some(&mut builder),
    };
    capture_source_entries(&root_dir, "", &mut snapshot, &mut context)?;
    let root_after = root_dir
        .dir_metadata()
        .map_err(|error| format!("failed to revalidate workspace root: {error}"))?;
    let path_after = open_workspace_root(root)
        .and_then(|root| root.dir_metadata().map_err(|error| error.to_string()))
        .map_err(|error| format!("failed to revalidate workspace root path: {error}"))?;
    if !metadata_matches(&metadata, &root_after) || !metadata_matches(&metadata, &path_after) {
        return Err("workspace changed while its image was being captured".to_string());
    }
    builder
        .file
        .flush()
        .map_err(|error| format!("failed to flush prepared source image: {error}"))?;
    builder
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to rewind prepared source image: {error}"))?;
    Ok(PreparedImageCapture {
        root_identity,
        snapshot,
        image: PreparedSourceImage {
            file: builder.file,
            payloads: builder.payloads,
        },
        work,
    })
}

struct SourceCaptureContext<'a> {
    work: &'a mut SourceCaptureWork,
    builder: Option<&'a mut PreparedImageBuilder>,
}

fn capture_source_entries(
    directory: &Dir,
    prefix: &str,
    snapshot: &mut WorkspaceSnapshot,
    context: &mut SourceCaptureContext<'_>,
) -> Result<(), String> {
    for entry in directory
        .read_dir(".")
        .map_err(|error| format!("failed to read workspace directory: {error}"))?
    {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        context.work.source_tree_entries_read =
            context.work.source_tree_entries_read.saturating_add(1);
        if name == ".git" {
            continue;
        }
        let relative = if prefix.is_empty() {
            name.to_string_lossy().into_owned()
        } else {
            format!("{prefix}/{}", name.to_string_lossy())
        };
        let path_metadata = directory
            .symlink_metadata(&name)
            .map_err(|error| format!("failed to inspect workspace path {relative}: {error}"))?;
        if is_reparse_point_cap(&path_metadata) {
            return Err(format!(
                "workspace contains an unsupported reparse point: {relative}"
            ));
        }
        if path_metadata.is_dir() {
            let child = directory.open_dir_nofollow(&name).map_err(|error| {
                format!("failed to open workspace directory {relative}: {error}")
            })?;
            let child_before = child.dir_metadata().map_err(|error| {
                format!("failed to inspect workspace directory {relative}: {error}")
            })?;
            if !metadata_matches(&path_metadata, &child_before) {
                return Err(format!(
                    "workspace path changed while opening directory {relative}"
                ));
            }
            snapshot.insert(
                relative.clone(),
                snapshot_entry_from_cap(&child_before, None)?,
            );
            capture_source_entries(&child, &relative, snapshot, context)?;
            let child_after = child.dir_metadata().map_err(|error| {
                format!("failed to revalidate workspace directory {relative}: {error}")
            })?;
            let path_after = directory.symlink_metadata(&name).map_err(|error| {
                format!("failed to revalidate workspace path {relative}: {error}")
            })?;
            if !metadata_matches(&child_before, &child_after)
                || !metadata_matches(&child_before, &path_after)
            {
                return Err(format!(
                    "workspace changed while directory {relative} was captured"
                ));
            }
        } else if path_metadata.is_file() {
            let mut options = CapOpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            let mut file = directory
                .open_with(&name, &options)
                .map_err(|error| format!("failed to open workspace file {relative}: {error}"))?;
            let before = file
                .metadata()
                .map_err(|error| format!("failed to inspect workspace file {relative}: {error}"))?;
            if !metadata_matches(&path_metadata, &before) {
                return Err(format!(
                    "workspace path changed while opening file {relative}"
                ));
            }
            let (digest, length) = if let Some(builder) = context.builder.as_deref_mut() {
                capture_file_payload(&mut file, &relative, builder, context.work)?
            } else {
                snapshot_file_payload(&mut file, &relative, context.work)?
            };
            validate_opened_file_capture(directory, &name, &file, &before, length, &relative)?;
            snapshot.insert(relative, snapshot_entry_from_cap(&before, Some(digest))?);
        } else if path_metadata.is_symlink() {
            let (digest, length) = if let Some(builder) = context.builder.as_deref_mut() {
                capture_symlink_payload(directory, &name, &relative, builder, context.work)?
            } else {
                snapshot_symlink_payload(directory, &name, &relative, context.work)?
            };
            let path_after = directory.symlink_metadata(&name).map_err(|error| {
                format!("failed to revalidate workspace link {relative}: {error}")
            })?;
            if !metadata_matches(&path_metadata, &path_after) || length != path_metadata.len() {
                return Err(format!(
                    "workspace link changed while it was captured: {relative}"
                ));
            }
            snapshot.insert(
                relative,
                snapshot_entry_from_cap(&path_metadata, Some(digest))?,
            );
        } else {
            return Err(format!(
                "workspace contains a non-regular entry: {relative}"
            ));
        }
    }
    Ok(())
}

/// Revalidate both the opened file object and its directory entry after content capture.
fn validate_opened_file_capture(
    directory: &Dir,
    name: &OsStr,
    file: &cap_std::fs::File,
    before: &Metadata,
    length: u64,
    relative: &str,
) -> Result<(), String> {
    let after = file
        .metadata()
        .map_err(|error| format!("failed to revalidate workspace file {relative}: {error}"))?;
    let path_after = directory
        .symlink_metadata(name)
        .map_err(|error| format!("failed to revalidate workspace path {relative}: {error}"))?;
    if !metadata_matches(before, &after) || !metadata_matches(before, &path_after) {
        return Err(format!(
            "workspace changed while file {relative} was captured"
        ));
    }
    if length != before.len() {
        return Err(format!(
            "workspace file length changed while it was captured: {relative}"
        ));
    }
    Ok(())
}

fn capture_file_payload(
    source: &mut cap_std::fs::File,
    relative: &str,
    builder: &mut PreparedImageBuilder,
    work: &mut SourceCaptureWork,
) -> Result<(String, u64), String> {
    let offset = builder.next_offset;
    let mut length = 0u64;
    let mut digest = Sha256::new();
    digest.update(b"file\0");
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|error| format!("failed to read workspace file {relative}: {error}"))?;
        if read == 0 {
            break;
        }
        builder
            .file
            .write_all(&buffer[..read])
            .map_err(|error| format!("failed to write prepared source image: {error}"))?;
        digest.update(&buffer[..read]);
        length = length
            .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
            .ok_or_else(|| "prepared source payload length overflowed".to_string())?;
    }
    publish_image_payload(relative, offset, length, builder, work)?;
    work.source_tree_content_reads = work.source_tree_content_reads.saturating_add(1);
    work.source_tree_content_bytes = work
        .source_tree_content_bytes
        .checked_add(length)
        .ok_or_else(|| "prepared source read byte count overflowed".to_string())?;
    Ok((format!("sha256:{:x}", digest.finalize()), length))
}

#[cfg(unix)]
fn capture_symlink_payload(
    directory: &Dir,
    name: &OsStr,
    relative: &str,
    builder: &mut PreparedImageBuilder,
    work: &mut SourceCaptureWork,
) -> Result<(String, u64), String> {
    let target = directory
        .read_link_contents(name)
        .map_err(|error| format!("failed to read workspace link {relative}: {error}"))?;
    let target_after = directory
        .read_link_contents(name)
        .map_err(|error| format!("failed to revalidate workspace link {relative}: {error}"))?;
    if target != target_after {
        return Err(format!(
            "workspace link changed while it was captured: {relative}"
        ));
    }
    let bytes = target.as_os_str().as_bytes();
    let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let offset = builder.next_offset;
    builder
        .file
        .write_all(bytes)
        .map_err(|error| format!("failed to write prepared source image: {error}"))?;
    publish_image_payload(relative, offset, length, builder, work)?;
    work.source_tree_content_reads = work.source_tree_content_reads.saturating_add(1);
    work.source_tree_content_bytes = work
        .source_tree_content_bytes
        .checked_add(length)
        .ok_or_else(|| "prepared source read byte count overflowed".to_string())?;
    let mut digest = Sha256::new();
    digest.update(b"symlink\0");
    digest.update(bytes);
    Ok((format!("sha256:{:x}", digest.finalize()), length))
}

#[cfg(not(unix))]
fn capture_symlink_payload(
    _directory: &Dir,
    _name: &OsStr,
    relative: &str,
    _builder: &mut PreparedImageBuilder,
    _work: &mut SourceCaptureWork,
) -> Result<(String, u64), String> {
    Err(format!(
        "workspace contains an unsupported reparse point: {relative}"
    ))
}

fn snapshot_file_payload(
    source: &mut cap_std::fs::File,
    relative: &str,
    work: &mut SourceCaptureWork,
) -> Result<(String, u64), String> {
    let mut digest = Sha256::new();
    digest.update(b"file\0");
    let mut buffer = [0u8; 64 * 1024];
    let mut length = 0u64;
    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|error| format!("failed to read workspace file {relative}: {error}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        length = length
            .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
            .ok_or_else(|| "workspace snapshot byte count overflowed".to_string())?;
    }
    work.source_tree_content_reads = work.source_tree_content_reads.saturating_add(1);
    work.source_tree_content_bytes = work
        .source_tree_content_bytes
        .checked_add(length)
        .ok_or_else(|| "workspace snapshot byte count overflowed".to_string())?;
    Ok((format!("sha256:{:x}", digest.finalize()), length))
}

#[cfg(unix)]
fn snapshot_symlink_payload(
    directory: &Dir,
    name: &OsStr,
    relative: &str,
    work: &mut SourceCaptureWork,
) -> Result<(String, u64), String> {
    let target = directory
        .read_link_contents(name)
        .map_err(|error| format!("failed to read workspace link {relative}: {error}"))?;
    let target_after = directory
        .read_link_contents(name)
        .map_err(|error| format!("failed to revalidate workspace link {relative}: {error}"))?;
    if target != target_after {
        return Err(format!(
            "workspace link changed while it was captured: {relative}"
        ));
    }
    let bytes = target.as_os_str().as_bytes();
    let mut digest = Sha256::new();
    digest.update(b"symlink\0");
    digest.update(bytes);
    let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    work.source_tree_content_reads = work.source_tree_content_reads.saturating_add(1);
    work.source_tree_content_bytes = work
        .source_tree_content_bytes
        .checked_add(length)
        .ok_or_else(|| "workspace snapshot byte count overflowed".to_string())?;
    Ok((format!("sha256:{:x}", digest.finalize()), length))
}

#[cfg(not(unix))]
fn snapshot_symlink_payload(
    _directory: &Dir,
    _name: &OsStr,
    relative: &str,
    _work: &mut SourceCaptureWork,
) -> Result<(String, u64), String> {
    Err(format!(
        "workspace contains an unsupported reparse point: {relative}"
    ))
}

fn publish_image_payload(
    relative: &str,
    offset: u64,
    length: u64,
    builder: &mut PreparedImageBuilder,
    work: &mut SourceCaptureWork,
) -> Result<(), String> {
    if builder
        .payloads
        .insert(relative.to_string(), ImagePayload { offset, length })
        .is_some()
    {
        return Err("prepared source image contains a duplicate path".to_string());
    }
    builder.next_offset = builder
        .next_offset
        .checked_add(length)
        .ok_or_else(|| "prepared source image offset overflowed".to_string())?;
    work.image_bytes = work
        .image_bytes
        .checked_add(length)
        .ok_or_else(|| "prepared source image byte count overflowed".to_string())?;
    Ok(())
}

fn anonymous_image_file() -> Result<File, String> {
    let directory = std::env::temp_dir();
    for _ in 0..16 {
        let path = directory.join(format!(".singularity-image-{}", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_ATTRIBUTE_TEMPORARY, FILE_FLAG_SEQUENTIAL_SCAN, FILE_SHARE_DELETE,
            };
            options
                .share_mode(FILE_SHARE_DELETE)
                .custom_flags(FILE_ATTRIBUTE_TEMPORARY | FILE_FLAG_SEQUENTIAL_SCAN);
        }
        match options.open(&path) {
            Ok(file) => {
                if let Err(error) = fs::remove_file(&path) {
                    drop(file);
                    let _ = fs::remove_file(&path);
                    return Err(format!(
                        "failed to unlink anonymous prepared source image: {error}"
                    ));
                }
                return Ok(file);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "failed to create anonymous prepared source image: {error}"
                ));
            }
        }
    }
    Err("failed to allocate a unique anonymous prepared source image".to_string())
}

/// 对工作区文件生成相对路径到 sha256 的快照。
#[cfg(test)]
pub(super) fn snapshot_workspace(root: &Path) -> Result<WorkspaceSnapshot, String> {
    snapshot_workspace_with_work(root).map(|(snapshot, _)| snapshot)
}

pub(super) fn snapshot_workspace_with_work(
    root: &Path,
) -> Result<(WorkspaceSnapshot, SourceCaptureWork), String> {
    let root_dir = open_workspace_root(root)?;
    let metadata = root_dir
        .dir_metadata()
        .map_err(|error| format!("failed to inspect workspace {}: {error}", root.display()))?;
    let mut snapshot = BTreeMap::new();
    snapshot.insert(".".to_string(), snapshot_entry_from_cap(&metadata, None)?);
    let mut work = SourceCaptureWork {
        source_tree_entries_read: 1,
        ..SourceCaptureWork::default()
    };
    let mut context = SourceCaptureContext {
        work: &mut work,
        builder: None,
    };
    capture_source_entries(&root_dir, "", &mut snapshot, &mut context)?;
    let root_after = root_dir
        .dir_metadata()
        .map_err(|error| format!("failed to revalidate workspace root: {error}"))?;
    let path_after = open_workspace_root(root)
        .and_then(|root| root.dir_metadata().map_err(|error| error.to_string()))
        .map_err(|error| format!("failed to revalidate workspace root path: {error}"))?;
    if !metadata_matches(&metadata, &root_after) || !metadata_matches(&metadata, &path_after) {
        return Err("workspace changed while its snapshot was being captured".to_string());
    }
    Ok((snapshot, work))
}

/// Update a snapshot from a producer-owned closed set of changed paths.
///
/// The caller must only use this when the workspace mutation observation is complete and bound
/// to the same revision.  Unchanged entries remain content-addressed snapshot facts; only the
/// changed paths, their parents, and newly created directory subtrees are inspected.
pub(super) fn snapshot_workspace_incremental(
    root: &Path,
    baseline: &WorkspaceSnapshot,
    changed_paths: &[String],
) -> Result<(WorkspaceSnapshot, SourceCaptureWork), String> {
    let root_dir = open_workspace_root(root)?;
    let root_metadata = root_dir
        .dir_metadata()
        .map_err(|error| format!("failed to inspect workspace {}: {error}", root.display()))?;
    let normalized = changed_paths
        .iter()
        .map(|path| normalize_incremental_path(path))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut snapshot = baseline.clone();
    let mut work = SourceCaptureWork {
        source_tree_entries_read: 1,
        ..SourceCaptureWork::default()
    };
    snapshot.insert(
        ".".to_string(),
        snapshot_entry_from_cap(&root_metadata, None)?,
    );

    // Existing directories are often reported alongside their changed child because their
    // metadata changed. They are only metadata checkpoints; recursively scan a directory when
    // it is newly created (or replaces a non-directory), and subsume descendants only then.
    let mut recursive_roots = Vec::new();
    for relative in &normalized {
        if recursive_roots.iter().any(|root_path: &String| {
            relative == root_path
                || relative
                    .strip_prefix(root_path)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }) {
            continue;
        }
        let baseline_kind = baseline.get(relative).map(|entry| entry.kind);
        let (parent, name) = open_relative_parent(&root_dir, relative)?;
        let metadata = match parent.symlink_metadata(&name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                remove_snapshot_subtree(&mut snapshot, relative);
                if baseline_kind == Some(WorkspaceSnapshotEntryKind::Directory) {
                    recursive_roots.push(relative.clone());
                }
                continue;
            }
            Err(error) => {
                return Err(format!(
                    "failed to inspect workspace path {relative}: {error}"
                ));
            }
        };
        work.source_tree_entries_read = work.source_tree_entries_read.saturating_add(1);
        if is_reparse_point_cap(&metadata) {
            #[cfg(unix)]
            let is_link = metadata.is_symlink();
            #[cfg(not(unix))]
            let is_link = false;
            if !is_link {
                return Err(format!(
                    "workspace contains an unsupported reparse point: {relative}"
                ));
            }
        }
        if metadata.is_symlink() {
            remove_snapshot_subtree(&mut snapshot, relative);
            let (digest, length) = snapshot_symlink_payload(&parent, &name, relative, &mut work)?;
            let after = parent.symlink_metadata(&name).map_err(|error| {
                format!("failed to revalidate workspace link {relative}: {error}")
            })?;
            if !metadata_matches(&metadata, &after) || length != metadata.len() {
                return Err(format!(
                    "workspace link changed while it was captured: {relative}"
                ));
            }
            snapshot.insert(
                relative.clone(),
                snapshot_entry_from_cap(&metadata, Some(digest))?,
            );
        } else if metadata.is_dir() {
            if baseline_kind != Some(WorkspaceSnapshotEntryKind::Directory) {
                remove_snapshot_subtree(&mut snapshot, relative);
                recursive_roots.push(relative.clone());
                let child = parent.open_dir_nofollow(&name).map_err(|error| {
                    format!("failed to open workspace directory {relative}: {error}")
                })?;
                let child_before = child.dir_metadata().map_err(|error| {
                    format!("failed to inspect workspace directory {relative}: {error}")
                })?;
                if !metadata_matches(&metadata, &child_before) {
                    return Err(format!(
                        "workspace path changed while opening directory {relative}"
                    ));
                }
                snapshot.insert(relative.clone(), snapshot_entry_from_cap(&metadata, None)?);
                let mut context = SourceCaptureContext {
                    work: &mut work,
                    builder: None,
                };
                capture_source_entries(&child, relative, &mut snapshot, &mut context)?;
                let child_after = child.dir_metadata().map_err(|error| {
                    format!("failed to revalidate workspace directory {relative}: {error}")
                })?;
                let path_after = parent.symlink_metadata(&name).map_err(|error| {
                    format!("failed to revalidate workspace path {relative}: {error}")
                })?;
                if !metadata_matches(&child_before, &child_after)
                    || !metadata_matches(&child_before, &path_after)
                {
                    return Err(format!(
                        "workspace changed while directory {relative} was captured"
                    ));
                }
            } else {
                snapshot.insert(relative.clone(), snapshot_entry_from_cap(&metadata, None)?);
            }
        } else if metadata.is_file() {
            remove_snapshot_subtree(&mut snapshot, relative);
            let mut options = CapOpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            let mut file = parent
                .open_with(&name, &options)
                .map_err(|error| format!("failed to open workspace file {relative}: {error}"))?;
            let before = file
                .metadata()
                .map_err(|error| format!("failed to inspect workspace file {relative}: {error}"))?;
            if !metadata_matches(&metadata, &before) {
                return Err(format!(
                    "workspace path changed while opening file {relative}"
                ));
            }
            let (digest, length) = snapshot_file_payload(&mut file, relative, &mut work)?;
            validate_opened_file_capture(&parent, &name, &file, &before, length, relative)?;
            snapshot.insert(
                relative.clone(),
                snapshot_entry_from_cap(&before, Some(digest))?,
            );
        } else {
            return Err(format!(
                "workspace contains a non-regular entry: {relative}"
            ));
        }
    }

    // Directory metadata can change without changing a child content digest.  Re-read only the
    // ancestor metadata needed to bind the changed entries, not each directory's full subtree.
    let mut parents = BTreeSet::new();
    for relative in &normalized {
        let mut current = PathBuf::from(relative);
        while let Some(parent_path) = current.parent().map(|path| path.to_path_buf()) {
            let parent = parent_path.to_string_lossy().replace('\\', "/");
            if parent.is_empty() || parent == "." {
                break;
            }
            parents.insert(parent.clone());
            current = parent_path;
        }
    }
    for parent in parents {
        if normalized.contains(&parent)
            || recursive_roots.iter().any(|root_path| {
                parent == *root_path
                    || parent
                        .strip_prefix(root_path)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            })
        {
            continue;
        }
        let (parent_dir, name) = open_relative_parent(&root_dir, &parent)?;
        let metadata = parent_dir.symlink_metadata(&name).map_err(|error| {
            format!("failed to inspect changed workspace parent {parent}: {error}")
        })?;
        work.source_tree_entries_read = work.source_tree_entries_read.saturating_add(1);
        if is_reparse_point_cap(&metadata) || !metadata.is_dir() {
            return Err(format!(
                "changed workspace parent is not a regular directory: {parent}"
            ));
        }
        let child = parent_dir.open_dir_nofollow(&name).map_err(|error| {
            format!("failed to open changed workspace parent {parent}: {error}")
        })?;
        let opened = child.dir_metadata().map_err(|error| {
            format!("failed to inspect changed workspace parent {parent}: {error}")
        })?;
        if !metadata_matches(&metadata, &opened) {
            return Err(format!("changed workspace parent was replaced: {parent}"));
        }
        let path_after = parent_dir.symlink_metadata(&name).map_err(|error| {
            format!("failed to revalidate changed workspace parent {parent}: {error}")
        })?;
        if !metadata_matches(&opened, &path_after) {
            return Err(format!("changed workspace parent was replaced: {parent}"));
        }
        let entry = snapshot_entry_from_cap(&opened, None)?;
        snapshot.insert(parent, entry);
    }
    let root_after = root_dir
        .dir_metadata()
        .map_err(|error| format!("failed to revalidate workspace root: {error}"))?;
    if !metadata_matches(&root_metadata, &root_after) {
        return Err(
            "workspace root changed while its incremental snapshot was captured".to_string(),
        );
    }
    Ok((snapshot, work))
}

fn open_relative_parent(root: &Dir, relative: &str) -> Result<(Dir, OsString), String> {
    let mut components = relative.split('/').peekable();
    let mut directory = root
        .try_clone()
        .map_err(|error| format!("workspace root clone failed: {error}"))?;
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            return Ok((directory, OsString::from(component)));
        }
        directory = match directory.open_dir_nofollow(component) {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!(
                    "workspace parent is missing for {relative}: {error}"
                ));
            }
            Err(error) => {
                return Err(format!(
                    "workspace parent open failed for {relative}: {error}"
                ));
            }
        };
    }
    Err("workspace incremental path is empty".to_string())
}

fn normalize_incremental_path(path: &str) -> Result<String, String> {
    let normalized = path.replace('\\', "/");
    let candidate = Path::new(&normalized);
    if normalized.is_empty()
        || candidate.is_absolute()
        || normalized == "."
        || normalized.starts_with("./")
        || normalized.ends_with('/')
        || normalized
            .split('/')
            .any(|component| component.is_empty() || component == "..")
    {
        return Err(format!(
            "workspace mutation path is not a safe relative path: {path}"
        ));
    }
    Ok(normalized)
}

fn remove_snapshot_subtree(snapshot: &mut WorkspaceSnapshot, relative: &str) {
    let prefix = format!("{relative}/");
    snapshot.retain(|path, _| path != relative && !path.starts_with(&prefix));
}

/// 复用已捕获的完整快照计算稳定摘要，避免再次读取工作区文件。
pub(super) fn workspace_snapshot_digest(snapshot: &WorkspaceSnapshot) -> Result<String, String> {
    let canonical = serde_json::to_vec(&snapshot)
        .map_err(|error| format!("failed to serialize workspace snapshot: {error}"))?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

fn snapshot_entry_from_cap(
    metadata: &Metadata,
    content_digest: Option<String>,
) -> Result<WorkspaceSnapshotEntry, String> {
    let kind = if metadata.is_symlink() {
        WorkspaceSnapshotEntryKind::Symlink
    } else if metadata.is_dir() {
        WorkspaceSnapshotEntryKind::Directory
    } else if metadata.is_file() {
        WorkspaceSnapshotEntryKind::File
    } else {
        return Err("workspace snapshot encountered a non-regular entry".to_string());
    };
    Ok(WorkspaceSnapshotEntry {
        kind,
        content_digest,
        platform_permissions: platform_permissions_cap(metadata),
        length: if kind == WorkspaceSnapshotEntryKind::Directory {
            0
        } else {
            metadata.len()
        },
    })
}

/// 返回前后快照中内容发生变化的路径。
pub(super) fn changed_paths(before: &WorkspaceSnapshot, after: &WorkspaceSnapshot) -> Vec<String> {
    before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect()
}

/// 归因变更并排除闭集工具链产物。
pub(super) fn evaluation_changed_paths(
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
    pristine_source: &WorkspaceSnapshot,
) -> Vec<String> {
    let changed = changed_paths(before, after);
    changed
        .iter()
        .filter(|path| {
            !is_new_changed_directory_ancestor(before, after, &changed, path)
                && (pristine_source.contains_key(path.as_str())
                    || !is_toolchain_artifact_path(path))
        })
        .cloned()
        .collect()
}

fn is_new_changed_directory_ancestor(
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
    changed: &[String],
    path: &str,
) -> bool {
    if before.contains_key(path)
        || after.get(path).map(|entry| entry.kind) != Some(WorkspaceSnapshotEntryKind::Directory)
    {
        return false;
    }
    let prefix = format!("{path}/");
    changed
        .iter()
        .any(|candidate| candidate.starts_with(&prefix))
}

/// 将工作区变更路径转换为前后内容 evidence。
pub(super) fn workspace_change_evidence(
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
    pristine_source: &WorkspaceSnapshot,
) -> Vec<WorkspaceChangeEvidence> {
    evaluation_changed_paths(before, after, pristine_source)
        .into_iter()
        .map(|path| WorkspaceChangeEvidence {
            change_kind: match (before.contains_key(&path), after.contains_key(&path)) {
                (false, true) => "added",
                (true, false) => "deleted",
                (true, true) => "modified",
                (false, false) => unreachable!("changed path must exist in one snapshot"),
            },
            before_sha256: before
                .get(&path)
                .and_then(|entry| entry.content_digest.clone()),
            after_sha256: after
                .get(&path)
                .and_then(|entry| entry.content_digest.clone()),
            path,
        })
        .collect()
}

/// 计算工作区变更 evidence 的稳定摘要。
pub(super) fn patch_evidence_digest(evidence: &[WorkspaceChangeEvidence]) -> Option<String> {
    if evidence.is_empty() {
        return None;
    }
    let canonical = serde_json::to_vec(evidence).expect("workspace evidence serializes");
    Some(format!("sha256:{:x}", Sha256::digest(canonical)))
}

/// 优先返回规范化路径，失败时保留原路径。
pub(super) fn canonical_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;

    use singularity_sandbox::{
        CommandRequest, CommandResult, SandboxCapabilities, SandboxPreflightFact,
        SandboxPreflightOutcome,
    };

    struct QueueObserver {
        observations: Arc<Mutex<VecDeque<PreparedWorkspaceObservation>>>,
    }

    impl PreparedWorkspaceObserver for QueueObserver {
        fn checkpoint(&mut self) -> Result<PreparedWorkspaceObservation, String> {
            self.observations
                .lock()
                .map_err(|_| "observation queue is poisoned".to_string())?
                .pop_front()
                .ok_or_else(|| "observation queue is exhausted".to_string())
        }
    }

    struct TestBackend {
        observations: Arc<Mutex<VecDeque<PreparedWorkspaceObservation>>>,
        observer_available: bool,
    }

    impl SandboxBackend for TestBackend {
        fn name(&self) -> &'static str {
            "prepared_test"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict().with_change_detection()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            CommandResult::unsupported(&request.command_id, "not used")
        }

        fn observe_prepared_workspace(
            &self,
            _workspace: &Path,
        ) -> Result<Option<Box<dyn PreparedWorkspaceObserver>>, String> {
            Ok(self.observer_available.then(|| {
                Box::new(QueueObserver {
                    observations: Arc::clone(&self.observations),
                }) as Box<dyn PreparedWorkspaceObserver>
            }))
        }
    }

    fn test_backend(
        observations: impl IntoIterator<Item = PreparedWorkspaceObservation>,
    ) -> TestBackend {
        TestBackend {
            observations: Arc::new(Mutex::new(observations.into_iter().collect())),
            observer_available: true,
        }
    }

    fn supported_preflight() -> SandboxPreflightReport {
        SandboxPreflightReport {
            outcome: SandboxPreflightOutcome::Supported,
            error_code: None,
            profile: "workspace_write_network_denied".to_string(),
            backend: "prepared_test".to_string(),
            missing_capabilities: Vec::new(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            kernel: Some("test-kernel".to_string()),
            filesystem: Some(if cfg!(windows) {
                "NTFS".to_string()
            } else {
                "test-filesystem".to_string()
            }),
            overlayfs: SandboxPreflightFact::Passed,
            user_namespace: SandboxPreflightFact::Passed,
            mount_namespace: SandboxPreflightFact::Passed,
            pid_namespace: SandboxPreflightFact::Passed,
            network_namespace: SandboxPreflightFact::Passed,
            no_new_privs: SandboxPreflightFact::Passed,
            seccomp: SandboxPreflightFact::Passed,
            landlock: SandboxPreflightFact::Passed,
            transactional_workspace: SandboxPreflightFact::Passed,
            network_denied: SandboxPreflightFact::Passed,
            protected_paths: SandboxPreflightFact::Passed,
        }
    }

    #[test]
    fn observed_prepared_source_reuses_generation_without_a_full_hash_scan() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        fs::create_dir(&source).expect("source");
        fs::write(source.join("first.txt"), b"first").expect("first");
        fs::write(source.join("second.txt"), b"second").expect("second");
        let backend = test_backend(std::iter::repeat_n(
            PreparedWorkspaceObservation::Unchanged,
            9,
        ));
        let capture = ObservedPreparedSource::capture(&source, &backend, &supported_preflight())
            .expect("capture");
        assert_eq!(capture.full_scans, 1);
        assert_eq!(
            capture.work,
            SourceCaptureWork {
                source_tree_entries_read: 3,
                source_tree_content_reads: 2,
                source_tree_content_bytes: 11,
                image_bytes: 11,
            }
        );
        let snapshot = capture.snapshot;
        let prepared = capture.observed.expect("continuous observer");

        let mut metrics = Vec::new();
        for stage in ["baseline", "agent", "public", "hidden"] {
            let destination = temp.path().join(stage);
            let materialized = materialize_prepared_workspace(
                stage,
                &source,
                &destination,
                &snapshot,
                Some(&prepared),
                backend.name(),
                &CancellationToken::new(),
            )
            .expect("materialize");
            assert_eq!(
                snapshot_workspace(&destination).expect("destination snapshot"),
                snapshot
            );
            metrics.push(materialized.metric);
        }

        assert!(metrics.iter().all(|metric| metric.observation == "reused"));
        assert_eq!(
            metrics
                .iter()
                .map(|metric| metric.copy_files)
                .sum::<usize>(),
            8
        );
        assert_eq!(
            metrics.iter().map(|metric| metric.copy_bytes).sum::<u64>(),
            44
        );
        assert_eq!(
            metrics
                .iter()
                .map(|metric| metric.source_tree_entry_reads)
                .sum::<usize>(),
            0
        );
        assert_eq!(
            metrics
                .iter()
                .map(|metric| metric.source_tree_content_reads)
                .sum::<usize>(),
            0
        );
        assert_eq!(
            metrics
                .iter()
                .map(|metric| metric.image_reads)
                .sum::<usize>(),
            8
        );
        assert_eq!(
            metrics.iter().map(|metric| metric.image_bytes).sum::<u64>(),
            44
        );
    }

    #[cfg(unix)]
    #[test]
    fn opened_file_capture_rejects_same_length_path_replacement() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        fs::create_dir(&source).expect("source");
        let path = source.join("value.txt");
        fs::write(&path, b"old!").expect("original file");

        let directory = open_workspace_root(&source).expect("open source root");
        let name = OsStr::new("value.txt");
        let mut options = CapOpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let mut file = directory
            .open_with(name, &options)
            .expect("open file handle");
        let before = file.metadata().expect("opened metadata");

        fs::rename(&path, temp.path().join("displaced.txt")).expect("displace original");
        fs::write(&path, b"new!").expect("same-length replacement");

        let error = validate_opened_file_capture(
            &directory,
            name,
            &file,
            &before,
            before.len(),
            "value.txt",
        )
        .expect_err("path replacement must fail closed");
        assert!(error.contains("changed"));

        let mut opened_bytes = Vec::new();
        file.read_to_end(&mut opened_bytes)
            .expect("read opened handle");
        assert_eq!(opened_bytes, b"old!");
    }

    #[test]
    fn observed_prepared_source_rejects_metadata_change_and_cleans_stage() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        let destination = temp.path().join("stage");
        fs::create_dir(&source).expect("source");
        fs::write(source.join("value.txt"), b"value").expect("value");
        let backend = test_backend([
            PreparedWorkspaceObservation::Unchanged,
            PreparedWorkspaceObservation::Unchanged,
            PreparedWorkspaceObservation::Changed(vec!["nested/file.txt".to_string()]),
        ]);
        let capture = ObservedPreparedSource::capture(&source, &backend, &supported_preflight())
            .expect("capture");
        let snapshot = capture.snapshot;
        let prepared = capture.observed.expect("continuous observer");

        let error = prepared
            .materialize(
                "agent",
                &destination,
                &snapshot,
                backend.name(),
                &CancellationToken::new(),
            )
            .expect_err("metadata drift must fail");

        assert!(error.contains("changed"));
        assert!(!destination.exists());
        let retry_destination = temp.path().join("retry");
        let retry = prepared
            .materialize(
                "public",
                &retry_destination,
                &snapshot,
                backend.name(),
                &CancellationToken::new(),
            )
            .expect_err("invalidated token must never be reused");
        assert!(retry.contains("invalidated"));
        assert!(!retry_destination.exists());
    }

    #[test]
    fn observed_prepared_source_unknown_observation_fails_before_copy() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        let destination = temp.path().join("stage");
        fs::create_dir(&source).expect("source");
        fs::write(source.join("value.txt"), b"value").expect("value");
        let backend = test_backend([
            PreparedWorkspaceObservation::Unchanged,
            PreparedWorkspaceObservation::Unknown,
        ]);
        let capture = ObservedPreparedSource::capture(&source, &backend, &supported_preflight())
            .expect("capture");
        let snapshot = capture.snapshot;
        let prepared = capture.observed.expect("continuous observer");

        let error = prepared
            .materialize(
                "hidden",
                &destination,
                &snapshot,
                backend.name(),
                &CancellationToken::new(),
            )
            .expect_err("unknown must fail");

        assert!(error.contains("incomplete"));
        assert!(!destination.exists());
        let retry = prepared
            .materialize(
                "public",
                &temp.path().join("retry"),
                &snapshot,
                backend.name(),
                &CancellationToken::new(),
            )
            .expect_err("incomplete observation must permanently invalidate");
        assert!(retry.contains("invalidated"));
    }

    #[test]
    fn observed_prepared_source_contract_mismatch_fails_before_copy() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        let destination = temp.path().join("stage");
        fs::create_dir(&source).expect("source");
        let backend = test_backend([PreparedWorkspaceObservation::Unchanged]);
        let capture = ObservedPreparedSource::capture(&source, &backend, &supported_preflight())
            .expect("capture");
        let snapshot = capture.snapshot;
        let prepared = capture.observed.expect("continuous observer");

        let error = prepared
            .materialize(
                "public",
                &destination,
                &snapshot,
                "different_backend",
                &CancellationToken::new(),
            )
            .expect_err("contract mismatch");

        assert!(error.contains("contract"));
        assert!(!destination.exists());
        let retry = prepared
            .materialize(
                "public",
                &temp.path().join("retry"),
                &snapshot,
                backend.name(),
                &CancellationToken::new(),
            )
            .expect_err("contract mismatch must permanently invalidate");
        assert!(retry.contains("invalidated"));
    }

    #[test]
    fn missing_continuous_observer_full_rescan_detects_drift() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        let destination = temp.path().join("stage");
        fs::create_dir(&source).expect("source");
        fs::create_dir(source.join("nested")).expect("nested");
        // 同字节长度内容：capture 后用等长内容替换，改 content_digest 不改 length，
        // 验证无 observer 时必须完整重读源树内容才能检测漂移，而不是复用旧镜像。
        fs::write(source.join("nested/value.txt"), b"AAAAA").expect("value");
        let backend = TestBackend {
            observations: Arc::new(Mutex::new(VecDeque::new())),
            observer_available: false,
        };
        let capture = ObservedPreparedSource::capture(&source, &backend, &supported_preflight())
            .expect("image capture");
        assert_eq!(capture.full_scans, 1);
        assert!(capture.observed.is_none());
        let snapshot = capture.snapshot;
        // 同长度内容替换：长度不变，content_digest 变化。
        fs::write(source.join("nested/value.txt"), b"BBBBB").expect("drift");

        let error = materialize_prepared_workspace(
            "baseline",
            &source,
            &destination,
            &snapshot,
            None,
            backend.name(),
            &CancellationToken::new(),
        )
        .expect_err("drift must be detected by full rescan");

        assert!(
            error.contains("prepared source changed before full workspace materialization"),
            "unexpected error: {error}"
        );
        assert!(!destination.exists());
    }

    #[test]
    fn missing_continuous_observer_materializes_unchanged_source_with_full_metrics() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        let destination = temp.path().join("stage");
        fs::create_dir(&source).expect("source");
        fs::create_dir(source.join("nested")).expect("nested");
        fs::write(source.join("nested/value.txt"), b"value").expect("value");
        fs::write(source.join("root.txt"), b"root").expect("root");
        let backend = TestBackend {
            observations: Arc::new(Mutex::new(VecDeque::new())),
            observer_available: false,
        };
        let capture = ObservedPreparedSource::capture(&source, &backend, &supported_preflight())
            .expect("image capture");
        assert_eq!(capture.full_scans, 1);
        assert!(capture.observed.is_none());
        let snapshot = capture.snapshot;

        let materialized = materialize_prepared_workspace(
            "baseline",
            &source,
            &destination,
            &snapshot,
            None,
            backend.name(),
            &CancellationToken::new(),
        )
        .expect("materialization");

        assert_eq!(materialized.metric.observation, "full");
        // 源树条目：根 + nested 目录 + nested/value.txt 文件 + root.txt 文件 = 4 次条目读取。
        assert_eq!(materialized.metric.source_tree_entry_reads, 4);
        // 源树内容读取：2 个文件内容各读一次 = 2 次。
        assert_eq!(materialized.metric.source_tree_content_reads, 2);
        // 物化内容：2 个文件 payload，image 读取次数与字节数与 fixture 一致。
        assert_eq!(materialized.metric.image_reads, 2);
        assert_eq!(materialized.metric.copy_files, 2);
        assert_eq!(materialized.metric.copy_bytes, 9);
        assert_eq!(materialized.metric.image_bytes, 9);
        assert!(destination.exists());
        assert_eq!(
            fs::read(destination.join("nested/value.txt")).expect("value"),
            b"value"
        );
    }

    #[test]
    fn observed_prepared_source_root_replacement_fails_closed() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        let displaced = temp.path().join("displaced");
        let destination = temp.path().join("stage");
        fs::create_dir(&source).expect("source");
        fs::write(source.join("value.txt"), b"value").expect("value");
        let backend = test_backend([PreparedWorkspaceObservation::Unchanged]);
        let capture = ObservedPreparedSource::capture(&source, &backend, &supported_preflight())
            .expect("capture");
        let snapshot = capture.snapshot;
        let prepared = capture.observed.expect("continuous observer");
        fs::rename(&source, &displaced).expect("displace root");
        fs::create_dir(&source).expect("replacement root");
        fs::write(source.join("value.txt"), b"value").expect("replacement value");

        let error = prepared
            .materialize(
                "baseline",
                &destination,
                &snapshot,
                backend.name(),
                &CancellationToken::new(),
            )
            .expect_err("root replacement");

        assert!(error.contains("root identity"));
        assert!(!destination.exists());
    }

    /// 回归：零长度文件与紧随其后写入的非空文件共享同一 image offset（写入顺序为
    /// 空文件先、不推进位置）。读取端必须按 (offset, length) 排序（同 offset 时
    /// 零长度在前），否则路径序会把非空文件排在空文件前，位置推进后空文件
    /// offset 不匹配而误报 "layout is not sequential"（Linux ext4 readdir 顺序
    /// 触发；Windows NTFS 顺序未触发）。
    #[test]
    fn image_layout_zero_length_payload_before_advancing_payload_is_sequential() {
        let mut file = anonymous_image_file().expect("image file");
        use std::io::Write as _;
        file.write_all(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabtai")
            .expect("image bytes");
        // 布局（写入顺序）：offset 0 处先写入空文件（0 字节，位置不推进），
        // 再写入 28 字节文件（位置推进到 28），offset 28 处 3 字节文件。
        let mut payloads = BTreeMap::new();
        payloads.insert(
            "empty.yml".to_string(),
            ImagePayload {
                offset: 0,
                length: 0,
            },
        );
        payloads.insert(
            "data.sql".to_string(),
            ImagePayload {
                offset: 0,
                length: 28,
            },
        );
        payloads.insert(
            "tail.txt".to_string(),
            ImagePayload {
                offset: 28,
                length: 3,
            },
        );
        let image = PreparedSourceImage { file, payloads };
        let mut expected = BTreeMap::new();
        let entry = |kind, length| WorkspaceSnapshotEntry {
            kind,
            content_digest: None,
            platform_permissions: 0o644,
            length,
        };
        expected.insert(
            ".".to_string(),
            entry(WorkspaceSnapshotEntryKind::Directory, 0),
        );
        expected.insert(
            "empty.yml".to_string(),
            entry(WorkspaceSnapshotEntryKind::File, 0),
        );
        expected.insert(
            "data.sql".to_string(),
            entry(WorkspaceSnapshotEntryKind::File, 28),
        );
        expected.insert(
            "tail.txt".to_string(),
            entry(WorkspaceSnapshotEntryKind::File, 3),
        );

        validate_image_layout(&image, &expected).expect("zero-length payload layout must validate");
    }

    #[test]
    fn prepared_materialization_honors_pre_cancelled_state() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        let destination = temp.path().join("stage");
        fs::create_dir(&source).expect("source");
        let snapshot = snapshot_workspace(&source).expect("snapshot");
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = materialize_prepared_workspace(
            "baseline",
            &source,
            &destination,
            &snapshot,
            None,
            "prepared_test",
            &cancellation,
        )
        .expect_err("cancelled");

        assert!(error.contains("cancelled"));
        assert!(!destination.exists());
    }

    #[test]
    fn observed_prepared_source_cancel_and_image_failure_are_not_reusable() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        fs::create_dir(&source).expect("source");
        fs::write(source.join("value.txt"), b"value").expect("value");

        let cancelled_backend = test_backend([PreparedWorkspaceObservation::Unchanged]);
        let cancelled_capture =
            ObservedPreparedSource::capture(&source, &cancelled_backend, &supported_preflight())
                .expect("cancel capture");
        let cancelled_snapshot = cancelled_capture.snapshot;
        let cancelled = cancelled_capture.observed.expect("cancel token");
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        cancelled
            .materialize(
                "baseline",
                &temp.path().join("cancelled"),
                &cancelled_snapshot,
                cancelled_backend.name(),
                &cancellation,
            )
            .expect_err("cancelled materialization");
        let retry = cancelled
            .materialize(
                "baseline",
                &temp.path().join("cancelled-retry"),
                &cancelled_snapshot,
                cancelled_backend.name(),
                &CancellationToken::new(),
            )
            .expect_err("cancelled token must stay invalid");
        assert!(retry.contains("invalidated"));

        let image_backend = test_backend([
            PreparedWorkspaceObservation::Unchanged,
            PreparedWorkspaceObservation::Unchanged,
        ]);
        let image_capture =
            ObservedPreparedSource::capture(&source, &image_backend, &supported_preflight())
                .expect("image capture");
        let image_snapshot = image_capture.snapshot;
        let image = image_capture.observed.expect("image token");
        image
            .image
            .lock()
            .expect("image lock")
            .file
            .set_len(0)
            .expect("truncate image");
        let error = image
            .materialize(
                "baseline",
                &temp.path().join("image"),
                &image_snapshot,
                image_backend.name(),
                &CancellationToken::new(),
            )
            .expect_err("truncated image");
        assert!(error.contains("length does not match"));
        let retry = image
            .materialize(
                "baseline",
                &temp.path().join("image-retry"),
                &image_snapshot,
                image_backend.name(),
                &CancellationToken::new(),
            )
            .expect_err("failed image must stay invalid");
        assert!(retry.contains("invalidated"));
    }

    #[test]
    fn observed_prepared_source_materializes_two_thousand_files_only_from_image() {
        const FILES: usize = 2_001;
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        fs::create_dir(&source).expect("source");
        for index in 0..FILES {
            fs::write(source.join(format!("file-{index:04}.txt")), b"x").expect("fixture file");
        }
        let backend = test_backend(std::iter::repeat_n(
            PreparedWorkspaceObservation::Unchanged,
            9,
        ));
        let capture = ObservedPreparedSource::capture(&source, &backend, &supported_preflight())
            .expect("capture");
        assert_eq!(capture.full_scans, 1);
        assert_eq!(capture.work.source_tree_entries_read, FILES + 1);
        assert_eq!(capture.work.source_tree_content_reads, FILES);
        assert_eq!(capture.work.source_tree_content_bytes, FILES as u64);
        assert_eq!(capture.work.image_bytes, FILES as u64);
        let snapshot = capture.snapshot;
        let prepared = capture.observed.expect("observed image");

        let mut metrics = Vec::new();
        for stage in ["baseline", "agent", "public", "hidden"] {
            let destination = temp.path().join(stage);
            let materialized = prepared
                .materialize(
                    stage,
                    &destination,
                    &snapshot,
                    backend.name(),
                    &CancellationToken::new(),
                )
                .expect("image materialization");
            assert_eq!(
                snapshot_workspace(&destination).expect("destination snapshot"),
                snapshot
            );
            metrics.push(materialized.metric);
        }

        assert_eq!(
            metrics
                .iter()
                .map(|metric| metric.source_tree_entry_reads)
                .sum::<usize>(),
            0
        );
        assert_eq!(
            metrics
                .iter()
                .map(|metric| metric.source_tree_content_reads)
                .sum::<usize>(),
            0
        );
        assert_eq!(
            metrics
                .iter()
                .map(|metric| metric.image_reads)
                .sum::<usize>(),
            FILES * 4
        );
        assert_eq!(
            metrics.iter().map(|metric| metric.image_bytes).sum::<u64>(),
            (FILES * 4) as u64
        );
        assert_eq!(
            metrics
                .iter()
                .map(|metric| metric.copy_files)
                .sum::<usize>(),
            FILES * 4
        );
        assert_eq!(
            metrics.iter().map(|metric| metric.copy_bytes).sum::<u64>(),
            (FILES * 4) as u64
        );
    }

    #[cfg(unix)]
    #[test]
    fn observed_image_preserves_symlink_empty_directory_and_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        let destination = temp.path().join("stage");
        fs::create_dir_all(source.join("empty").join("nested")).expect("empty tree");
        fs::write(source.join("script.sh"), b"#!/bin/sh\n").expect("script");
        let mut permissions = fs::metadata(source.join("script.sh"))
            .expect("metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(source.join("script.sh"), permissions).expect("mode");
        symlink("script.sh", source.join("script-link")).expect("symlink");
        let backend = test_backend([
            PreparedWorkspaceObservation::Unchanged,
            PreparedWorkspaceObservation::Unchanged,
            PreparedWorkspaceObservation::Unchanged,
        ]);
        let capture = ObservedPreparedSource::capture(&source, &backend, &supported_preflight())
            .expect("capture");
        let snapshot = capture.snapshot;
        let prepared = capture.observed.expect("image");

        prepared
            .materialize(
                "baseline",
                &destination,
                &snapshot,
                backend.name(),
                &CancellationToken::new(),
            )
            .expect("materialize");

        assert_eq!(
            snapshot_workspace(&destination).expect("snapshot"),
            snapshot
        );
        assert!(destination.join("empty").join("nested").is_dir());
        assert_eq!(
            fs::read_link(destination.join("script-link")).expect("link target"),
            PathBuf::from("script.sh")
        );
        assert_eq!(
            fs::metadata(destination.join("script.sh"))
                .expect("mode")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[test]
    fn workspace_snapshot_preserves_empty_directories_during_copy() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir_all(source.join("empty").join("nested")).expect("empty tree");

        let source_snapshot = snapshot_workspace(&source).expect("source snapshot");
        let copied_snapshot = copy_tree_checked(&source, &destination).expect("copy");

        assert!(source_snapshot.contains_key("empty"));
        assert!(source_snapshot.contains_key("empty/nested"));
        assert_eq!(copied_snapshot, source_snapshot);
        assert!(destination.join("empty").join("nested").is_dir());
    }

    #[test]
    fn workspace_snapshot_reports_actual_scan_work() {
        let temp = tempfile::tempdir().expect("workspace");
        fs::create_dir_all(temp.path().join("nested")).expect("nested directory");
        fs::write(temp.path().join("top.txt"), b"top").expect("top file");
        fs::write(temp.path().join("nested").join("child.txt"), b"child").expect("child file");

        let (snapshot, work) = snapshot_workspace_with_work(temp.path()).expect("snapshot");

        assert_eq!(snapshot.len(), 4);
        assert_eq!(work.source_tree_entries_read, 4);
        assert_eq!(work.source_tree_content_reads, 2);
        assert_eq!(work.source_tree_content_bytes, 8);
    }

    #[test]
    fn incremental_snapshot_reuses_unchanged_entries_and_reads_changed_file_only() {
        let temp = tempfile::tempdir().expect("workspace");
        fs::create_dir_all(temp.path().join("nested")).expect("nested directory");
        fs::write(temp.path().join("unchanged.txt"), b"unchanged").expect("unchanged");
        fs::write(temp.path().join("nested").join("value.txt"), b"before").expect("value");
        let before = snapshot_workspace(temp.path()).expect("before snapshot");

        fs::write(temp.path().join("nested").join("value.txt"), b"after").expect("modified value");
        let (after, work) =
            snapshot_workspace_incremental(temp.path(), &before, &["nested/value.txt".to_string()])
                .expect("incremental snapshot");
        assert_eq!(changed_paths(&before, &after), ["nested/value.txt"]);
        assert_eq!(work.source_tree_content_reads, 1);
        assert_eq!(work.source_tree_content_bytes, 5);
        assert!(work.source_tree_entries_read < before.len());
    }

    #[test]
    fn incremental_snapshot_handles_created_and_deleted_paths() {
        let temp = tempfile::tempdir().expect("workspace");
        fs::create_dir_all(temp.path().join("nested")).expect("nested directory");
        fs::write(temp.path().join("nested").join("removed.txt"), b"removed").expect("removed");
        fs::write(
            temp.path().join("nested").join("unchanged.txt"),
            b"unchanged",
        )
        .expect("unchanged");
        let before = snapshot_workspace(temp.path()).expect("before snapshot");
        fs::remove_file(temp.path().join("nested").join("removed.txt")).expect("delete");
        fs::write(temp.path().join("nested").join("created.txt"), b"created").expect("create");

        let (after, work) = snapshot_workspace_incremental(
            temp.path(),
            &before,
            &[
                "nested".to_string(),
                "nested/removed.txt".to_string(),
                "nested/created.txt".to_string(),
            ],
        )
        .expect("incremental snapshot");
        assert_eq!(
            changed_paths(&before, &after),
            ["nested/created.txt", "nested/removed.txt"]
        );
        assert_eq!(work.source_tree_content_reads, 1);
        assert_eq!(
            after
                .get("nested/unchanged.txt")
                .and_then(|entry| entry.content_digest.as_deref()),
            before
                .get("nested/unchanged.txt")
                .and_then(|entry| entry.content_digest.as_deref())
        );
    }

    #[test]
    fn incremental_snapshot_rejects_a_missing_ancestor_as_drift() {
        let temp = tempfile::tempdir().expect("workspace");
        fs::create_dir_all(temp.path().join("nested")).expect("nested directory");
        fs::write(temp.path().join("nested").join("value.txt"), b"value").expect("value");
        let before = snapshot_workspace(temp.path()).expect("before snapshot");
        fs::remove_dir_all(temp.path().join("nested")).expect("remove ancestor");

        let error =
            snapshot_workspace_incremental(temp.path(), &before, &["nested/value.txt".to_string()])
                .expect_err("an undeclared ancestor deletion must fail closed");

        assert!(error.contains("workspace parent is missing"));
    }

    #[test]
    fn incremental_snapshot_scans_new_directory_subtree_once() {
        let temp = tempfile::tempdir().expect("workspace");
        let before = snapshot_workspace(temp.path()).expect("before snapshot");
        fs::create_dir_all(temp.path().join("new").join("nested")).expect("new directory");
        fs::write(temp.path().join("new").join("value.txt"), b"value").expect("value");
        fs::write(
            temp.path().join("new").join("nested").join("child.txt"),
            b"child",
        )
        .expect("child");
        let (after, work) = snapshot_workspace_incremental(
            temp.path(),
            &before,
            &[
                "new".to_string(),
                "new/value.txt".to_string(),
                "new/nested".to_string(),
                "new/nested/child.txt".to_string(),
            ],
        )
        .expect("incremental snapshot");

        assert_eq!(
            changed_paths(&before, &after),
            ["new", "new/nested", "new/nested/child.txt", "new/value.txt",]
        );
        assert_eq!(work.source_tree_content_reads, 2);
    }

    #[test]
    fn incremental_snapshot_rejects_unsafe_paths() {
        let temp = tempfile::tempdir().expect("workspace");
        let before = snapshot_workspace(temp.path()).expect("before snapshot");
        let error =
            snapshot_workspace_incremental(temp.path(), &before, &["../outside.txt".to_string()])
                .expect_err("path traversal must fail closed");
        assert!(error.contains("not a safe relative path"));
    }

    #[cfg(windows)]
    #[test]
    fn workspace_snapshot_detects_windows_readonly_metadata_changes() {
        let temp = tempfile::tempdir().expect("temp");
        let file = temp.path().join("value.txt");
        fs::write(&file, b"value").expect("value");
        let before = snapshot_workspace(temp.path()).expect("before");
        let mut permissions = fs::metadata(&file).expect("metadata").permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&file, permissions).expect("readonly");
        let after = snapshot_workspace(temp.path()).expect("after");

        assert_ne!(before, after);
        assert_eq!(changed_paths(&before, &after), ["value.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_snapshot_detects_unix_executable_bit_changes() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().expect("temp");
        let script = temp.path().join("script.sh");
        fs::write(&script, b"#!/bin/sh\n").expect("script");
        let mut permissions = fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&script, permissions).expect("initial mode");
        let before = snapshot_workspace(temp.path()).expect("before");
        let mut permissions = fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("executable mode");
        let after = snapshot_workspace(temp.path()).expect("after");

        assert_ne!(before, after);
        assert_eq!(changed_paths(&before, &after), ["script.sh"]);
    }
}
