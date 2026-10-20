import { SessionStore, type AppState, type QuestionDraft } from './sessionStore'
import { actionOrigin } from './storeActions'
export { actionOrigin, hasInlineActionError, pendingKey } from './storeActions'
export type { AppState, ActionError } from './sessionStore'
import { effectiveSelector, selectedModel } from './modelChoices'
import { defaultAnchor, normalizeMessageFontSize, clampSidebarWidth, type PersistedView, type WorkspaceAppearance } from './viewPersistence'
export type { WorkspaceAppearance } from './viewPersistence'
import { useRef, useSyncExternalStore } from 'react'
import { emptyDraft, imageUpload, type Draft } from './drafts'
import type { ProviderConfigurationInput, RpcMethod, RpcParams, SkillMetadata, ViewportAnchor } from './protocol'
import { selectedSessionPhase } from './sessionState'

type SessionActionMethod = {
  [Method in RpcMethod]: RpcParams<Method> extends { sessionId: string } ? Method : never
}[RpcMethod]

class AppStore extends SessionStore {
  setDraft(text: string, skill?: SkillMetadata): void {
    const id = this.draftKey()
    if (id === null) return
    const draft = this.inputDraft()
    const skills = Object.fromEntries(Object.entries(draft.skills ?? {}).filter(([name]) => text.includes(`/${name}`)))
    if (skill !== undefined) skills[skill.name] = skill.path
    void this.drafts.set(id, { ...draft, text, skills })
  }

  inputDraft(): Draft {
    const id = this.draftKey()
    return id === null ? emptyDraft : this.drafts.get(id)
  }

  setImages(id: string, images: Draft['images'], append = false): void {
    const draft = this.drafts.get(id)
    void this.drafts.set(id, { ...draft, images: append ? [...draft.images, ...images] : images })
  }

  modelSelector(): string | null {
    return effectiveSelector(this.state.session?.runtime.selector ?? this.inputDraft().selector, this.state.bootstrap?.modelCatalog)
  }

  modelAvailable(): boolean {
    return selectedModel(this.state.bootstrap?.modelCatalog,
      this.modelSelector()) !== undefined
  }

  submissionState() {
    const state = this.state
    const draft = this.inputDraft()
    const phase = selectedSessionPhase(state)
    const active = phase === 'running' || phase === 'compacting' || phase === 'reserved'
    const submitPending = this.isPending('session.submit', actionOrigin.session(state.selectedSessionId))
    // 按任务选择、连接、任务读取和运行阶段给出第一项阻止原因。
    let blockedReason: string | null = null
    if (state.selectedWorkspaceId === null) blockedReason = '请先选择项目'
    else if (state.connection !== 'ready') blockedReason = '连接未就绪'
    else if (state.sessionLoad.status === 'loading') blockedReason = '正在读取任务'
    else if (state.selectedSessionId !== null && state.session === null) blockedReason = state.sessionLoad.status === 'error'
      ? '任务读取失败' : '正在读取任务'
    else if (phase === 'stopping') blockedReason = '正在停止'
    else if (submitPending) blockedReason = '正在发送…'
    else if (!active && !this.modelAvailable()) blockedReason = '请选择模型'
    return { canSubmit: state.drafts !== null && state.selectedWorkspaceId !== null && blockedReason === null && (draft.text.trim() !== '' || draft.images.length > 0), blockedReason } as const
  }

  async submitDraft(): Promise<boolean> {
    const { canSubmit } = this.submissionState()
    const sessionId = this.state.selectedSessionId
    const workspaceId = this.state.selectedWorkspaceId
    const key = this.draftKey()
    if (!canSubmit || workspaceId === null || key === null) return false
    const draft = this.inputDraft()
    const selector = this.modelSelector()
    const selection = this.selectionRequest
    return this.action('session.submit', actionOrigin.session(sessionId), async () => {
      const input = { text: draft.text, skills: draft.skills ?? {}, images: await Promise.all(draft.images.map(imageUpload)) }
      if (sessionId === null) await this.startSession({ ...input, workspaceId, selector }, selection)
      else await this.transport.rpc('session.submit', { ...input, sessionId })
      await this.drafts.clearIfUnchanged(key, draft)
    })
  }

  async stopActive(): Promise<boolean> {
    return this.sessionAction('session.abort', sessionId => ({ sessionId }))
  }

  async stopBackgroundTask(sessionId: string, taskId: string): Promise<boolean> {
    return this.action('session.stopBackgroundTask', actionOrigin.control(sessionId, taskId),
      () => this.transport.rpc('session.stopBackgroundTask', { sessionId, taskId }))
  }

  setQuestionDraft(sessionId: string, draft: QuestionDraft): void {
    this.patch({ questionDrafts: { ...this.state.questionDrafts, [sessionId]: draft } })
  }

  async answerQuestion(itemId: string, answers: import('./protocol').UserQuestionAnswer[]): Promise<boolean> {
    const sessionId = this.state.selectedSessionId
    const accepted = await this.sessionAction('session.answerQuestion', sessionId => ({ sessionId, itemId, answers }), itemId)
    if (accepted && sessionId !== null && this.state.questionDrafts[sessionId]?.itemId === itemId) {
      this.retainQuestionDraft(sessionId)
    }
    return accepted
  }

  async compact(): Promise<boolean> {
    if (!this.modelAvailable()) return false
    return this.sessionAction('session.compact', sessionId => ({ sessionId }))
  }

  async renameSession(sessionId: string, name: string): Promise<boolean> {
    if (name.trim() === '') return false
    return this.action('session.rename', actionOrigin.inline('session.rename', sessionId),
      () => this.transport.rpc('session.rename', { sessionId, name }))
  }

  async archiveSession(sessionId: string): Promise<boolean> {
    return this.action('session.archive', actionOrigin.session(sessionId), async () => {
      await this.transport.rpc('session.archive', { sessionId })
      this.clearSessionAnchors([sessionId])
      await this.drafts.remove([sessionId], '任务已归档')
    })
  }

  async updateSettings(selector: string): Promise<boolean> {
    if (this.state.selectedSessionId === null) {
      const key = this.draftKey()
      return key !== null ? this.drafts.set(key, { ...this.inputDraft(), selector }) : false
    }
    return this.sessionAction('session.updateSettings', sessionId => ({ sessionId, selector }))
  }

  async removeWorkspace(workspaceId: string): Promise<boolean> {
    const sessions = this.state.bootstrap?.sessionsByWorkspace[workspaceId] ?? []
    const removedDrafts = sessions.map(session => session.threadId)
    return this.action('workspace.remove', actionOrigin.inline('workspace.remove', workspaceId), async () => {
      await this.transport.rpc('workspace.remove', { workspaceId })
      this.clearSessionAnchors(removedDrafts)
      const workspaceAppearance = { ...this.state.workspaceAppearance }
      delete workspaceAppearance[workspaceId]
      this.saveView({ workspaceAppearance, sidebarView: {
        collapsed: this.state.sidebarView.collapsed.filter(id => id !== workspaceId),
      } })
      await this.drafts.remove(removedDrafts, '项目已移除')
    })
  }

  async saveProvider(provider: ProviderConfigurationInput, apiKey?: string): Promise<boolean> {
    return this.action('model.saveProvider', actionOrigin.inline('model.saveProvider', provider.providerId),
      () => this.transport.rpc('model.saveProvider', { provider, apiKey: apiKey || undefined }))
  }

  async removeProvider(providerId: string): Promise<boolean> {
    return this.action('model.removeProvider', actionOrigin.inline('model.removeProvider', providerId),
      () => this.transport.rpc('model.removeProvider', { providerId }))
  }

  openDirectoryPicker(): void {
    void this.action('directory.pick', actionOrigin.directoryPicker, async () => {
      const result = await this.transport.rpc('directory.pick', {})
      if (result.path === null) return
      const workspace = await this.transport.rpc('workspace.add', { root: result.path })
      this.newTask(workspace.workspaceId)
    })
  }

  setSettingsOpen(settingsOpen: boolean): void {
    this.patch({ settingsOpen })
  }

  setSidebarWidth(sidebarWidth: number): void {
    this.saveView({ sidebarWidth: clampSidebarWidth(sidebarWidth) }, true)
  }

  setBackgroundWidth(backgroundWidth: number): void {
    this.saveView({ backgroundWidth: clampSidebarWidth(backgroundWidth) }, true)
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
      && previous.turnId === anchor.turnId
      && Math.abs(previous.offset - anchor.offset) < 1) return
    const viewportAnchors = { ...this.state.viewportAnchors }
    if (anchor.mode === 'following') delete viewportAnchors[id]
    else viewportAnchors[id] = anchor
    this.saveView({ viewportAnchors }, true)
  }

  clearError(origin: string): void {
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

  private async sessionAction<M extends SessionActionMethod>(
    method: M,
    params: (sessionId: string) => RpcParams<M>,
    target?: string,
  ): Promise<boolean> {
    const sessionId = this.state.selectedSessionId
    if (sessionId === null) return false
    const origin = target === undefined ? actionOrigin.session(sessionId) : actionOrigin.control(sessionId, target)
    return this.action(method, origin,
      () => this.transport.rpc(method, params(sessionId)))
  }

  setSidebarView(value: Partial<PersistedView['sidebarView']>): void {
    this.saveView({ sidebarView: { ...this.state.sidebarView, ...value } })
  }

  setBackgroundOpen(backgroundOpen: boolean): void {
    this.saveView({ backgroundOpen })
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
