//! 可在线程和 provider 边界传播的取消令牌。
//!
//! 取消状态为原子布尔 + 通知器：同步侧（工具执行、bash 泵）继续用
//! `is_cancelled` 检查；异步侧（provider HTTP 等待、重试退避）用
//! `cancelled_notified` 挂起等待取消事件，无需轮询。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// 可在线程与 provider 之间共享的取消状态。
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancellationToken {
    /// 创建未取消的 token。
    pub fn new() -> Self {
        Self::default()
    }

    /// 标记取消并向所有持有者传播。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// 判断取消是否已请求。
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// 等待取消事件：取消已请求时立即完成，否则挂起直到下一次 `cancel`。
    ///
    /// 先注册通知再复查状态，避免「检查 → 注册」窗口内丢失取消信号。
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
