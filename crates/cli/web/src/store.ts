import { appendEvent } from './eventLog'
import { eventTurnId } from './protocol'
import { loadPersisted, persistView, normalizeMessageFontSize, clampSidebarWidth, storageKey, draftStoragePrefix, type PersistedView, type WorkspaceAppearance } from './viewPersistence'
export type { WorkspaceAppearance } from './viewPersistence'
import { useRef, useSyncExternalStore } from 'react'
import { RpcFailure, WorkbenchConnection } from './connection'
import type {
  ActionReceipt,
  ConnectionStatus,
  DeliveryIntent,
  DirectoryEntry,
  DiscoveredModel,
  ProviderConfigurationInput,
  RedactedModelCatalog,
  SessionPhase,
  SessionReadResult,
  SessionSnapshot,
  SettingsUpdateResult,
  StreamEnvelope,
  ThreadSummary,
  TurnEventEnvelope,
  ViewportAnchor,
  WorkbenchBootstrap,
  Workspace,
} from './protocol'

export interface ActionError {
  origin: string
  code: string
  message: string
  recovery: string
}

export interface DirectoryPickerState {
  open: boolean
  path: string | null
  entries: DirectoryEntry[]
  loading: boolean
  error: ActionError | null
}

export interface SessionLoadState {
  workspaceId: string | null
  sessionId: string | null
  status: 'idle' | 'loading' | 'error'
  error: ActionError | null
}

export interface LiveSessionState {
  sessionRevision: number
  phase: SessionPhase
  terminal: SessionSnapshot['terminal']
}


export interface WorkbenchState extends PersistedView {
  connection: ConnectionStatus
  bootstrap: WorkbenchBootstrap | null
  session: SessionReadResult | null
  sessionLoad: SessionLoadState
  liveSessions: Record<string, LiveSessionState>
  unreadSessions: ReadonlySet<string>
  pendingActions: ReadonlySet<string>
  actionErrors: Readonly<Record<string, ActionError>>
  actionError: ActionError | null
  settingsOpen: boolean
  directoryPicker: DirectoryPickerState
  fileCandidates: Array<{ path: string; kind: string }>
  fileCandidateStatus: 'idle' | 'loading' | 'empty' | 'ready' | 'error'
  fileCandidateError: ActionError | null
  fileCandidateQuery: string
}

const defaultAnchor = (): ViewportAnchor => ({
  mode: 'following',
  anchorItemId: null,
  offset: 0,
})

class WorkbenchStore {
  private state: WorkbenchState = {
    ...loadPersisted(),
    connection: 'connecting',
    bootstrap: null,
    session: null,
    sessionLoad: { workspaceId: null, sessionId: null, status: 'idle', error: null },
    liveSessions: {},
    unreadSessions: new Set(),
    pendingActions: new Set(),
    actionErrors: {},
    actionError: null,
    settingsOpen: false,
    directoryPicker: { open: false, path: null, entries: [], loading: false, error: null },
    fileCandidates: [],
    fileCandidateStatus: 'idle',
    fileCandidateError: null,
    fileCandidateQuery: '',
  }
  private readonly listeners = new Set<() => void>()
  private readonly connection = new WorkbenchConnection(
    (frame) => this.onFrame(frame),
    (connection) => this.patch({ connection }, false),
  )
  private started = false
  private generation: string | null = null
  private revision = 0
  private queuedFrames: StreamEnvelope[] = []
  private resyncing: Promise<void> | null = null
  private sessionReadRequest = 0
  private fileSearchRequest = 0
  private directoryRequest = 0

  readonly subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener)
    return () => this.listeners.delete(listener)
  }

  readonly getSnapshot = (): WorkbenchState => this.state

  readonly onStorage = (event: StorageEvent): void => {
    if (event.key?.startsWith(draftStoragePrefix)) {
      this.patch({ drafts: { ...this.state.drafts, [event.key.slice(draftStoragePrefix.length)]: event.newValue ?? '' } }, false)
      return
    }
    if (event.key !== storageKey || event.newValue === null) return
    const persisted = loadPersisted()
    this.patch({
      viewportAnchors: persisted.viewportAnchors,
      sidebarView: persisted.sidebarView,
    }, false)
  }

  start(): void {
    if (this.started) return
    this.started = true
    window.addEventListener('storage', this.onStorage)
    this.connection.start()
  }

  stop(): void {
    if (!this.started) return
    this.started = false
    window.removeEventListener('storage', this.onStorage)
    this.connection.stop()
  }

  retrySession(): void {
    const workspaceId = this.state.selectedWorkspaceId
    const sessionId = this.state.selectedSessionId
    if (sessionId !== null) void this.readSession(workspaceId, sessionId)
  }

  selectWorkspace(workspaceId: string): void {
    const first = this.state.bootstrap?.sessionsByWorkspace[workspaceId]?.[0]?.threadId ?? null
    this.cancelCandidates()
    this.patch({
      selectedWorkspaceId: workspaceId,
      selectedSessionId: first,
      session: null,
      sessionLoad: first === null
        ? { workspaceId, sessionId: null, status: 'idle', error: null }
        : { workspaceId, sessionId: first, status: 'loading', error: null },
    })
    if (first !== null) void this.readSession(workspaceId, first)
  }

  async selectSession(sessionId: string): Promise<void> {
    const workspaceId = this.workspaceForSession(sessionId)
    if (workspaceId === undefined) return
    if (sessionId === this.state.selectedSessionId) {
      if (this.state.session === null) await this.readSession(workspaceId, sessionId)
      return
    }
    this.cancelCandidates()
    this.patch({
      selectedWorkspaceId: workspaceId,
      selectedSessionId: sessionId,
      session: null,
      sessionLoad: { workspaceId, sessionId, status: 'loading', error: null },
    })
    await this.readSession(workspaceId, sessionId)
  }

  async createSession(workspaceId = this.state.selectedWorkspaceId, transferDraft = false): Promise<boolean> {
    if (workspaceId === null) { this.openDirectoryPicker(); return false }
    if (this.isPending('session.create', `workspace:${workspaceId}`)) return false
    const sourceKey = this.draftKey()
    const sourceDraft = transferDraft ? this.draft() : ''
    const blank = this.sessions(workspaceId).find((session) =>
      session.turnCount === 0 && session.status !== 'running'
      && (this.state.liveSessions[session.threadId]?.phase ?? 'idle') === 'idle'
      && (sourceDraft === '' || sourceKey === session.threadId || (this.state.drafts[session.threadId] ?? '') === ''))
    if (blank !== undefined) {
      const selecting = this.selectSession(blank.threadId)
      if (sourceDraft !== '' && sourceKey !== blank.threadId) {
        this.setDraftFor(blank.threadId, sourceDraft)
        this.setDraftFor(sourceKey, '')
      }
      await selecting
      return this.state.selectedSessionId === blank.threadId && this.state.session !== null
    }
    // Switch the editable surface immediately: keystrokes during creation belong to the new task.
    this.cancelCandidates()
    this.patch({ selectedWorkspaceId: workspaceId, selectedSessionId: null, session: null,
      sessionLoad: { workspaceId, sessionId: null, status: 'loading', error: null } })
    const newDraftKey = this.draftKey()
    if (sourceDraft !== '' && sourceKey !== newDraftKey) {
      this.setDraftFor(newDraftKey, sourceDraft)
      this.setDraftFor(sourceKey, '')
    }
    let activated = false
    const accepted = await this.action('session.create', `workspace:${workspaceId}`, async () => {
      const session = await this.connection.rpc<SessionReadResult>('session.create', {
        workspaceId,
        settings: null,
      })
      if (this.state.selectedWorkspaceId !== workspaceId || this.state.selectedSessionId !== null) {
        await this.refreshBootstrap()
        return
      }
      const newDraft = this.state.drafts[newDraftKey] ?? ''
      this.patch({
        selectedWorkspaceId: workspaceId,
        selectedSessionId: session.summary.threadId,
        session,
        sessionLoad: { workspaceId, sessionId: session.summary.threadId, status: 'idle', error: null },
      })
      if (newDraft !== '') {
        this.setDraftFor(session.summary.threadId, newDraft)
        this.setDraftFor(newDraftKey, '')
      }
      activated = true
      this.updateLiveSession(session.summary.threadId, session.runtime)
      await this.refreshBootstrap()
    })
    return accepted && activated
  }

  async readOlder(): Promise<boolean> {
    const { selectedWorkspaceId, selectedSessionId, session } = this.state
    const beforeTurn = session?.history.nextCursor
    if (selectedSessionId === null || beforeTurn == null) return false
    return this.action('history.older', `session:${selectedSessionId}`, async () => {
      const older = await this.connection.rpc<SessionReadResult>('session.read', {
        workspaceId: selectedWorkspaceId,
        sessionId: selectedSessionId,
        beforeTurn,
        limit: 40,
      })
      if (this.state.selectedSessionId !== selectedSessionId || this.state.session === null) return
      this.patch({
        session: {
          ...this.state.session,
          summary: older.summary,
          history: {
            ...this.state.session.history,
            turns: [...older.history.turns, ...this.state.session.history.turns],
            nextCursor: older.history.nextCursor,
          },
        },
      }, false)
    })
  }

  setDraft(text: string): void {
    this.setDraftFor(this.draftKey(), text)
  }

  draft(): string {
    return this.state.drafts[this.draftKey()] ?? ''
  }

  async submitDraft(intent: DeliveryIntent = 'follow_up'): Promise<boolean> {
    if (this.state.connection !== 'ready' || this.draft().trim() === '') return false
    if (this.state.selectedSessionId === null) {
      if (!await this.createSession(this.state.selectedWorkspaceId, true)) return false
    }
    const { selectedWorkspaceId: workspaceId, selectedSessionId: sessionId, session } = this.state
    const draftKey = this.draftKey()
    const text = this.state.drafts[draftKey] ?? ''
    if (sessionId === null || session === null || this.state.connection !== 'ready' || text.trim() === '') return false
    const phase = session?.runtime.phase ?? this.state.liveSessions[sessionId]?.phase ?? 'idle'
    if (phase === 'compacting' || phase === 'stopping' || phase === 'reserved') return false
    const method = phase === 'running'
      ? intent === 'steer' ? 'session.steer' : 'session.followUp'
      : 'session.submit'
    return this.action(method, `session:${sessionId}`, async () => {
      await this.connection.rpc<ActionReceipt>(method, { workspaceId, sessionId, text })
      if ((this.state.drafts[draftKey] ?? '') === text) this.setDraftFor(draftKey, '')
    }, { key: draftKey, text })
  }

  async stopActive(): Promise<boolean> {
    return this.sessionAction('session.abort')
  }

  async compact(): Promise<boolean> {
    return this.sessionAction('session.compact')
  }

  async withdraw(controlId: string): Promise<boolean> {
    return this.sessionAction('session.queueWithdraw', { controlId }, undefined, controlId)
  }

  async replace(controlId: string, text: string): Promise<boolean> {
    return this.sessionAction('session.queueReplace', { controlId, text }, undefined, controlId)
  }

  async sendQueuedNow(): Promise<boolean> {
    const { selectedWorkspaceId: workspaceId, selectedSessionId: sessionId, session } = this.state
    if (sessionId === null || session === null) return false
    for (const control of session.runtime.pendingControls) {
      if (control.channel !== 'follow_up') continue
      const accepted = await this.action('session.queueSendNow', `control:${sessionId}:${control.controlId}`, async () => {
        await this.connection.rpc('session.queueSendNow', { workspaceId, sessionId, controlId: control.controlId })
      })
      if (!accepted) return false
    }
    return true
  }

  async sendNow(controlId: string): Promise<boolean> {
    return this.sessionAction('session.queueSendNow', { controlId }, undefined, controlId)
  }

  async renameWorkspace(workspaceId: string, name: string): Promise<boolean> {
    return this.action('workspace.rename', `workspace:${workspaceId}`, async () => {
      await this.connection.rpc('workspace.rename', { workspaceId, name })
      await this.refreshBootstrap()
    })
  }

  sessions(workspaceId = this.state.selectedWorkspaceId): ThreadSummary[] {
    return workspaceId === null ? [] : this.state.bootstrap?.sessionsByWorkspace[workspaceId] ?? []
  }

  private workspaceForSession(sessionId: string): string | null | undefined {
    return Object.entries(this.state.bootstrap?.sessionsByWorkspace ?? {}).find(([, sessions]) => sessions.some(session => session.threadId === sessionId))?.[0]
  }

  async renameSession(sessionId: string, name: string): Promise<boolean> {
    const workspaceId = this.workspaceForSession(sessionId)
    if (workspaceId === undefined || name.trim() === '') return false
    return this.action('session.rename', `session:${sessionId}`, async () => {
      await this.connection.rpc<ThreadSummary>('session.rename', { workspaceId, sessionId, name })
      await this.refreshBootstrap()
    })
  }

  async archiveSession(sessionId: string): Promise<boolean> {
    const workspaceId = this.workspaceForSession(sessionId)
    if (workspaceId === undefined) return false
    return this.action('session.archive', `session:${sessionId}`, async () => {
      await this.connection.rpc('session.archive', { workspaceId, sessionId })
      if (this.state.selectedSessionId === sessionId) {
        this.patch({ selectedSessionId: null, session: null })
      }
      const liveSessions = { ...this.state.liveSessions }
      delete liveSessions[sessionId]
      this.patch({ liveSessions }, false)
      await this.refreshBootstrap()
    })
  }

  async updateSettings(selector: string): Promise<boolean> {
    const workspaceId = this.state.selectedWorkspaceId
    if (this.state.selectedSessionId === null && !await this.createSession(workspaceId, true)) return false
    const sessionId = this.state.selectedSessionId
    if (sessionId === null) return false
    return this.action('session.updateSettings', `session:${sessionId}`, async () => {
      const result = await this.connection.rpc<SettingsUpdateResult>('session.updateSettings', {
        workspaceId,
        sessionId,
        selector,
      })
      if (this.state.selectedSessionId === sessionId && this.state.session !== null) {
        this.patch({
          session: {
            ...this.state.session,
            runtime: { ...this.state.session.runtime, selector: result.selector },
          },
        }, false)
      }
    })
  }

  async addWorkspace(root: string): Promise<boolean> {
    return this.action('workspace.add', `directory:${root}`, async () => {
      const workspace = await this.connection.rpc<Workspace>('workspace.add', { root })
      await this.refreshBootstrap()
      await this.createSession(workspace.workspaceId, true)
      this.closeDirectoryPicker()
    })
  }

  async removeWorkspace(workspaceId: string): Promise<boolean> {
    const sessions = this.state.bootstrap?.sessionsByWorkspace[workspaceId] ?? []
    const hasDraft = this.state.drafts[`new:${workspaceId}`]?.trim()
      || sessions.some((session) => this.state.drafts[session.threadId]?.trim())
    if (hasDraft) {
      this.reportError(new RpcFailure(
        'draft_present',
        '这个项目中还有未提交的草稿。',
        '请先发送或清空草稿，再移除项目。',
      ), `workspace:${workspaceId}`)
      return false
    }
    return this.action('workspace.remove', `workspace:${workspaceId}`, async () => {
      await this.connection.rpc('workspace.remove', { workspaceId })
      const workspaceAppearance = { ...this.state.workspaceAppearance }
      delete workspaceAppearance[workspaceId]
      this.patch({ workspaceAppearance })
      if (this.state.selectedWorkspaceId === workspaceId) {
        this.patch({ selectedWorkspaceId: null, selectedSessionId: null, session: null })
      }
      await this.refreshBootstrap()
    })
  }

  async saveProvider(provider: ProviderConfigurationInput): Promise<boolean> {
    return this.action('model.saveProvider', `provider:${provider.providerId}`, async () => {
      const modelCatalog = await this.connection.rpc<RedactedModelCatalog>('model.saveProvider', { provider })
      if (this.state.bootstrap !== null) {
        this.patch({ bootstrap: { ...this.state.bootstrap, modelCatalog } }, false)
      }
      await this.refreshBootstrap()
    })
  }

  async setApiKey(providerId: string, apiKey: string): Promise<boolean> {
    return this.action('model.setApiKey', `provider-key:${providerId}`, async () => {
      await this.connection.rpc('model.setApiKey', { providerId, apiKey })
      await this.refreshBootstrap()
    })
  }

  async discoverModels(providerId: string, baseUrl: string, apiKey: string): Promise<DiscoveredModel[]> {
    return this.connection.rpc<DiscoveredModel[]>('model.discover', { providerId, baseUrl, apiKey: apiKey || null })
  }

  async removeProvider(providerId: string): Promise<boolean> {
    return this.action('model.removeProvider', `provider:${providerId}`, async () => {
      await this.connection.rpc('model.removeProvider', { providerId })
      await this.refreshBootstrap()
    })
  }

  async searchFiles(query: string): Promise<void> {
    const workspaceId = this.state.selectedWorkspaceId
    const sessionId = this.state.selectedSessionId
    const normalized = query.trim()
    const request = ++this.fileSearchRequest
    if ((workspaceId === null && sessionId === null) || normalized === '') {
      this.patch({
        fileCandidates: [],
        fileCandidateStatus: 'idle',
        fileCandidateError: null,
        fileCandidateQuery: normalized,
      }, false)
      return
    }
    this.patch({
      fileCandidates: [],
      fileCandidateStatus: 'loading',
      fileCandidateError: null,
      fileCandidateQuery: normalized,
    }, false)
    try {
      const fileCandidates = await this.connection.rpc<Array<{ path: string; kind: string }>>(
        'file.search',
        { workspaceId, sessionId, query: normalized, limit: 12 },
      )
      if (request !== this.fileSearchRequest
        || this.state.selectedWorkspaceId !== workspaceId
        || this.state.selectedSessionId !== sessionId
        || this.state.fileCandidateQuery !== normalized) return
      this.patch({
        fileCandidates,
        fileCandidateStatus: fileCandidates.length === 0 ? 'empty' : 'ready',
        fileCandidateError: null,
      }, false)
    } catch (error) {
      if (request !== this.fileSearchRequest) return
      this.patch({
        fileCandidates: [],
        fileCandidateStatus: 'error',
        fileCandidateError: this.toActionError(error, `file-search:${workspaceId}`),
      }, false)
    }
  }

  clearFileCandidates(): void {
    this.cancelCandidates()
  }

  openDirectoryPicker(): void {
    void this.action('directory.pick', 'directory:picker', async () => {
      const result = await this.connection.rpc<{ native: boolean; path: string | null }>('directory.pick', {})
      if (!result.native) { this.openDirectoryBrowser(); return }
      if (result.path !== null) await this.addWorkspace(result.path)
    })
  }

  private openDirectoryBrowser(): void {
    this.patch({ directoryPicker: { open: true, path: null, entries: [], loading: true, error: null } }, false)
    void this.browseDirectory(null)
  }

  closeDirectoryPicker(): void {
    this.directoryRequest += 1
    this.patch({ directoryPicker: { ...this.state.directoryPicker, open: false } }, false)
  }

  async browseDirectory(path: string | null): Promise<void> {
    const request = ++this.directoryRequest
    this.patch({ directoryPicker: { open: true, path, entries: [], loading: true, error: null } }, false)
    try {
      const entries = await this.connection.rpc<DirectoryEntry[]>('directory.list', { path })
      if (request !== this.directoryRequest || !this.state.directoryPicker.open) return
      this.patch({ directoryPicker: { open: true, path, entries, loading: false, error: null } }, false)
    } catch (error) {
      if (request !== this.directoryRequest || !this.state.directoryPicker.open) return
      this.patch({
        directoryPicker: {
          open: true,
          path,
          entries: [],
          loading: false,
          error: this.toActionError(error, `directory:${path ?? 'root'}`),
        },
      }, false)
    }
  }

  setSettingsOpen(settingsOpen: boolean): void {
    this.patch({ settingsOpen }, false)
  }

  setSidebarWidth(sidebarWidth: number): void {
    this.patch({ sidebarWidth: clampSidebarWidth(sidebarWidth) })
  }

  toggleSidebar(): void {
    this.patch({ sidebarCollapsed: !this.state.sidebarCollapsed })
  }

  viewportAnchor(): ViewportAnchor {
    const id = this.state.selectedSessionId
    return id === null ? defaultAnchor() : this.state.viewportAnchors[id] ?? defaultAnchor()
  }

  setViewportAnchor(anchor: ViewportAnchor): void {
    const id = this.state.selectedSessionId
    if (id === null) return
    const previous = this.state.viewportAnchors[id]
    if (previous?.mode === anchor.mode
      && previous.anchorItemId === anchor.anchorItemId
      && Math.abs(previous.offset - anchor.offset) < 1) return
    this.patch({ viewportAnchors: { ...this.state.viewportAnchors, [id]: anchor } })
  }

  isPending(method: string, origin?: string, target?: string): boolean {
    return this.state.pendingActions.has(this.mutationKey(method, origin, target))
  }

  clearError(origin?: string): void {
    if (origin === undefined) {
      this.patch({ actionErrors: {}, actionError: null }, false)
      return
    }
    const actionErrors = { ...this.state.actionErrors }
    delete actionErrors[origin]
    this.patch({
      actionErrors,
      actionError: this.state.actionError?.origin === origin ? null : this.state.actionError,
    }, false)
  }

  private draftKey(): string {
    return this.state.selectedSessionId ?? `new:${this.state.selectedWorkspaceId ?? 'none'}`
  }

  private setDraftFor(key: string, text: string): void {
    this.patch({ drafts: { ...this.state.drafts, [key]: text } })
    try {
      localStorage.setItem(draftStoragePrefix + key, text)
    } catch {
      this.reportError(new RpcFailure('storage', '草稿暂时只能保留在当前页面。', '请复制草稿后检查浏览器存储空间。'), `session:${key}`)
    }
  }

  private async sessionAction(
    method: string,
    extra: Record<string, unknown> = {},
    preservedDraft?: { key: string; text: string },
    target?: string,
  ): Promise<boolean> {
    const workspaceId = this.state.selectedWorkspaceId
    const sessionId = this.state.selectedSessionId
    if (sessionId === null) return false
    const origin = target === undefined ? `session:${sessionId}` : `control:${sessionId}:${target}`
    return this.action(method, origin, async () => {
      await this.connection.rpc<ActionReceipt>(method, { workspaceId, sessionId, ...extra })
    }, preservedDraft)
  }

  private async readSession(workspaceId: string | null, sessionId: string): Promise<void> {
    const request = ++this.sessionReadRequest
    this.patch({ sessionLoad: { workspaceId, sessionId, status: 'loading', error: null } }, false)
    try {
      const session = await this.connection.rpc<SessionReadResult>('session.read', {
        workspaceId,
        sessionId,
        beforeTurn: null,
        limit: 40,
      })
      if (request !== this.sessionReadRequest
        || this.state.selectedWorkspaceId !== workspaceId
        || this.state.selectedSessionId !== sessionId) return
      if (this.state.session !== null && session.runtime.sessionRevision < this.state.session.runtime.sessionRevision) {
        this.patch({ sessionLoad: { workspaceId, sessionId, status: 'idle', error: null } }, false)
        return
      }
      this.patch({
        session,
        sessionLoad: { workspaceId, sessionId, status: 'idle', error: null },
      }, false)
      this.updateLiveSession(sessionId, session.runtime)
    } catch (error) {
      if (request !== this.sessionReadRequest
        || this.state.selectedWorkspaceId !== workspaceId
        || this.state.selectedSessionId !== sessionId) return
      const actionError = this.toActionError(error, `session:${sessionId}`)
      this.patch({
        session: null,
        sessionLoad: { workspaceId, sessionId, status: 'error', error: actionError },
      }, false)
    } finally {
      if (request === this.sessionReadRequest && this.resyncing === null) this.flushFrames()
    }
  }

  private onFrame(frame: StreamEnvelope): void {
    if (frame.type === 'ready') {
      void this.resync()
      return
    }
    if (this.state.bootstrap === null || this.resyncing !== null || this.state.sessionLoad.status === 'loading') {
      this.queuedFrames.push(frame)
      return
    }
    this.applyFrame(frame)
  }

  private applyFrame(frame: StreamEnvelope): void {
    if (frame.generation !== this.generation) {
      void this.resync()
      return
    }
    if (frame.revision <= this.revision) return
    if (frame.revision !== this.revision + 1) {
      void this.resync()
      return
    }
    this.revision = frame.revision
    if (frame.type === 'workbench_changed') {
      this.updateBootstrap({ ...frame.payload as WorkbenchBootstrap, revision: frame.revision })
      return
    }
    if (frame.type === 'resync_required') {
      void this.resync()
      return
    }
    const sessionId = frame.sessionId
    if (sessionId !== undefined && frame.type === 'session_changed') {
      const runtime = frame.payload as SessionSnapshot
      if (!this.updateLiveSession(sessionId, runtime)) return
      if (sessionId !== this.state.selectedSessionId || this.state.session === null) return
      if (runtime.sessionRevision <= this.state.session.runtime.sessionRevision) return
      this.patch({
        session: { ...this.state.session, runtime },
      }, false)
      return
    }
    if (sessionId !== undefined && frame.type === 'turn_event') {
      const event = frame.payload as TurnEventEnvelope
      const previous = this.state.liveSessions[sessionId]
      const phase = previous?.phase === 'stopping' ? 'stopping' : 'running'
      if (!this.updateLiveSession(sessionId, {
        sessionRevision: event.sessionRevision,
        phase,
        terminal: previous?.terminal ?? null,
      })) return
      if (sessionId !== this.state.selectedSessionId || this.state.session === null) return
      const runtime = this.state.session.runtime
      if (event.sessionRevision <= runtime.sessionRevision) return
      const turnId = eventTurnId(event)
      const active = runtime.activeTurn ?? {
        turnId,
        events: [],
        startedAt: new Date().toISOString(),
      }
      this.patch({
        session: {
          ...this.state.session,
          runtime: {
            ...runtime,
            sessionRevision: event.sessionRevision,
            phase,
            activeTurn: {
              ...active,
              turnId: event.method === 'turn/started' ? turnId : active.turnId,
              startedAt: event.method === 'turn/started' && typeof event.params.startedAt === 'string' ? event.params.startedAt : active.startedAt,
              events: appendEvent(active.events, event),
            },
          },
        },
      }, false)
      return
    }
    if (sessionId !== undefined && frame.type === 'session_settled') {
      const payload = frame.payload as { runtime?: SessionSnapshot }
      if (payload.runtime !== undefined && !this.updateLiveSession(sessionId, payload.runtime)) return
      if (sessionId === this.state.selectedSessionId) {
        void this.readSession(this.state.selectedWorkspaceId, sessionId)
      }
      void this.refreshBootstrap()
    }
  }

  private resync(): Promise<void> {
    if (this.resyncing !== null) return this.resyncing
    this.resyncing = (async () => {
      try {
        const bootstrap = await this.connection.rpc<WorkbenchBootstrap>('workbench.bootstrap', {})
        const hostChanged = this.generation !== bootstrap.generation
        this.generation = bootstrap.generation
        this.revision = bootstrap.revision
        let selectedWorkspaceId = this.state.selectedWorkspaceId
        if (!bootstrap.workspaces.some((workspace) => workspace.workspaceId === selectedWorkspaceId)) {
          selectedWorkspaceId = null
        }
        let selectedSessionId = this.state.selectedSessionId
        const sessions = selectedWorkspaceId === null ? [] : bootstrap.sessionsByWorkspace[selectedWorkspaceId] ?? []
        if (!sessions.some((session) => session.threadId === selectedSessionId)) {
          selectedSessionId = sessions[0]?.threadId ?? null
        }
        this.patch({
          bootstrap,
          session: hostChanged ? null : this.state.session,
          liveSessions: Object.fromEntries(Object.entries(bootstrap.sessionPhases).map(([id, phase]) => [id, { sessionRevision: 0, phase, terminal: null }])),
          selectedWorkspaceId,
          selectedSessionId,
          connection: 'ready',
        })
        if (selectedSessionId !== null) {
          await this.readSession(selectedWorkspaceId, selectedSessionId)
        } else {
          this.patch({
            session: null,
            sessionLoad: { workspaceId: selectedWorkspaceId, sessionId: null, status: 'idle', error: null },
          }, false)
        }
      } catch (error) {
        if (error instanceof RpcFailure && error.code === 'unauthorized') {
          this.patch({ connection: 'unauthorized' }, false)
        } else {
          this.patch({ connection: 'unavailable' }, false)
        }
        this.reportError(error, 'connection')
      } finally {
        this.resyncing = null
        this.flushFrames()
      }
    })()
    return this.resyncing
  }

  private flushFrames(): void {
    const queued = this.queuedFrames
    this.queuedFrames = []
    for (const frame of queued) {
      if (frame.generation === this.generation && frame.revision > this.revision) this.onFrame(frame)
    }
  }

  private async refreshBootstrap(): Promise<void> {
    try {
      const bootstrap = await this.connection.rpc<WorkbenchBootstrap>('workbench.bootstrap', {})
      if (bootstrap.generation !== this.generation) {
        await this.resync()
        return
      }
      this.updateBootstrap(bootstrap)
    } catch (error) {
      this.reportError(error, 'workbench')
    }
  }

  private async action(
    method: string,
    origin: string,
    operation: () => Promise<void>,
    preservedDraft?: { key: string; text: string },
    target?: string,
  ): Promise<boolean> {
    const key = this.mutationKey(method, origin, target)
    if (this.state.pendingActions.has(key)) return false
    const pendingActions = new Set(this.state.pendingActions)
    pendingActions.add(key)
    const actionErrors = { ...this.state.actionErrors }
    delete actionErrors[origin]
    this.patch({ pendingActions, actionErrors, actionError: null }, false)
    try {
      await operation()
      return true
    } catch (error) {
      this.reportError(error, origin, preservedDraft)
      return false
    } finally {
      const next = new Set(this.state.pendingActions)
      next.delete(key)
      this.patch({ pendingActions: next }, false)
    }
  }

  private reportError(error: unknown, origin: string, preservedDraft?: { key: string; text: string }): void {
    const actionError = this.toActionError(error, origin)
    if (preservedDraft !== undefined) {
      const text = error instanceof RpcFailure ? error.preservedInput ?? preservedDraft.text : preservedDraft.text
      if (text !== '' && (this.state.drafts[preservedDraft.key] ?? '') === '') {
        this.setDraftFor(preservedDraft.key, text)
      }
    }
    if (actionError.code === 'unavailable') return
    this.patch({
      actionErrors: { ...this.state.actionErrors, [origin]: actionError },
      actionError,
    }, false)
  }

  private toActionError(error: unknown, origin: string): ActionError {
    if (error instanceof RpcFailure) {
      return { origin, code: error.code, message: error.message, recovery: error.recovery }
    }
    return {
      origin,
      code: 'internal',
      message: error instanceof Error ? error.message : '发生了未知错误。',
      recovery: '请刷新页面后重试。',
    }
  }


  private updateBootstrap(bootstrap: WorkbenchBootstrap): void {
    if (this.state.bootstrap !== null && bootstrap.revision < this.state.bootstrap.revision) return
    this.patch({ bootstrap }, false)
  }

  private updateLiveSession(sessionId: string, runtime: LiveSessionState): boolean {
    const previous = this.state.liveSessions[sessionId]
    if (previous !== undefined && runtime.sessionRevision <= previous.sessionRevision) return false
    this.patch({
      liveSessions: {
        ...this.state.liveSessions,
        [sessionId]: { sessionRevision: runtime.sessionRevision, phase: runtime.phase, terminal: runtime.terminal },
      },
    }, false)
    return true
  }


  private mutationKey(method: string, origin?: string, target?: string): string {
    return [method, origin, target].filter((value) => value !== undefined && value !== '').join(':')
  }

  private cancelCandidates(): void {
    this.fileSearchRequest += 1
    this.patch({
      fileCandidates: [],
      fileCandidateStatus: 'idle',
      fileCandidateError: null,
      fileCandidateQuery: '',
    }, false)
  }

  private patch(patch: Partial<WorkbenchState>, persist = true): void {
    if (patch.liveSessions !== undefined || patch.selectedSessionId !== undefined) {
      const unreadSessions = new Set(this.state.unreadSessions)
      const selected = patch.selectedSessionId === undefined ? this.state.selectedSessionId : patch.selectedSessionId
      if (patch.liveSessions !== undefined) {
        for (const [id, runtime] of Object.entries(patch.liveSessions)) {
          const previous = this.state.liveSessions[id]
          if (runtime.phase !== 'idle') unreadSessions.delete(id)
          else if (previous !== undefined && previous.phase !== 'idle' && id !== selected) unreadSessions.add(id)
        }
        for (const id of unreadSessions) if (patch.liveSessions[id] === undefined) unreadSessions.delete(id)
      }
      if (selected !== null) unreadSessions.delete(selected)
      const unchanged = unreadSessions.size === this.state.unreadSessions.size && [...unreadSessions].every(id => this.state.unreadSessions.has(id))
      patch = { ...patch, unreadSessions: unchanged ? this.state.unreadSessions : unreadSessions }
    }
    this.state = { ...this.state, ...patch }
    if (persist) {
      try { persistView(this.state) } catch { /* View preferences must not block editing or runtime updates. */ }
    }
    for (const listener of this.listeners) listener()
  }

  setSidebarView(value: Partial<PersistedView['sidebarView']>): void {
    this.patch({ sidebarView: { ...this.state.sidebarView, ...value } })
  }

  setTrajectoryOpen(trajectoryOpen: boolean): void {
    this.patch({ trajectoryOpen })
  }

  setWorkspaceAppearance(workspaceId: string, appearance: WorkspaceAppearance): void {
    this.patch({ workspaceAppearance: { ...this.state.workspaceAppearance, [workspaceId]: appearance } })
  }

  setMessageFontSize(value: number): void {
    this.patch({ messageFontSize: normalizeMessageFontSize(value) })
  }

  setTheme(theme: PersistedView['theme']): void {
    this.patch({ theme })
  }
}


export const workbenchStore = new WorkbenchStore()

/** Subscribe to the fields consumed by one view; stream watermarks do not redraw session lists. */
export function sameWorkbenchFields(previous: WorkbenchState, next: WorkbenchState, fields: readonly (keyof WorkbenchState)[]): boolean {
  return fields.every(key => {
    if (key !== 'liveSessions') return Object.is(previous[key], next[key])
    const left = previous.liveSessions, right = next.liveSessions
    return Object.keys(left).length === Object.keys(right).length && Object.entries(left).every(([id, value]) =>
      value.phase === right[id]?.phase && value.terminal?.status === right[id]?.terminal?.status && value.terminal?.message === right[id]?.terminal?.message)
  })
}

export function useWorkbenchStore(fields: readonly (keyof WorkbenchState)[]): WorkbenchState {
  const cached = useRef<WorkbenchState | null>(null)
  const snapshot = () => {
    const next = workbenchStore.getSnapshot()
    if (cached.current === null || !sameWorkbenchFields(cached.current, next, fields)) cached.current = next
    return cached.current
  }
  return useSyncExternalStore(workbenchStore.subscribe, snapshot)
}
