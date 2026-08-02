use anyhow::Result;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use std::thread::JoinHandle;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_IO_INCOMPLETE, ERROR_NOT_FOUND, ERROR_NOTIFY_ENUM_DIR,
    ERROR_OPERATION_ABORTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED,
    FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SECURITY,
    FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    ReadDirectoryChangesW,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};

const CHANGE_BUFFER_BYTES: usize = 64 * 1024;
const MAX_CAPTURED_PATHS: usize = 4096;
const MAX_CAPTURED_PATH_CHARS: usize = 1024 * 1024;
const CHANGE_FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_ATTRIBUTES
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_CREATION
    | FILE_NOTIFY_CHANGE_SECURITY;

/// A fail-closed observation of workspace mutations during one sandbox command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceChangeObservation {
    Unchanged,
    Changed(Vec<WorkspacePathChange>),
    Unknown,
}

/// One complete relative-path notification captured while the sandbox child was alive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspacePathChange {
    pub path: String,
    pub kind: WorkspacePathChangeKind,
}

/// The stable mutation classes reported by `ReadDirectoryChangesW`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspacePathChangeKind {
    Added,
    Removed,
    Modified,
    RenamedOld,
    RenamedNew,
}

/// Registers a recursive Windows directory-change request before the sandbox child starts.
pub struct WorkspaceChangeMonitor {
    stop_event: HANDLE,
    worker: Option<JoinHandle<Result<WorkspaceChangeObservation>>>,
}

impl WorkspaceChangeMonitor {
    /// Starts monitoring an existing workspace without following a final reparse point.
    pub fn start(workspace: &Path) -> Result<Self> {
        let mut wide = workspace.as_os_str().encode_wide().collect::<Vec<_>>();
        wide.push(0);
        let directory = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_OVERLAPPED,
                0,
            )
        };
        if directory == INVALID_HANDLE_VALUE {
            anyhow::bail!(
                "workspace change monitor open failed with error {}",
                unsafe { GetLastError() }
            );
        }
        let change_event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if change_event == 0 {
            let code = unsafe { GetLastError() };
            unsafe { CloseHandle(directory) };
            anyhow::bail!("workspace change monitor event failed with error {code}");
        }
        let stop_event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if stop_event == 0 {
            let code = unsafe { GetLastError() };
            unsafe {
                CloseHandle(change_event);
                CloseHandle(directory);
            }
            anyhow::bail!("workspace change monitor stop event failed with error {code}");
        }
        let directory_value = directory as usize;
        let change_event_value = change_event as usize;
        let stop_event_value = stop_event as usize;
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let directory = directory_value as HANDLE;
            let change_event = change_event_value as HANDLE;
            let stop_event = stop_event_value as HANDLE;
            let mut overlapped = Box::new(unsafe { std::mem::zeroed::<OVERLAPPED>() });
            overlapped.hEvent = change_event;
            let mut buffer = Box::new([0u8; CHANGE_BUFFER_BYTES]);
            if !start_read(directory, &mut buffer, &mut overlapped) {
                let code = unsafe { GetLastError() };
                let _ = started_tx.send(Err(code));
                unsafe {
                    CloseHandle(change_event);
                    CloseHandle(directory);
                }
                anyhow::bail!("workspace change monitor registration failed with error {code}");
            }
            let _ = started_tx.send(Ok(()));
            monitor_changes(directory, change_event, stop_event, buffer, overlapped)
        });
        match started_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(code)) => {
                let _ = worker.join();
                unsafe { CloseHandle(stop_event) };
                anyhow::bail!("workspace change monitor registration failed with error {code}");
            }
            Err(_) => {
                let _ = worker.join();
                unsafe { CloseHandle(stop_event) };
                anyhow::bail!("workspace change monitor worker stopped during registration");
            }
        }
        Ok(Self {
            stop_event,
            worker: Some(worker),
        })
    }

    /// Finishes the observation after the sandbox child and its Job Object have exited.
    pub fn finish(mut self) -> Result<WorkspaceChangeObservation> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<WorkspaceChangeObservation> {
        if self.worker.is_none() {
            return Ok(WorkspaceChangeObservation::Unknown);
        }
        if unsafe { SetEvent(self.stop_event) } == 0 {
            anyhow::bail!(
                "workspace change monitor stop failed with error {}",
                unsafe { GetLastError() }
            );
        }
        let worker = self.worker.take().expect("worker checked above");
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("workspace change monitor worker panicked"))?
    }
}

impl Drop for WorkspaceChangeMonitor {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
        if self.worker.is_none() {
            unsafe { CloseHandle(self.stop_event) };
        } else if let Some(worker) = self.worker.take() {
            // A failed stop signal leaves the worker dependent on this event. Leaking both is
            // safer than closing a handle that an in-flight Win32 wait still owns.
            std::mem::forget(worker);
        }
    }
}

fn start_read(
    directory: HANDLE,
    buffer: &mut [u8; CHANGE_BUFFER_BYTES],
    overlapped: &mut OVERLAPPED,
) -> bool {
    unsafe {
        ReadDirectoryChangesW(
            directory,
            buffer.as_mut_ptr() as *mut c_void,
            CHANGE_BUFFER_BYTES as u32,
            1,
            CHANGE_FILTER,
            std::ptr::null_mut(),
            overlapped,
            None,
        ) != 0
    }
}

fn monitor_changes(
    directory: HANDLE,
    change_event: HANDLE,
    stop_event: HANDLE,
    mut buffer: Box<[u8; CHANGE_BUFFER_BYTES]>,
    mut overlapped: Box<OVERLAPPED>,
) -> Result<WorkspaceChangeObservation> {
    let result = monitor_changes_inner(
        directory,
        change_event,
        stop_event,
        &mut buffer,
        &mut overlapped,
    );
    unsafe {
        CloseHandle(change_event);
        CloseHandle(directory);
    }
    result
}

fn monitor_changes_inner(
    directory: HANDLE,
    change_event: HANDLE,
    stop_event: HANDLE,
    buffer: &mut [u8; CHANGE_BUFFER_BYTES],
    overlapped: &mut OVERLAPPED,
) -> Result<WorkspaceChangeObservation> {
    let handles = [change_event, stop_event];
    let mut changes = BTreeMap::<(String, u8), WorkspacePathChange>::new();
    loop {
        let wait = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
        if wait == WAIT_OBJECT_0 {
            let Some(batch) = completed_batch(directory, buffer, overlapped)? else {
                return Ok(WorkspaceChangeObservation::Unknown);
            };
            if !merge_changes(&mut changes, batch) {
                return Ok(WorkspaceChangeObservation::Unknown);
            }
            if unsafe { ResetEvent(change_event) } == 0 {
                anyhow::bail!(
                    "workspace change monitor reset failed with error {}",
                    unsafe { GetLastError() }
                );
            }
            *overlapped = unsafe { std::mem::zeroed() };
            overlapped.hEvent = change_event;
            if !start_read(directory, buffer, overlapped) {
                anyhow::bail!(
                    "workspace change monitor rearm failed with error {}",
                    unsafe { GetLastError() }
                );
            }
            continue;
        }
        if wait == WAIT_OBJECT_0 + 1 {
            let cancelled = unsafe { CancelIoEx(directory, overlapped) };
            if cancelled == 0 && unsafe { GetLastError() } != ERROR_NOT_FOUND {
                anyhow::bail!(
                    "workspace change monitor cancel failed with error {}",
                    unsafe { GetLastError() }
                );
            }
            let wait = unsafe { WaitForSingleObject(change_event, INFINITE) };
            if wait != WAIT_OBJECT_0 {
                anyhow::bail!("workspace change monitor wait failed with status {wait}");
            }
            match completed_batch(directory, buffer, overlapped) {
                Ok(Some(batch)) => {
                    if !merge_changes(&mut changes, batch) {
                        return Ok(WorkspaceChangeObservation::Unknown);
                    }
                }
                Ok(None) => return Ok(WorkspaceChangeObservation::Unknown),
                Err(error)
                    if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                        error.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32)
                    }) => {}
                Err(error) => return Err(error),
            }
            return Ok(if changes.is_empty() {
                WorkspaceChangeObservation::Unchanged
            } else {
                WorkspaceChangeObservation::Changed(changes.into_values().collect())
            });
        }
        anyhow::bail!("workspace change monitor wait failed with status {wait}");
    }
}

fn completed_batch(
    directory: HANDLE,
    buffer: &[u8; CHANGE_BUFFER_BYTES],
    overlapped: &mut OVERLAPPED,
) -> Result<Option<Vec<WorkspacePathChange>>> {
    let mut transferred = 0u32;
    let completed = unsafe { GetOverlappedResult(directory, overlapped, &mut transferred, 0) };
    if completed == 0 {
        let code = unsafe { GetLastError() };
        if code == ERROR_NOTIFY_ENUM_DIR {
            return Ok(None);
        }
        if code == ERROR_IO_INCOMPLETE {
            anyhow::bail!("workspace change monitor completed without a result");
        }
        return Err(std::io::Error::from_raw_os_error(code as i32).into());
    }
    if transferred == 0 {
        return Ok(None);
    }
    let bytes = buffer
        .get(..transferred as usize)
        .ok_or_else(|| anyhow::anyhow!("workspace change monitor returned an oversized buffer"))?;
    parse_change_buffer(bytes).map(Some)
}

fn merge_changes(
    accumulated: &mut BTreeMap<(String, u8), WorkspacePathChange>,
    changes: Vec<WorkspacePathChange>,
) -> bool {
    for change in changes {
        if change.kind == WorkspacePathChangeKind::Removed {
            let path = change.path.as_str();
            let added = accumulated.contains_key(&(
                change.path.clone(),
                change_kind_order(WorkspacePathChangeKind::Added),
            ));
            let preexisting_transition = [
                WorkspacePathChangeKind::Removed,
                WorkspacePathChangeKind::RenamedOld,
                WorkspacePathChangeKind::RenamedNew,
            ]
            .into_iter()
            .any(|kind| accumulated.contains_key(&(change.path.clone(), change_kind_order(kind))));
            if added && !preexisting_transition {
                let descendant_prefix = format!("{path}/");
                accumulated.retain(|(observed, _), _| {
                    observed != path && !observed.starts_with(&descendant_prefix)
                });
                continue;
            }
        }
        if added_ancestor(accumulated, &change.path) {
            continue;
        }
        accumulated.insert(
            (change.path.clone(), change_kind_order(change.kind)),
            change,
        );
    }
    accumulated.len() <= MAX_CAPTURED_PATHS
        && accumulated
            .keys()
            .map(|(path, _)| path.len())
            .sum::<usize>()
            <= MAX_CAPTURED_PATH_CHARS
}

pub(crate) fn merge_workspace_change_observations(
    first: WorkspaceChangeObservation,
    second: WorkspaceChangeObservation,
) -> WorkspaceChangeObservation {
    match (first, second) {
        (WorkspaceChangeObservation::Unknown, _) | (_, WorkspaceChangeObservation::Unknown) => {
            WorkspaceChangeObservation::Unknown
        }
        (WorkspaceChangeObservation::Unchanged, observation)
        | (observation, WorkspaceChangeObservation::Unchanged) => observation,
        (
            WorkspaceChangeObservation::Changed(first),
            WorkspaceChangeObservation::Changed(second),
        ) => {
            let mut changes = BTreeMap::new();
            if !merge_changes(&mut changes, first) || !merge_changes(&mut changes, second) {
                WorkspaceChangeObservation::Unknown
            } else {
                WorkspaceChangeObservation::Changed(changes.into_values().collect())
            }
        }
    }
}

fn added_ancestor(accumulated: &BTreeMap<(String, u8), WorkspacePathChange>, path: &str) -> bool {
    let mut end = path.len();
    while let Some(separator) = path[..end].rfind('/') {
        let ancestor = path[..separator].to_string();
        if accumulated.contains_key(&(ancestor, change_kind_order(WorkspacePathChangeKind::Added)))
        {
            return true;
        }
        end = separator;
    }
    false
}

fn change_kind_order(kind: WorkspacePathChangeKind) -> u8 {
    match kind {
        WorkspacePathChangeKind::Added => 0,
        WorkspacePathChangeKind::Removed => 1,
        WorkspacePathChangeKind::Modified => 2,
        WorkspacePathChangeKind::RenamedOld => 3,
        WorkspacePathChangeKind::RenamedNew => 4,
    }
}

fn parse_change_buffer(buffer: &[u8]) -> Result<Vec<WorkspacePathChange>> {
    let mut changes = Vec::new();
    let mut offset = 0usize;
    loop {
        let header = buffer
            .get(offset..offset.saturating_add(12))
            .ok_or_else(|| anyhow::anyhow!("workspace change monitor returned a short record"))?;
        let next = u32::from_ne_bytes(header[0..4].try_into().expect("fixed slice")) as usize;
        let action = u32::from_ne_bytes(header[4..8].try_into().expect("fixed slice"));
        let name_bytes =
            u32::from_ne_bytes(header[8..12].try_into().expect("fixed slice")) as usize;
        if name_bytes == 0 || !name_bytes.is_multiple_of(2) {
            anyhow::bail!("workspace change monitor returned an invalid file name");
        }
        let name_end = offset
            .checked_add(12)
            .and_then(|value| value.checked_add(name_bytes))
            .ok_or_else(|| anyhow::anyhow!("workspace change monitor record overflowed"))?;
        let name = buffer.get(offset + 12..name_end).ok_or_else(|| {
            anyhow::anyhow!("workspace change monitor returned a short file name")
        })?;
        let wide = name
            .chunks_exact(2)
            .map(|chunk| u16::from_ne_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        let path = String::from_utf16(&wide)
            .map_err(|_| anyhow::anyhow!("workspace change monitor returned a non-Unicode path"))?
            .replace('\\', "/");
        if path.is_empty()
            || path.starts_with('/')
            || path
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | "..") || part.contains(':'))
        {
            anyhow::bail!("workspace change monitor returned an unsafe relative path");
        }
        let kind = match action {
            FILE_ACTION_ADDED => WorkspacePathChangeKind::Added,
            FILE_ACTION_REMOVED => WorkspacePathChangeKind::Removed,
            FILE_ACTION_MODIFIED => WorkspacePathChangeKind::Modified,
            FILE_ACTION_RENAMED_OLD_NAME => WorkspacePathChangeKind::RenamedOld,
            FILE_ACTION_RENAMED_NEW_NAME => WorkspacePathChangeKind::RenamedNew,
            _ => anyhow::bail!("workspace change monitor returned an unknown action"),
        };
        changes.push(WorkspacePathChange { path, kind });
        if next == 0 {
            break;
        }
        if next < 12 || !next.is_multiple_of(4) {
            anyhow::bail!("workspace change monitor returned an invalid record offset");
        }
        offset = offset
            .checked_add(next)
            .ok_or_else(|| anyhow::anyhow!("workspace change monitor offset overflowed"))?;
        if offset >= buffer.len() {
            anyhow::bail!("workspace change monitor record offset exceeded the buffer");
        }
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::{
        WorkspaceChangeMonitor, WorkspaceChangeObservation, WorkspacePathChange,
        WorkspacePathChangeKind, merge_changes, merge_workspace_change_observations,
        parse_change_buffer,
    };
    use std::collections::BTreeMap;

    #[test]
    fn monitor_distinguishes_changed_and_unchanged_workspaces() {
        let workspace = tempfile::tempdir().expect("workspace");
        let unchanged = WorkspaceChangeMonitor::start(workspace.path()).expect("start monitor");
        assert_eq!(
            unchanged.finish().expect("finish unchanged monitor"),
            WorkspaceChangeObservation::Unchanged
        );

        let changed = WorkspaceChangeMonitor::start(workspace.path()).expect("start monitor");
        std::fs::write(workspace.path().join("changed.txt"), b"changed").expect("write change");
        let WorkspaceChangeObservation::Changed(changes) =
            changed.finish().expect("finish changed monitor")
        else {
            panic!("write must be observed as a changed path");
        };
        assert!(changes.iter().any(|change| {
            change.path == "changed.txt"
                && matches!(
                    change.kind,
                    WorkspacePathChangeKind::Added | WorkspacePathChangeKind::Modified
                )
        }));
    }

    #[test]
    fn monitor_coalesces_a_created_then_removed_temporary_file() {
        let workspace = tempfile::tempdir().expect("workspace");
        let monitor = WorkspaceChangeMonitor::start(workspace.path()).expect("start monitor");
        let temporary = workspace.path().join("temporary.txt");
        std::fs::write(&temporary, b"temporary").expect("write temporary file");
        std::fs::remove_file(temporary).expect("remove temporary file");

        assert_eq!(
            monitor.finish().expect("finish temporary monitor"),
            WorkspaceChangeObservation::Unchanged
        );
    }

    #[test]
    fn monitor_coalesces_a_large_added_subtree_without_losing_the_observation() {
        const FILES: usize = 5_000;
        let workspace = tempfile::tempdir().expect("workspace");
        let monitor = WorkspaceChangeMonitor::start(workspace.path()).expect("start monitor");
        let added = workspace.path().join(".environment");
        std::fs::create_dir(&added).expect("create added directory");
        for index in 0..FILES {
            std::fs::write(added.join(format!("file-{index:04}.txt")), b"x")
                .expect("write added file");
        }

        let WorkspaceChangeObservation::Changed(changes) =
            monitor.finish().expect("finish monitor")
        else {
            panic!("a completely observed added subtree must not become unknown");
        };
        assert!(changes.iter().any(|change| {
            change.path == ".environment" && change.kind == WorkspacePathChangeKind::Added
        }));
        assert!(changes.len() < FILES);
    }

    #[test]
    fn parser_rejects_parent_traversal() {
        let mut record = vec![0u8; 12];
        record[4..8].copy_from_slice(&3u32.to_ne_bytes());
        let name = "..\\outside"
            .encode_utf16()
            .flat_map(u16::to_ne_bytes)
            .collect::<Vec<_>>();
        record[8..12].copy_from_slice(&(name.len() as u32).to_ne_bytes());
        record.extend(name);

        assert!(parse_change_buffer(&record).is_err());
    }

    #[test]
    fn added_directory_coalesces_descendant_notifications_before_the_path_bound() {
        let mut accumulated = BTreeMap::new();
        let mut changes = vec![WorkspacePathChange {
            path: ".environment".to_string(),
            kind: WorkspacePathChangeKind::Added,
        }];
        changes.extend((0..5_000).map(|index| WorkspacePathChange {
            path: format!(".environment/packages/package-{index:04}.py"),
            kind: WorkspacePathChangeKind::Added,
        }));

        assert!(merge_changes(&mut accumulated, changes));
        assert_eq!(
            accumulated.into_values().collect::<Vec<_>>(),
            vec![WorkspacePathChange {
                path: ".environment".to_string(),
                kind: WorkspacePathChangeKind::Added,
            }]
        );
    }

    #[test]
    fn added_then_removed_path_returns_to_no_observed_change() {
        let mut accumulated = BTreeMap::new();
        assert!(merge_changes(
            &mut accumulated,
            vec![WorkspacePathChange {
                path: "temporary.txt".to_string(),
                kind: WorkspacePathChangeKind::Added,
            }],
        ));
        assert!(merge_changes(
            &mut accumulated,
            vec![WorkspacePathChange {
                path: "temporary.txt".to_string(),
                kind: WorkspacePathChangeKind::Removed,
            }],
        ));

        assert!(accumulated.is_empty());
    }

    #[test]
    fn removed_then_added_path_remains_observed_as_a_replacement() {
        let mut accumulated = BTreeMap::new();
        assert!(merge_changes(
            &mut accumulated,
            vec![WorkspacePathChange {
                path: "existing.txt".to_string(),
                kind: WorkspacePathChangeKind::Removed,
            }],
        ));
        assert!(merge_changes(
            &mut accumulated,
            vec![WorkspacePathChange {
                path: "existing.txt".to_string(),
                kind: WorkspacePathChangeKind::Added,
            }],
        ));

        assert_eq!(accumulated.len(), 2);
    }

    #[test]
    fn consecutive_guard_observations_preserve_the_complete_changed_boundary() {
        let merged = merge_workspace_change_observations(
            WorkspaceChangeObservation::Changed(vec![WorkspacePathChange {
                path: ".environment".to_string(),
                kind: WorkspacePathChangeKind::Added,
            }]),
            WorkspaceChangeObservation::Changed(vec![WorkspacePathChange {
                path: ".environment/package.py".to_string(),
                kind: WorkspacePathChangeKind::Modified,
            }]),
        );

        assert_eq!(
            merged,
            WorkspaceChangeObservation::Changed(vec![WorkspacePathChange {
                path: ".environment".to_string(),
                kind: WorkspacePathChangeKind::Added,
            }])
        );
    }
}
