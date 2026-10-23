//! 工作台全部 RPC 方法名、请求参数与返回类型的对应关系。

use crate::*;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[cfg_attr(feature = "typescript", ts(type = "Record<string, never>"))]
pub struct EmptyParams {}

/// 停止当前会话内的指定后台命令。
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskParams {
    pub session_id: String,
    pub task_id: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct FileSearchParams {
    pub workspace_root: String,
    pub query: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct WorkspaceAddParams {
    pub root: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceParams {
    pub workspace_root: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProviderSaveParams {
    pub provider: ProviderConfigurationInput,
    /// 只在写入时使用的新密钥；省略表示保留已有密钥。
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProviderParams {
    pub provider_id: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct DiscoverModelsParams {
    pub provider_id: String,
    pub base_url: String,
    #[cfg_attr(feature = "typescript", ts(optional = nullable))]
    pub api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionParams {
    pub session_id: String,
}

/// 首次发送时创建会话，空白页只保存在工作台草稿中。
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionStartParams {
    pub workspace_root: String,
    pub selection: crate::ModelSelection,
    pub text: String,
    pub images: Vec<crate::ImageUpload>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionTextParams {
    pub session_id: String,
    pub text: String,
    pub images: Vec<crate::ImageUpload>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionImageParams {
    pub session_id: String,
    pub image_id: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionRenameParams {
    pub session_id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct UpdateSettingsParams {
    pub session_id: String,
    pub selection: crate::ModelSelection,
}

// 请求参数和客户端返回类型由同一份方法表声明。
macro_rules! rpc_methods {
    ($($variant:ident => $wire:literal ($params:ty) -> $result:ty),* $(,)?) => {
        /// 工作台请求；入口解析后，各操作直接使用自己的具体参数。
        #[derive(Debug, Deserialize)]
        #[serde(tag = "method", content = "params")]
        pub enum RpcRequest {
            $(#[serde(rename = $wire)] $variant($params),)*
        }
        #[cfg(feature = "typescript")]
        pub(crate) fn export_rpc_types(out: &mut crate::typescript::Bindings) -> String {
            $(out.add::<$params>(); out.add::<$result>();)*
            format!("export interface RpcContract {{\n{}\n}}",
                [$(format!("  {}: {{ params: {}; result: {} }}",
                    stringify!($wire),
                    <$params as ts_rs::TS>::name(out.config()),
                    <$result as ts_rs::TS>::name(out.config())
                )),*].join("\n"))
        }
    };
}
rpc_methods! {
    AppBootstrap => "app.bootstrap" (EmptyParams) -> crate::AppBootstrap,
    DirectoryPick => "directory.pick" (EmptyParams) -> DirectoryPickResult,
    FileSearch => "file.search" (FileSearchParams) -> Vec<FileCandidate>,
    SkillsList => "skills.list" (WorkspaceParams) -> SkillCatalog,
    WorkspaceAdd => "workspace.add" (WorkspaceAddParams) -> Workspace,
    WorkspaceRemove => "workspace.remove" (WorkspaceParams) -> (),
    ModelSaveProvider => "model.saveProvider" (ProviderSaveParams) -> ModelCatalogResult,
    ModelDiscover => "model.discover" (DiscoverModelsParams) -> Vec<DiscoveredModel>,
    ModelRemoveProvider => "model.removeProvider" (ProviderParams) -> ModelCatalogResult,
    McpList => "mcp.list" (McpListParams) -> Vec<McpServerState>,
    McpSave => "mcp.save" (McpSaveParams) -> Vec<McpServerState>,
    McpRemove => "mcp.remove" (McpServerParams) -> Vec<McpServerState>,
    McpToggle => "mcp.toggle" (McpToggleParams) -> Vec<McpServerState>,
    McpReconnect => "mcp.reconnect" (McpServerParams) -> McpServerState,
    SessionStart => "session.start" (SessionStartParams) -> SessionStartResult,
    SessionRead => "session.read" (SessionParams) -> SessionReadResult,
    SessionImageRead => "session.imageRead" (SessionImageParams) -> String,
    SessionRename => "session.rename" (SessionRenameParams) -> (),
    SessionDelete => "session.delete" (SessionParams) -> (),
    SessionSubmit => "session.submit" (SessionTextParams) -> (),
    SessionAnswerQuestion => "session.answerQuestion" (crate::QuestionAnswerParams) -> (),
    SessionAbort => "session.abort" (SessionParams) -> (),
    SessionCompact => "session.compact" (SessionParams) -> (),
    BackgroundTaskStop => "session.stopBackgroundTask" (BackgroundTaskParams) -> (),
    SessionUpdateSettings => "session.updateSettings" (UpdateSettingsParams) -> (),
}
