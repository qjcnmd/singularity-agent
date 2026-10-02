//! 工作台 RPC 边界的方法、参数与结果类型之间的关联。

use crate::*;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[cfg_attr(feature = "typescript", ts(type = "Record<string, never>"))]
pub struct EmptyParams {}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct FileSearchParams {
    pub workspace_id: String,
    pub query: String,
    pub limit: usize,
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
    pub workspace_id: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRemoveParams {
    pub workspace_id: String,
    pub draft_session_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRenameParams {
    pub workspace_id: String,
    pub name: String,
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
    pub api_protocol: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionReadParams {
    pub session_id: String,
    #[cfg_attr(feature = "typescript", ts(optional = nullable))]
    pub before_turn: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionParams {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionTextParams {
    pub session_id: String,
    pub text: String,
    #[serde(default)]
    #[cfg_attr(
        feature = "typescript",
        ts(as = "Option<Vec<crate::ImageUpload>>", optional)
    )]
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
pub struct QueueControlParams {
    pub session_id: String,
    pub control_id: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct QueueReplaceParams {
    pub session_id: String,
    pub control_id: String,
    pub text: String,
    #[serde(default)]
    #[cfg_attr(
        feature = "typescript",
        ts(as = "Option<Vec<crate::ImageUpload>>", optional)
    )]
    pub images: Vec<crate::ImageUpload>,
}

/// 「立即发送」的目标：指定一条待处理输入，或者省略 `controlId` 表示当前队列里的全部待处理
/// 输入。两种目标都由服务端在队列的临界区内读取并交接，客户端不用枚举自己快照里的条目。
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct QueueSendParams {
    pub session_id: String,
    #[cfg_attr(feature = "typescript", ts(optional = nullable))]
    pub control_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct UpdateSettingsParams {
    pub session_id: String,
    pub selector: String,
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
        pub(crate) fn export_rpc_types(out: &mut crate::typescript::Bindings) {
            $(out.add::<$params>(); out.add::<$result>();)*
            out.push(format!("export interface RpcContract {{\n{}\n}}",
                [$(format!("  {}: {{ params: {}; result: {} }}",
                    stringify!($wire),
                    <$params as ts_rs::TS>::name(out.config()),
                    <$result as ts_rs::TS>::name(out.config())
                )),*].join("\n")));
        }
    };
}
rpc_methods! {
    AppBootstrap => "app.bootstrap" (EmptyParams) -> crate::AppBootstrap,
    DirectoryPick => "directory.pick" (EmptyParams) -> DirectoryPickResult,
    FileSearch => "file.search" (FileSearchParams) -> Vec<FileCandidate>,
    SkillsList => "skills.list" (WorkspaceParams) -> SkillCatalog,
    WorkspaceAdd => "workspace.add" (WorkspaceAddParams) -> Workspace,
    WorkspaceRemove => "workspace.remove" (WorkspaceRemoveParams) -> Vec<String>,
    WorkspaceRename => "workspace.rename" (WorkspaceRenameParams) -> (),
    ModelSaveProvider => "model.saveProvider" (ProviderSaveParams) -> (),
    ModelDiscover => "model.discover" (DiscoverModelsParams) -> Vec<DiscoveredModel>,
    ModelRemoveProvider => "model.removeProvider" (ProviderParams) -> (),
    McpList => "mcp.list" (EmptyParams) -> Vec<McpServerInput>,
    McpSave => "mcp.save" (McpSaveParams) -> (),
    McpRemove => "mcp.remove" (McpServerParams) -> (),
    McpToggle => "mcp.toggle" (McpToggleParams) -> (),
    McpInspect => "mcp.inspect" (McpInspectParams) -> McpInspection,
    SessionCreate => "session.create" (WorkspaceParams) -> SessionReadResult,
    SessionRead => "session.read" (SessionReadParams) -> SessionReadResult,
    SessionImageRead => "session.imageRead" (SessionImageParams) -> String,
    SessionRename => "session.rename" (SessionRenameParams) -> (),
    SessionArchive => "session.archive" (SessionParams) -> (),
    SessionSubmit => "session.submit" (SessionTextParams) -> (),
    SessionSteer => "session.steer" (SessionTextParams) -> (),
    SessionFollowUp => "session.followUp" (SessionTextParams) -> (),
    SessionQueueWithdraw => "session.queueWithdraw" (QueueControlParams) -> (),
    SessionQueueReplace => "session.queueReplace" (QueueReplaceParams) -> (),
    SessionQueueSendNow => "session.queueSendNow" (QueueSendParams) -> (),
    SessionAnswerQuestion => "session.answerQuestion" (crate::QuestionAnswerParams) -> (),
    SessionAbort => "session.abort" (SessionParams) -> (),
    SessionCompact => "session.compact" (SessionParams) -> (),
    SessionUpdateSettings => "session.updateSettings" (UpdateSettingsParams) -> (),
}
