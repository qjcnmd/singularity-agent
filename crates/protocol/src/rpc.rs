#[cfg(feature = "typescript")]
use crate::{
    AppBootstrap, ContextModule, DirectoryPickResult, McpServerState, ModelCatalogResult,
    SessionChangedFilesResult, SessionFileDiffResult, SessionReadResult, SessionStartResult, SkillCatalog,
    TextDocument, Workspace,
};
use crate::{
    ImageUpload, McpListParams, McpSaveParams, McpServerParams, ModelSelection, ProviderApiProtocol,
    ProviderConfigurationInput, QuestionAnswerParams,
};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[cfg_attr(feature = "typescript", ts(type = "Record<string, never>"))]
#[serde(deny_unknown_fields)]
pub struct EmptyParams {}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskParams {
    pub session_id: String,
    pub task_id: String,
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
pub struct InstructionsReadParams {
    pub workspace_root: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct InstructionsSaveParams {
    pub workspace_root: Option<String>,
    pub content: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct FileReadParams {
    pub cwd: String,
    pub path: String,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProviderSaveParams {
    pub provider: ProviderConfigurationInput,
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub api_key: Option<String>,
}

impl fmt::Debug for ProviderSaveParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderSaveParams")
            .field("provider", &self.provider)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProviderParams {
    pub provider_id: String,
}

#[derive(Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct WebSearchSaveParams {
    pub base_url: String,
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub api_key: Option<String>,
}

impl fmt::Debug for WebSearchSaveParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebSearchSaveParams")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

#[derive(Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct DiscoverModelsParams {
    pub provider_id: String,
    pub api_protocol: ProviderApiProtocol,
    pub base_url: String,
    #[cfg_attr(feature = "typescript", ts(optional = nullable))]
    pub api_key: Option<String>,
}

impl fmt::Debug for DiscoverModelsParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscoverModelsParams")
            .field("provider_id", &self.provider_id)
            .field("api_protocol", &self.api_protocol)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .finish()
    }
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
pub struct SessionFocusParams {
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionStartParams {
    pub workspace_root: String,
    pub selection: ModelSelection,
    pub text: String,
    pub images: Vec<ImageUpload>,
    pub files: Vec<String>,
    pub disabled_modules: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionTextParams {
    pub session_id: String,
    pub text: String,
    pub images: Vec<ImageUpload>,
    pub files: Vec<String>,
    #[serde(default)]
    #[cfg_attr(
        feature = "typescript",
        ts(as = "Option<Vec<crate::MessageAnnotation>>", optional)
    )]
    pub annotations: Vec<crate::MessageAnnotation>,
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
    pub selection: ModelSelection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionChangedFilesParams {
    #[serde(default)]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub session_id: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub workspace_root: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "typescript", ts(as = "Option<bool>", optional))]
    pub include_diffs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionFileDiffParams {
    #[serde(default)]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub session_id: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub workspace_root: Option<String>,
    pub path: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionRewindInput {
    pub text: String,
    #[serde(default)]
    pub images: Vec<ImageUpload>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    #[cfg_attr(
        feature = "typescript",
        ts(as = "Option<Vec<crate::MessageAnnotation>>", optional)
    )]
    pub annotations: Vec<crate::MessageAnnotation>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionRewindParams {
    pub session_id: String,
    pub message_id: String,
    #[serde(default)]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub resubmit: Option<SessionRewindInput>,
}

macro_rules! rpc_methods {
    ($($variant:ident => $wire:literal ($params:ty) -> $result:ty),* $(,)?) => {
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
    AppBootstrap => "app.bootstrap" (EmptyParams) -> AppBootstrap,
    DirectoryPick => "directory.pick" (EmptyParams) -> DirectoryPickResult,
    FileRead => "file.read" (FileReadParams) -> TextDocument,
    SkillsList => "skills.list" (WorkspaceParams) -> SkillCatalog,
    InstructionsRead => "instructions.read" (InstructionsReadParams) -> TextDocument,
    InstructionsSave => "instructions.save" (InstructionsSaveParams) -> TextDocument,
    WorkspaceAdd => "workspace.add" (WorkspaceAddParams) -> Workspace,
    WorkspaceRemove => "workspace.remove" (WorkspaceParams) -> (),
    ModelRead => "model.read" (EmptyParams) -> ModelCatalogResult,
    ModelSaveProvider => "model.saveProvider" (ProviderSaveParams) -> ModelCatalogResult,
    ModelDiscover => "model.discover" (DiscoverModelsParams) -> Vec<String>,
    ModelRemoveProvider => "model.removeProvider" (ProviderParams) -> ModelCatalogResult,
    ModelSaveWebSearch => "model.saveWebSearch" (WebSearchSaveParams) -> ModelCatalogResult,
    McpList => "mcp.list" (McpListParams) -> Vec<McpServerState>,
    McpSave => "mcp.save" (McpSaveParams) -> Vec<McpServerState>,
    McpRemove => "mcp.remove" (McpServerParams) -> Vec<McpServerState>,
    McpReconnect => "mcp.reconnect" (McpServerParams) -> McpServerState,
    ContextPreview => "context.preview" (WorkspaceParams) -> Vec<ContextModule>,
    SessionStart => "session.start" (SessionStartParams) -> SessionStartResult,
    SessionRead => "session.read" (SessionParams) -> SessionReadResult,
    SessionFocus => "session.focus" (SessionFocusParams) -> (),
    SessionImageRead => "session.imageRead" (SessionImageParams) -> String,
    SessionRename => "session.rename" (SessionRenameParams) -> (),
    SessionDelete => "session.delete" (SessionParams) -> (),
    SessionSubmit => "session.submit" (SessionTextParams) -> (),
    SessionAnswerQuestion => "session.answerQuestion" (QuestionAnswerParams) -> (),
    SessionAbort => "session.abort" (SessionParams) -> (),
    SessionCompact => "session.compact" (SessionParams) -> (),
    SessionContext => "session.context" (SessionParams) -> (),
    BackgroundTaskStop => "session.stopBackgroundTask" (BackgroundTaskParams) -> (),
    SessionUpdateSettings => "session.updateSettings" (UpdateSettingsParams) -> (),
    SessionChangedFiles => "session.changedFiles" (SessionChangedFilesParams) -> SessionChangedFilesResult,
    SessionFileDiff => "session.fileDiff" (SessionFileDiffParams) -> SessionFileDiffResult,
    SessionRewind => "session.rewind" (SessionRewindParams) -> SessionReadResult,
}
