//! 有界本机目录选择与已登记 Workspace 内文件候选。

use std::path::Path;

use serde::Serialize;
use singularity_protocol::{DirectoryEntry, DirectoryEntryKind, RpcError};

const MAX_SCANNED_DIRECTORIES: usize = 2_000;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileCandidate {
    pub path: String,
    pub kind: DirectoryEntryKind,
}

pub fn list_directory(path: Option<&str>) -> Result<Vec<DirectoryEntry>, String> {
    let Some(path) = path else {
        return Ok(system_roots());
    };
    let directory = singularity_core::canonicalize_workspace(path)?;
    let mut entries = Vec::new();
    if let Some(parent) = directory.as_path().parent() {
        entries.push(DirectoryEntry {
            name: "..".to_string(),
            path: display_existing_path(parent)?,
            kind: DirectoryEntryKind::Parent,
        });
    }
    for entry in std::fs::read_dir(directory.as_path())
        .map_err(|error| format!("directory could not be read: {error}"))?
    {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() && !file_type.is_symlink() {
            let path = entry.path();
            let Ok(path) = display_existing_path(&path) else {
                continue;
            };
            entries.push(DirectoryEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path,
                kind: DirectoryEntryKind::Directory,
            });
        }
    }
    entries.sort_by(|left, right| {
        (left.kind != DirectoryEntryKind::Parent)
            .cmp(&(right.kind != DirectoryEntryKind::Parent))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(entries)
}

pub fn search_files(
    directory: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<FileCandidate>, String> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let limit = limit.clamp(1, 100);
    let root = singularity_core::canonicalize_workspace(directory)?;
    let mut pending = vec![root.as_path().to_path_buf()];
    let mut scanned = 0;
    let mut candidates = Vec::new();
    while let Some(directory) = pending.pop() {
        if scanned >= MAX_SCANNED_DIRECTORIES || candidates.len() >= limit {
            break;
        }
        scanned += 1;
        let mut entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("workspace directory could not be read: {error}"))?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name().to_string_lossy().to_lowercase());
        for entry in entries {
            if candidates.len() >= limit {
                break;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                if !is_ignored_directory(&entry.file_name().to_string_lossy()) {
                    pending.push(path);
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Ok(relative) = path.strip_prefix(root.as_path()) else {
                continue;
            };
            let relative = singularity_core::display_path(relative);
            if relative.to_lowercase().contains(&query) {
                candidates.push(FileCandidate {
                    path: relative,
                    kind: DirectoryEntryKind::File,
                });
            }
        }
    }
    candidates.sort_by(|left, right| {
        left.path
            .to_lowercase()
            .cmp(&right.path.to_lowercase())
            .then_with(|| left.path.cmp(&right.path))
    });
    candidates.truncate(limit);
    Ok(candidates)
}

fn is_ignored_directory(name: &str) -> bool {
    matches!(name, ".git" | "node_modules" | "target")
}

fn display_existing_path(path: &Path) -> Result<String, String> {
    singularity_core::canonicalize_workspace(path).map(|path| path.display().to_string())
}

fn system_roots() -> Vec<DirectoryEntry> {
    #[cfg(windows)]
    {
        (b'A'..=b'Z')
            .map(|letter| format!("{}:/", letter as char))
            .filter(|path| Path::new(path).is_dir())
            .map(|path| DirectoryEntry {
                name: path.clone(),
                path,
                kind: DirectoryEntryKind::Root,
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![DirectoryEntry {
            name: "/".to_string(),
            path: "/".to_string(),
            kind: DirectoryEntryKind::Root,
        }]
    }
}

/// The desktop folder chooser returns a host path; cancellation does not add a workspace.
pub async fn pick_directory() -> Result<serde_json::Value, RpcError> {
    #[cfg(windows)]
    {
        static PICKER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _guard = PICKER.try_lock().map_err(|_| {
            RpcError::new(
                singularity_protocol::RpcErrorCode::InvalidRequest,
                "文件夹选择窗口已经打开。",
                "请先选择或取消已经打开的窗口。",
            )
        })?;
        // Capture the window that initiated the interaction before leaving this thread.
        // The native modal dialog uses it as owner, so it opens above the browser.
        let owner =
            unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() }.0 as isize;
        let selected = tokio::task::spawn_blocking(move || pick_windows_folder(owner))
            .await
            .map_err(|error| picker_error(error.to_string()))?
            .map_err(|error| picker_error(error.to_string()))?;
        Ok(serde_json::json!({ "native": true, "path": selected }))
    }
    #[cfg(not(windows))]
    {
        Ok(serde_json::json!({ "native": false, "path": null }))
    }
}

#[cfg(windows)]
fn picker_error(message: String) -> RpcError {
    RpcError::new(
        singularity_protocol::RpcErrorCode::Internal,
        format!("无法打开文件夹选择窗口：{message}"),
        "请重试添加工作区。",
    )
}

/// Opens a Windows common dialog on its own COM apartment and releases COM before returning.
#[cfg(windows)]
fn pick_windows_folder(owner: isize) -> windows::core::Result<Option<String>> {
    use windows::{
        Win32::{
            Foundation::{ERROR_CANCELLED, HWND},
            System::Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoTaskMemFree, CoUninitialize,
            },
            UI::Shell::{
                FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog,
                SIGDN_FILESYSPATH,
            },
        },
        core::{HRESULT, w},
    };
    // SAFETY: COM and its interfaces stay on this blocking worker. The owner HWND is
    // passed only to the OS modal API; no Rust reference or ownership is created for it.
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let result = (|| {
            let dialog: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
            dialog.SetOptions(dialog.GetOptions()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)?;
            dialog.SetTitle(w!("选择工作区文件夹"))?;
            if let Err(error) = dialog.Show(Some(HWND(owner as *mut _))) {
                if error.code() == HRESULT::from_win32(ERROR_CANCELLED.0) {
                    return Ok(None);
                }
                return Err(error);
            }
            let path = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
            let selected = path.to_string();
            CoTaskMemFree(Some(path.0.cast()));
            Ok(Some(selected?))
        })();
        CoUninitialize();
        result
    }
}
