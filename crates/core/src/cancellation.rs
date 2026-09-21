//! 可在线程之间、以及与 provider 交互时传播的取消令牌。
//!
//! 取消状态由一个原子布尔值和一个通知器组成：同步代码（工具执行、bash 输出
//! 泵）直接调用 is_cancelled 查询；异步代码（等待 provider HTTP 响应、重试
//! 退避）用 cancelled_notified 挂起等待取消事件，不必反复轮询。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// 可以在多个线程和 provider 侧共同持有的取消状态。
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// 标记为已取消，并把这件事通知给所有等待者。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// 等待取消事件：已经请求过取消就立刻返回，否则挂起，直到有人调用 cancel。
    ///
    /// 先登记通知、再复查状态，避免在「检查状态 → 登记通知」的空隙里漏掉
    /// 一次取消。
    pub async fn cancelled_notified(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}
