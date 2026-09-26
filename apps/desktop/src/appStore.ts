import { AppStoreCore, SESSION_PAGE_SIZE, type AppState } from './appStoreCore'
import { actionOrigin, pendingKey } from './storeActions'
export { actionOrigin, hasInlineActionError, pendingKey } from './storeActions'
export type { AppState, ActionError } from './appStoreCore'
import { prependExecutionHistory } from './execution'
import { isBlankSession } from './sessionState'
import { defaultAnchor, persistDraft, normalizeMessageFontSize, clampSidebarWidth, type PersistedView, type WorkspaceAppearance } from './viewPersistence'
export type { WorkspaceAppearance } from './viewPersistence'
import { useRef, useSyncExternalStore } from 'react'
import { RpcFailure } from './rpcClient'
import type { DeliveryIntent, ProviderConfigurationInput, ThreadSummary, ViewportAnchor } from './protocol'

class AppStore extends AppStoreCore {
  async retrySession(): Promise<void> {
    const workspaceId = this.state.selectedWorkspaceId
    const sessionId = this.state.selectedSessionId
    if (sessionId !== null) await this.readSession(workspaceId, sessionId)
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
    if (workspaceId === null) return false
    if (this.isPending('session.create', actionOrigin.workspace(workspaceId))) return false
    if (this.state.sidebarView.collapsed.includes(workspaceId)) {
      this.setSidebarView({ collapsed: this.state.sidebarView.collapsed.filter(id => id !== workspaceId) })
    }
    const sourceKey = this.draftKey()
    const sourceDraft = transferDraft ? this.draft() : ''
    const blank = this.sessions(workspaceId).find((session) =>
      isBlankSession(session)
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
    return this.createSelectedSession(workspaceId, sessionId => {
      const newDraft = this.state.drafts[newDraftKey] ?? ''
      if (newDraft !== '') this.moveDraft(newDraftKey, sessionId, newDraft)
    })
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
  private readonly runtimeSynced = (): boolean =>
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
      this.clearSessionView([sessionId])
    })
  }

  async updateSettings(selector: string): Promise<boolean> {
    const workspaceId = this.state.selectedWorkspaceId
    if (this.state.selectedSessionId === null && !await this.createSession(workspaceId, true)) return false
    return this.sessionAction('session.updateSettings', ids => this.transport.rpc('session.updateSettings', { ...ids, selector }))
  }

  private async addWorkspace(root: string): Promise<boolean> {
    return this.action('workspace.add', actionOrigin.directoryPicker, async () => {
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
      this.clearSessionView([...sessions.map(session => session.threadId), `new:${workspaceId}`])
      const workspaceAppearance = { ...this.state.workspaceAppearance }
      delete workspaceAppearance[workspaceId]
      this.saveView({ workspaceAppearance, sidebarView: {
        collapsed: this.state.sidebarView.collapsed.filter(id => id !== workspaceId),
      } })
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
    if (previous === undefined && anchor.mode === 'following') return
    if (previous?.mode === anchor.mode
      && previous.anchorItemId === anchor.anchorItemId
      && Math.abs(previous.offset - anchor.offset) < 1) return
    const viewportAnchors = { ...this.state.viewportAnchors }
    if (anchor.mode === 'following') delete viewportAnchors[id]
    else viewportAnchors[id] = anchor
    this.saveView({ viewportAnchors })
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
    const drafts = { ...this.state.drafts }
    if (text === '') delete drafts[key]
    else drafts[key] = text
    this.patch({ drafts })
    try {
      persistDraft(key, text)
    } catch {
      this.reportError(new RpcFailure('storage', '草稿暂时只能保留在当前页面。', '请复制草稿后检查本地存储空间。'), actionOrigin.session(key))
    }
  }

  /** 只清理已成功归档/移除的对象；在途期间输入的非空草稿仍可恢复。 */
  private clearSessionView(ids: string[]): void {
    const viewportAnchors = { ...this.state.viewportAnchors }
    const drafts = { ...this.state.drafts }
    let storageError: unknown
    for (const id of ids) {
      delete viewportAnchors[id]
      if (drafts[id] === '') {
        delete drafts[id]
        try { persistDraft(id, '') } catch (error) { storageError = error }
      }
    }
    this.patch({ drafts })
    this.saveView({ viewportAnchors })
    if (storageError) this.reportError(storageError, 'app')
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

/** 订阅单个 view 消费的字段；stream 水印不会重绘 session 列表。 */
function sameAppFields(previous: Partial<AppState>, next: AppState, fields: readonly (keyof AppState)[]): boolean {
  return fields.every(key => {
    if (key !== 'liveSessions') return Object.is(previous[key], next[key])
    const left = previous.liveSessions, right = next.liveSessions
    return left !== undefined && Object.keys(left).length === Object.keys(right).length && Object.entries(left).every(([id, value]) =>
      value.phase === right[id]?.phase && value.terminal?.source === right[id]?.terminal?.source
      && value.terminal?.status === right[id]?.terminal?.status && value.terminal?.message === right[id]?.terminal?.message
      && value.terminal?.manuallyStopped === right[id]?.terminal?.manuallyStopped)
  })
}

/** 订阅与缓存都只持有所需字段，避免不读取会话的组件保留旧正文和执行快照。 */
export function useAppStore<K extends keyof AppState>(
  fields: readonly K[],
): Pick<AppState, K> {
  const cached = useRef<Pick<AppState, K> | null>(null)
  const snapshot = () => {
    const next = appStore.getSnapshot()
    if (cached.current === null || !sameAppFields(cached.current, next, fields)) {
      cached.current = Object.fromEntries(fields.map(key => [key, next[key]])) as Pick<AppState, K>
    }
    return cached.current
  }
  return useSyncExternalStore(appStore.subscribe, snapshot)
}
