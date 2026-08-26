//! AppServer 生命周期适配层：协议投影。
//!
//! 唯一执行实现位于 `singularity_runtime`（TurnRunner/Conversation/TurnEvent）；
//! 本目录只保留投影适配器与终态分类。

use super::*;
mod projection;

pub(crate) use projection::{TurnProjection, classify_run_result};
