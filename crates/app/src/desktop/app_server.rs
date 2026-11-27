mod actions;
mod workspace;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::runtime::{CatalogError, Conversation, ConversationError, ThreadCatalog};
use agent::agent::UserInput;
use model::{ModelConfigManager, ProviderError};
use protocol::{ProviderConfigurationInput, RpcError, SessionReadResult, StreamEnvelope, StreamEvent};
use tokio::sync::mpsc;

use super::stream_hub::StreamHub;
use super::workspace_store::WorkspaceStore;

const MAX_CACHED_SESSIONS: usize = 16;

#[derive(Default)]
pub(super) struct SessionCache {
    entries: HashMap<String, (Arc<Conversation>, u64)>,
    deleting_sessions: HashSet<String>,
    next_access: u64,
}

impl SessionCache {
    pub(super) fn get(&mut self, id: &str) -> Option<Arc<Conversation>> {
        let (conversation, last_access) = self.entries.get_mut(id)?;
        self.next_access += 1;
        *last_access = self.next_access;
        Some(Arc::clone(conversation))
    }

    pub(super) fn is_deleting(&self, id: &str) -> bool {
        self.deleting_sessions.contains(id)
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

pub(crate) struct AppServer {
    hub: Arc<StreamHub>,
    app_publication: Mutex<()>,
    catalog: ThreadCatalog,
    workspaces: WorkspaceStore,
    models: Arc<ModelConfigManager>,
    pub(super) mcp: Arc<mcp::McpManager>,
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

    pub(crate) fn save_web_search(
        &self,
        base_url: &str,
        api_key: Option<&str>,
    ) -> Result<protocol::ModelCatalogResult, RpcError> {
        self.update_models(|models| models.save_web_search(base_url, api_key))
    }

    fn update_models(
        &self,
        update: impl FnOnce(&ModelConfigManager) -> Result<protocol::RedactedModelCatalog, ProviderError>,
    ) -> Result<protocol::ModelCatalogResult, RpcError> {
        let _publication = self.lock_app_publication();
        let catalog = update(&self.models).map_err(RpcError::new)?;
        Ok(self.model_catalog_result(catalog))
    }

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
        self.models
            .discover_models(provider_id, api_protocol, base_url, api_key)
            .await
            .map_err(RpcError::new)
    }

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
            Vec::new(),
            std::path::Path::new(&workspace.root),
            &self.home,
            skill_expansion,
        )
        .map_err(RpcError::new)?;
        let mut sessions = self.lock_sessions();
        let workspace = self.workspace(&params.workspace_root)?;
        let thread = self
            .catalog
            .create_thread(&workspace.root, params.selection, params.disabled_modules)
            .map_err(catalog_error)?;
        let conversation = self.insert_conversation(&mut sessions, thread);
        conversation.start(input).map_err(conversation_error)?;
        Ok(conversation.start_result())
    }

    pub(crate) fn read_session(
        &self,
        session_id: &str,
        focus_epoch: u64,
    ) -> Result<SessionReadResult, RpcError> {
        let conversation = {
            let mut sessions = self.lock_sessions();
            self.open_conversation(&mut sessions, session_id, false)?
        };
        self.set_focused_session(focus_epoch, Some(session_id.to_owned()));
        conversation.read().map_err(catalog_error)
    }

    pub(super) fn open_conversation(
        &self,
        sessions: &mut SessionCache,
        session_id: &str,
        require_workspace: bool,
    ) -> Result<Arc<Conversation>, RpcError> {
        if sessions.is_deleting(session_id) {
            return Err(session_busy());
        }
        if let Some(conversation) = sessions.get(session_id) {
            if require_workspace {
                self.workspace(&conversation.cwd)?;
            }
            return Ok(conversation);
        }
        let thread = self.catalog.open_session(session_id).map_err(catalog_error)?;
        if require_workspace {
            self.workspace(&thread.data().cwd_string())?;
        }
        Ok(self.insert_conversation(sessions, thread))
    }

    fn insert_conversation(
        &self,
        sessions: &mut SessionCache,
        session: agent::session::SessionManager,
    ) -> Arc<Conversation> {
        let id = session.data().session_id().to_owned();
        let conversation = Conversation::new(
            session,
            self.home.clone(),
            Arc::clone(&self.models),
            Arc::clone(&self.mcp),
            self.hub.revision(),
            Arc::clone(&self.hub),
        );
        sessions.insert(id, Arc::clone(&conversation));
        conversation
    }

    fn publish_app_snapshot(&self) {
        let _publication = self.lock_app_publication();
        match self.bootstrap_with_catalog(self.models.redacted_catalog()) {
            Ok(payload) => {
                self.hub.emit(StreamEvent::AppChanged { payload });
            }
            Err(error) => {
                self.hub.emit(StreamEvent::AppReadFailed { error });
            }
        }
    }

    fn lock_app_publication(&self) -> std::sync::MutexGuard<'_, ()> {
        self.app_publication.lock().expect("app_server publication lock poisoned")
    }

    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, SessionCache> {
        self.sessions.lock().expect("app_server session map lock poisoned")
    }

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

pub(super) struct SessionDeleteGuard {
    app: Arc<AppServer>,
    session_id: Option<String>,
    conversation: Option<Arc<Conversation>>,
}

impl SessionDeleteGuard {
    pub(super) fn reserve(
        app: &Arc<AppServer>,
        sessions: &mut SessionCache,
        session_id: &str,
    ) -> Result<Self, RpcError> {
        if !sessions.deleting_sessions.insert(session_id.to_owned()) {
            return Err(session_busy());
        }
        let conversation = sessions.get(session_id);
        if let Some(conversation) = &conversation {
            conversation.mark_closing();
        }
        Ok(Self {
            app: Arc::clone(app),
            session_id: Some(session_id.to_owned()),
            conversation,
        })
    }

    pub(super) fn conversation(&self) -> Option<&Arc<Conversation>> {
        self.conversation.as_ref()
    }

    pub(super) fn complete_locked(mut self, sessions: &mut SessionCache) {
        if let Some(session_id) = self.session_id.take() {
            self.conversation.take();
            sessions.remove(&session_id);
            sessions.deleting_sessions.remove(&session_id);
        }
    }
}

impl Drop for SessionDeleteGuard {
    fn drop(&mut self) {
        if let Some(session_id) = self.session_id.take() {
            let mut sessions = self.app.lock_sessions();
            if let Some(conversation) = self.conversation.take() {
                conversation.clear_closing();
            }
            sessions.deleting_sessions.remove(&session_id);
        }
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
