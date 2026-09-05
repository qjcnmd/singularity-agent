import { useSyncExternalStore } from 'react'
import { RpcFailure, WorkbenchConnection } from './connection'
import type {
  ActionReceipt,
  ConnectionStatus,
  DeliveryIntent,
  DirectoryEntry,
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

const storageKey = 'singularity.workbench.view.v1'
const draftStoragePrefix = `${storageKey}:draft:`

interface DetailSelection {
  sessionId: string
  itemId: string
}

interface PersistedView {
  version: 1
  selectedWorkspaceId: string | null
  selectedSessionId: string | null
  drafts: Record<string, string>
  deliveryIntent: Record<string, DeliveryIntent>
  sidebarWidth: number
  sidebarCollapsed: boolean
  detailsWidth: number
  detailSelection: DetailSelection | null
  viewportAnchors: Record<string, ViewportAnchor>
  guideCollapsed: boolean
}

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

export interface SettingsFeedback {
  selector: string | null
  applyTiming: SettingsUpdateResult['applyTiming']
}

export interface WorkbenchState extends PersistedView {
  connection: ConnectionStatus
  bootstrap: WorkbenchBootstrap | null
  session: SessionReadResult | null
  sessionLoad: SessionLoadState
  liveSessions: Record<string, LiveSessionState>
  pendingActions: ReadonlySet<string>
  actionErrors: Readonly<Record<string, ActionError>>
  actionError: ActionError | null
  settingsFeedback: Readonly<Record<string, SettingsFeedback>>
  settingsOpen: boolean
  helpOpen: boolean
  detailsOpen: boolean
  detailsItemId: string | null
  directoryPicker: DirectoryPickerState
  fileCandidates: Array<{ path: string; kind: string }>
  fileCandidateStatus: 'idle' | 'loading' | 'empty' | 'ready' | 'error'
  fileCandidateError: ActionError | null
  fileCandidateQuery: string
  sessionSearch: string
  timelineChange: { sessionId: string; kind: 'replace' | 'prepend' | 'append' | 'update'; version: number } | null
}

const defaultAnchor = (): ViewportAnchor => ({
  mode: 'following',
  anchorItemId: null,
  offset: 0,
  unseenCount: 0,
})

function loadPersisted(): PersistedView {
  const fallback: PersistedView = {
    version: 1,
    selectedWorkspaceId: null,
    selectedSessionId: null,
    drafts: {},
    deliveryIntent: {},
    sidebarWidth: 280,
    sidebarCollapsed: false,
    detailsWidth: 420,
    detailSelection: null,
    viewportAnchors: {},
    guideCollapsed: false,
  }
  try {
    const value = JSON.parse(localStorage.getItem(storageKey) ?? 'null') as Partial<PersistedView> | null
    if (value?.version !== 1) return fallback
    // Move existing drafts once; subsequent writes touch only the target Session.
    for (const [id, text] of Object.entries(value.drafts ?? {})) {
      if (localStorage.getItem(draftStoragePrefix + id) === null) localStorage.setItem(draftStoragePrefix + id, text)
    }
    const drafts: Record<string, string> = {}
    for (let index = 0; index < localStorage.length; index += 1) {
      const key = localStorage.key(index)
      if (key?.startsWith(draftStoragePrefix)) drafts[key.slice(draftStoragePrefix.length)] = localStorage.getItem(key) ?? ''
    }
    return {
      ...fallback,
      selectedWorkspaceId: value.selectedWorkspaceId ?? null,
      selectedSessionId: value.selectedSessionId ?? null,
      drafts,
      deliveryIntent: value.deliveryIntent ?? {},
      sidebarWidth: clamp(value.sidebarWidth ?? 280, 220, 420),
      sidebarCollapsed: value.sidebarCollapsed ?? false,
      detailsWidth: clamp(value.detailsWidth ?? 420, 300, 640),
      detailSelection: value.detailSelection ?? null,
      viewportAnchors: value.viewportAnchors ?? {},
      guideCollapsed: value.guideCollapsed ?? false,
    }
  } catch {
    return fallback
  }
}

class WorkbenchStore {
  private state: WorkbenchState = {
    ...loadPersisted(),
    connection: 'connecting',
    bootstrap: null,
    session: null,
    sessionLoad: { workspaceId: null, sessionId: null, status: 'idle', error: null },
    liveSessions: {},
    pendingActions: new Set(),
    actionErrors: {},
    actionError: null,
    settingsFeedback: {},
    settingsOpen: false,
    helpOpen: false,
    detailsOpen: false,
    detailsItemId: null,
    directoryPicker: { open: false, path: null, entries: [], loading: false, error: null },
    fileCandidates: [],
    fileCandidateStatus: 'idle',
    fileCandidateError: null,
    fileCandidateQuery: '',
    sessionSearch: '',
    timelineChange: null,
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
  private timelineVersion = 0

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
    const detailMatches = persisted.detailSelection?.sessionId === this.state.selectedSessionId
    this.patch({
      deliveryIntent: persisted.deliveryIntent,
      viewportAnchors: persisted.viewportAnchors,
      detailSelection: persisted.detailSelection,
      detailsItemId: detailMatches ? persisted.detailSelection?.itemId ?? null : this.state.detailsItemId,
      guideCollapsed: persisted.guideCollapsed,
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

  retryConnection(): void {
    this.connection.retry()
  }

  retrySession(): void {
    const workspaceId = this.state.selectedWorkspaceId
    const sessionId = this.state.selectedSessionId
    if (workspaceId !== null && sessionId !== null) void this.readSession(workspaceId, sessionId)
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
      detailsOpen: false,
      detailsItemId: null,
    })
    if (first !== null) void this.readSession(workspaceId, first)
  }

  selectSession(sessionId: string): void {
    const workspaceId = this.state.selectedWorkspaceId
    if (workspaceId === null || sessionId === this.state.selectedSessionId) return
    this.cancelCandidates()
    const savedDetail = this.state.detailSelection?.sessionId === sessionId ? this.state.detailSelection.itemId : null
    this.patch({
      selectedSessionId: sessionId,
      session: null,
      sessionLoad: { workspaceId, sessionId, status: 'loading', error: null },
      detailsOpen: savedDetail !== null,
      detailsItemId: savedDetail,
    })
    void this.readSession(workspaceId, sessionId)
  }

  async createSession(): Promise<boolean> {
    const workspaceId = this.state.selectedWorkspaceId
    const selectedSessionId = this.state.selectedSessionId
    if (workspaceId === null) return false
    const current = this.state.session
    if (current !== null
      && current.summary.threadId === this.state.selectedSessionId
      && current.summary.title === null
      && current.summary.turnCount === 0
      && current.runtime.phase === 'idle'
      && current.runtime.pendingControls.length === 0
      && this.draft().trim() === '') {
      return true
    }
    return this.action('session.create', `workspace:${workspaceId}`, async () => {
      const session = await this.connection.rpc<SessionReadResult>('session.create', {
        workspaceId,
        settings: null,
      })
      if (this.state.selectedWorkspaceId !== workspaceId || this.state.selectedSessionId !== selectedSessionId) {
        await this.refreshBootstrap()
        return
      }
      this.patch({
        selectedSessionId: session.summary.threadId,
        session,
        sessionLoad: { workspaceId, sessionId: session.summary.threadId, status: 'idle', error: null },
        timelineChange: this.nextTimelineChange(session.summary.threadId, 'replace'),
      })
      this.updateLiveSession(session.summary.threadId, session.runtime)
      await this.refreshBootstrap()
    })
  }

  async readOlder(): Promise<boolean> {
    const { selectedWorkspaceId, selectedSessionId, session } = this.state
    const beforeTurn = session?.history.nextCursor
    if (selectedWorkspaceId === null || selectedSessionId === null || beforeTurn == null) return false
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
        timelineChange: this.nextTimelineChange(selectedSessionId, 'prepend'),
      }, false)
    })
  }

  setDraft(text: string): void {
    this.setDraftFor(this.draftKey(), text)
  }

  draft(): string {
    return this.state.drafts[this.draftKey()] ?? ''
  }

  setDeliveryIntent(intent: DeliveryIntent): void {
    const sessionId = this.state.selectedSessionId
    if (sessionId === null) return
    this.patch({ deliveryIntent: { ...this.state.deliveryIntent, [sessionId]: intent } })
  }

  currentDeliveryIntent(): DeliveryIntent {
    const sessionId = this.state.selectedSessionId
    return (sessionId === null ? undefined : this.state.deliveryIntent[sessionId]) ?? 'steer'
  }

  async submitDraft(): Promise<boolean> {
    const { selectedWorkspaceId: workspaceId, selectedSessionId: sessionId, session } = this.state
    const draftKey = this.draftKey()
    const text = this.state.drafts[draftKey] ?? ''
    if (workspaceId === null || sessionId === null || text.trim() === '') return false
    const phase = session?.runtime.phase ?? this.state.liveSessions[sessionId]?.phase ?? 'idle'
    const method = phase === 'running' || phase === 'stopping'
      ? this.currentDeliveryIntent() === 'steer' ? 'session.steer' : 'session.followUp'
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

  async sendNow(controlId: string): Promise<boolean> {
    return this.sessionAction('session.queueSendNow', { controlId }, undefined, controlId)
  }

  async renameSession(sessionId: string, name: string): Promise<boolean> {
    const workspaceId = this.state.selectedWorkspaceId
    if (workspaceId === null || name.trim() === '') return false
    return this.action('session.rename', `session:${sessionId}`, async () => {
      await this.connection.rpc<ThreadSummary>('session.rename', { workspaceId, sessionId, name })
      await this.refreshBootstrap()
    })
  }

  async archiveSession(sessionId: string): Promise<boolean> {
    const workspaceId = this.state.selectedWorkspaceId
    if (workspaceId === null) return false
    return this.action('session.archive', `session:${sessionId}`, async () => {
      await this.connection.rpc('session.archive', { workspaceId, sessionId })
      if (this.state.selectedSessionId === sessionId) {
        this.patch({ selectedSessionId: null, session: null, detailsOpen: false, detailsItemId: null })
      }
      const liveSessions = { ...this.state.liveSessions }
      delete liveSessions[sessionId]
      this.patch({ liveSessions }, false)
      await this.refreshBootstrap()
    })
  }

  async updateSettings(selector: string): Promise<boolean> {
    const workspaceId = this.state.selectedWorkspaceId
    const sessionId = this.state.selectedSessionId
    if (workspaceId === null || sessionId === null) return false
    return this.action('session.updateSettings', `session:${sessionId}`, async () => {
      const result = await this.connection.rpc<SettingsUpdateResult>('session.updateSettings', {
        workspaceId,
        sessionId,
        selector,
      })
      const settingsFeedback = {
        ...this.state.settingsFeedback,
        [sessionId]: { selector: result.selector, applyTiming: result.applyTiming },
      }
      if (this.state.selectedSessionId === sessionId && this.state.session !== null) {
        this.patch({
          settingsFeedback,
          session: {
            ...this.state.session,
            runtime: { ...this.state.session.runtime, selector: result.selector },
          },
        }, false)
      } else {
        this.patch({ settingsFeedback }, false)
      }
    })
  }

  async addWorkspace(root: string): Promise<boolean> {
    return this.action('workspace.add', `directory:${root}`, async () => {
      const workspace = await this.connection.rpc<Workspace>('workspace.add', { root })
      await this.refreshBootstrap()
      this.selectWorkspace(workspace.workspaceId)
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

  async searchFiles(query: string): Promise<void> {
    const workspaceId = this.state.selectedWorkspaceId
    const normalized = query.trim()
    const request = ++this.fileSearchRequest
    if (workspaceId === null || normalized === '') {
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
        { workspaceId, query: normalized, limit: 12 },
      )
      if (request !== this.fileSearchRequest
        || this.state.selectedWorkspaceId !== workspaceId
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

  setHelpOpen(helpOpen: boolean): void {
    this.patch({ helpOpen }, false)
  }

  selectDetails(detailsItemId: string | null): void {
    const sessionId = this.state.selectedSessionId
    const detailSelection = detailsItemId !== null && sessionId !== null
      ? { sessionId, itemId: detailsItemId }
      : null
    this.patch({ detailSelection, detailsItemId, detailsOpen: detailsItemId !== null })
  }

  closeDetails(): void {
    this.patch({ detailsOpen: false }, false)
  }

  setSidebarWidth(sidebarWidth: number): void {
    this.patch({ sidebarWidth: clamp(sidebarWidth, 220, 420) })
  }

  setDetailsWidth(detailsWidth: number): void {
    this.patch({ detailsWidth: clamp(detailsWidth, 300, 640) })
  }

  toggleSidebar(): void {
    this.patch({ sidebarCollapsed: !this.state.sidebarCollapsed })
  }

  setGuideCollapsed(guideCollapsed: boolean): void {
    this.patch({ guideCollapsed })
  }

  setSessionSearch(sessionSearch: string): void {
    this.patch({ sessionSearch }, false)
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
      && Math.abs(previous.offset - anchor.offset) < 1
      && previous.unseenCount === anchor.unseenCount) return
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
    if (workspaceId === null || sessionId === null) return false
    const origin = target === undefined ? `session:${sessionId}` : `control:${sessionId}:${target}`
    return this.action(method, origin, async () => {
      await this.connection.rpc<ActionReceipt>(method, { workspaceId, sessionId, ...extra })
    }, preservedDraft)
  }

  private async readSession(workspaceId: string, sessionId: string): Promise<void> {
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
        timelineChange: this.nextTimelineChange(sessionId, 'replace'),
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
      const previousRuntime = this.state.session.runtime
      this.patch({
        session: { ...this.state.session, runtime },
        timelineChange: this.nextTimelineChange(
          sessionId,
          runtime.controls.some((control) => !previousRuntime.controls.some((existing) => existing.controlId === control.controlId))
            ? 'append'
            : 'update',
        ),
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
      const nestedTurn = typeof event.params.turn === 'object' && event.params.turn !== null
        ? String((event.params.turn as Record<string, unknown>).turnId ?? '')
        : ''
      const eventTurnId = String(event.params.turnId ?? nestedTurn)
      const active = runtime.activeTurn ?? {
        turnId: eventTurnId,
        events: [],
        startedAt: new Date().toISOString(),
      }
      const changeKind = turnEventChange(active.events, event)
      this.patch({
        session: {
          ...this.state.session,
          runtime: {
            ...runtime,
            sessionRevision: event.sessionRevision,
            phase,
            activeTurn: {
              ...active,
              turnId: event.method === 'turn/started' && eventTurnId !== '' ? eventTurnId : active.turnId,
              startedAt: event.method === 'turn/started' && typeof event.params.startedAt === 'string' ? event.params.startedAt : active.startedAt,
              events: [...active.events, event],
            },
          },
        },
        timelineChange: this.nextTimelineChange(sessionId, changeKind),
      }, false)
      return
    }
    if (sessionId !== undefined && frame.type === 'session_settled') {
      const payload = frame.payload as { runtime?: SessionSnapshot }
      if (payload.runtime !== undefined && !this.updateLiveSession(sessionId, payload.runtime)) return
      if (sessionId === this.state.selectedSessionId && this.state.selectedWorkspaceId !== null) {
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
          selectedWorkspaceId = bootstrap.workspaces[0]?.workspaceId ?? null
        }
        let selectedSessionId = this.state.selectedSessionId
        const sessions = selectedWorkspaceId === null ? [] : bootstrap.sessionsByWorkspace[selectedWorkspaceId] ?? []
        if (!sessions.some((session) => session.threadId === selectedSessionId)) {
          selectedSessionId = sessions[0]?.threadId ?? null
        }
        const savedDetail = this.state.detailSelection?.sessionId === selectedSessionId
          ? this.state.detailSelection.itemId
          : null
        this.patch({
          bootstrap,
          session: hostChanged ? null : this.state.session,
          liveSessions: Object.fromEntries(Object.entries(bootstrap.sessionPhases).map(([id, phase]) => [id, { sessionRevision: 0, phase, terminal: null }])),
          selectedWorkspaceId,
          selectedSessionId,
          connection: 'ready',
          detailsItemId: savedDetail,
          detailsOpen: savedDetail !== null,
        })
        if (selectedWorkspaceId !== null && selectedSessionId !== null) {
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

  private nextTimelineChange(sessionId: string, kind: 'replace' | 'prepend' | 'append' | 'update') {
    this.timelineVersion += 1
    return { sessionId, kind, version: this.timelineVersion }
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
    this.state = { ...this.state, ...patch }
    if (persist) {
      try { this.persist() } catch { /* View preferences must not block editing or runtime updates. */ }
    }
    for (const listener of this.listeners) listener()
  }

  private persist(): void {
    const view: Omit<PersistedView, 'drafts'> = {
      version: 1,
      selectedWorkspaceId: this.state.selectedWorkspaceId,
      selectedSessionId: this.state.selectedSessionId,
      deliveryIntent: this.state.deliveryIntent,
      sidebarWidth: this.state.sidebarWidth,
      sidebarCollapsed: this.state.sidebarCollapsed,
      detailsWidth: this.state.detailsWidth,
      detailSelection: this.state.detailSelection,
      viewportAnchors: this.state.viewportAnchors,
      guideCollapsed: this.state.guideCollapsed,
    }
    localStorage.setItem(storageKey, JSON.stringify(view))
  }
}


function turnEventChange(events: TurnEventEnvelope[], event: TurnEventEnvelope): 'append' | 'update' {
  if (event.method === 'tool/execution/update'
    || event.method === 'tool/execution/end'
    || event.method === 'item/completed'
    || event.method === 'item/failed') return 'update'
  if (event.method === 'provider/attempt') {
    return events.some((existing) => existing.method === event.method
      && existing.params.modelTurnOrdinal === event.params.modelTurnOrdinal
      && existing.params.attempt === event.params.attempt)
      ? 'update'
      : 'append'
  }
  const itemId = eventItemId(event)
  return itemId !== '' && events.some((existing) => eventItemId(existing) === itemId) ? 'update' : 'append'
}

function eventItemId(event: TurnEventEnvelope): string {
  const item = event.params.item
  if (typeof item === 'object' && item !== null && typeof (item as Record<string, unknown>).itemId === 'string') {
    return (item as Record<string, string>).itemId
  }
  return typeof event.params.itemId === 'string' ? event.params.itemId : ''
}

function clamp(value: number, minimum: number, maximum: number): number {
  return Math.min(maximum, Math.max(minimum, value))
}

export const workbenchStore = new WorkbenchStore()

export function useWorkbenchStore(): WorkbenchState {
  return useSyncExternalStore(workbenchStore.subscribe, workbenchStore.getSnapshot)
}
