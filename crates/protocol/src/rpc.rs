//! 工作台 RPC 边界的方法、参数与结果类型之间的关联；协商用的版本号只有
//! `PROTOCOL_VERSION` 这一处来源。

use crate::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "typescript", ts(type = "Record<string, never>"))]
pub struct EmptyParams {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileSearchParams {
    pub workspace_id: String,
    #[cfg_attr(feature = "typescript", ts(optional = nullable))]
    pub session_id: Option<String>,
    pub query: String,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillsListParams {
    pub workspace_id: String,
    #[cfg_attr(feature = "typescript", ts(optional = nullable))]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct WorkspaceAddParams {
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceParams {
    pub workspace_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceRenameParams {
    pub workspace_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderSaveParams {
    pub provider: ProviderConfigurationInput,
    /// 只在写入时使用的新密钥；省略或传空字符串表示保留已有密钥。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApiKeyParams {
    pub provider_id: String,
    pub api_key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderParams {
    pub provider_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiscoverModelsParams {
    pub provider_id: String,
    pub base_url: String,
    #[cfg_attr(feature = "typescript", ts(optional = nullable))]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionSettingsInput {
    #[cfg_attr(feature = "typescript", ts(optional = nullable))]
    pub selector: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionCreateParams {
    pub workspace_id: String,
    #[cfg_attr(feature = "typescript", ts(optional = nullable))]
    pub settings: Option<SessionSettingsInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionReadParams {
    pub workspace_id: String,
    pub session_id: String,
    #[cfg_attr(feature = "typescript", ts(optional = nullable))]
    pub before_turn: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionParams {
    pub workspace_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionTextParams {
    pub workspace_id: String,
    pub session_id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionRenameParams {
    pub workspace_id: String,
    pub session_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueControlParams {
    pub workspace_id: String,
    pub session_id: String,
    pub control_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueReplaceParams {
    pub workspace_id: String,
    pub session_id: String,
    pub control_id: String,
    pub text: String,
}

/// 「立即发送」的目标：指定一条待处理输入，或者省略 `controlId` 表示当前队列里的全部待处理
/// 输入。两种目标都由服务端在队列的临界区内读取并交接，客户端不用枚举自己快照里的条目。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueSendParams {
    pub workspace_id: String,
    pub session_id: String,
    #[cfg_attr(feature = "typescript", ts(optional = nullable))]
    pub control_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateSettingsParams {
    pub workspace_id: String,
    pub session_id: String,
    pub selector: String,
}

/// 方法参数与结果类型的关联；RPC adapter 和客户端类型生成共用它。
pub trait RpcCall {
    type Params: serde::de::DeserializeOwned;
    type Output: Serialize;
}
macro_rules! rpc_methods {
    ($($variant:ident => $wire:literal ($params:ty) -> $result:ty),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        pub enum RpcMethod {
            $(#[serde(rename = $wire)] $variant,)*
        }
        /// HTTP adapter 使用的带类型方法标记。
        pub mod calls {
            use super::*;
            $(
                pub struct $variant;
                impl RpcCall for $variant {
                    type Params = $params;
                    type Output = $result;
                }
            )*
        }
        #[cfg(feature = "typescript")]
        pub(crate) fn export_rpc_types(out: &mut crate::typescript::Bindings) {
            $(out.add::<<calls::$variant as RpcCall>::Params>();
              out.add::<<calls::$variant as RpcCall>::Output>();)*
            out.push(format!("export interface RpcContract {{\n{}\n}}",
                [$(format!("  {}: {{ params: {}; result: {} }}",
                    stringify!($wire),
                    <<calls::$variant as RpcCall>::Params as ts_rs::TS>::name(out.config()),
                    <<calls::$variant as RpcCall>::Output as ts_rs::TS>::name(out.config())
                )),*].join("\n")));
        }
    };
}
rpc_methods! {
    AppBootstrap => "app.bootstrap" (EmptyParams) -> crate::AppBootstrap,
    DirectoryPick => "directory.pick" (EmptyParams) -> DirectoryPickResult,
    FileSearch => "file.search" (FileSearchParams) -> Vec<FileCandidate>,
    SkillsList => "skills.list" (SkillsListParams) -> SkillCatalog,
    WorkspaceAdd => "workspace.add" (WorkspaceAddParams) -> Workspace,
    WorkspaceRemove => "workspace.remove" (WorkspaceParams) -> (),
    WorkspaceRename => "workspace.rename" (WorkspaceRenameParams) -> (),
    ModelSaveProvider => "model.saveProvider" (ProviderSaveParams) -> (),
    ModelSetApiKey => "model.setApiKey" (ApiKeyParams) -> (),
    ModelDiscover => "model.discover" (DiscoverModelsParams) -> Vec<DiscoveredModel>,
    ModelRemoveProvider => "model.removeProvider" (ProviderParams) -> (),
    SessionCreate => "session.create" (SessionCreateParams) -> SessionReadResult,
    SessionRead => "session.read" (SessionReadParams) -> SessionReadResult,
    SessionRename => "session.rename" (SessionRenameParams) -> (),
    SessionArchive => "session.archive" (SessionParams) -> (),
    SessionSubmit => "session.submit" (SessionTextParams) -> (),
    SessionSteer => "session.steer" (SessionTextParams) -> (),
    SessionFollowUp => "session.followUp" (SessionTextParams) -> (),
    SessionQueueWithdraw => "session.queueWithdraw" (QueueControlParams) -> (),
    SessionQueueReplace => "session.queueReplace" (QueueReplaceParams) -> (),
    SessionQueueSendNow => "session.queueSendNow" (QueueSendParams) -> (),
    SessionAbort => "session.abort" (SessionParams) -> (),
    SessionCompact => "session.compact" (SessionParams) -> (),
    SessionUpdateSettings => "session.updateSettings" (UpdateSettingsParams) -> (),
}
