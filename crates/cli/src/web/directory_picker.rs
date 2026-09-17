//! Windows 原生文件夹选择窗口：同步模态对话框、COM apartment 与并发串行化。
//!
//! 与工作区文件搜索不同，这里的输入是用户交互，执行方式是阻塞 worker，
//! 生命周期由桌面窗口决定，因此单独成模块，搜索模块不引入 COM。

use singularity_protocol::{DirectoryPickResult, RpcError};

/// 桌面文件夹选择器返回宿主路径；取消不会新增 workspace。
pub async fn pick_directory() -> Result<DirectoryPickResult, RpcError> {
    static PICKER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let guard = PICKER.try_lock().map_err(|_| {
        RpcError::new(
            singularity_protocol::RpcErrorCode::InvalidRequest,
            "文件夹选择窗口已经打开。",
            "请先选择或取消已经打开的窗口。",
        )
    })?;
    // 在离开此线程前捕获发起交互的窗口。
    // 原生模态对话框以它为 owner，因此会显示在浏览器之上。
    let owner =
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() }.0 as isize;
    let selected = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        pick_windows_folder(owner)
    })
    .await
    .map_err(|error| picker_error(error.to_string()))?
    .map_err(|error| picker_error(error.to_string()))?;
    Ok(DirectoryPickResult { path: selected })
}

fn picker_error(message: String) -> RpcError {
    RpcError::new(
        singularity_protocol::RpcErrorCode::Internal,
        format!("无法打开文件夹选择窗口：{message}"),
        "请重试添加工作区。",
    )
}

/// 在独立 COM apartment 中打开 Windows 通用对话框，并在返回前释放 COM。
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
    // SAFETY: COM 及其接口都留在该阻塞 worker 上。owner HWND 仅传给
    // OS 模态 API；不会为它创建 Rust 引用或所有权。
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
