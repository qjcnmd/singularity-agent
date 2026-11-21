//! 工作台后端的入口：项目、任务、模型设置和运行状态在这里组合到一起。
//! 项目登记、任务列表和按项目分组在 `workspace` 子模块；单个任务的执行、快照和事件
//! 由 runtime::Conversation 负责。本模块负责组装依赖、查找已打开的任务、处理 RPC 和发布全局事件。

mod actions;
mod workspace;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::runtime::{CatalogError, Conversation, ConversationError, ThreadCatalog};
use agent::agent::UserInput;
use model::{ModelConfigManager, ProviderError};
use protocol::{
    AppBootstrap, ProviderConfigurationInput, RpcError, SessionReadResult, StreamEnvelope, StreamEvent,
};
use tokio::sync::mpsc;

use super::stream_hub::StreamHub;
use super::workspace_store::WorkspaceStore;

const MAX_CACHED_SESSIONS: usize = 16;

#[derive(Default)]
pub(super) struct SessionCache {
    entries: HashMap<String, (Arc<Conversation>, u64)>,
    next_access: u64,
}

impl SessionCache {
    pub(super) fn get(&mut self, id: &str) -> Option<Arc<Conversation>> {
        let (conversation, last_access) = self.entries.get_mut(id)?;
        self.next_access += 1;
        *last_access = self.next_access;
        Some(Arc::clone(conversation))
    }

    pub(super) fn insert(&mut self, id: String, conversation: Arc<Conversation>) {
        if !self.entries.contains_key(&id) && self.entries.len() >= MAX_CACHED_SESSIONS {
            self.evict_idle(self.entries.len() + 1 - MAX_CACHED_SESSIONS);
        }
        self.next_access += 1;
        self.entries.insert(id, (conversation, self.next_access));
    }

    pub(super) fn remove(&mut self, id: &str) -> Option<Arc<Conversation>> {
        self.entries.remove(id).map(|(conversation, _)| conversation)
    }

    pub(super) fn retain(&mut self, mut predicate: impl FnMut(&str, &Arc<Conversation>) -> bool) {
        self.entries.retain(|id, (conversation, _)| predicate(id, conversation));
    }

    pub(super) fn conversations(&self) -> impl Iterator<Item = (&str, &Arc<Conversation>)> {
        self.entries.iter().map(|(id, (conversation, _))| (id.as_str(), conversation))
    }

    fn evict_idle(&mut self, overflow: usize) {
        let mut candidates: Vec<(u64, String)> = self
            .entries
            .iter()
            .filter(|(_, (conversation, _))| conversation.can_evict())
            .map(|(id, (_, access))| (*access, id.clone()))
            .collect();
        candidates.sort_unstable_by_key(|(access, _)| *access);
        for (_, id) in candidates.into_iter().take(overflow) {
            self.entries.remove(&id);
        }
    }
}

/// 提交 RPC 在消息入队后返回；后续准备、保存和执行状态通过会话事件发布。
pub(crate) struct AppServer {
    hub: Arc<StreamHub>,
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
    pub(super) mcp: Arc<mcp::McpManager>,
    /// 应用数据目录；技能列表和任务执行读取同一个目录。
    home: std::path::PathBuf,
    sessions: Mutex<SessionCache>,
}

impl AppServer {
    pub(crate) fn new(
        catalog: ThreadCatalog,
        workspaces: WorkspaceStore,
        models: Arc<ModelConfigManager>,
        mcp: Arc<mcp::McpManager>,
        home: std::path::PathBuf,
        stream: mpsc::Sender<StreamEnvelope>,
    ) -> Arc<Self> {
        Arc::new(Self {
            hub: StreamHub::new(stream),
            app_publication: Mutex::new(()),
            lifecycle: Mutex::new(()),
            catalog,
            workspaces,
            models,
            mcp,
            home,
            sessions: Mutex::new(SessionCache::default()),
        })
    }

    pub(crate) fn set_focused_session(&self, epoch: u64, session_id: Option<String>) {
        self.hub.set_focused_session(epoch, session_id);
    }

    pub(crate) fn revision(&self) -> u64 {
        self.hub.revision()
    }

    pub(super) fn scope_directory(&self, workspace: Option<&str>) -> Result<std::path::PathBuf, RpcError> {
        workspace.map_or_else(
            || Ok(self.home.clone()),
            |id| self.workspace(id).map(|workspace| workspace.root.into()),
        )
    }

    pub(crate) fn save_provider(
        &self,
        provider: ProviderConfigurationInput,
        api_key: Option<&str>,
    ) -> Result<protocol::ModelCatalogResult, RpcError> {
        self.update_models(|models| models.save_provider(provider, api_key))
    }

    /// 设置页只重读模型配置，不依赖项目和会话日志。
    pub(crate) fn read_models(&self) -> protocol::ModelCatalogResult {
        let _publication = self.lock_app_publication();
        self.model_catalog_result(self.models.redacted_catalog())
    }

    pub(crate) fn remove_provider(
        &self,
        provider_id: &str,
    ) -> Result<protocol::ModelCatalogResult, RpcError> {
        self.update_models(|models| models.remove_provider(provider_id))
    }

    fn update_models(
        &self,
        update: impl FnOnce(&ModelConfigManager) -> Result<protocol::RedactedModelCatalog, ProviderError>,
    ) -> Result<protocol::ModelCatalogResult, RpcError> {
        let _publication = self.lock_app_publication();
        let catalog = update(&self.models).map_err(RpcError::new)?;
        Ok(self.model_catalog_result(catalog))
    }

    // 重读也取得新序号，避免此前生成的工作台快照覆盖刚读到的文件配置。
    fn model_catalog_result(&self, catalog: protocol::RedactedModelCatalog) -> protocol::ModelCatalogResult {
        protocol::ModelCatalogResult {
            catalog,
            revision: self.hub.next_revision(),
        }
    }

    pub(crate) async fn discover_models(
        &self,
        provider_id: &str,
        api_protocol: protocol::ProviderApiProtocol,
        base_url: &str,
        api_key: Option<&str>,
    ) -> Result<Vec<String>, RpcError> {
        // 先取出本次查询用的密钥，网络请求期间不持有配置锁。
        let models = Arc::clone(&self.models);
        let provider_id = provider_id.to_owned();
        let api_key = api_key.map(str::to_owned);
        let api_key = tokio::task::spawn_blocking(move || {
            models.discovery_credential(&provider_id, api_key.as_deref())
        })
        .await
        .expect("model credential lookup completes while the backend is running")
        .map_err(RpcError::new)?;
        model::discover_models(base_url, &api_key, api_protocol).await.map_err(RpcError::new)
    }

    /// 收到第一条消息才创建任务；空白页在后端没有任务和日志文件。
    pub(crate) fn start_session(
        &self,
        params: protocol::SessionStartParams,
    ) -> Result<protocol::SessionStartResult, RpcError> {
        let workspace = self.workspace(&params.workspace_root)?;
        let skill_expansion = agent::agent::SkillExpansion::from_disabled_modules(&params.disabled_modules);
        let input = UserInput::from_uploads(
            params.text,
            params.images,
            params.files,
            std::path::Path::new(&workspace.root),
            &self.home,
            skill_expansion,
        )
        .map_err(RpcError::new)?;
        let _lifecycle = self.lock_lifecycle();
        let workspace = self.workspace(&params.workspace_root)?;
        let thread = self
            .catalog
            .create_thread(&workspace.root, params.selection, params.disabled_modules)
            .map_err(catalog_error)?;
        let conversation = self.insert_conversation(thread);
        conversation.start(input).map_err(conversation_error)?;
        Ok(conversation.start_result())
    }

    pub(crate) fn read_session(
        &self,
        session_id: &str,
        focus_epoch: u64,
    ) -> Result<SessionReadResult, RpcError> {
        // 只有查找或打开任务需要生命周期锁；确认存在后再按请求序号切焦点，避免无效 ID 或过期读取覆盖当前会话焦点。
        let conversation = {
            let _lifecycle = self.lock_lifecycle();
            self.open_conversation(session_id)?
        };
        self.set_focused_session(focus_epoch, Some(session_id.to_owned()));
        conversation.read().map_err(catalog_error)
    }

    fn open_conversation(&self, session_id: &str) -> Result<Arc<Conversation>, RpcError> {
        // 任务表的锁只用于查找；打开日志文件在锁外进行。
        let open = self.lock_sessions().get(session_id);
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
    fn insert_conversation(&self, session: agent::session::SessionManager) -> Arc<Conversation> {
        let id = session.data().session_id().to_owned();
        let conversation = Conversation::new(
            session,
            self.home.clone(),
            Arc::clone(&self.models),
            Arc::clone(&self.mcp),
            self.hub.revision(),
            self.hub.publisher(),
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

    fn emit(&self, event: StreamEvent) -> u64 {
        self.hub.emit(event)
    }

    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, SessionCache> {
        self.sessions.lock().expect("app_server session map lock poisoned")
    }

    /// 管道关闭时等待已接受的操作和子进程结束。
    pub(crate) async fn shutdown(&self) {
        let sessions: Vec<_> =
            self.lock_sessions().conversations().map(|(_, conv)| Arc::clone(conv)).collect();
        let mut stops = tokio::task::JoinSet::new();
        for session in sessions {
            stops.spawn(async move { session.stop().await });
        }
        while let Some(result) = stops.join_next().await {
            if let Err(error) = result.expect("session shutdown completes while the runtime is running") {
                tracing::warn!(error = %error, "session shutdown reported an error");
                eprintln!("{}: {error}", crate::PROGRAM_NAME);
            }
        }
        self.mcp.shutdown().await;
    }
}

pub(super) fn conversation_error(error: ConversationError) -> RpcError {
    match error {
        ConversationError::Busy => session_busy(),
        other => RpcError::new(other),
    }
}

pub(super) fn catalog_error(error: CatalogError) -> RpcError {
    match error {
        CatalogError::NotFound(_) => RpcError::new("任务不存在或已删除。"),
        other => RpcError::new(other),
    }
}

pub(super) fn session_busy() -> RpcError {
    RpcError::new("当前任务正在处理另一项操作。")
}
