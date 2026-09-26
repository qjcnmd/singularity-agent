//! Electron desktop transport around the shared AppServer.

mod app_server;
mod rpc;
mod transport;
mod workspace_files;

pub use transport::run;
