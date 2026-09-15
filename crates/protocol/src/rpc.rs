//! 工作台 RPC 边界的方法、参数与结果关联；协商版本号只有
//! `WORKBENCH_PROTOCOL_VERSION` 一处来源。

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
    /// 只写的新密钥；省略或空字符串表示保留已有密钥。
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateSettingsParams {
    pub workspace_id: String,
    pub session_id: String,
    pub selector: String,
}

/// 方法参数与结果类型的关联，adapter 与客户端生成共用。
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
    WorkbenchBootstrap => "workbench.bootstrap" (EmptyParams) -> crate::WorkbenchBootstrap,
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
    SessionQueueSendNow => "session.queueSendNow" (QueueControlParams) -> (),
    SessionAbort => "session.abort" (SessionParams) -> (),
    SessionCompact => "session.compact" (SessionParams) -> (),
    SessionUpdateSettings => "session.updateSettings" (UpdateSettingsParams) -> (),
}
