//! 可在线程和 provider 边界传播的取消令牌。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// 可在线程与 provider 之间共享的取消状态。
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    /// 创建未取消的 token。
    pub fn new() -> Self {
        Self::default()
    }

    /// 标记取消并向所有持有者传播。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// 判断取消是否已请求。
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}
