//! 工作台后端的入口：项目、任务、模型设置和运行状态在这里组合到一起。
//! 项目登记、任务列表和按项目分组在 `workspace` 子模块；单个任务的执行、快照和事件
//! 由 runtime::Conversation 负责。本模块负责组装依赖、查找已打开的任务、处理 RPC 和发布全局事件。

mod actions;
mod errors;
mod shutdown;
mod workspace;

pub(super) use self::errors::invalid_request;
use self::errors::*;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::runtime::{CatalogError, Conversation, ConversationError, ThreadCatalog};
use singularity_agent::agent::UserInput;
use singularity_model::{ModelConfigManager, ProviderError};
use singularity_protocol::{
    AppBootstrap, ProviderConfigurationInput, RpcError, RpcErrorCode, SessionReadResult, StreamEnvelope,
    StreamEvent,
};
use tokio::sync::mpsc;

use super::workspace_store::{WorkspaceError, WorkspaceStore};

/// 提交 RPC 在消息入队后返回；后续准备、保存和执行状态通过会话事件发布。
pub struct AppServer {
    weak: std::sync::Weak<Self>,
    revision: Mutex<u64>,
    /// 保证完整工作台快照按构造顺序发布；不涉及任务执行和普通增量事件。
    app_publication: Mutex<()>,
    /// 任务创建、删除和项目移除的短锁：「查找或创建任务 → 接受输入」与
    /// 「检查是否空闲 → 修改文件 → 注销」互斥，删除不会插到任务启动的中间。
    /// 只包住这几步，不覆盖模型请求、工具执行或整个任务。
    lifecycle: Mutex<()>,
    catalog: ThreadCatalog,
    workspaces: WorkspaceStore,
    /// 工作台与会话共用的模型配置入口。
    models: Arc<ModelConfigManager>,
    pub(super) mcp: Arc<singularity_mcp::McpManager>,
    /// 应用数据目录；技能列表和任务执行读取同一个目录。
    home: std::path::PathBuf,
    sessions: Mutex<HashMap<String, Arc<Conversation>>>,
    stream: mpsc::UnboundedSender<StreamEnvelope>,
}

impl AppServer {
    pub fn new(
        catalog: ThreadCatalog,
        workspaces: WorkspaceStore,
        models: Arc<ModelConfigManager>,
        mcp: Arc<singularity_mcp::McpManager>,
        home: std::path::PathBuf,
        stream: mpsc::UnboundedSender<StreamEnvelope>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            weak: weak.clone(),
            revision: Mutex::new(0),
            app_publication: Mutex::new(()),
            lifecycle: Mutex::new(()),
            catalog,
            workspaces,
            models,
            mcp,
            home,
            sessions: Mutex::new(HashMap::new()),
            stream,
        })
    }

    pub fn revision(&self) -> u64 {
        *self.revision.lock().expect("stream revision lock poisoned")
    }

    pub(super) fn mcp_directory(&self, workspace: Option<&str>) -> Result<std::path::PathBuf, RpcError> {
        workspace
            .map(|id| self.workspace(id).map(|workspace| workspace.root.into()))
            .unwrap_or_else(|| Ok(self.home.clone()))
    }

    pub fn save_provider(
        &self,
        provider: ProviderConfigurationInput,
        api_key: Option<&str>,
    ) -> Result<singularity_protocol::ModelCatalogResult, RpcError> {
        self.update_models(|models| models.save_provider(provider, api_key))
    }

    /// 设置页只重读模型配置，不依赖项目和会话日志。
    pub fn read_models(&self) -> singularity_protocol::ModelCatalogResult {
        let _publication = self.lock_app_publication();
        self.model_catalog_result(self.models.redacted_catalog())
    }

    pub fn remove_provider(
        &self,
        provider_id: &str,
    ) -> Result<singularity_protocol::ModelCatalogResult, RpcError> {
        self.update_models(|models| models.remove_provider(provider_id))
    }

    fn update_models(
        &self,
        update: impl FnOnce(
            &ModelConfigManager,
        ) -> Result<singularity_protocol::RedactedModelCatalog, ProviderError>,
    ) -> Result<singularity_protocol::ModelCatalogResult, RpcError> {
        let _publication = self.lock_app_publication();
        let catalog = update(&self.models).map_err(model_error)?;
        Ok(self.model_catalog_result(catalog))
    }

    // 重读也取得新序号，避免此前生成的工作台快照覆盖刚读到的文件配置。
    fn model_catalog_result(
        &self,
        catalog: singularity_protocol::RedactedModelCatalog,
    ) -> singularity_protocol::ModelCatalogResult {
        let mut revision = self.revision.lock().expect("stream revision lock poisoned");
        *revision += 1;
        singularity_protocol::ModelCatalogResult { catalog, revision: *revision }
    }

    pub async fn discover_models(
        &self,
        provider_id: &str,
        base_url: &str,
        api_key: Option<&str>,
    ) -> Result<Vec<String>, RpcError> {
        // 先取出本次查询用的密钥，网络请求期间不持有配置锁。
        let api_key =
            self.models.discovery_credential(provider_id, api_key).map_err(model_discovery_error)?;
        singularity_model::discover_models(base_url, &api_key)
            .await
            .map_err(model_discovery_error)
    }

    /// 收到第一条消息才创建任务；空白页在后端没有任务和日志文件。
    pub fn start_session(
        self: &Arc<Self>,
        params: singularity_protocol::SessionStartParams,
    ) -> Result<singularity_protocol::SessionStartResult, RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let workspace = self.workspace(&params.workspace_root)?;
        let input = UserInput::from_uploads(
            params.text,
            params.images,
            params.files,
            std::path::Path::new(&workspace.root),
            &self.home,
        )
        .map_err(invalid_request)?;
        let thread = self
            .catalog
            .create_thread(&workspace.root, params.selection, params.disabled_modules)
            .map_err(catalog_error)?;
        let conversation = self.insert_conversation(thread);
        conversation.start(input).map_err(conversation_error)?;
        Ok(conversation.start_result())
    }

    pub fn read_session(&self, session_id: &str) -> Result<SessionReadResult, RpcError> {
        // 只有查找或打开任务需要生命周期锁；读取历史在锁外进行。
        let conversation = {
            let _lifecycle = self.lock_lifecycle();
            self.open_conversation(session_id)?
        };
        conversation.read().map_err(catalog_error)
    }

    fn open_conversation(&self, session_id: &str) -> Result<Arc<Conversation>, RpcError> {
        // 任务表的锁只用于查找；打开日志文件在锁外进行。
        let open = self.lock_sessions().get(session_id).cloned();
        if let Some(conversation) = open {
            return Ok(conversation);
        }
        let thread = self.catalog.open_session(session_id).map_err(catalog_error)?;
        Ok(self.insert_conversation(thread))
    }

    /// 在生命周期锁内打开任务，正在删除的任务拒绝新操作；删除等待只影响该任务。
    fn open_action_conversation(&self, session_id: &str) -> Result<Arc<Conversation>, RpcError> {
        let conversation = self.open_conversation(session_id)?;
        if conversation.is_deleting() {
            return Err(session_busy());
        }
        Ok(conversation)
    }

    /// 在生命周期锁内登记新打开的任务；已打开的任务由 open_conversation 直接返回。
    fn insert_conversation(&self, session: singularity_agent::session::SessionManager) -> Arc<Conversation> {
        let id = session.session_id().to_owned();
        let weak = self.weak.clone();
        let conversation = Conversation::new(
            session,
            self.home.clone(),
            Arc::clone(&self.models),
            Arc::clone(&self.mcp),
            Arc::new(move |event| weak.upgrade().map(|app| app.emit(event)).unwrap_or_default()),
        );
        self.lock_sessions().insert(id, Arc::clone(&conversation));
        conversation
    }

    /// 发布完整的工作台快照；快照生成失败不影响已保存的操作结果。
    /// 在释放生命周期锁之后调用，读取目录期间其他任务仍可操作。
    fn publish_app_snapshot(&self) {
        let _publication = self.lock_app_publication();
        self.publish_app_result(self.bootstrap_with_catalog(self.models.redacted_catalog()));
    }

    fn publish_app_result(&self, snapshot: Result<AppBootstrap, RpcError>) {
        match snapshot {
            Ok(payload) => {
                self.emit(StreamEvent::AppChanged { payload });
            }
            // 操作已经保存，RPC 照常返回结果；读取列表失败由客户端显示列表错误，
            // 不悄悄保留旧列表。
            Err(error) => {
                self.emit(StreamEvent::AppReadFailed { error });
            }
        }
    }

    /// RPC 读取和快照发布都在这把锁内生成内容并取得事件序号；否则较早生成的
    /// 快照可能拿到比新快照更大的序号。会话事件不经过这把锁，避免与会话状态锁
    /// 形成相反的加锁顺序。
    fn lock_app_publication(&self) -> std::sync::MutexGuard<'_, ()> {
        self.app_publication.lock().expect("app_server publication lock poisoned")
    }

    /// 生命周期锁。加锁顺序是 lifecycle → publication → sessions → 会话状态；
    /// 只在公开入口的最外层获取。
    fn lock_lifecycle(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lifecycle.lock().expect("app_server lifecycle lock poisoned")
    }

    /// 发布一个事件：在这里递增全局事件序号，随事件一起发给前端。
    fn emit(&self, event: StreamEvent) -> u64 {
        let mut order = self.revision.lock().expect("stream revision lock poisoned");
        *order += 1;
        let _ = self.stream.send(StreamEnvelope { revision: *order, event });
        *order
    }

    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Conversation>>> {
        self.sessions.lock().expect("app_server session map lock poisoned")
    }
}
