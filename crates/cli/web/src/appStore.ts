import { prependExecutionHistory } from './execution'
import { reduceUnread, initialSyncState, acceptBootstrap, acceptSessionRead, resetBaseline, reduceStream, type SyncState } from './sync'
import { defaultAnchor, loadPersisted, persistDraft, persistView, normalizeMessageFontSize, clampSidebarWidth, type PersistedView, type WorkspaceAppearance } from './viewPersistence'
export type { WorkspaceAppearance } from './viewPersistence'
import { useRef, useSyncExternalStore } from 'react'
import { RpcFailure, RpcClient, isConnectionFailure, type RpcTransport, type StreamListener, type StatusListener } from './rpcClient'
import type {
  ConnectionStatus,
  DeliveryIntent,
  ProviderConfigurationInput,
  StreamEnvelope,
  ThreadSummary,
  ViewportAnchor,
  AppBootstrap,
} from './protocol'

const SESSION_PAGE_SIZE = 40

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
type SessionReadOutcome =
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


interface StoreDependencies {
  createTransport: (onFrame: StreamListener, onStatus: StatusListener) => RpcTransport
}

export class AppStore {
  private state: AppState = {
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
  readonly transport: RpcTransport
  private started = false
  private queuedFrames: StreamEnvelope[] = []

  constructor(dependencies: StoreDependencies = { createTransport: (onFrame, onStatus) => new RpcClient(onFrame, onStatus) }) {
    this.transport = dependencies.createTransport(frame => this.onFrame(frame), connection => this.patch({ connection }))
  }

  private resyncing: Promise<void> | null = null
  private sessionReadRequest = 0
  /** 最近一次 session 读取。被取代的读取跟随它收敛，使「哪次读取代表当前
   *  基线」只有一个答案，不需要第二套同步控制。 */
  private latestRead: { request: number; promise: Promise<SessionReadOutcome> } | null = null
  private createdIdentity: { sessionId: string; generation: string | null } | null = null

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

  async retrySession(): Promise<void> {
    const workspaceId = this.state.selectedWorkspaceId
    const sessionId = this.state.selectedSessionId
    if (sessionId !== null) await this.readSession(workspaceId, sessionId)
  }

  private beginSessionSelection(workspaceId: string | null, sessionId: string | null): void {
    this.patch({ selectedWorkspaceId: workspaceId, selectedSessionId: sessionId, session: null,
      sessionLoad: { status: 'loading', error: null } })
    this.saveSelection()
  }

  private moveDraft(source: string, destination: string, draft: string): void {
    this.setDraftFor(destination, draft)
    this.setDraftFor(source, '')
  }

  async selectSession(sessionId: string): Promise<void> {
    const workspaceId = this.workspaceForSession(sessionId)
    if (workspaceId === undefined) return
    if (sessionId === this.state.selectedSessionId) {
      if (this.state.session === null) await this.readSession(workspaceId, sessionId)
      return
    }
    this.beginSessionSelection(workspaceId, sessionId)
    await this.readSession(workspaceId, sessionId)
  }

  async createSession(workspaceId = this.state.selectedWorkspaceId, transferDraft = false): Promise<boolean> {
    if (workspaceId === null) { this.openDirectoryPicker(); return false }
    if (this.isPending('session.create', actionOrigin.workspace(workspaceId))) return false
    if (this.state.sidebarView.collapsed.includes(workspaceId)) {
      this.setSidebarView({ collapsed: this.state.sidebarView.collapsed.filter(id => id !== workspaceId) })
    }
    const sourceKey = this.draftKey()
    const sourceDraft = transferDraft ? this.draft() : ''
    const blank = this.sessions(workspaceId).find((session) =>
      session.turnCount === 0
      && (this.state.liveSessions[session.threadId]?.phase ?? 'idle') === 'idle'
      && (sourceDraft === '' || sourceKey === session.threadId || (this.state.drafts[session.threadId] ?? '') === ''))
    if (blank !== undefined) {
      const selecting = this.selectSession(blank.threadId)
      if (sourceDraft !== '' && sourceKey !== blank.threadId) {
        this.moveDraft(sourceKey, blank.threadId, sourceDraft)
      }
      await selecting
      return this.state.selectedSessionId === blank.threadId && this.state.session !== null
    }
    // 立即切换可编辑表面：创建期间的按键输入属于新 task。
    this.beginSessionSelection(workspaceId, null)
    const newDraftKey = this.draftKey()
    if (sourceDraft !== '' && sourceKey !== newDraftKey) {
      this.moveDraft(sourceKey, newDraftKey, sourceDraft)
    }
    let createdSessionId: string | null = null
    const accepted = await this.action('session.create', actionOrigin.workspace(workspaceId), async () => {
      const session = await this.transport.rpc('session.create', {
        workspaceId,
        settings: null,
      })
      if (this.state.selectedWorkspaceId !== workspaceId || this.state.selectedSessionId !== null) {
        return
      }
      const newDraft = this.state.drafts[newDraftKey] ?? ''
      // AppServer 事件在 RPC 返回前就已发出，但可能仍被此加载
      // 表面缓冲。在对应 catalog 帧到达前保护返回的身份。
      this.createdIdentity = { sessionId: session.history.summary.threadId, generation: this.state.generation }
      const acceptedSession = acceptSessionRead(this.state, session)
      this.patch({
        selectedWorkspaceId: workspaceId,
        selectedSessionId: session.history.summary.threadId,
        session: acceptedSession.session,
        liveSessions: acceptedSession.liveSessions,
        sessionLoad: { status: 'idle', error: null },
      })
      this.saveSelection()
      if (newDraft !== '') {
        this.moveDraft(newDraftKey, session.history.summary.threadId, newDraft)
      }
      createdSessionId = session.history.summary.threadId
    })
    if (createdSessionId === null && this.state.selectedWorkspaceId === workspaceId && this.state.selectedSessionId === null) {
      this.patch({ sessionLoad: { status: 'idle', error: null } })
    }
    if (this.resyncing === null) this.flushFrames()
    return accepted && createdSessionId !== null
      && this.state.selectedWorkspaceId === workspaceId && this.state.selectedSessionId === createdSessionId
  }

  async readOlder(): Promise<boolean> {
    const { selectedWorkspaceId, selectedSessionId, session } = this.state
    const beforeTurn = session?.nextCursor
    const generation = this.state.generation
    if (selectedWorkspaceId === null || selectedSessionId === null || beforeTurn == null) return false
    return this.action('history.older', actionOrigin.session(selectedSessionId), async () => {
      const older = await this.transport.rpc('session.read', {
        workspaceId: selectedWorkspaceId,
        sessionId: selectedSessionId,
        beforeTurn,
        limit: SESSION_PAGE_SIZE,
      })
      if (this.state.generation !== generation
        || this.state.selectedWorkspaceId !== selectedWorkspaceId
        || this.state.selectedSessionId !== selectedSessionId
        || this.state.session?.nextCursor !== beforeTurn) return
      this.patch({ session: prependExecutionHistory(this.state.session, older.history) })
    })
  }

  setDraft(text: string): void {
    this.setDraftFor(this.draftKey(), text)
  }

  draft(): string {
    return this.state.drafts[this.draftKey()] ?? ''
  }

  /** 按 phase 路由的动作只有在所选 session 的 runtime 快照可信后才会触发。 */
  readonly runtimeSynced = (): boolean =>
    this.state.connection === 'ready' && this.state.sessionLoad.status !== 'loading'

  submissionState(intent: DeliveryIntent = 'follow_up') {
    const state = this.state
    const phase = state.session?.runtime.phase ?? 'idle'
    const submitPending = ['session.submit', 'session.followUp', 'session.steer'].some(method => this.isPending(method, actionOrigin.session(state.selectedSessionId)))
    const creating = this.isPending('session.create', actionOrigin.workspace(state.selectedWorkspaceId))
    // 阻止提交的原因按优先级排列：先说明连接与基线读取，再说明创建或本任务的
    // 读取，最后才是当前 phase 与在途提交。用户只会看到第一条成立的原因。
    let blockedReason: string | null = null
    if (state.connection !== 'ready') blockedReason = '连接恢复后即可发送，草稿会保留。'
    else if (!this.runtimeSynced()) blockedReason = '正在同步任务状态，稍后即可发送。'
    else if (creating) blockedReason = '正在准备新任务，输入的内容会保留。'
    else if (state.selectedSessionId !== null && state.session === null) blockedReason = state.sessionLoad.status === 'error'
      ? '任务读取失败，请点击上方“重试读取”。' : '正在读取任务，稍后即可发送。'
    else if (phase === 'stopping') blockedReason = '正在停止当前任务，结束后即可发送。'
    else if (phase === 'reserved') blockedReason = '正在启动任务，稍后可继续发送。'
    else if (phase === 'compacting') blockedReason = '上下文整理完成后即可发送，也可以先停止整理。'
    else if (submitPending) blockedReason = '正在发送…'
    const method = phase === 'running'
      ? intent === 'steer' ? 'session.steer' : 'session.followUp'
      : 'session.submit'
    return { canSubmit: state.selectedWorkspaceId !== null && blockedReason === null && this.draft().trim() !== '', blockedReason, method } as const
  }

  async submitDraft(intent: DeliveryIntent = 'follow_up'): Promise<boolean> {
    if (!this.submissionState(intent).canSubmit) return false
    if (this.state.selectedSessionId === null) {
      if (!await this.createSession(this.state.selectedWorkspaceId, true)) return false
    }
    const { selectedWorkspaceId: workspaceId, selectedSessionId: sessionId, session } = this.state
    const draftKey = this.draftKey()
    const text = this.state.drafts[draftKey] ?? ''
    if (workspaceId === null || sessionId === null || session === null || !this.runtimeSynced() || text.trim() === '') return false
    const { canSubmit, method } = this.submissionState(intent)
    if (!canSubmit) return false
    return this.action(method, actionOrigin.session(sessionId), async () => {
      await this.transport.rpc(method, { workspaceId, sessionId, text })
      if ((this.state.drafts[draftKey] ?? '') === text) this.setDraftFor(draftKey, '')
    })
  }

  async stopActive(): Promise<boolean> {
    return this.sessionAction('session.abort', ids => this.transport.rpc('session.abort', ids))
  }

  async compact(): Promise<boolean> {
    return this.sessionAction('session.compact', ids => this.transport.rpc('session.compact', ids))
  }

  async withdraw(controlId: string): Promise<boolean> {
    return this.sessionAction('session.queueWithdraw', ids => this.transport.rpc('session.queueWithdraw', { ...ids, controlId }), controlId)
  }

  async replace(controlId: string, text: string): Promise<boolean> {
    return this.sessionAction('session.queueReplace', ids => this.transport.rpc('session.queueReplace', { ...ids, controlId, text }), controlId)
  }

  /** 立即发送全部待执行输入：目标集合由服务端在当前队列上确定，前端不枚举
   * 自己的快照，因此不会对已被消费的条目重复请求。 */
  async sendQueuedNow(): Promise<boolean> {
    return this.sessionAction('session.queueSendNow', ids => this.transport.rpc('session.queueSendNow', ids))
  }

  async sendNow(controlId: string): Promise<boolean> {
    return this.sessionAction('session.queueSendNow', ids => this.transport.rpc('session.queueSendNow', { ...ids, controlId }), controlId)
  }

  async renameWorkspace(workspaceId: string, name: string): Promise<boolean> {
    return this.action('workspace.rename', actionOrigin.workspace(workspaceId), async () => {
      await this.transport.rpc('workspace.rename', { workspaceId, name })
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
    return this.action('session.rename', actionOrigin.session(sessionId), async () => {
      await this.transport.rpc('session.rename', { workspaceId, sessionId, name })
    })
  }

  async archiveSession(sessionId: string): Promise<boolean> {
    const workspaceId = this.workspaceForSession(sessionId)
    if (workspaceId === undefined) return false
    return this.action('session.archive', actionOrigin.session(sessionId), async () => {
      await this.transport.rpc('session.archive', { workspaceId, sessionId })
    })
  }

  async updateSettings(selector: string): Promise<boolean> {
    const workspaceId = this.state.selectedWorkspaceId
    if (this.state.selectedSessionId === null && !await this.createSession(workspaceId, true)) return false
    return this.sessionAction('session.updateSettings', ids => this.transport.rpc('session.updateSettings', { ...ids, selector }))
  }

  async addWorkspace(root: string): Promise<boolean> {
    return this.action('workspace.add', actionOrigin.directory(root), async () => {
      const workspace = await this.transport.rpc('workspace.add', { root })
      await this.createSession(workspace.workspaceId, true)
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
      ), actionOrigin.workspace(workspaceId))
      return false
    }
    return this.action('workspace.remove', actionOrigin.workspace(workspaceId), async () => {
      await this.transport.rpc('workspace.remove', { workspaceId })
      const workspaceAppearance = { ...this.state.workspaceAppearance }
      delete workspaceAppearance[workspaceId]
      this.saveView({ workspaceAppearance })
    })
  }

  async saveProvider(provider: ProviderConfigurationInput, apiKey?: string): Promise<boolean> {
    return this.action('model.saveProvider', actionOrigin.provider(provider.providerId), async () => {
      await this.transport.rpc('model.saveProvider', { provider, apiKey: apiKey || undefined })
    })
  }

  async setApiKey(providerId: string, apiKey: string): Promise<boolean> {
    return this.action('model.setApiKey', actionOrigin.providerKey(providerId), async () => {
      await this.transport.rpc('model.setApiKey', { providerId, apiKey })
    })
  }

  async removeProvider(providerId: string): Promise<boolean> {
    return this.action('model.removeProvider', actionOrigin.provider(providerId), async () => {
      await this.transport.rpc('model.removeProvider', { providerId })
    })
  }

  openDirectoryPicker(): void {
    void this.action('directory.pick', actionOrigin.directoryPicker, async () => {
      const result = await this.transport.rpc('directory.pick', {})
      if (result.path !== null) await this.addWorkspace(result.path)
    })
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
    this.saveView({ viewportAnchors: { ...this.state.viewportAnchors, [id]: anchor } })
  }

  private isPending(method: string, origin?: string): boolean {
    return this.state.pendingActions.has(pendingKey(method, origin))
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
      persistDraft(key, text)
    } catch {
      this.reportError(new RpcFailure('storage', '草稿暂时只能保留在当前页面。', '请复制草稿后检查浏览器存储空间。'), actionOrigin.session(key))
    }
  }

  private async sessionAction(
    method: import('./protocol').RpcMethod,
    operation: (ids: import('./protocol').SessionParams) => Promise<null>,
    target?: string,
  ): Promise<boolean> {
    const workspaceId = this.state.selectedWorkspaceId
    const sessionId = this.state.selectedSessionId
    if (workspaceId === null || sessionId === null) return false
    const origin = target === undefined ? actionOrigin.session(sessionId) : actionOrigin.control(sessionId, target)
    return this.action(method, origin, async () => { await operation({ workspaceId, sessionId }) })
  }

  /** 读取所选 session 的基线快照。返回收敛结果而不是 void：读侧的
   *  sessionLoad 错误只描述这次业务读取，调用方（resync）必须据此决定连接
   *  是否可宣告就绪，不能把连接级失败当成「读侧已经处理」。 */
  private readSession(workspaceId: string | null, sessionId: string): Promise<SessionReadOutcome> {
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

  private flushFrames(): void {
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

  private async action(
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

  private reportError(error: unknown, origin: string): void {
    const actionError = this.toActionError(error, origin)
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

  private updateBootstrap(bootstrap: AppBootstrap): void {
    this.applySync(acceptBootstrap(this.state, bootstrap))
    if (this.resyncing === null && this.state.sessionLoad.status !== 'loading') this.flushFrames()
  }

  private applySync(state: SyncState, progress = false): void {
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

  private patch(patch: Partial<AppState>, progress = false): void {
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

  private saveView(patch: Partial<Omit<PersistedView, 'drafts'>>): void {
    this.patch(patch)
    this.saveSelection()
  }

  private saveSelection(): void {
    try {
      persistView(this.state)
    } catch { /* Preferences must not block navigation. */ }
  }

  setSidebarView(value: Partial<PersistedView['sidebarView']>): void {
    this.saveView({ sidebarView: { ...this.state.sidebarView, ...value } })
  }

  setTrajectoryOpen(trajectoryOpen: boolean): void {
    this.saveView({ trajectoryOpen })
  }

  setWorkspaceAppearance(workspaceId: string, appearance: WorkspaceAppearance): void {
    this.saveView({ workspaceAppearance: { ...this.state.workspaceAppearance, [workspaceId]: appearance } })
  }

  setMessageFontSize(value: number): void {
    this.saveView({ messageFontSize: normalizeMessageFontSize(value) })
  }

  setTheme(theme: PersistedView['theme']): void {
    this.saveView({ theme })
  }
}


export const appStore = new AppStore()

/** 动作来源键由发起方与原位反馈共用。 */
export const actionOrigin = {
  session: (id: string | null) => `session:${id}`,
  workspace: (id: string | null) => `workspace:${id}`,
  control: (sessionId: string | null, controlId: string) => `control:${sessionId}:${controlId}`,
  provider: (id: string) => `provider:${id}`,
  providerKey: (id: string) => `provider-key:${id}`,
  directory: (path: string) => `directory:${path}`,
  directoryPicker: 'directory:picker',
}

const inlineActionPrefixes = [actionOrigin.control('', ''), actionOrigin.provider(''), actionOrigin.providerKey(''), actionOrigin.directory('')]
  .map(key => key.slice(0, key.indexOf(':') + 1))

/** 这些动作在对应控件显示错误；目录选择器的错误仍显示在工作台。 */
export function hasInlineActionError(origin: string): boolean {
  return origin !== actionOrigin.directoryPicker && inlineActionPrefixes.some(prefix => origin.startsWith(prefix))
}

/** 待处理动作键：方法名与来源。查询方按同一规则在已订阅的 pendingActions
 *  上查自己的键，不再为了读 pending 去碰全局 store。 */
export function pendingKey(method: string, origin?: string): string {
  return [method, origin].filter((value) => value !== undefined && value !== '').join(':')
}

/** 订阅单个 view 消费的字段；stream 水印不会重绘 session 列表。 */
export function sameAppFields(previous: AppState, next: AppState, fields: readonly (keyof AppState)[]): boolean {
  return fields.every(key => {
    if (key !== 'liveSessions') return Object.is(previous[key], next[key])
    const left = previous.liveSessions, right = next.liveSessions
    return Object.keys(left).length === Object.keys(right).length && Object.entries(left).every(([id, value]) =>
      value.phase === right[id]?.phase && value.terminal?.source === right[id]?.terminal?.source
      && value.terminal?.status === right[id]?.terminal?.status && value.terminal?.message === right[id]?.terminal?.message)
  })
}

const ignoreStoreUpdates = (_listener: () => void): (() => void) => () => {}

/** 返回类型只声明已订阅字段：调用方无法通过类型读到未订阅的值。返回的仍是
 *  backing snapshot 本身，不为窄类型构造新对象。 */
export function useAppStore<K extends keyof AppState>(
  fields: readonly K[],
  active = true,
): Pick<AppState, K> {
  const cached = useRef<AppState | null>(null)
  const snapshot = () => {
    const next = appStore.getSnapshot()
    if (cached.current === null || (active && !sameAppFields(cached.current, next, fields))) cached.current = next
    return cached.current
  }
  return useSyncExternalStore(active ? appStore.subscribe : ignoreStoreUpdates, snapshot)
}
