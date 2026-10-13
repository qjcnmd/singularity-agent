import { reduceUnread, initialSyncState, acceptBootstrap, acceptSessionRead, resetBaseline, reduceStream, type SyncState } from './sync'
import { loadPersisted, persistView, type PersistedView } from './viewPersistence'
import { RpcFailure, RpcClient, isConnectionFailure } from './rpcClient'
import type { ConnectionStatus, StreamEnvelope, AppBootstrap, ThreadSummary, UserQuestionAnswer } from './protocol'
import { actionOrigin, pendingKey } from './storeActions'
import { DraftStore, DraftStorageError, hasDraft, type DraftSnapshot } from './drafts'
import { prependExecutionHistory } from './execution'
import { buildTimeline } from './timeline'
import { isBlankSession } from './sessionState'

const SESSION_PAGE_SIZE = 40

export interface ActionError {
  origin: string
  code: string
  message: string
  recovery: string
}

/** 忙碌时未重复执行，error 为 null；执行失败直接携带本次反馈，不必回读全局状态。 */
export type ActionResult = { ok: true } | { ok: false; error: ActionError | null }

interface SessionLoadState {
  status: 'idle' | 'loading' | 'error'
  error: ActionError | null
}

interface SessionReadFailure { error: unknown }

export interface QuestionDraft { itemId: string; index: number; answers: UserQuestionAnswer[] }

export interface AppState extends PersistedView, SyncState {
  drafts: DraftSnapshot | null
  questionDrafts: Readonly<Record<string, QuestionDraft>>
  connection: ConnectionStatus
  sessionLoad: SessionLoadState
  unreadSessions: ReadonlySet<string>
  pendingActions: ReadonlySet<string>
  actionErrors: Readonly<Record<string, ActionError>>
  actionErrorOrigin: string | null
  settingsOpen: boolean
}


/** 任务导航和读取/事件同步的状态所有者；导航请求的身份与接纳规则留在内部。 */
export class SessionStore {
  protected state: AppState = {
    ...loadPersisted(),
    ...initialSyncState(),
    drafts: null,
    questionDrafts: {},
    connection: 'connecting',
    sessionLoad: { status: 'idle', error: null },
    unreadSessions: new Set(),
    pendingActions: new Set(),
    actionErrors: {},
    actionErrorOrigin: null,
    settingsOpen: false,
  }
  private readonly listeners = new Set<() => void>()
  private notification: number | null = null
  private viewSave: ReturnType<typeof setTimeout> | null = null
  /** Store 持有的连接。设置、补全等局部查询直接复用它，不为每个查询维护
   *  专用转发方法；传输生命周期（start/stop）与状态同步仍由 Store 独占。 */
  readonly transport = new RpcClient(frame => this.onFrame(frame), connection => this.patch({ connection }))
  protected readonly drafts = new DraftStore(
    drafts => this.patch({ drafts }),
    (error, sessionId) => this.reportError(error, sessionId === null ? 'draft.storage' : actionOrigin.session(sessionId)),
  )
  private started = false
  private queuedFrames: StreamEnvelope[] = []

  private resyncing: Promise<void> | null = null
  private selectionRequest = 0
  /** 最近一次 session 读取。被取代的读取等它结束再返回，保证「哪次读取代表当前
   *  基线」只有一个答案，不需要第二套同步控制。 */
  private latestRead: { request: symbol; promise: Promise<SessionReadFailure | null> } | null = null
  private createdSessionId: string | null = null

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
    const workspaceId = Object.entries(this.state.bootstrap?.sessionsByWorkspace ?? {})
      .find(([, sessions]) => sessions.some(session => session.threadId === sessionId))?.[0]
    if (workspaceId === undefined) return
    this.selectionRequest += 1
    if (sessionId === this.state.selectedSessionId) {
      if (this.state.session === null) await this.readSession(sessionId)
      return
    }
    this.patch({ selectedWorkspaceId: workspaceId, selectedSessionId: sessionId, session: null,
      sessionLoad: { status: 'loading', error: null } })
    this.saveSelection()
    await this.readSession(sessionId)
  }

  /** 新任务先取得真实身份；创建或读取失败、后续导航均保留原任务与草稿。 */
  async createSession(workspaceId = this.state.selectedWorkspaceId, transferDraft = false): Promise<boolean> {
    if (workspaceId === null || this.isPending('session.create', actionOrigin.workspace(workspaceId))) return false
    const selection = ++this.selectionRequest
    const sourceKey = this.state.selectedSessionId
    if (this.state.sidebarView.collapsed.includes(workspaceId)) {
      this.saveView({ sidebarView: { ...this.state.sidebarView,
        collapsed: this.state.sidebarView.collapsed.filter(id => id !== workspaceId) } })
    }
    const blank = this.sessions(workspaceId).find(session =>
      isBlankSession(session)
      && (this.state.liveSessions[session.threadId]?.phase ?? 'idle') === 'idle'
      && (!transferDraft || sourceKey === session.threadId || !hasDraft(this.state.drafts?.[session.threadId])))
    let createdSessionId: string | null = null
    const accepted = await this.action('session.create', actionOrigin.workspace(workspaceId), async () => {
      const session = blank === undefined
        ? await this.transport.rpc('session.create', { workspaceId })
        : await this.transport.rpc('session.read', { sessionId: blank.threadId, beforeTurn: null, limit: SESSION_PAGE_SIZE })
      if (selection !== this.selectionRequest) return
      // AppServer 事件在 RPC 返回前就已发出，但可能仍被这次加载表面缓冲。
      // 在对应 catalog 帧到达前要保护返回的身份。
      this.createdSessionId = session.history.summary.threadId
      const acceptedSession = acceptSessionRead(this.state, session)
      this.patch({
        selectedWorkspaceId: workspaceId,
        selectedSessionId: session.history.summary.threadId,
        session: acceptedSession.session,
        liveSessions: acceptedSession.liveSessions,
        sessionLoad: { status: 'idle', error: null },
      })
      this.saveSelection()
      if (transferDraft && sourceKey !== null) {
        await this.drafts.transfer(sourceKey, session.history.summary.threadId)
      }
      createdSessionId = session.history.summary.threadId
    })
    if (this.resyncing === null) this.flushFrames()
    return accepted && createdSessionId !== null
      && this.state.selectedWorkspaceId === workspaceId && this.state.selectedSessionId === createdSessionId
  }

  sessions(workspaceId = this.state.selectedWorkspaceId): ThreadSummary[] {
    return workspaceId === null ? [] : this.state.bootstrap?.sessionsByWorkspace[workspaceId] ?? []
  }

  async readOlder(): Promise<boolean> {
    const { selectedSessionId, session } = this.state
    const beforeTurn = session?.nextCursor
    if (selectedSessionId === null || beforeTurn == null) return false
    return this.action('history.older', actionOrigin.session(selectedSessionId), async () => {
      const older = await this.transport.rpc('session.read', {
        sessionId: selectedSessionId, beforeTurn, limit: SESSION_PAGE_SIZE,
      })
      if (this.state.selectedSessionId !== selectedSessionId
        || this.state.session?.nextCursor !== beforeTurn) return
      this.patch({ session: prependExecutionHistory(this.state.session, older.history) })
    })
  }

  /** 业务读取失败由 sessionLoad 展示；原始错误返回给 resync，供它处理连接失败。 */
  private readSession(sessionId: string): Promise<SessionReadFailure | null> {
    const request = Symbol(sessionId)
    const promise = this.performRead(request, sessionId)
    this.latestRead = { request, promise }
    return promise
  }

  private async performRead(request: symbol, sessionId: string): Promise<SessionReadFailure | null> {
    this.patch({ sessionLoad: { status: 'loading', error: null } })
    try {
      const session = await this.transport.rpc('session.read', {
        sessionId,
        beforeTurn: null,
        limit: SESSION_PAGE_SIZE,
      })
      if (!this.readIsCurrent(request, sessionId)) return await this.followLatestRead(request)
      const accepted = acceptSessionRead(this.state, session)
      let restored = accepted.session
      const anchor = this.state.viewportAnchors[sessionId]
      // 保存的阅读位置可能在当前页之前，先补齐历史再显示，避免跳到另一轮。
      while (restored !== null && restored.nextCursor !== null && anchor?.mode === 'anchored'
        && anchor.anchorItemId !== null && !buildTimeline(restored).some(item => item.key === anchor.anchorItemId)) {
        const older = await this.transport.rpc('session.read', {
          sessionId, beforeTurn: restored.nextCursor, limit: SESSION_PAGE_SIZE,
        })
        if (!this.readIsCurrent(request, sessionId)) return await this.followLatestRead(request)
        restored = prependExecutionHistory(restored, older.history)
      }
      this.applySync({ ...accepted, session: restored })
      this.patch({ sessionLoad: { status: 'idle', error: null } })
      return null
    } catch (error) {
      if (!this.readIsCurrent(request, sessionId)) return await this.followLatestRead(request)
      this.patch({
        session: null,
        sessionLoad: { status: 'error', error: this.toActionError(error, actionOrigin.session(sessionId)) },
      })
      return { error }
    } finally {
      if (request === this.latestRead?.request && this.resyncing === null) this.flushFrames()
    }
  }

  /** 读取是否仍代表当前选择：一旦有更新的请求或选择，旧读取不得写入状态。 */
  private readIsCurrent(request: symbol, sessionId: string): boolean {
    return request === this.latestRead?.request
      && this.state.selectedSessionId === sessionId
  }

  /** 被取代的读取等取代它的那次读取结束再返回，避免旧读取把新读取的连接级失败
   *  覆盖成就绪。取代者是选择变更本身（没有新的读取）时，当前已没有待读取的选择，
   *  本次读取直接结束。 */
  private async followLatestRead(request: symbol): Promise<SessionReadFailure | null> {
    const latest = this.latestRead
    if (latest === null || latest.request === request) return null
    return await latest.promise
  }

  private onFrame(frame: StreamEnvelope): void {
    // 正在重同步或初始化加载时暂存接收到的事件，就绪后再回放
    if (this.resyncing !== null
      || (frame.type !== 'ready' && (this.state.bootstrap === null || this.state.sessionLoad.status === 'loading'))) {
      this.queuedFrames.push(frame)
      return
    }
    this.applyFrame(frame)
  }

  private applyFrame(frame: StreamEnvelope): void {
    const { state, effects } = reduceStream(this.state, this.state.selectedSessionId, frame)
    this.applySync(state, frame.type === 'turn_event' && (
      frame.payload.method === 'item/agentMessage/delta' || frame.payload.method === 'item/agentThinking/delta'
      || frame.payload.method === 'tool/execution/update'))
    if ((frame.type === 'session_changed' || frame.type === 'session_settled')
      && state.liveSessions[frame.sessionId]?.sessionRevision === frame.payload.sessionRevision) {
      this.retainQuestionDraft(frame.sessionId, frame.payload.pendingQuestion?.itemId)
    }
    if (effects.includes('resync')) void this.resync()
    if (effects.includes('read_selected') && this.state.selectedSessionId !== null) {
      void this.readSession(this.state.selectedSessionId).then(() => this.refreshBootstrap())
    } else if (effects.includes('refresh_bootstrap')) void this.refreshBootstrap()
  }

  private resync(): Promise<void> {
    if (this.resyncing !== null) return this.resyncing
    // 开始重同步：暂停 UI 提交
    if (this.state.connection === 'ready') this.patch({ connection: 'recovering' })
    this.resyncing = (async () => {
      let converged = false
      try {
        const bootstrap = await this.transport.rpc('app.bootstrap', {})
        this.createdSessionId = null
        this.applySync(resetBaseline(this.state, bootstrap))
        const workspaceId = this.state.selectedWorkspaceId
        if (workspaceId !== null && this.state.selectedSessionId === null
          && !this.isPending('session.create', actionOrigin.workspace(workspaceId))) {
          const first = bootstrap.sessionsByWorkspace[workspaceId]?.[0]?.threadId ?? null
          if (first !== null) {
            this.patch({ selectedSessionId: first, session: null })
            this.saveSelection()
          } else {
            this.patch({ selectedWorkspaceId: null })
            this.saveSelection()
          }
        }
        // 等待全局元数据与当前会话数据均拉取完毕
        const { selectedSessionId } = this.state
        if (selectedSessionId !== null) {
          const read = await this.readSession(selectedSessionId)
          // 仅网络/连接故障需阻断 ready；普通业务错误（如会话已归档）不阻断连接
          if (read !== null && isConnectionFailure(read.error)) throw read.error
        } else {
          this.patch({
            session: null,
            sessionLoad: { status: 'idle', error: null },
          })
        }
        converged = true
      } catch (error) {
        // stdio 管道故障时断开连接并清空队列
        this.transport.stop()
        this.queuedFrames = []
        this.patch({ connection: 'recovering' })
        this.reportError(error, 'connection')
      } finally {
        this.resyncing = null
        this.flushFrames()
        // 暂存帧回放完毕且无新重同步时，恢复 ready 状态
        if (converged && this.resyncing === null) this.patch({ connection: 'ready' })
      }
    })()
    return this.resyncing
  }

  private flushFrames(): void {
    const queued = this.queuedFrames
    this.queuedFrames = []
    // 依次分发回放暂存的帧
    for (const frame of queued) this.onFrame(frame)
  }

  async refreshBootstrap(): Promise<void> {
    try {
      const bootstrap = await this.transport.rpc('app.bootstrap', {})
      this.updateBootstrap(bootstrap)
    } catch (error) {
      this.reportError(error, 'app')
    }
  }

  protected async action(
    method: string,
    origin: string,
    operation: () => Promise<unknown>,
  ): Promise<boolean> {
    return (await this.actionResult(method, origin, operation)).ok
  }

  protected async actionResult(
    method: string,
    origin: string,
    operation: () => Promise<unknown>,
  ): Promise<ActionResult> {
    const key = pendingKey(method, origin)
    if (this.state.pendingActions.has(key)) return { ok: false, error: null }
    const pendingActions = new Set(this.state.pendingActions)
    pendingActions.add(key)
    const actionErrors = { ...this.state.actionErrors }
    delete actionErrors[origin]
    this.patch({ pendingActions, actionErrors, actionErrorOrigin: null })
    try {
      await operation()
      return { ok: true }
    } catch (error) {
      return { ok: false, error: this.reportError(error, origin) }
    } finally {
      const next = new Set(this.state.pendingActions)
      next.delete(key)
      this.patch({ pendingActions: next })
    }
  }

  protected reportError(error: unknown, origin: string): ActionError {
    const actionError = this.toActionError(error, origin)
    if (actionError.code !== 'unavailable') {
      this.patch({
        actionErrors: { ...this.state.actionErrors, [origin]: actionError },
        actionErrorOrigin: origin,
      })
    }
    return actionError
  }

  private toActionError(error: unknown, origin: string): ActionError {
    if (error instanceof RpcFailure || error instanceof DraftStorageError) {
      return { origin, code: error instanceof DraftStorageError ? 'storage' : error.code, message: error.message, recovery: error.recovery }
    }
    return {
      origin,
      code: 'internal',
      message: error instanceof Error ? error.message : '发生了未知错误。',
      recovery: '',
    }
  }

  private updateBootstrap(bootstrap: AppBootstrap): void {
    this.applySync(acceptBootstrap(this.state, bootstrap))
    if (this.resyncing === null && this.state.sessionLoad.status !== 'loading') this.flushFrames()
  }

  private applySync(state: SyncState, progress = false): void {
    if (state === this.state) return
    const { revision, bootstrap, session, liveSessions } = state
    const patch: Partial<AppState> = { revision, bootstrap, session, liveSessions }
    if (bootstrap !== null && bootstrap !== this.state.bootstrap) {
      const workspaceId = this.state.selectedWorkspaceId
      const sessionId = this.state.selectedSessionId
      const workspaces = new Set(bootstrap.workspaces.map(workspace => workspace.workspaceId))
      const sessions = new Set(Object.values(bootstrap.sessionsByWorkspace).flat().map(session => session.threadId))
      const created = this.createdSessionId
      // 兼容新创建会话尚未包含在 catalog 快照中的过渡状态，避免被意外过滤
      const protectedId = created !== null && !sessions.has(created)
        ? created : null
      if (created !== null && sessions.has(created)) this.createdSessionId = null
      patch.liveSessions = Object.fromEntries(Object.entries(liveSessions).filter(([id]) => sessions.has(id) || id === protectedId))
      if (session !== null && !sessions.has(session.summary.threadId) && session.summary.threadId !== protectedId) patch.session = null
      const workspaceRemoved = workspaceId !== null && !workspaces.has(workspaceId)
      const sessionRemoved = sessionId !== null && !sessions.has(sessionId) && sessionId !== protectedId
      if (workspaceRemoved || sessionRemoved) {
        this.latestRead = null
        this.selectionRequest += 1
        patch.selectedWorkspaceId = workspaceRemoved ? null : workspaceId
        patch.selectedSessionId = null
        patch.session = null
        patch.sessionLoad = { status: 'idle', error: null }
      }
    }
    this.patch(patch, progress)
    if (patch.selectedWorkspaceId !== undefined || patch.selectedSessionId !== undefined) this.saveSelection()
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
    // 同一帧的增量合并通知；终态与连接状态变化立即通知。
    if (progress) this.notification ??= requestAnimationFrame(this.notify)
    else this.notify()
  }

  private readonly notify = (): void => {
    if (this.notification !== null) cancelAnimationFrame(this.notification)
    this.notification = null
    for (const listener of this.listeners) listener()
  }

  /** 切任务时 session 暂为空；只在后端确认问题结束或被替换后丢弃答案。 */
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
    } catch { /* 偏好保存失败不阻塞导航。 */ }
  }
}

function retainedQuestionDrafts(drafts: AppState['questionDrafts'], sessionId: string, itemId?: string): AppState['questionDrafts'] {
  if (drafts[sessionId] === undefined || drafts[sessionId].itemId === itemId) return drafts
  const retained = { ...drafts }
  delete retained[sessionId]
  return retained
}
