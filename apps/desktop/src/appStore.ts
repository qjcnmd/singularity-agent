import { workspaceKey } from './workspace'
import { SessionStore, type AppState, type QuestionDraft } from './sessionStore'
import { actionOrigin } from './storeActions'
export { actionOrigin, hasInlineActionError, pendingKey } from './storeActions'
export type { AppState, ActionError, FileTab } from './sessionStore'
import { effectiveSelection, selectedModel } from './modelChoices'
import { defaultAnchor, normalizeMessageFontSize, clampSidebarWidth, clampFilePanelWidth, type PersistedView } from './viewPersistence'
import { useRef, useSyncExternalStore } from 'react'
import { emptyDraft, imageUpload, type Draft } from './drafts'
import type { ModelSelection, ProviderConfigurationInput, RpcMethod, RpcParams, ViewportAnchor } from './protocol'
import { selectedSessionPhase } from './sessionState'
import { acceptModelCatalog } from './sync'

type SessionActionMethod = {
  [Method in RpcMethod]: RpcParams<Method> extends { sessionId: string } ? Method : never
}[RpcMethod]

class AppStore extends SessionStore {
  setPage(page: AppState['page']): void {
    this.selectionRequest += 1
    this.patch({ page })
  }

  setDraft(text: string): void {
    const id = this.draftKey()
    if (id === null) return
    const draft = this.inputDraft()
    void this.drafts.set(id, { ...draft, text })
  }

  inputDraft(): Draft {
    const id = this.draftKey()
    return id === null ? emptyDraft : this.drafts.get(id)
  }

  setImages(id: string, images: Draft['images'], append = false): void {
    const draft = this.drafts.get(id)
    void this.drafts.set(id, { ...draft, images: append ? [...draft.images, ...images] : images })
  }

  /** 附件按路径去重；append 为 false 时整体替换（用于移除）。 */
  setFiles(id: string, files: string[], append = false): void {
    const draft = this.drafts.get(id)
    void this.drafts.set(id, { ...draft, files: append ? [...new Set([...draft.files, ...files])] : files })
  }

  /** 新任务页的上下文胶囊：只记关掉的模块；随草稿保存，发送后和草稿一起清空，下次新建任务重新全开。 */
  setModuleEnabled(id: string, enabled: boolean): void {
    const key = this.draftKey()
    if (key === null) return
    const draft = this.inputDraft()
    const rest = (draft.disabledModules ?? []).filter(disabled => disabled !== id)
    void this.drafts.set(key, { ...draft, disabledModules: enabled ? rest : [...rest, id] })
  }

  modelSelection(): ModelSelection | null {
    if (this.state.selectedSessionId !== null) return this.state.session?.runtime.selection ?? null
    return effectiveSelection(this.inputDraft().selection, this.state.bootstrap?.modelCatalog)
  }

  modelAvailable(): boolean {
    return selectedModel(this.state.bootstrap?.modelCatalog,
      this.modelSelection()) !== undefined
  }

  submissionState() {
    const state = this.state
    const draft = this.inputDraft()
    const phase = selectedSessionPhase(state)
    const active = phase === 'running' || phase === 'compacting'
    const submitPending = this.isPending('session.submit', actionOrigin.session(state.selectedSessionId))
    // 按优先级检查当前不能发送的原因。
    let blockedReason: string | null = null
    if (state.selectedWorkspaceRoot === null) blockedReason = '请先选择项目'
    else if (state.connection !== 'ready') blockedReason = '连接未就绪'
    else if (state.bootstrapLoad.status !== 'idle') blockedReason = state.bootstrapLoad.status === 'error' ? '项目读取失败' : '正在读取项目'
    else if (state.sessionLoad.status === 'loading') blockedReason = '正在读取任务'
    else if (state.selectedSessionId !== null && state.session === null) blockedReason = state.sessionLoad.status === 'error'
      ? '任务读取失败' : '正在读取任务'
    else if (phase === 'stopping') blockedReason = '正在停止'
    else if (state.session?.runtime.pendingQuestion) blockedReason = '请先回答问题'
    else if (submitPending) blockedReason = '正在发送…'
    else if (!active && !this.modelAvailable()) blockedReason = '请选择模型'
    return { canSubmit: state.drafts !== null && state.selectedWorkspaceRoot !== null && blockedReason === null && (draft.text.trim() !== '' || draft.images.length > 0 || draft.files.length > 0), blockedReason } as const
  }

  async submitDraft(): Promise<boolean> {
    const { canSubmit } = this.submissionState()
    const sessionId = this.state.selectedSessionId
    const workspaceRoot = this.state.selectedWorkspaceRoot
    const key = this.draftKey()
    if (!canSubmit || workspaceRoot === null || key === null) return false
    const draft = this.inputDraft()
    const selection = this.modelSelection()
    const navigation = this.selectionRequest
    this.setViewportAnchor(defaultAnchor())
    return this.action('session.submit', actionOrigin.session(sessionId), async () => {
      const input = { text: draft.text, images: await Promise.all(draft.images.map(imageUpload)), files: draft.files }
      if (sessionId === null) {
        if (selection === null) throw new Error('请选择模型')
        await this.startSession({ ...input, workspaceRoot, selection, disabledModules: draft.disabledModules ?? [] }, navigation)
      } else await this.transport.rpc('session.submit', { ...input, sessionId })
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

  async deleteSession(sessionId: string): Promise<boolean> {
    return this.action('session.delete', actionOrigin.session(sessionId), async () => {
      await this.transport.rpc('session.delete', { sessionId })
      this.clearSessionAnchors([sessionId])
      await this.drafts.remove([sessionId])
    })
  }

  async updateSettings(selection: ModelSelection): Promise<boolean> {
    if (this.state.selectedSessionId === null) {
      const key = this.draftKey()
      return key !== null ? this.drafts.set(key, { ...this.inputDraft(), selection }) : false
    }
    return this.sessionAction('session.updateSettings', sessionId => ({ sessionId, selection }))
  }

  async removeWorkspace(workspaceRoot: string): Promise<boolean> {
    const sessions = this.state.bootstrap?.sessionsByWorkspace[workspaceRoot] ?? []
    const removedDrafts = sessions.map(session => session.threadId)
    return this.action('workspace.remove', actionOrigin.inline('workspace.remove', workspaceRoot), async () => {
      await this.transport.rpc('workspace.remove', { workspaceRoot })
      this.clearSessionAnchors(removedDrafts)
      this.saveView({ sidebarView: {
        collapsed: this.state.sidebarView.collapsed.filter(id => id !== workspaceRoot),
      } })
      await this.drafts.remove(removedDrafts)
    })
  }

  async saveProvider(provider: ProviderConfigurationInput, apiKey?: string): Promise<boolean> {
    return this.action('model.saveProvider', actionOrigin.inline('model.saveProvider', provider.providerId),
      async () => {
        const result = await this.transport.rpc('model.saveProvider', { provider, apiKey: apiKey || undefined })
        this.applySync(acceptModelCatalog(this.state, result))
      })
  }

  async refreshModels(): Promise<void> {
    try {
      this.applySync(acceptModelCatalog(this.state, await this.transport.rpc('model.read', {})))
    } catch (error) {
      this.reportError(error, 'app')
    }
  }

  async removeProvider(providerId: string): Promise<boolean> {
    return this.action('model.removeProvider', actionOrigin.inline('model.removeProvider', providerId),
      async () => {
        const result = await this.transport.rpc('model.removeProvider', { providerId })
        this.applySync(acceptModelCatalog(this.state, result))
      })
  }

  openDirectoryPicker(): void {
    void this.action('directory.pick', actionOrigin.directoryPicker, async () => {
      const result = await this.transport.rpc('directory.pick', {})
      if (result.path === null) return
      const workspace = await this.transport.rpc('workspace.add', { root: result.path })
      this.newTask(workspaceKey(workspace.root))
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

  clearError(origin: string): void {
    const actionErrors = { ...this.state.actionErrors }
    delete actionErrors[origin]
    this.patch({
      actionErrors,
      actionErrorOrigin: this.state.actionErrorOrigin === origin ? null : this.state.actionErrorOrigin,
    })
  }

  /** 删除任务或移除项目成功后清理相应阅读锚点。 */
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

  setBackgroundOpen(backgroundOpen: boolean): void {
    this.saveView({ backgroundOpen })
  }

  setSidebarView(value: Partial<PersistedView['sidebarView']>): void {
    this.saveView({ sidebarView: { ...this.state.sidebarView, ...value } })
  }


  setMessageFontSize(value: number): void {
    this.saveView({ messageFontSize: normalizeMessageFontSize(value) })
  }

  setTheme(theme: PersistedView['theme']): void {
    this.saveView({ theme })
  }

  /** 打开文件标签；已打开时切过去并重新读取。路径按任务工作目录解析，所以同名相对路径在不同项目里是不同的标签。 */
  openFile(path: string, cwd: string, title = path.split(/[\\/]/).filter(Boolean).pop() ?? path): void {
    let target = path.trim().replace(/\\/g, '/')
    if (target.startsWith('~/') && this.state.bootstrap?.userHome) {
      target = `${this.state.bootstrap.userHome.replace(/\\/g, '/')}/${target.slice(2)}`
    }
    const directory = cwd.replace(/\\/g, '/').replace(/\/+$/, '')
    // 文件 URL 统一相对路径和目录层级；文件名中的百分号、# 和 ? 保留原意。
    const encodePath = (value: string) => encodeURI(value).replace(/[?#]/g, encodeURIComponent)
    const id = new URL(
      encodePath(/^[a-z]:\//i.test(target) ? `file:///${target}` : target),
      encodePath(directory.startsWith('//') ? `file:${directory}/` : `file:///${directory}/`),
    ).href.toLowerCase()
    const fileTabs = this.state.fileTabs.some(tab => tab.id === id)
      ? this.state.fileTabs.map(tab => tab.id === id ? { ...tab, opened: tab.opened + 1 } : tab)
      : [...this.state.fileTabs, { id, path, cwd, title, opened: 0 }]
    this.patch({ fileTabs, activeFileTab: id })
  }

  closeFile(id: string): void {
    const index = this.state.fileTabs.findIndex(tab => tab.id === id)
    if (index < 0) return
    const fileTabs = this.state.fileTabs.filter(tab => tab.id !== id)
    // 关掉当前标签时切到右边一个，没有就左边一个。
    const activeFileTab = this.state.activeFileTab !== id ? this.state.activeFileTab : (fileTabs[index] ?? fileTabs[index - 1])?.id ?? null
    this.patch({ fileTabs, activeFileTab })
  }

  selectFile(id: string): void {
    this.patch({ activeFileTab: id })
  }

  setFilePanelWidth(value: number): void {
    this.saveView({ filePanelWidth: clampFilePanelWidth(value) }, true)
  }

  setBubbleColor(kind: 'assistantBubble' | 'userBubble', id: string): void {
    this.saveView({ [kind]: id })
  }
}


export const appStore = new AppStore()

/** 订阅指定的状态字段；默认按字段引用比较，也可传入自定义相等判断。 */
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
