//! 测试注入点：内核拒绝调用才能触发的失败路径。
//!
//! 绑定、恢复、终止与观察子进程都取决于系统调用的结果，测试无法稳定构造它们
//! 的失败。这里提供一次性开关，让这些分支与常规路径共用同一套回收与报告逻辑
//! 并因此可被断言。开关是线程局部的：每个测试在自己的线程上执行本模块，互不
//! 干扰，也不需要全局锁；本模块只在测试构建中存在。

use std::cell::Cell;
use std::io;

thread_local! {
    static ASSIGN_FAILURE: Cell<bool> = const { Cell::new(false) };
    static RESUME_FAILURE: Cell<bool> = const { Cell::new(false) };
    static TERMINATE_FAILURE: Cell<bool> = const { Cell::new(false) };
    static WAIT_FAILURE: Cell<bool> = const { Cell::new(false) };
    static WAIT_EXPIRY: Cell<bool> = const { Cell::new(false) };
}

/// 让本线程下一次作业绑定直接失败：子进程因此真的不属于本次作业。
pub(super) fn fail_next_assign() {
    ASSIGN_FAILURE.with(|flag| flag.set(true));
}

pub(super) fn take_assign_failure() -> bool {
    ASSIGN_FAILURE.with(|flag| flag.replace(false))
}

/// 让本线程下一次恢复挂起的主线程失败：子进程已归属本次作业但不执行用户命令。
pub(super) fn fail_next_resume() {
    RESUME_FAILURE.with(|flag| flag.set(true));
}

pub(super) fn take_resume_failure() -> bool {
    RESUME_FAILURE.with(|flag| flag.replace(false))
}

/// 让本线程下一次作业终止返回失败：进程树因此保持存活。
pub(super) fn fail_next_terminate() {
    TERMINATE_FAILURE.with(|flag| flag.set(true));
}

pub(super) fn take_terminate_failure() -> bool {
    TERMINATE_FAILURE.with(|flag| flag.replace(false))
}

/// 让本线程下一次观察子进程状态返回失败。
pub(super) fn fail_next_wait() {
    WAIT_FAILURE.with(|flag| flag.set(true));
}

pub(super) fn take_wait_failure() -> Option<io::Error> {
    if WAIT_FAILURE.with(|flag| flag.replace(false)) {
        return Some(io::Error::other("injected try_wait failure"));
    }
    None
}

/// 让本线程下一次有界等待立即按窗口到点返回。
pub(super) fn expire_next_wait() {
    WAIT_EXPIRY.with(|flag| flag.set(true));
}

pub(super) fn take_wait_expiry() -> bool {
    WAIT_EXPIRY.with(|flag| flag.replace(false))
}
