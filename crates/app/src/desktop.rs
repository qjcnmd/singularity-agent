//! 桌面端服务入口：连接 `AppServer` 与 Electron 进程的 stdio 传输。

mod app_server;
mod file_view;
mod rpc;
mod transport;
mod workspace_files;
pub(crate) mod workspace_store;

pub(crate) use app_server::AppServer;
pub use transport::run;
