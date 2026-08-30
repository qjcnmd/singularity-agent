//! `AppServer` 的传输层：stdio JSON-Lines 控制面。
//!
//! 单一分发 owner（对齐 codex 的 MessageProcessor 形状）：stdin reader 只把
//! 解析后的消息排入唯一有界队列，dispatch 任务按到达顺序处理；所有输出通过
//! 唯一 writer 顺序写出。

pub(crate) mod error;
pub(crate) mod framing;
pub(crate) mod output;
pub(crate) mod supervisor;

pub(crate) use error::error_value;
pub(crate) use framing::read_bounded_line;
pub(crate) use output::{
    send_app_server_outputs, send_output, send_output_async, write_output_queue,
};
pub(crate) use supervisor::run;

use crate::AppServerCancellationHandle;
use std::time::Duration;

pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
pub(crate) const OUTPUT_QUEUE_CAPACITY: usize = 256;
/// 单一分发队列容量：满时向请求方回复内部错误，不阻塞 stdin。
pub(crate) const REQUEST_QUEUE_CAPACITY: usize = 64;
/// 单条 JSON-Lines frame（含 JSON-RPC 请求/响应）的字节上限。
pub(crate) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub(crate) trait ExecutionStop: Send + Sync {
    fn request_execution_stop(&self);
}

impl ExecutionStop for AppServerCancellationHandle {
    fn request_execution_stop(&self) {
        let _ = AppServerCancellationHandle::request_execution_stop(self);
    }
}
