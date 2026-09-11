import { initialSyncState, acceptBootstrap, acceptLiveSession, acceptSessionRead, resetBaseline, reduceStream, type SyncState, type LiveSessionState } from './sync'
export type { LiveSessionState } from './sync'
import { loadPersisted, persistView, normalizeMessageFontSize, clampSidebarWidth, storageKey, draftStoragePrefix, type PersistedView, type WorkspaceAppearance } from './viewPersistence'
export type { WorkspaceAppearance } from './viewPersistence'
import { useRef, useSyncExternalStore } from 'react'
import { RpcFailure, WorkbenchConnection, type WorkbenchTransport, type StreamListener, type StatusListener } from './connection'
import type {
  ActionReceipt,
  ConnectionStatus,
  DeliveryIntent,
  DirectoryEntry,
  FileCandidate,
  DiscoveredModel,
  ProviderConfigurationInput,
  RedactedModelCatalog,
  SessionPhase,
  SessionReadResult,
  SessionSnapshot,
  StreamEnvelope,
  ThreadReadPage,
  ThreadSummary,
  TurnEventEnvelope,
  ViewportAnchor,
  WorkbenchBootstrap,
  Workspace,
} from './protocol'

const SESSION_PAGE_SIZE = 40

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

export interface WorkbenchState extends PersistedView, SyncState {
  connection: ConnectionStatus
  sessionLoad: SessionLoadState
  unreadSessions: ReadonlySet<string>
  pendingActions: ReadonlySet<string>
  actionErrors: Readonly<Record<string, ActionError>>
  actionError: ActionError | null
  settingsOpen: boolean
  directoryPicker: DirectoryPickerState
  fileCandidates: FileCandidate[]
  fileCandidateStatus: 'idle' | 'loading' | 'empty' | 'ready' | 'error'
  fileCandidateError: ActionError | null
  fileCandidateQuery: string
}

const defaultAnchor = (): ViewportAnchor => ({
  mode: 'following',
  anchorItemId: null,
  offset: 0,
})

export interface StoreDependencies {
  createTransport: (onFrame: StreamListener, onStatus: StatusListener) => WorkbenchTransport
}

export class WorkbenchStore {
  private state: WorkbenchState = {
    ...loadPersisted(),
    ...initialSyncState(),
    connection: 'connecting',
    sessionLoad: { workspaceId: null, sessionId: null, status: 'idle', error: null },
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
  private readonly connection: WorkbenchTransport
  private started = false
  private queuedFrames: StreamEnvelope[] = []

  constructor(dependencies: StoreDependencies = { createTransport: (onFrame, onStatus) => new WorkbenchConnection(onFrame, onStatus) }) {
    this.connection = dependencies.createTransport(frame => this.onFrame(frame), connection => this.patch({ connection }))
  }

  private resyncing: Promise<void> | null = null
  private sessionReadRequest = 0
  private fileSearchRequest = 0
  private directoryRequest = 0
  private createdIdentity: { sessionId: string; generation: string | null } | null = null

  readonly subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener)
    return () => this.listeners.delete(listener)
  }

  readonly getSnapshot = (): WorkbenchState => this.state

  readonly onStorage = (event: StorageEvent): void => {
    if (event.key?.startsWith(draftStoragePrefix)) {
      this.patch({ drafts: { ...this.state.drafts, [event.key.slice(draftStoragePrefix.length)]: event.newValue ?? '' } })
      return
    }
    if (event.key !== storageKey || event.newValue === null) return
    const persisted = loadPersisted()
    this.patch({
      theme: persisted.theme,
      messageFontSize: persisted.messageFontSize,
      sidebarWidth: persisted.sidebarWidth,
      sidebarCollapsed: persisted.sidebarCollapsed,
      trajectoryOpen: persisted.trajectoryOpen,
      workspaceAppearance: persisted.workspaceAppearance,
      viewportAnchors: persisted.viewportAnchors,
      sidebarView: persisted.sidebarView,
    })
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

  async retrySession(): Promise<void> {
    const workspaceId = this.state.selectedWorkspaceId
    const sessionId = this.state.selectedSessionId
    if (sessionId !== null) await this.readSession(workspaceId, sessionId)
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
    this.saveSelection()
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
    this.saveSelection()
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
    this.saveSelection()
    const newDraftKey = this.draftKey()
    if (sourceDraft !== '' && sourceKey !== newDraftKey) {
      this.setDraftFor(newDraftKey, sourceDraft)
      this.setDraftFor(sourceKey, '')
    }
    let createdSessionId: string | null = null
    const accepted = await this.action('session.create', `workspace:${workspaceId}`, async () => {
      const session = await this.connection.rpc('session.create', {
        workspaceId,
        settings: null,
      })
      if (this.state.selectedWorkspaceId !== workspaceId || this.state.selectedSessionId !== null) {
        return
      }
      const newDraft = this.state.drafts[newDraftKey] ?? ''
      // Workbench events were emitted before the RPC returned, but may still be buffered by
      // this loading surface. Protect the returned identity until its catalog frame arrives.
      this.createdIdentity = { sessionId: session.summary.threadId, generation: this.state.generation }
      this.patch({
        selectedWorkspaceId: workspaceId,
        selectedSessionId: session.summary.threadId,
        session,
        sessionLoad: { workspaceId, sessionId: session.summary.threadId, status: 'idle', error: null },
      })
      this.saveSelection()
      if (newDraft !== '') {
        this.setDraftFor(session.summary.threadId, newDraft)
        this.setDraftFor(newDraftKey, '')
      }
      createdSessionId = session.summary.threadId
      this.updateLiveSession(session.summary.threadId, session.runtime)
    })
    if (createdSessionId === null && this.state.selectedWorkspaceId === workspaceId && this.state.selectedSessionId === null) {
      this.patch({ sessionLoad: { workspaceId, sessionId: null, status: 'idle', error: null } })
    }
    if (this.resyncing === null) this.flushFrames()
    return accepted && createdSessionId !== null
      && this.state.selectedWorkspaceId === workspaceId && this.state.selectedSessionId === createdSessionId
  }

  async readOlder(): Promise<boolean> {
    const { selectedWorkspaceId, selectedSessionId, session } = this.state
    const beforeTurn = session?.history.nextCursor
    const generation = this.state.generation
    if (selectedWorkspaceId === null || selectedSessionId === null || beforeTurn == null) return false
    return this.action('history.older', `session:${selectedSessionId}`, async () => {
      const older = await this.connection.rpc('session.read', {
        workspaceId: selectedWorkspaceId,
        sessionId: selectedSessionId,
        beforeTurn,
        limit: SESSION_PAGE_SIZE,
      })
      if (this.state.generation !== generation
        || this.state.selectedWorkspaceId !== selectedWorkspaceId
        || this.state.selectedSessionId !== selectedSessionId
        || this.state.session?.history.nextCursor !== beforeTurn) return
      this.patch({
        session: {
          ...this.state.session,
          history: {
            ...this.state.session.history,
            turns: [...older.history.turns, ...this.state.session.history.turns],
            nextCursor: older.history.nextCursor,
          },
        },
      })
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
    if (workspaceId === null || sessionId === null || session === null || this.state.connection !== 'ready' || text.trim() === '') return false
    const phase = session?.runtime.phase ?? this.state.liveSessions[sessionId]?.phase ?? 'idle'
    if (phase === 'compacting' || phase === 'stopping' || phase === 'reserved') return false
    const method = phase === 'running'
      ? intent === 'steer' ? 'session.steer' : 'session.followUp'
      : 'session.submit'
    return this.action(method, `session:${sessionId}`, async () => {
      await this.connection.rpc(method, { workspaceId, sessionId, text })
      if ((this.state.drafts[draftKey] ?? '') === text) this.setDraftFor(draftKey, '')
    }, { key: draftKey, text })
  }

  async stopActive(): Promise<boolean> {
    return this.sessionAction('session.abort', ids => this.connection.rpc('session.abort', ids))
  }

  async compact(): Promise<boolean> {
    return this.sessionAction('session.compact', ids => this.connection.rpc('session.compact', ids))
  }

  async withdraw(controlId: string): Promise<boolean> {
    return this.sessionAction('session.queueWithdraw', ids => this.connection.rpc('session.queueWithdraw', { ...ids, controlId }), controlId)
  }

  async replace(controlId: string, text: string): Promise<boolean> {
    return this.sessionAction('session.queueReplace', ids => this.connection.rpc('session.queueReplace', { ...ids, controlId, text }), controlId)
  }

  async sendQueuedNow(): Promise<boolean> {
    const { selectedWorkspaceId: workspaceId, selectedSessionId: sessionId, session } = this.state
    if (workspaceId === null || sessionId === null || session === null) return false
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
    return this.sessionAction('session.queueSendNow', ids => this.connection.rpc('session.queueSendNow', { ...ids, controlId }), controlId)
  }

  async renameWorkspace(workspaceId: string, name: string): Promise<boolean> {
    return this.action('workspace.rename', `workspace:${workspaceId}`, async () => {
      await this.connection.rpc('workspace.rename', { workspaceId, name })
    })
  }

  sessions(workspaceId = this.state.selectedWorkspaceId): ThreadSummary[] {
    return workspaceId === null ? [] : this.state.bootstrap?.sessionsByWorkspace[workspaceId] ?? []
  }

  private workspaceForSession(sessionId: string): string | undefined {
    return Object.entries(this.state.bootstrap?.sessionsByWorkspace ?? {}).find(([, sessions]) => sessions.some(session => session.threadId === sessionId))?.[0]
  }

  async renameSession(sessionId: string, name: string): Promise<boolean> {
    const workspaceId = this.workspaceForSession(sessionId)
    if (workspaceId === undefined || name.trim() === '') return false
    return this.action('session.rename', `session:${sessionId}`, async () => {
      await this.connection.rpc('session.rename', { workspaceId, sessionId, name })
    })
  }

  async archiveSession(sessionId: string): Promise<boolean> {
    const workspaceId = this.workspaceForSession(sessionId)
    if (workspaceId === undefined) return false
    return this.action('session.archive', `session:${sessionId}`, async () => {
      await this.connection.rpc('session.archive', { workspaceId, sessionId })
    })
  }

  async updateSettings(selector: string): Promise<boolean> {
    const workspaceId = this.state.selectedWorkspaceId
    if (this.state.selectedSessionId === null && !await this.createSession(workspaceId, true)) return false
    return this.sessionAction('session.updateSettings', ids => this.connection.rpc('session.updateSettings', { ...ids, selector }))
  }

  async addWorkspace(root: string): Promise<boolean> {
    return this.action('workspace.add', `directory:${root}`, async () => {
      const workspace = await this.connection.rpc('workspace.add', { root })
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
      const workspaceAppearance = { ...loadPersisted().workspaceAppearance }
      delete workspaceAppearance[workspaceId]
      this.saveView({ workspaceAppearance })
    })
  }

  async saveProvider(provider: ProviderConfigurationInput): Promise<boolean> {
    return this.action('model.saveProvider', `provider:${provider.providerId}`, async () => {
      const modelCatalog = await this.connection.rpc('model.saveProvider', { provider })
      if (this.state.bootstrap !== null) {
        this.patch({ bootstrap: { ...this.state.bootstrap, modelCatalog } })
      }
    })
  }

  async setApiKey(providerId: string, apiKey: string): Promise<boolean> {
    return this.action('model.setApiKey', `provider-key:${providerId}`, async () => {
      await this.connection.rpc('model.setApiKey', { providerId, apiKey })
    })
  }

  async discoverModels(providerId: string, baseUrl: string, apiKey: string): Promise<DiscoveredModel[]> {
    return this.connection.rpc('model.discover', { providerId, baseUrl, apiKey: apiKey || null })
  }

  async removeProvider(providerId: string): Promise<boolean> {
    return this.action('model.removeProvider', `provider:${providerId}`, async () => {
      await this.connection.rpc('model.removeProvider', { providerId })
    })
  }

  async searchFiles(query: string): Promise<void> {
    const workspaceId = this.state.selectedWorkspaceId
    const sessionId = this.state.selectedSessionId
    const normalized = query.trim()
    const request = ++this.fileSearchRequest
    if (workspaceId === null || normalized === '') {
      this.patch({
        fileCandidates: [],
        fileCandidateStatus: 'idle',
        fileCandidateError: null,
        fileCandidateQuery: normalized,
      })
      return
    }
    this.patch({
      fileCandidates: [],
      fileCandidateStatus: 'loading',
      fileCandidateError: null,
      fileCandidateQuery: normalized,
    })
    try {
      const fileCandidates = await this.connection.rpc(
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
      })
    } catch (error) {
      if (request !== this.fileSearchRequest) return
      this.patch({
        fileCandidates: [],
        fileCandidateStatus: 'error',
        fileCandidateError: this.toActionError(error, `file-search:${workspaceId}`),
      })
    }
  }

  clearFileCandidates(): void {
    this.cancelCandidates()
  }

  async loadRequest(requestId: string): Promise<import('./protocol').ModelRequestSnapshot> {
    const { selectedWorkspaceId: workspaceId, selectedSessionId: sessionId } = this.state
    if (workspaceId === null || sessionId === null) throw new RpcFailure('session_not_found', '没有选中的任务。', '请先打开任务。')
    return this.connection.rpc('session.request', { workspaceId, sessionId, requestId })
  }

  async listSkills(): Promise<import('./protocol').SkillCatalog> {
    const { selectedWorkspaceId: workspaceId, selectedSessionId: sessionId } = this.state
    if (workspaceId === null) return { skills: [], diagnostics: [] }
    return this.connection.rpc('skills.list', { workspaceId, sessionId })
  }

  openDirectoryPicker(): void {
    void this.action('directory.pick', 'directory:picker', async () => {
      const result = await this.connection.rpc('directory.pick', {})
      if (!result.native) { this.openDirectoryBrowser(); return }
      if (result.path !== null) await this.addWorkspace(result.path)
    })
  }

  private openDirectoryBrowser(): void {
    this.patch({ directoryPicker: { open: true, path: null, entries: [], loading: true, error: null } })
    void this.browseDirectory(null)
  }

  closeDirectoryPicker(): void {
    this.directoryRequest += 1
    this.patch({ directoryPicker: { ...this.state.directoryPicker, open: false } })
  }

  async browseDirectory(path: string | null): Promise<void> {
    const request = ++this.directoryRequest
    this.patch({ directoryPicker: { open: true, path, entries: [], loading: true, error: null } })
    try {
      const entries = await this.connection.rpc('directory.list', { path })
      if (request !== this.directoryRequest || !this.state.directoryPicker.open) return
      this.patch({ directoryPicker: { open: true, path, entries, loading: false, error: null } })
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
      })
    }
  }

  setSettingsOpen(settingsOpen: boolean): void {
    this.patch({ settingsOpen })
  }

  setSidebarWidth(sidebarWidth: number): void {
    this.saveView({ sidebarWidth: clampSidebarWidth(sidebarWidth) })
  }

  toggleSidebar(): void {
    this.saveView({ sidebarCollapsed: !this.state.sidebarCollapsed })
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
    this.saveView({ viewportAnchors: { ...loadPersisted().viewportAnchors, [id]: anchor } })
  }

  isPending(method: string, origin?: string, target?: string): boolean {
    return this.state.pendingActions.has(this.mutationKey(method, origin, target))
  }

  clearError(origin?: string): void {
    if (origin === undefined) {
      this.patch({ actionErrors: {}, actionError: null })
      return
    }
    const actionErrors = { ...this.state.actionErrors }
    delete actionErrors[origin]
    this.patch({
      actionErrors,
      actionError: this.state.actionError?.origin === origin ? null : this.state.actionError,
    })
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
    method: import('./protocol').RpcMethod,
    operation: (ids: import('./protocol').SessionParams) => Promise<ActionReceipt>,
    target?: string,
  ): Promise<boolean> {
    const workspaceId = this.state.selectedWorkspaceId
    const sessionId = this.state.selectedSessionId
    if (workspaceId === null || sessionId === null) return false
    const origin = target === undefined ? `session:${sessionId}` : `control:${sessionId}:${target}`
    return this.action(method, origin, async () => { await operation({ workspaceId, sessionId }) })
  }

  private async readSession(workspaceId: string | null, sessionId: string): Promise<void> {
    if (workspaceId === null) return
    const request = ++this.sessionReadRequest
    this.patch({ sessionLoad: { workspaceId, sessionId, status: 'loading', error: null } })
    try {
      const session = await this.connection.rpc('session.read', {
        workspaceId,
        sessionId,
        beforeTurn: null,
        limit: SESSION_PAGE_SIZE,
      })
      if (request !== this.sessionReadRequest
        || this.state.selectedWorkspaceId !== workspaceId
        || this.state.selectedSessionId !== sessionId) return
      this.applySync(acceptSessionRead(this.state, session))
      this.patch({ sessionLoad: { workspaceId, sessionId, status: 'idle', error: null } })
    } catch (error) {
      if (request !== this.sessionReadRequest
        || this.state.selectedWorkspaceId !== workspaceId
        || this.state.selectedSessionId !== sessionId) return
      const actionError = this.toActionError(error, `session:${sessionId}`)
      this.patch({
        session: null,
        sessionLoad: { workspaceId, sessionId, status: 'error', error: actionError },
      })
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
    const { state, effects } = reduceStream(this.state, this.state.selectedSessionId, frame, new Date().toISOString())
    this.applySync(state)
    for (const effect of effects) {
      if (effect === 'resync') void this.resync()
      else if (effect === 'refresh_bootstrap') void this.refreshBootstrap()
      else if (this.state.selectedSessionId !== null) void this.readSession(this.state.selectedWorkspaceId, this.state.selectedSessionId)
    }
  }

  private resync(): Promise<void> {
    if (this.resyncing !== null) return this.resyncing
    this.resyncing = (async () => {
      try {
        const bootstrap = await this.connection.rpc('workbench.bootstrap', {})
        // A resync baseline is authoritative even if a prior creation frame was lost.
        this.createdIdentity = null
        this.applySync(resetBaseline(this.state, bootstrap))
        const workspaceId = this.state.selectedWorkspaceId
        if (workspaceId !== null && this.state.selectedSessionId === null
          && !this.isPending('session.create', `workspace:${workspaceId}`)) {
          const first = bootstrap.sessionsByWorkspace[workspaceId]?.[0]?.threadId ?? null
          if (first !== null) {
            this.cancelCandidates()
            this.patch({ selectedSessionId: first, session: null })
            this.saveSelection()
          }
        }
        this.patch({ connection: 'ready' })
        const { selectedWorkspaceId, selectedSessionId } = this.state
        if (selectedSessionId !== null) {
          await this.readSession(selectedWorkspaceId, selectedSessionId)
        } else {
          this.patch({
            session: null,
            sessionLoad: { workspaceId: selectedWorkspaceId, sessionId: null, status: 'idle', error: null },
          })
        }
      } catch (error) {
        if (error instanceof RpcFailure && error.code === 'forbidden') {
          this.patch({ connection: 'forbidden' })
        } else {
          this.connection.reconnect()
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
      if (frame.generation === this.state.generation && frame.revision > this.state.revision) this.onFrame(frame)
    }
  }

  private async refreshBootstrap(): Promise<void> {
    try {
      const bootstrap = await this.connection.rpc('workbench.bootstrap', {})
      if (bootstrap.generation !== this.state.generation) {
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
    this.patch({ pendingActions, actionErrors, actionError: null })
    try {
      await operation()
      return true
    } catch (error) {
      this.reportError(error, origin, preservedDraft)
      return false
    } finally {
      const next = new Set(this.state.pendingActions)
      next.delete(key)
      this.patch({ pendingActions: next })
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
    })
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
    this.applySync(acceptBootstrap(this.state, bootstrap))
    if (this.resyncing === null && this.state.sessionLoad.status !== 'loading') this.flushFrames()
  }

  private updateLiveSession(sessionId: string, runtime: LiveSessionState): void {
    this.applySync(acceptLiveSession(this.state, sessionId, runtime))
  }

  private applySync(state: SyncState): void {
    if (state === this.state) return
    const { generation, revision, bootstrap, session, liveSessions } = state
    const patch: Partial<WorkbenchState> = { generation, revision, bootstrap, session, liveSessions }
    if (bootstrap !== null && bootstrap !== this.state.bootstrap) {
      const workspaceId = this.state.selectedWorkspaceId
      const sessionId = this.state.selectedSessionId
      const workspaces = new Set(bootstrap.workspaces.map(workspace => workspace.workspaceId))
      const sessions = new Set(Object.values(bootstrap.sessionsByWorkspace).flat().map(session => session.threadId))
      const created = this.createdIdentity
      // Creation can finish before its already-emitted catalog snapshots are applied.
      const protectedId = created !== null && created.generation === generation && !sessions.has(created.sessionId)
        ? created.sessionId : null
      if (created !== null && (sessions.has(created.sessionId) || created.generation !== generation)) this.createdIdentity = null
      patch.liveSessions = Object.fromEntries(Object.entries(liveSessions).filter(([id]) => sessions.has(id) || id === protectedId))
      if (session !== null && !sessions.has(session.summary.threadId) && session.summary.threadId !== protectedId) patch.session = null
      const workspaceRemoved = workspaceId !== null && !workspaces.has(workspaceId)
      const sessionRemoved = sessionId !== null && !sessions.has(sessionId) && sessionId !== protectedId
      if (workspaceRemoved || sessionRemoved) {
        this.sessionReadRequest += 1
        this.fileSearchRequest += 1
        patch.fileCandidates = []
        patch.fileCandidateStatus = 'idle'
        patch.fileCandidateError = null
        patch.fileCandidateQuery = ''
        patch.selectedWorkspaceId = workspaceRemoved ? null : workspaceId
        patch.selectedSessionId = null
        patch.session = null
        patch.sessionLoad = { workspaceId: patch.selectedWorkspaceId, sessionId: null, status: 'idle', error: null }
      }
    }
    this.patch(patch)
    if (patch.selectedWorkspaceId !== undefined || patch.selectedSessionId !== undefined) this.saveSelection()
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
    })
  }

  private patch(patch: Partial<WorkbenchState>): void {
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
    for (const listener of this.listeners) listener()
  }

  private saveView(patch: Partial<Omit<PersistedView, 'drafts'>>): void {
    this.patch(patch)
    try { persistView(patch) } catch { /* Preferences must not block editing. */ }
  }

  private saveSelection(): void {
    const { selectedWorkspaceId, selectedSessionId } = this.state
    try { persistView({ selectedWorkspaceId, selectedSessionId }) } catch { /* Preferences must not block navigation. */ }
  }

  setSidebarView(value: Partial<PersistedView['sidebarView']>): void {
    this.saveView({ sidebarView: { ...loadPersisted().sidebarView, ...value } })
  }

  setTrajectoryOpen(trajectoryOpen: boolean): void {
    this.saveView({ trajectoryOpen })
  }

  setWorkspaceAppearance(workspaceId: string, appearance: WorkspaceAppearance): void {
    this.saveView({ workspaceAppearance: { ...loadPersisted().workspaceAppearance, [workspaceId]: appearance } })
  }

  setMessageFontSize(value: number): void {
    this.saveView({ messageFontSize: normalizeMessageFontSize(value) })
  }

  setTheme(theme: PersistedView['theme']): void {
    this.saveView({ theme })
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

const ignoreStoreUpdates = (_listener: () => void): (() => void) => () => {}

export function useWorkbenchStore(
  fields: readonly (keyof WorkbenchState)[],
  active = true,
): WorkbenchState {
  const cached = useRef<WorkbenchState | null>(null)
  const snapshot = () => {
    const next = workbenchStore.getSnapshot()
    if (cached.current === null || (active && !sameWorkbenchFields(cached.current, next, fields))) cached.current = next
    return cached.current
  }
  return useSyncExternalStore(active ? workbenchStore.subscribe : ignoreStoreUpdates, snapshot)
}
