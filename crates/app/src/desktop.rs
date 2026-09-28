//! Electron desktop transport around the shared AppServer.

mod app_server;
mod rpc;
mod transport;
mod workspace_files;
pub(crate) mod workspace_store;

pub use transport::run;
