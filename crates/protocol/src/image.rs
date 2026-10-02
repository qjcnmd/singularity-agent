use serde::{Deserialize, Serialize};

/// 图片快照的公开描述；像素内容按需读取，不随历史和运行状态重复传输。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageAttachment {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
}

/// 工作台提交的图片；格式和尺寸由 Rust 根据实际内容确定。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageUpload {
    pub name: String,
    pub data_url: String,
}
