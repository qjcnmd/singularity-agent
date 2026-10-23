#![forbid(unsafe_code)]

//! 前后端与各 crate 共用的 RPC 请求、事件、历史和配置类型。

mod app;
mod background;
mod event;
mod image;
mod inspection;
mod mcp;
mod params;
mod question;
mod rpc;
#[cfg(feature = "typescript")]
pub mod typescript;

pub use app::*;
pub use background::*;
pub use event::*;
pub use image::*;
pub use inspection::*;
pub use mcp::*;
pub use params::*;
pub use question::*;
pub use rpc::*;
