//! Method, parameter and result associations for the version 1 RPC boundary.

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
pub struct DirectoryListParams {
    #[cfg_attr(feature = "typescript", ts(optional = nullable))]
    pub path: Option<String>,
}

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
pub struct SessionRequestParams {
    pub workspace_id: String,
    pub session_id: String,
    pub request_id: String,
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

/// A method's parameter and result types, shared by adapters and client generation.
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
        /// Typed method markers used by the HTTP adapter.
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
    DirectoryList => "directory.list" (DirectoryListParams) -> Vec<DirectoryEntry>,
    DirectoryPick => "directory.pick" (EmptyParams) -> DirectoryPickResult,
    FileSearch => "file.search" (FileSearchParams) -> Vec<FileCandidate>,
    SkillsList => "skills.list" (SkillsListParams) -> SkillCatalog,
    WorkspaceAdd => "workspace.add" (WorkspaceAddParams) -> Workspace,
    WorkspaceRemove => "workspace.remove" (WorkspaceParams) -> WorkspaceRemoved,
    WorkspaceRename => "workspace.rename" (WorkspaceRenameParams) -> Workspace,
    ModelSaveProvider => "model.saveProvider" (ProviderSaveParams) -> RedactedModelCatalog,
    ModelSetApiKey => "model.setApiKey" (ApiKeyParams) -> CredentialConfigured,
    ModelDiscover => "model.discover" (DiscoverModelsParams) -> Vec<DiscoveredModel>,
    ModelRemoveProvider => "model.removeProvider" (ProviderParams) -> RedactedModelCatalog,
    SessionCreate => "session.create" (SessionCreateParams) -> SessionReadResult,
    SessionRead => "session.read" (SessionReadParams) -> SessionReadResult,
    SessionRequest => "session.request" (SessionRequestParams) -> ModelRequestSnapshot,
    SessionRename => "session.rename" (SessionRenameParams) -> ThreadSummary,
    SessionArchive => "session.archive" (SessionParams) -> SessionArchived,
    SessionSubmit => "session.submit" (SessionTextParams) -> ActionReceipt,
    SessionSteer => "session.steer" (SessionTextParams) -> ActionReceipt,
    SessionFollowUp => "session.followUp" (SessionTextParams) -> ActionReceipt,
    SessionQueueWithdraw => "session.queueWithdraw" (QueueControlParams) -> ActionReceipt,
    SessionQueueReplace => "session.queueReplace" (QueueReplaceParams) -> ActionReceipt,
    SessionQueueSendNow => "session.queueSendNow" (QueueControlParams) -> ActionReceipt,
    SessionAbort => "session.abort" (SessionParams) -> ActionReceipt,
    SessionCompact => "session.compact" (SessionParams) -> ActionReceipt,
    SessionUpdateSettings => "session.updateSettings" (UpdateSettingsParams) -> ActionReceipt,
}
