import { reduceUnread, initialSyncState, acceptSessionStart, acceptSessionRead, resetBaseline, reduceStream, pruneStaleSelection, type SyncState } from './sync'
import { loadPersisted, persistView, type PersistedView } from './viewPersistence'
import { RpcFailure, RpcClient } from './rpcClient'
import type { ConnectionStatus, StreamEnvelope, ThreadSummary, UserQuestionAnswer, SessionStartParams } from './protocol'
import { DraftStore, type DraftSnapshot } from './drafts'

export const actionOrigin = {
  session: (id: string | null) => `session:${id}`,
  control: (sessionId: string | null, controlId: string) => `control:${sessionId}:${controlId}`,
  inline: (method: string, id: string | null) => `inline:${method}:${id}`,
  directoryPicker: 'directory:picker',
}

export function hasInlineActionError(origin: string): boolean {
  return origin.startsWith('control:') || origin.startsWith('inline:')
}

export function pendingKey(method: string, origin: string): string {
  return `${method}:${origin}`
}


export interface ActionError {
  message: string
}

interface SessionLoadState {
  status: 'idle' | 'loading' | 'error'
  error: ActionError | null
}

export interface FileTab {
  id: string; path: string; cwd: string; title: string
  opened: number
}

export interface QuestionDraft { itemId: string; index: number; answers: UserQuestionAnswer[] }

export interface AppState extends PersistedView, SyncState {
  drafts: DraftSnapshot | null
  questionDrafts: Readonly<Record<string, QuestionDraft>>
  connection: ConnectionStatus
  bootstrapLoad: SessionLoadState
  sessionLoad: SessionLoadState
  unreadSessions: ReadonlySet<string>
  pendingActions: ReadonlySet<string>
  actionErrors: Readonly<Record<string, ActionError>>
  actionErrorOrigin: string | null
  page: 'chat' | 'instructions' | 'skills' | 'mcp'
  settingsOpen: boolean
  fileTabs: readonly FileTab[]
  activeFileTab: string | null
}


export class SessionStore {
  protected state: AppState = {
    ...loadPersisted(),
    ...initialSyncState(),
    drafts: null,
    questionDrafts: {},
    connection: 'connecting',
    bootstrapLoad: { status: 'idle', error: null },
    sessionLoad: { status: 'idle', error: null },
    unreadSessions: new Set(),
    pendingActions: new Set(),
    actionErrors: {},
    actionErrorOrigin: null,
    page: 'chat',
    settingsOpen: false,
    fileTabs: [],
    activeFileTab: null,
  }
  private readonly listeners = new Set<() => void>()
  private notification: number | null = null
  private viewSave: ReturnType<typeof setTimeout> | null = null
  readonly transport = new RpcClient(frame => this.onFrame(frame), connection => this.patch({ connection }))
  protected readonly drafts = new DraftStore(drafts => this.patch({ drafts }))
  private started = false
  private queuedFrames: StreamEnvelope[] = []

  private resyncing: Promise<void> | null = null
  protected selectionRequest = 0
  private latestRead: symbol | null = null

  readonly subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener)
    return () => this.listeners.delete(listener)
  }

  readonly getSnapshot = (): AppState => this.state

  start(): void {
    if (this.started) return
    this.started = true
    window.addEventListener('pagehide', this.flushView)
    void this.drafts.load()
    this.transport.start()
  }

  stop(): void {
    if (!this.started) return
    this.started = false
    this.flushView()
    window.removeEventListener('pagehide', this.flushView)
    this.transport.stop()
    if (this.notification !== null) cancelAnimationFrame(this.notification)
    this.notification = null
  }

  protected isPending(method: string, origin: string): boolean {
    return this.state.pendingActions.has(pendingKey(method, origin))
  }

  async retrySession(): Promise<void> {
    const sessionId = this.state.selectedSessionId
    if (sessionId !== null) await this.readSession(sessionId)
  }

  async selectSession(sessionId: string): Promise<void> {
    const workspaceRoot = Object.entries(this.state.bootstrap?.sessionsByWorkspace ?? {})
      .find(([, sessions]) => sessions.some(session => session.threadId === sessionId))?.[0]
    if (workspaceRoot === undefined) return
    this.selectionRequest += 1
    if (sessionId === this.state.selectedSessionId) {
      this.patch({ page: 'chat' })
      if (this.state.session === null) await this.readSession(sessionId)
      return
    }
    this.patch({ page: 'chat', selectedWorkspaceRoot: workspaceRoot, selectedSessionId: sessionId, session: null,
      sessionLoad: { status: 'loading', error: null } })
    this.saveSelection()
    await this.readSession(sessionId)
  }

  newTask(workspaceRoot = this.state.selectedWorkspaceRoot): void {
    if (workspaceRoot === null) {
      this.patch({ page: 'chat' })
      return
    }
    this.selectionRequest += 1
    this.latestRead = null
    const collapsed = this.state.sidebarView.collapsed
    this.patch({
      page: 'chat',
      selectedWorkspaceRoot: workspaceRoot,
      selectedSessionId: null,
      session: null,
      sessionLoad: { status: 'idle', error: null },
      ...(collapsed.includes(workspaceRoot)
        ? { sidebarView: { ...this.state.sidebarView, collapsed: collapsed.filter(id => id !== workspaceRoot) } }
        : {}),
    })
    this.saveSelection()
    this.syncFocus(null)
    if (this.resyncing === null) this.flushFrames()
  }

  private syncFocus(sessionId: string | null): void {
    if (this.state.connection !== 'ready') return
    void this.transport.rpc('session.focus', { sessionId }).catch(() => {})
  }

  draftKey(): string | null {
    return this.state.selectedSessionId ?? (this.state.selectedWorkspaceRoot === null ? null : 'new-task')
  }

  protected async startSession(params: SessionStartParams, selection: number): Promise<void> {
    const started = await this.transport.rpc('session.start', params)
    this.applySync(acceptSessionStart(this.state, started))
    if (selection === this.selectionRequest) void this.selectSession(started.summary.threadId)
  }

  sessions(): ThreadSummary[] {
    const workspaceRoot = this.state.selectedWorkspaceRoot
    return workspaceRoot === null ? [] : this.state.bootstrap?.sessionsByWorkspace[workspaceRoot] ?? []
  }

  private async readSession(sessionId: string): Promise<void> {
    const request = Symbol()
    this.latestRead = request
    this.patch({ sessionLoad: { status: 'loading', error: null } })
    try {
      const session = await this.transport.rpc('session.read', { sessionId })
      if (!this.readIsCurrent(request, sessionId)) return
      const accepted = acceptSessionRead(this.state, session)
      this.applySync(accepted)
      this.patch({ sessionLoad: { status: 'idle', error: null } })
    } catch (error) {
      if (!this.readIsCurrent(request, sessionId)) return
      this.patch({
        session: null,
        sessionLoad: { status: 'error', error: this.toActionError(error) },
      })
    } finally {
      if (request === this.latestRead) {
        this.latestRead = null
        if (this.resyncing === null) this.flushFrames()
      }
    }
  }

  private readIsCurrent(request: symbol, sessionId: string): boolean {
    return request === this.latestRead && this.state.selectedSessionId === sessionId
  }

  private onFrame(frame: StreamEnvelope): void {
    if (this.resyncing === null && this.latestRead == null && this.state.bootstrapLoad.status === 'error') {
      if (frame.type === 'ready' || frame.type === 'app_changed' || frame.type === 'session_changed' || frame.type === 'session_settled' || frame.type === 'background_task_updated') {
        this.applyFrame(frame)
      }
      return
    }
    if (this.resyncing !== null || this.latestRead != null
      || (frame.type !== 'ready' && this.state.bootstrap === null)) {
      this.queuedFrames.push(frame)
      return
    }
    this.applyFrame(frame)
  }

  private applyFrame(frame: StreamEnvelope): void {
    if (frame.type === 'ready') {
      void this.resync()
      return
    }
    const previousSelectedSessionId = this.state.selectedSessionId
    const recoveredBootstrap = frame.type === 'app_changed'
      && frame.revision > this.state.revision
      && (this.state.bootstrapLoad.status === 'error' || this.state.bootstrap === null)
    if (frame.type === 'app_read_failed' && frame.revision > this.state.revision) {
      this.patch({ bootstrapLoad: { status: 'error', error: frame.error } })
    } else if (recoveredBootstrap) {
      this.patch({ bootstrapLoad: { status: 'idle', error: null } })
    }
    const state = reduceStream(this.state, frame)
    this.applySync(state, (frame.type === 'background_task_updated' && frame.payload.task.state.status === 'running')
      || (frame.type === 'turn_event' && (
        frame.payload.method === 'item/textDelta'
        || frame.payload.method === 'tool/output')))
    if (recoveredBootstrap) {
      const { selectedSessionId } = this.state
      if (selectedSessionId !== null) void this.readSession(selectedSessionId)
      else if (previousSelectedSessionId === null) this.syncFocus(null)
    }
    if ((frame.type === 'session_changed' || frame.type === 'session_settled')
      && this.state.session?.summary.threadId !== frame.sessionId
      && state.liveSessions[frame.sessionId]?.revision === frame.revision) {
      this.retainQuestionDraft(frame.sessionId, frame.payload.pendingQuestion?.itemId)
    }
  }

  resync(): Promise<void> {
    if (this.resyncing !== null) return this.resyncing
    const showLoading = this.state.bootstrap === null || this.state.bootstrapLoad.status === 'error'
    if (showLoading) this.patch({ bootstrapLoad: { status: 'loading', error: null } })
    this.resyncing = (async () => {
      try {
        const bootstrap = await this.transport.rpc('app.bootstrap', {})
        this.applySync(resetBaseline(this.state, bootstrap))
        const { selectedSessionId } = this.state
        if (selectedSessionId !== null) {
          await this.readSession(selectedSessionId)
          if (this.state.bootstrapLoad.status !== 'idle') {
            this.patch({ bootstrapLoad: { status: 'idle', error: null } })
          }
        } else {
          this.patch({
            session: null,
            sessionLoad: { status: 'idle', error: null },
            bootstrapLoad: { status: 'idle', error: null },
          })
          this.syncFocus(null)
        }
      } catch (error) {
        this.patch({ bootstrapLoad: { status: 'error', error: this.toActionError(error) } })
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
    for (const frame of queued) this.onFrame(frame)
  }

  protected async action(
    method: string,
    origin: string,
    operation: () => Promise<unknown>,
  ): Promise<boolean> {
    const key = pendingKey(method, origin)
    if (this.state.pendingActions.has(key)) return false
    const pendingActions = new Set(this.state.pendingActions)
    pendingActions.add(key)
    const actionErrors = { ...this.state.actionErrors }
    delete actionErrors[origin]
    this.patch({ pendingActions, actionErrors, actionErrorOrigin: null })
    try {
      await operation()
      return true
    } catch (error) {
      this.reportError(error, origin)
      return false
    } finally {
      const next = new Set(this.state.pendingActions)
      next.delete(key)
      this.patch({ pendingActions: next })
    }
  }

  protected reportError(error: unknown, origin: string): void {
    if (error instanceof RpcFailure && error.unavailable) return
    this.patch({
      actionErrors: { ...this.state.actionErrors, [origin]: this.toActionError(error) },
      actionErrorOrigin: origin,
    })
  }

  private toActionError(error: unknown): ActionError {
    return { message: error instanceof Error ? error.message : '发生了未知错误。' }
  }

  protected applySync(state: SyncState, progress = false): void {
    if (state === this.state) return
    const { revision, modelCatalogRevision, bootstrap, session, liveSessions } = state
    const patch: Partial<AppState> = { revision, modelCatalogRevision, bootstrap, session, liveSessions }
    let selectionDropped = false
    if (bootstrap !== null && bootstrap !== this.state.bootstrap) {
      const pruned = pruneStaleSelection(bootstrap, { ...this.state, session, liveSessions })
      patch.liveSessions = pruned.liveSessions
      patch.session = pruned.session
      selectionDropped = pruned.selectionDropped
      if (selectionDropped) {
        this.latestRead = null
        this.selectionRequest += 1
        patch.selectedWorkspaceRoot = pruned.selectedWorkspaceRoot
        patch.selectedSessionId = pruned.selectedSessionId
        patch.sessionLoad = { status: 'idle', error: null }
      }
    }
    this.patch(patch, progress)
    if (selectionDropped) {
      this.saveSelection()
      this.syncFocus(null)
      if (this.resyncing === null) this.flushFrames()
    }
  }

  protected patch(patch: Partial<AppState>, progress = false): void {
    if (patch.session) patch = { ...patch, questionDrafts: retainedQuestionDrafts(
      this.state.questionDrafts, patch.session.summary.threadId, patch.session.runtime.pendingQuestion?.itemId,
    ) }
    if (patch.liveSessions !== undefined || patch.selectedSessionId !== undefined) {
      patch = { ...patch, unreadSessions: reduceUnread(
        this.state.unreadSessions,
        this.state.liveSessions,
        patch.liveSessions ?? this.state.liveSessions,
        patch.selectedSessionId === undefined ? this.state.selectedSessionId : patch.selectedSessionId,
      ) }
    }
    this.state = { ...this.state, ...patch }
    if (progress) this.notification ??= requestAnimationFrame(this.notify)
    else this.notify()
  }

  private readonly notify = (): void => {
    if (this.notification !== null) cancelAnimationFrame(this.notification)
    this.notification = null
    for (const listener of this.listeners) listener()
  }

  protected retainQuestionDraft(sessionId: string, itemId?: string): void {
    const questionDrafts = retainedQuestionDrafts(this.state.questionDrafts, sessionId, itemId)
    if (questionDrafts !== this.state.questionDrafts) this.patch({ questionDrafts })
  }

  protected saveView(patch: Partial<PersistedView>, continuous = false): void {
    this.patch(patch)
    if (continuous) this.viewSave ??= setTimeout(this.flushView, 100)
    else this.saveSelection()
  }

  private readonly flushView = (): void => {
    if (this.viewSave !== null) this.saveSelection()
  }

  private saveSelection(): void {
    if (this.viewSave !== null) clearTimeout(this.viewSave)
    this.viewSave = null
    try {
      persistView(this.state)
    } catch {}
  }
}

function retainedQuestionDrafts(drafts: AppState['questionDrafts'], sessionId: string, itemId?: string): AppState['questionDrafts'] {
  if (drafts[sessionId] === undefined || drafts[sessionId].itemId === itemId) return drafts
  const retained = { ...drafts }
  delete retained[sessionId]
  return retained
}
