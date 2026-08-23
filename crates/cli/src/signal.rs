//! 平台信号处理：一次性 Ctrl+C 计数器。
//!
//! 处理器只把按键记录进进程级计数，绝不阻塞或分配；轮询方（无交互主循环）
//! 是唯一读取者。第一次 Ctrl+C 触发优雅中断，第二次为显式强制退出。

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

static COUNT: AtomicU8 = AtomicU8::new(0);
static HANDLER: OnceLock<Result<(), &'static str>> = OnceLock::new();

/// 在进程内注册一次处理器；失败时 turn 循环退化为只等正常终态路径。
pub fn ensure_installed() -> &'static Result<(), &'static str> {
    HANDLER.get_or_init(install)
}

/// 当前累计按键次数。
pub fn count() -> u8 {
    COUNT.load(Ordering::SeqCst)
}

/// 清零计数；每个 turn 开始前调用，使本轮只观察本轮期间的按键。
pub fn reset() {
    COUNT.store(0, Ordering::SeqCst);
}

fn record_press() {
    let _ = COUNT.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
        Some(value.saturating_add(1))
    });
}

#[cfg(windows)]
fn install() -> Result<(), &'static str> {
    // 控制台处理器同时观察 Ctrl+C (0) 与 Ctrl+Break (1)；两者都视为一次
    // 取消请求。返回 TRUE 标记事件已处理，避免默认进程退出与
    // 「先优雅后强制」的循环竞争。
    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    unsafe extern "system" fn on_control_event(ctrl_type: u32) -> i32 {
        match ctrl_type {
            CTRL_C_EVENT | CTRL_BREAK_EVENT => {
                record_press();
                1
            }
            _ => 0,
        }
    }

    unsafe extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }
    if unsafe { SetConsoleCtrlHandler(Some(on_control_event), 1) == 0 } {
        Err("SetConsoleCtrlHandler failed")
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn install() -> Result<(), &'static str> {
    unsafe extern "C" fn on_sigint(_signum: i32) {
        record_press();
    }

    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = on_sigint as *const () as libc::sighandler_t;
    action.sa_flags = libc::SA_RESTART;
    if unsafe { libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) == 0 } {
        Ok(())
    } else {
        Err("sigaction(SIGINT) failed")
    }
}
