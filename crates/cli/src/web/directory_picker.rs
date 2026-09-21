//! Windows 原生文件夹选择窗口：同步模态对话框、COM apartment 和并发串行化；与文件搜索
//! 不同，这里的输入来自用户交互，执行方式是阻塞 worker，生命周期由桌面窗口决定，所以
//! 单独成模块，搜索模块也就不必引入 COM。

use singularity_protocol::{DirectoryPickResult, RpcError};

/// 返回用户选中的宿主路径；取消选择时不会新增 workspace。
pub async fn pick_directory() -> Result<DirectoryPickResult, RpcError> {
    static PICKER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let guard = PICKER.try_lock().map_err(|_| {
        RpcError::new(
            singularity_protocol::RpcErrorCode::InvalidRequest,
            "文件夹选择窗口已经打开。",
            "请先选择或取消已经打开的窗口。",
        )
    })?;
    // 必须在这里、还没离开当前线程时取到发起交互的窗口：
    // 原生模态对话框拿它当 owner，才能显示在浏览器窗口之上。
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
    // SAFETY: COM 和它的接口都不离开这个阻塞 worker；owner HWND 只是传给
    // 系统模态 API 的裸句柄，不会为它创建 Rust 引用或所有权。
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
