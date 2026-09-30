import { AppStoreCore, SESSION_PAGE_SIZE, type AppState } from './appStoreCore'
import { actionOrigin } from './storeActions'
export { actionOrigin, hasInlineActionError, pendingKey } from './storeActions'
export type { AppState, ActionError } from './appStoreCore'
import { prependExecutionHistory } from './execution'
import { isBlankSession } from './sessionState'
import { effectiveSelector, selectedModel } from './modelChoices'
import { defaultAnchor, normalizeMessageFontSize, clampSidebarWidth, type PersistedView, type WorkspaceAppearance } from './viewPersistence'
export type { WorkspaceAppearance } from './viewPersistence'
import { useRef, useSyncExternalStore } from 'react'
import { RpcFailure } from './rpcClient'
import type { DeliveryIntent, ProviderConfigurationInput, ThreadSummary, ViewportAnchor } from './protocol'

class AppStore extends AppStoreCore {
  async retrySession(): Promise<void> {
    const sessionId = this.state.selectedSessionId
    if (sessionId !== null) await this.readSession(sessionId)
  }

  async selectSession(sessionId: string): Promise<void> {
    const workspaceId = this.workspaceForSession(sessionId)
    if (workspaceId === undefined) return
    if (sessionId === this.state.selectedSessionId) {
      this.selectionRequest += 1
      if (this.state.session === null) await this.readSession(sessionId)
      return
    }
    this.beginSessionSelection(workspaceId, sessionId)
    await this.readSession(sessionId)
  }

  async createSession(workspaceId = this.state.selectedWorkspaceId, transferDraft = false): Promise<boolean> {
    if (workspaceId === null) return false
    if (this.isPending('session.create', actionOrigin.workspace(workspaceId))) return false
    const selection = ++this.selectionRequest
    const sourceKey = this.state.selectedSessionId
    if (this.state.sidebarView.collapsed.includes(workspaceId)) {
      this.setSidebarView({ collapsed: this.state.sidebarView.collapsed.filter(id => id !== workspaceId) })
    }
    const blank = this.sessions(workspaceId).find((session) =>
      isBlankSession(session)
      && (this.state.liveSessions[session.threadId]?.phase ?? 'idle') === 'idle'
      && (!transferDraft || sourceKey === session.threadId || (this.state.drafts[session.threadId] ?? '') === ''))
    return this.selectDraftSession(workspaceId, blank?.threadId ?? null, selection, sessionId => {
      if (!transferDraft || sourceKey === null || sourceKey === sessionId) return
      const text = this.state.drafts[sourceKey] ?? ''
      if (text !== '' && this.setDraftFor(sessionId, text)) this.setDraftFor(sourceKey, '')
    })
  }

  async readOlder(): Promise<boolean> {
    const { selectedSessionId, session } = this.state
    const beforeTurn = session?.nextCursor
    if (selectedSessionId === null || beforeTurn == null) return false
    return this.action('history.older', actionOrigin.session(selectedSessionId), async () => {
      const older = await this.transport.rpc('session.read', {
        sessionId: selectedSessionId,
        beforeTurn,
        limit: SESSION_PAGE_SIZE,
      })
      if (this.state.selectedSessionId !== selectedSessionId
        || this.state.session?.nextCursor !== beforeTurn) return
      this.patch({ session: prependExecutionHistory(this.state.session, older.history) })
    })
  }

  setDraft(text: string): void {
    const id = this.state.selectedSessionId
    if (id !== null) this.setDraftFor(id, text)
  }

  draft(): string {
    const id = this.state.selectedSessionId
    return id === null ? '' : this.state.drafts[id] ?? ''
  }

  /** 按 phase 路由的动作只有在所选 session 的 runtime 快照可信后才会触发。 */
  private runtimeSynced(): boolean {
    return this.state.connection === 'ready' && this.state.sessionLoad.status !== 'loading'
  }

  modelAvailable(): boolean {
    return selectedModel(this.state.bootstrap?.modelCatalog,
      effectiveSelector(this.state.session?.runtime.selector, this.state.bootstrap?.modelCatalog)) !== undefined
  }

  submissionState(intent: DeliveryIntent = 'follow_up') {
    const state = this.state
    const phase = state.session?.runtime.phase ?? 'idle'
    const submitPending = ['session.submit', 'session.followUp', 'session.steer'].some(method => this.isPending(method, actionOrigin.session(state.selectedSessionId)))
    // 按连接、任务读取、运行阶段和在途提交的顺序给出第一项阻止原因。
    let blockedReason: string | null = null
    if (state.connection !== 'ready') blockedReason = '连接恢复后即可发送，草稿会保留。'
    else if (!this.runtimeSynced()) blockedReason = '正在同步任务状态，稍后即可发送。'
    else if (state.selectedSessionId !== null && state.session === null) blockedReason = state.sessionLoad.status === 'error'
      ? '任务读取失败，请点击上方“重试读取”。' : '正在读取任务，稍后即可发送。'
    else if (phase === 'stopping') blockedReason = '正在停止当前任务，结束后即可发送。'
    else if (phase === 'reserved') blockedReason = '正在启动任务，稍后可继续发送。'
    else if (phase === 'compacting') blockedReason = '上下文整理完成后即可发送，也可以先停止整理。'
    else if (submitPending) blockedReason = '正在发送…'
    else if (!(phase === 'running' && intent === 'steer') && !this.modelAvailable()) blockedReason = '请选择模型后发送。'
    const method = phase === 'running'
      ? intent === 'steer' ? 'session.steer' : 'session.followUp'
      : 'session.submit'
    return { canSubmit: state.selectedWorkspaceId !== null && blockedReason === null && this.draft().trim() !== '', blockedReason, method } as const
  }

  async submitDraft(intent: DeliveryIntent = 'follow_up'): Promise<boolean> {
    const { canSubmit, method } = this.submissionState(intent)
    const sessionId = this.state.selectedSessionId
    if (!canSubmit || sessionId === null) return false
    const text = this.state.drafts[sessionId] ?? ''
    return this.action(method, actionOrigin.session(sessionId), async () => {
      await this.transport.rpc(method, { sessionId, text })
      if ((this.state.drafts[sessionId] ?? '') === text) this.setDraftFor(sessionId, '')
    })
  }

  async stopActive(): Promise<boolean> {
    return this.sessionAction('session.abort', ids => this.transport.rpc('session.abort', ids))
  }

  async compact(): Promise<boolean> {
    if (!this.modelAvailable()) return false
    return this.sessionAction('session.compact', ids => this.transport.rpc('session.compact', ids))
  }

  async withdraw(controlId: string): Promise<boolean> {
    return this.sessionAction('session.queueWithdraw', ids => this.transport.rpc('session.queueWithdraw', { ...ids, controlId }), controlId)
  }

  async replace(controlId: string, text: string): Promise<boolean> {
    return this.sessionAction('session.queueReplace', ids => this.transport.rpc('session.queueReplace', { ...ids, controlId, text }), controlId)
  }

  /** 指定条目立即发送，省略身份时发送全部待执行输入。 */
  canSendNow(): boolean {
    const phase = this.state.session?.runtime.phase
    return this.runtimeSynced() && (phase === 'running' || (phase === 'idle' && this.modelAvailable()))
  }

  async sendNow(controlId?: string): Promise<boolean> {
    if (!this.canSendNow()) return false
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
    if (name.trim() === '') return false
    return this.action('session.rename', actionOrigin.session(sessionId), async () => {
      await this.transport.rpc('session.rename', { sessionId, name })
    })
  }

  async archiveSession(sessionId: string): Promise<boolean> {
    return this.action('session.archive', actionOrigin.session(sessionId), async () => {
      await this.transport.rpc('session.archive', { sessionId })
      this.clearSessionAnchors([sessionId])
    })
  }

  async updateSettings(selector: string): Promise<boolean> {
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
    const hasDraft = sessions.some((session) => this.state.drafts[session.threadId]?.trim())
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
      this.clearSessionAnchors(sessions.map(session => session.threadId))
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
    this.saveView({ sidebarWidth: clampSidebarWidth(sidebarWidth) }, true)
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
    this.saveView({ viewportAnchors }, true)
  }

  clearError(origin?: string): void {
    if (origin === undefined) {
      this.patch({ actionErrors: {}, actionErrorOrigin: null })
      return
    }
    const actionErrors = { ...this.state.actionErrors }
    delete actionErrors[origin]
    this.patch({
      actionErrors,
      actionErrorOrigin: this.state.actionErrorOrigin === origin ? null : this.state.actionErrorOrigin,
    })
  }

  /** 归档任务或移除项目成功后清理相应阅读锚点。 */
  private clearSessionAnchors(ids: string[]): void {
    const viewportAnchors = { ...this.state.viewportAnchors }
    for (const id of ids) {
      delete viewportAnchors[id]
    }
    this.saveView({ viewportAnchors })
  }

  private async sessionAction(
    method: import('./protocol').RpcMethod,
    operation: (ids: import('./protocol').SessionParams) => Promise<null>,
    target?: string,
  ): Promise<boolean> {
    const sessionId = this.state.selectedSessionId
    if (sessionId === null) return false
    const origin = target === undefined ? actionOrigin.session(sessionId) : actionOrigin.control(sessionId, target)
    return this.action(method, origin, async () => { await operation({ sessionId }) })
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

/** 默认按字段引用订阅；消费者若只显示部分内容，可显式提供自己的比较规则。 */
export function useAppStore<K extends keyof AppState>(
  fields: readonly K[],
  equal?: (previous: Pick<AppState, K>, next: Pick<AppState, K>) => boolean,
): Pick<AppState, K> {
  const cached = useRef<Pick<AppState, K> | null>(null)
  const snapshot = () => {
    const state = appStore.getSnapshot()
    const next = Object.fromEntries(fields.map(key => [key, state[key]])) as Pick<AppState, K>
    const previous = cached.current
    if (previous !== null && (equal ? equal(previous, next) : fields.every(key => Object.is(previous[key], next[key])))) return previous
    cached.current = next
    return next
  }
  return useSyncExternalStore(appStore.subscribe, snapshot)
}
