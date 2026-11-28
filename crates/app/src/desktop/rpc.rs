use super::app_server::AppServer;
use protocol::{RpcError, RpcRequest};
use serde::Serialize;
use serde_json::value::{RawValue, to_raw_value};
use std::sync::Arc;

pub(super) async fn handle(
    app: &Arc<AppServer>,
    request: RpcRequest,
    shutdown: &tokio_util::sync::CancellationToken,
) -> Result<Box<RawValue>, RpcError> {
    dispatch(Arc::clone(app), request, shutdown)
        .await
        .inspect_err(|error| tracing::warn!(error = %error.message, "RPC request failed"))
}

async fn dispatch(
    app: Arc<AppServer>,
    request: RpcRequest,
    shutdown: &tokio_util::sync::CancellationToken,
) -> Result<Box<RawValue>, RpcError> {
    match request {
        RpcRequest::McpReconnect(params) => {
            Ok(value(app.mcp_reconnect(params.workspace_root, &params.server_id, shutdown).await?))
        }
        RpcRequest::ModelDiscover(params) => Ok(value(
            app.discover_models(
                &params.provider_id,
                params.api_protocol,
                &params.base_url,
                params.api_key.as_deref(),
                shutdown,
            )
            .await?,
        )),
        RpcRequest::BackgroundTaskStop(params) => {
            Ok(value(app.stop_background_task(&params.session_id, &params.task_id).await?))
        }
        RpcRequest::WorkspaceRemove(params) => Ok(value(app.remove_workspace(&params.workspace_root).await?)),
        RpcRequest::SessionDelete(params) => Ok(value(app.delete_session(&params.session_id).await?)),
        RpcRequest::SessionContext(params) => {
            Ok(value(app.inspect_context(&params.session_id, shutdown).await?))
        }
        RpcRequest::DirectoryPick(_) => Err(RpcError::new("目录选择由 Electron 处理。")),
        RpcRequest::AppBootstrap(_) => blocking(move || app.bootstrap()).await,
        RpcRequest::SkillsList(params) => blocking(move || app.skills(&params.workspace_root)).await,
        RpcRequest::InstructionsRead(params) => {
            blocking(move || app.read_instructions(params.workspace_root.as_deref())).await
        }
        RpcRequest::InstructionsSave(params) => {
            blocking(move || app.save_instructions(params.workspace_root.as_deref(), &params.content)).await
        }
        RpcRequest::FileRead(params) => blocking(move || app.read_text_file(&params.cwd, &params.path)).await,
        RpcRequest::WorkspaceAdd(params) => blocking(move || app.add_workspace(&params.root)).await,
        RpcRequest::ModelRead(_) => blocking(move || Ok(app.read_models())).await,
        RpcRequest::ModelSaveProvider(params) => {
            blocking(move || app.save_provider(params.provider, params.api_key.as_deref())).await
        }
        RpcRequest::ModelRemoveProvider(params) => {
            blocking(move || app.remove_provider(&params.provider_id)).await
        }
        RpcRequest::ModelSaveWebSearch(params) => {
            blocking(move || app.save_web_search(&params.base_url, params.api_key.as_deref())).await
        }
        RpcRequest::McpList(params) => blocking(move || app.mcp_list(params.workspace_root.as_deref())).await,
        RpcRequest::McpSave(params) => {
            blocking(move || app.mcp_save(params.workspace_root.as_deref(), params.server)).await
        }
        RpcRequest::McpRemove(params) => {
            blocking(move || app.mcp_remove(params.workspace_root.as_deref(), &params.server_id)).await
        }
        RpcRequest::ContextPreview(params) => {
            blocking(move || app.context_preview(&params.workspace_root)).await
        }
        RpcRequest::SessionStart(params) => blocking(move || app.start_session(params)).await,
        RpcRequest::SessionRead(params) => {
            app.set_focused_session(Some(params.session_id.clone()));
            blocking(move || app.read_session(&params.session_id)).await
        }
        RpcRequest::SessionFocus(params) => {
            app.set_focused_session(params.session_id);
            Ok(value(()))
        }
        RpcRequest::SessionImageRead(params) => {
            blocking(move || app.image_data(&params.session_id, &params.image_id)).await
        }
        RpcRequest::SessionRename(params) => {
            blocking(move || app.rename_session(&params.session_id, &params.name)).await
        }
        RpcRequest::SessionSubmit(params) => blocking(move || app.submit(params)).await,
        RpcRequest::SessionAnswerQuestion(params) => {
            blocking(move || app.answer_question(&params.session_id, &params.item_id, params.answers)).await
        }
        RpcRequest::SessionAbort(params) => blocking(move || app.abort(&params.session_id)).await,
        RpcRequest::SessionCompact(params) => blocking(move || app.compact(&params.session_id)).await,
        RpcRequest::SessionUpdateSettings(params) => {
            blocking(move || app.update_settings(&params.session_id, &params.selection)).await
        }
        RpcRequest::SessionChangedFiles(params) => blocking(move || app.changed_files(&params)).await,
        RpcRequest::SessionFileDiff(params) => blocking(move || app.file_diff(&params)).await,
        RpcRequest::SessionRewind(params) => blocking(move || app.rewind_session(params)).await,
    }
}

async fn blocking<T: Serialize>(
    operation: impl FnOnce() -> Result<T, RpcError> + Send + 'static,
) -> Result<Box<RawValue>, RpcError> {
    tokio::task::spawn_blocking(move || Ok(value(operation()?)))
        .await
        .expect("RPC worker completes while the backend is running")
}

fn value(output: impl Serialize) -> Box<RawValue> {
    to_raw_value(&output).expect("RPC output contains JSON-compatible protocol values")
}
