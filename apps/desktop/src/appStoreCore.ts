import { reduceUnread, initialSyncState, acceptBootstrap, acceptSessionRead, resetBaseline, reduceStream, type SyncState } from './sync'
import { loadPersisted, persistView, type PersistedView } from './viewPersistence'
import { RpcFailure, RpcClient, isConnectionFailure } from './rpcClient'
import type { ConnectionStatus, StreamEnvelope, AppBootstrap } from './protocol'
import { actionOrigin, pendingKey } from './storeActions'
import { loadDrafts, persistDraft, hasDraft, type Draft } from './drafts'

export const SESSION_PAGE_SIZE = 40

export interface ActionError {
  origin: string
  code: string
  message: string
  recovery: string
}

interface SessionLoadState {
  status: 'idle' | 'loading' | 'error'
  error: ActionError | null
}

interface SessionReadFailure { error: unknown }

export interface AppState extends PersistedView, SyncState {
  drafts: Record<string, Draft> | null
  connection: ConnectionStatus
  sessionLoad: SessionLoadState
  unreadSessions: ReadonlySet<string>
  pendingActions: ReadonlySet<string>
  actionErrors: Readonly<Record<string, ActionError>>
  actionErrorOrigin: string | null
  settingsOpen: boolean
}


export class AppStoreCore {
  protected state: AppState = {
    ...loadPersisted(),
    ...initialSyncState(),
    drafts: null,
    connection: 'connecting',
    sessionLoad: { status: 'idle', error: null },
    unreadSessions: new Set(),
    pendingActions: new Set(),
    actionErrors: {},
    actionErrorOrigin: null,
    settingsOpen: false,
  }
  private readonly listeners = new Set<() => void>()
  private notification: ReturnType<typeof setTimeout> | null = null
  private viewSave: ReturnType<typeof setTimeout> | null = null
  /** Store 持有的唯一连接。设置、补全等局部查询直接复用它，不再为每个
   *  查询维护专用转发方法；传输生命周期（start/stop）与状态同步
   *  仍由 Store 独占。 */
  readonly transport = new RpcClient(frame => this.onFrame(frame), connection => this.patch({ connection }))
  private draftLoad: Promise<void> | null = null
  private started = false
  private queuedFrames: StreamEnvelope[] = []

  private resyncing: Promise<void> | null = null
  private sessionReadRequest = 0
  protected selectionRequest = 0
  /** 最近一次 session 读取。被取代的读取跟随它收敛，使「哪次读取代表当前
   *  基线」只有一个答案，不需要第二套同步控制。 */
  private latestRead: { request: number; promise: Promise<SessionReadFailure | null> } | null = null
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
    void this.restoreDrafts()
    this.transport.start()
  }

  stop(): void {
    if (!this.started) return
    this.started = false
    this.flushView()
    window.removeEventListener('pagehide', this.flushView)
    this.transport.stop()
    if (this.notification !== null) clearTimeout(this.notification)
    this.notification = null
  }

  protected isPending(method: string, origin?: string): boolean {
    return this.state.pendingActions.has(pendingKey(method, origin))
  }

  protected beginSessionSelection(workspaceId: string | null, sessionId: string | null): void {
    this.selectionRequest += 1
    this.patch({ selectedWorkspaceId: workspaceId, selectedSessionId: sessionId, session: null,
      sessionLoad: { status: 'loading', error: null } })
    this.saveSelection()
  }

  /** 新任务先取得真实身份；创建或读取失败、后续导航均保留原任务与草稿。 */
  protected async selectDraftSession(workspaceId: string, reusableId: string | null, selection: number, onSelected: (sessionId: string) => void | Promise<void>): Promise<boolean> {
    let createdSessionId: string | null = null
    const accepted = await this.action('session.create', actionOrigin.workspace(workspaceId), async () => {
      const session = reusableId === null
        ? await this.transport.rpc('session.create', { workspaceId })
        : await this.transport.rpc('session.read', { sessionId: reusableId, beforeTurn: null, limit: SESSION_PAGE_SIZE })
      if (selection !== this.selectionRequest) return
      // AppServer 事件在 RPC 返回前就已发出，但可能仍被此加载
      // 表面缓冲。在对应 catalog 帧到达前保护返回的身份。
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
      await onSelected(session.history.summary.threadId)
      createdSessionId = session.history.summary.threadId
    })
    if (this.resyncing === null) this.flushFrames()
    return accepted && createdSessionId !== null
      && this.state.selectedWorkspaceId === workspaceId && this.state.selectedSessionId === createdSessionId
  }

  protected restoreDrafts(): Promise<void> {
    if (this.state.drafts !== null) return Promise.resolve()
    return this.draftLoad ??= loadDrafts().then(
      drafts => this.patch({ drafts }),
      error => this.reportError(new RpcFailure('storage', `无法读取草稿：${error instanceof Error ? error.message : String(error)}`, '检查本地存储空间后刷新页面。'), 'draft.storage'),
    ).finally(() => { this.draftLoad = null })
  }

  protected async setDraftFor(key: string, draft: Draft): Promise<boolean> {
    if (this.state.drafts === null) return false
    const drafts = { ...this.state.drafts }
    if (hasDraft(draft)) drafts[key] = draft
    else delete drafts[key]
    this.patch({ drafts })
    try { await persistDraft(key, draft); return true }
    catch { this.reportError(new RpcFailure('storage', '草稿暂时只能保留在当前页面。', '请保留页面并检查本地存储空间后重试。'), actionOrigin.session(key)); return false }
  }

  /** 业务读取失败由 sessionLoad 展示；原始错误返回给 resync，供它处理连接失败。 */
  protected readSession(sessionId: string): Promise<SessionReadFailure | null> {
    const request = ++this.sessionReadRequest
    const promise = this.performRead(request, sessionId)
    this.latestRead = { request, promise }
    return promise
  }

  private async performRead(request: number, sessionId: string): Promise<SessionReadFailure | null> {
    this.patch({ sessionLoad: { status: 'loading', error: null } })
    try {
      const session = await this.transport.rpc('session.read', {
        sessionId,
        beforeTurn: null,
        limit: SESSION_PAGE_SIZE,
      })
      if (!this.readIsCurrent(request, sessionId)) return await this.followLatestRead(request)
      this.applySync(acceptSessionRead(this.state, session))
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
      if (request === this.sessionReadRequest && this.resyncing === null) this.flushFrames()
    }
  }

  /** 读取是否仍代表当前选择：一旦有更新的请求或选择，旧读取不得写入状态。 */
  private readIsCurrent(request: number, sessionId: string): boolean {
    return request === this.sessionReadRequest
      && this.state.selectedSessionId === sessionId
  }

  /** 被取代的读取不自行宣告收敛，而是等待取代它的那次读取，避免旧读取把
   *  新读取的连接级失败覆盖成就绪。取代者是选择变更本身（没有新的读取）时，
   *  当前已没有待读取的选择，本次读取直接结束。 */
  private async followLatestRead(request: number): Promise<SessionReadFailure | null> {
    const latest = this.latestRead
    if (latest === null || latest.request === request) return null
    return await latest.promise
  }

  private onFrame(frame: StreamEnvelope): void {
    // ready 发起首次基线读取；读取在途的增量先缓冲，是否已被快照覆盖由 reducer 决定。
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
    if (effects.includes('resync')) void this.resync()
    if (effects.includes('read_selected') && this.state.selectedSessionId !== null) {
      void this.readSession(this.state.selectedSessionId).then(() => this.refreshBootstrap())
    } else if (effects.includes('refresh_bootstrap')) void this.refreshBootstrap()
  }

  private resync(): Promise<void> {
    if (this.resyncing !== null) return this.resyncing
    // 每个重同步入口先自行撤销可提交状态：同一连接上的逻辑重同步
    // （resync_required）不依赖传输层是否已宣告 recovering。
    if (this.state.connection === 'ready') this.patch({ connection: 'recovering' })
    this.resyncing = (async () => {
      let converged = false
      try {
        const bootstrap = await this.transport.rpc('app.bootstrap', {})
        // 即使先前的创建帧丢失，resync baseline 仍具权威性。
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
        // 应用就绪在 bootstrap 与选中会话读取都收敛后才写入：就绪前的旧
        // 会话快照不可作为 phase 路由的依据。
        const { selectedSessionId } = this.state
        if (selectedSessionId !== null) {
          const read = await this.readSession(selectedSessionId)
          // 连接级失败（unavailable）不能被读侧的
          // sessionLoad 吞掉：交回本方法既有的连接状态处理，绝不宣告就绪。业务
          // 读失败（任务不存在或已归档、会话内容损坏等）已由 sessionLoad 独立可见，属于明确
          // 允许的读失败，既不伪装成基线成功，也不把整条连接卡在 recovering。
          if (read !== null && isConnectionFailure(read.error)) throw read.error
        } else {
          this.patch({
            session: null,
            sessionLoad: { status: 'idle', error: null },
          })
        }
        converged = true
      } catch (error) {
        // 私有管道没有可重连的服务；基线失败后停止接收增量，刷新再建立基线。
        this.transport.stop()
        this.queuedFrames = []
        this.patch({ connection: 'recovering' })
        this.reportError(error, 'connection')
      } finally {
        this.resyncing = null
        this.flushFrames()
        // 缓冲帧可能包含新的重同步通知；只有缓冲收敛且没有
        // 新的恢复进行时才宣告可提交。
        if (converged && this.resyncing === null) this.patch({ connection: 'ready' })
      }
    })()
    return this.resyncing
  }

  private flushFrames(): void {
    const queued = this.queuedFrames
    this.queuedFrames = []
    // 缓冲释放不预先按 revision 过滤：帧全部交给同一个 reducer，
    // 由它判断哪些已被快照覆盖、哪些仍要求重同步。真正过期的增量只在
    // reduceStream 里被丢弃，这个判断只有一处。
    for (const frame of queued) this.onFrame(frame)
  }

  private async refreshBootstrap(): Promise<void> {
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
    operation: () => Promise<void>,
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
    const actionError = this.toActionError(error, origin)
    if (actionError.code === 'unavailable') return
    this.patch({
      actionErrors: { ...this.state.actionErrors, [origin]: actionError },
      actionErrorOrigin: origin,
    })
  }

  protected toActionError(error: unknown, origin: string): ActionError {
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

  private updateBootstrap(bootstrap: AppBootstrap): void {
    this.applySync(acceptBootstrap(this.state, bootstrap))
    if (this.resyncing === null && this.state.sessionLoad.status !== 'loading') this.flushFrames()
  }

  protected applySync(state: SyncState, progress = false): void {
    if (state === this.state) return
    const { revision, bootstrap, session, liveSessions } = state
    const patch: Partial<AppState> = { revision, bootstrap, session, liveSessions }
    if (bootstrap !== null && bootstrap !== this.state.bootstrap) {
      const workspaceId = this.state.selectedWorkspaceId
      const sessionId = this.state.selectedSessionId
      const workspaces = new Set(bootstrap.workspaces.map(workspace => workspace.workspaceId))
      const sessions = new Set(Object.values(bootstrap.sessionsByWorkspace).flat().map(session => session.threadId))
      const created = this.createdSessionId
      // 创建可能在其已发出的 catalog 快照被应用前就完成。
      const protectedId = created !== null && !sessions.has(created)
        ? created : null
      if (created !== null && sessions.has(created)) this.createdSessionId = null
      patch.liveSessions = Object.fromEntries(Object.entries(liveSessions).filter(([id]) => sessions.has(id) || id === protectedId))
      if (session !== null && !sessions.has(session.summary.threadId) && session.summary.threadId !== protectedId) patch.session = null
      const workspaceRemoved = workspaceId !== null && !workspaces.has(workspaceId)
      const sessionRemoved = sessionId !== null && !sessions.has(sessionId) && sessionId !== protectedId
      if (workspaceRemoved || sessionRemoved) {
        this.sessionReadRequest += 1
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
    if (patch.liveSessions !== undefined || patch.selectedSessionId !== undefined) {
      patch = { ...patch, unreadSessions: reduceUnread(
        this.state.unreadSessions,
        this.state.liveSessions,
        patch.liveSessions ?? this.state.liveSessions,
        patch.selectedSessionId === undefined ? this.state.selectedSessionId : patch.selectedSessionId,
      ) }
    }
    this.state = { ...this.state, ...patch }
    // 协议状态逐帧归约；只合并显示通知。操作、终态及连接变化立即交付最新状态。
    if (progress) this.notification ??= setTimeout(this.notify, 50)
    else this.notify()
  }

  private readonly notify = (): void => {
    if (this.notification !== null) clearTimeout(this.notification)
    this.notification = null
    for (const listener of this.listeners) listener()
  }

  protected saveView(patch: Partial<PersistedView>, continuous = false): void {
    this.patch(patch)
    if (continuous) this.viewSave ??= setTimeout(this.flushView, 100)
    else this.saveSelection()
  }

  private readonly flushView = (): void => {
    if (this.viewSave !== null) this.saveSelection()
  }

  protected saveSelection(): void {
    if (this.viewSave !== null) clearTimeout(this.viewSave)
    this.viewSave = null
    try {
      persistView(this.state)
    } catch { /* Preferences must not block navigation. */ }
  }
}
