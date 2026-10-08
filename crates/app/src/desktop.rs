//! 桌面端传输：围绕共享的 AppServer 对接 Electron。

mod app_server;
mod rpc;
mod transport;
mod workspace_files;
pub(crate) mod workspace_store;

pub use transport::run;
