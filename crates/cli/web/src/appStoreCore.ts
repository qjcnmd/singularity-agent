import { reduceUnread, initialSyncState, acceptBootstrap, acceptSessionRead, resetBaseline, reduceStream, type SyncState } from './sync'
import { loadPersisted, persistView, type PersistedView } from './viewPersistence'
import { RpcFailure, RpcClient, isConnectionFailure } from './rpcClient'
import type { ConnectionStatus, StreamEnvelope, AppBootstrap } from './protocol'
import { actionOrigin, pendingKey } from './storeActions'

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

/** 一次 session 基线读取的收敛结果。读取属于同步生命周期而不是普通查询，
 *  所以它必须向调用方报告自己是否真的落地：
 *  - applied：快照已接纳，基线完成；
 *  - failed：读取失败。error 保留原始失败，调用方据此区分连接级失败与
 *    业务读失败——连接级失败不得被 sessionLoad 吞成「读侧已处理」；
 *  - superseded：读取被更新的选择或请求取代，收敛由取代它的读取负责。 */
export type SessionReadOutcome =
  | { status: 'applied' }
  | { status: 'failed'; error: unknown }
  | { status: 'superseded' }

export interface AppState extends PersistedView, SyncState {
  connection: ConnectionStatus
  sessionLoad: SessionLoadState
  unreadSessions: ReadonlySet<string>
  pendingActions: ReadonlySet<string>
  actionErrors: Readonly<Record<string, ActionError>>
  actionError: ActionError | null
  settingsOpen: boolean
}


export class AppStoreCore {
  protected state: AppState = {
    ...loadPersisted(),
    ...initialSyncState(),
    connection: 'connecting',
    sessionLoad: { status: 'idle', error: null },
    unreadSessions: new Set(),
    pendingActions: new Set(),
    actionErrors: {},
    actionError: null,
    settingsOpen: false,
  }
  private readonly listeners = new Set<() => void>()
  private notification: ReturnType<typeof setTimeout> | null = null
  /** Store 持有的唯一连接。设置、补全等局部查询直接复用它，不再为每个
   *  查询维护专用转发方法；传输生命周期（start/stop/reconnect）与状态同步
   *  仍由 Store 独占。 */
  readonly transport = new RpcClient(frame => this.onFrame(frame), connection => this.patch({ connection }))
  private started = false
  private queuedFrames: StreamEnvelope[] = []

  protected resyncing: Promise<void> | null = null
  private sessionReadRequest = 0
  /** 最近一次 session 读取。被取代的读取跟随它收敛，使「哪次读取代表当前
   *  基线」只有一个答案，不需要第二套同步控制。 */
  private latestRead: { request: number; promise: Promise<SessionReadOutcome> } | null = null
  protected createdIdentity: { sessionId: string; generation: string | null } | null = null

  readonly subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener)
    return () => this.listeners.delete(listener)
  }

  readonly getSnapshot = (): AppState => this.state

  start(): void {
    if (this.started) return
    this.started = true
    this.transport.start()
  }

  stop(): void {
    if (!this.started) return
    this.started = false
    this.transport.stop()
    if (this.notification !== null) clearTimeout(this.notification)
    this.notification = null
  }

  protected isPending(method: string, origin?: string): boolean {
    return this.state.pendingActions.has(pendingKey(method, origin))
  }

  /** 读取所选 session 的基线快照。返回收敛结果而不是 void：读侧的
   *  sessionLoad 错误只描述这次业务读取，调用方（resync）必须据此决定连接
   *  是否可宣告就绪，不能把连接级失败当成「读侧已经处理」。 */
  protected readSession(workspaceId: string | null, sessionId: string): Promise<SessionReadOutcome> {
    if (workspaceId === null) return Promise.resolve({ status: 'superseded' })
    const request = ++this.sessionReadRequest
    const promise = this.performRead(request, workspaceId, sessionId)
    this.latestRead = { request, promise }
    return promise
  }

  private async performRead(request: number, workspaceId: string, sessionId: string): Promise<SessionReadOutcome> {
    this.patch({ sessionLoad: { status: 'loading', error: null } })
    try {
      const session = await this.transport.rpc('session.read', {
        workspaceId,
        sessionId,
        beforeTurn: null,
        limit: SESSION_PAGE_SIZE,
      })
      if (!this.readIsCurrent(request, workspaceId, sessionId)) return await this.followLatestRead(request)
      this.applySync(acceptSessionRead(this.state, session))
      this.patch({ sessionLoad: { status: 'idle', error: null } })
      return { status: 'applied' }
    } catch (error) {
      if (!this.readIsCurrent(request, workspaceId, sessionId)) return await this.followLatestRead(request)
      this.patch({
        session: null,
        sessionLoad: { status: 'error', error: this.toActionError(error, actionOrigin.session(sessionId)) },
      })
      return { status: 'failed', error }
    } finally {
      if (request === this.sessionReadRequest && this.resyncing === null) this.flushFrames()
    }
  }

  /** 读取是否仍代表当前选择：一旦有更新的请求或选择，旧读取不得写入状态。 */
  private readIsCurrent(request: number, workspaceId: string, sessionId: string): boolean {
    return request === this.sessionReadRequest
      && this.state.selectedWorkspaceId === workspaceId
      && this.state.selectedSessionId === sessionId
  }

  /** 被取代的读取不自行宣告收敛，而是等待取代它的那次读取，避免旧读取把
   *  新读取的连接级失败覆盖成就绪。取代者是选择变更本身（没有新的读取）时，
   *  当前已没有待读取的选择，本次读取直接以 superseded 结束。 */
  private async followLatestRead(request: number): Promise<SessionReadOutcome> {
    const latest = this.latestRead
    if (latest === null || latest.request === request) return { status: 'superseded' }
    return await latest.promise
  }

  private onFrame(frame: StreamEnvelope): void {
    // ready 是新一代连接的基线要求：重同步在途时它不能被 resync 的 Promise
    // 去重吞掉，先留在缓冲里，由当前重同步收敛后的同一条路径再安排一次
    // 基线读取。其余帧只缓冲，不在这里筛选——是否已被快照覆盖由 reducer 决定。
    if (this.resyncing !== null
      || (frame.type !== 'ready' && (this.state.bootstrap === null || this.state.sessionLoad.status === 'loading'))) {
      this.queuedFrames.push(frame)
      return
    }
    this.applyFrame(frame)
  }

  private applyFrame(frame: StreamEnvelope): void {
    const { state, effects } = reduceStream(this.state, this.state.selectedSessionId, frame, new Date().toISOString())
    this.applySync(state, frame.type === 'turn_event' && (
      frame.payload.method === 'item/agentMessage/delta' || frame.payload.method === 'item/agentThinking/delta'
      || frame.payload.method === 'tool/execution/update'))
    if (effects.includes('resync')) void this.resync()
    // 历史读取同时填充服务端摘要缓存；列表随后刷新即可复用同一版本的解析结果。
    if (effects.includes('read_selected') && this.state.selectedSessionId !== null) {
      void this.readSession(this.state.selectedWorkspaceId, this.state.selectedSessionId).then(() => this.refreshBootstrap())
    } else if (effects.includes('refresh_bootstrap')) void this.refreshBootstrap()
  }

  private resync(): Promise<void> {
    if (this.resyncing !== null) return this.resyncing
    // 每个重同步入口先自行撤销可提交状态：同一连接上的逻辑重同步
    // （revision 缺口、resync_required）不依赖传输层是否已宣告 recovering。
    if (this.state.connection === 'ready') this.patch({ connection: 'recovering' })
    this.resyncing = (async () => {
      let converged = false
      try {
        const bootstrap = await this.transport.rpc('app.bootstrap', {})
        // 即使先前的创建帧丢失，resync baseline 仍具权威性。
        this.createdIdentity = null
        this.applySync(resetBaseline(this.state, bootstrap))
        const workspaceId = this.state.selectedWorkspaceId
        if (workspaceId !== null && this.state.selectedSessionId === null
          && !this.isPending('session.create', actionOrigin.workspace(workspaceId))) {
          const first = bootstrap.sessionsByWorkspace[workspaceId]?.[0]?.threadId ?? null
          if (first !== null) {
            this.patch({ selectedSessionId: first, session: null })
            this.saveSelection()
          }
        }
        // 应用就绪在 bootstrap 与选中会话读取都收敛后才写入：就绪前的旧
        // 会话快照不可作为 phase 路由的依据。
        const { selectedWorkspaceId, selectedSessionId } = this.state
        if (selectedSessionId !== null) {
          const read = await this.readSession(selectedWorkspaceId, selectedSessionId)
          // 连接级失败（unavailable/forbidden/invalid_response）不能被读侧的
          // sessionLoad 吞掉：交回本方法既有的连接状态处理，绝不宣告就绪。业务
          // 读失败（任务不存在或已归档、会话内容损坏等）已由 sessionLoad 独立可见，属于明确
          // 允许的读失败，既不伪装成基线成功，也不把整条连接卡在 recovering。
          if (read.status === 'failed' && isConnectionFailure(read.error)) throw read.error
        } else {
          this.patch({
            session: null,
            sessionLoad: { status: 'idle', error: null },
          })
        }
        converged = true
      } catch (error) {
        if (error instanceof RpcFailure && error.code === 'forbidden') {
          this.patch({ connection: 'forbidden' })
        } else {
          this.transport.reconnect()
        }
        this.reportError(error, 'connection')
      } finally {
        this.resyncing = null
        this.flushFrames()
        // 缓冲帧可能再次暴露缺口并开启下一次重同步；只有缓冲收敛且没有
        // 新的恢复进行时才宣告可提交。
        if (converged && this.resyncing === null) this.patch({ connection: 'ready' })
      }
    })()
    return this.resyncing
  }

  protected flushFrames(): void {
    const queued = this.queuedFrames
    this.queuedFrames = []
    // 缓冲释放不预先按 generation/revision 过滤：帧全部交给同一个 reducer，
    // 由它判断哪些已被快照覆盖、哪些仍要求重同步。真正过期的增量只在
    // reduceStream 里被丢弃，这个判断只有一处。
    for (const frame of queued) this.onFrame(frame)
  }

  private async refreshBootstrap(): Promise<void> {
    try {
      const bootstrap = await this.transport.rpc('app.bootstrap', {})
      if (bootstrap.generation !== this.state.generation) {
        await this.resync()
        return
      }
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
    this.patch({ pendingActions, actionErrors, actionError: null })
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
      actionError,
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
    const { generation, revision, bootstrap, session, liveSessions } = state
    const patch: Partial<AppState> = { generation, revision, bootstrap, session, liveSessions }
    if (bootstrap !== null && bootstrap !== this.state.bootstrap) {
      const workspaceId = this.state.selectedWorkspaceId
      const sessionId = this.state.selectedSessionId
      const workspaces = new Set(bootstrap.workspaces.map(workspace => workspace.workspaceId))
      const sessions = new Set(Object.values(bootstrap.sessionsByWorkspace).flat().map(session => session.threadId))
      const created = this.createdIdentity
      // 创建可能在其已发出的 catalog 快照被应用前就完成。
      const protectedId = created !== null && created.generation === generation && !sessions.has(created.sessionId)
        ? created.sessionId : null
      if (created !== null && (sessions.has(created.sessionId) || created.generation !== generation)) this.createdIdentity = null
      patch.liveSessions = Object.fromEntries(Object.entries(liveSessions).filter(([id]) => sessions.has(id) || id === protectedId))
      if (session !== null && !sessions.has(session.summary.threadId) && session.summary.threadId !== protectedId) patch.session = null
      const workspaceRemoved = workspaceId !== null && !workspaces.has(workspaceId)
      const sessionRemoved = sessionId !== null && !sessions.has(sessionId) && sessionId !== protectedId
      if (workspaceRemoved || sessionRemoved) {
        this.sessionReadRequest += 1
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

  protected saveView(patch: Partial<Omit<PersistedView, 'drafts'>>): void {
    this.patch(patch)
    this.saveSelection()
  }

  protected saveSelection(): void {
    try {
      persistView(this.state)
    } catch { /* Preferences must not block navigation. */ }
  }
}
