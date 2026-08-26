//! bash 进程生命周期接缝。

use std::time::Duration;

/// 进程终止后的有界回收窗口。
pub(super) const WAIT_GRACE: Duration = Duration::from_secs(5);
