import { workspaceKey } from './workspace'
import { SessionStore, actionOrigin, type AppState, type QuestionDraft } from './sessionStore'
export { actionOrigin, hasInlineActionError, pendingKey } from './sessionStore'
export type { AppState, ActionError, FileTab } from './sessionStore'
import { closeFileTab, closeWorkspaceTabs, openChangedFilesTab, openFileTab, selectFileTab } from './fileTabs'
import { selectedModel } from './models'
import { defaultAnchor, normalizeMessageFontSize, clampSidebarWidth, clampFilePanelWidth, type PersistedView } from './viewPersistence'
import { useRef, useSyncExternalStore } from 'react'
import { emptyDraft, hasDraftContent, imageUpload, type Draft, type DraftAnnotation } from './drafts'
import type { ImageAttachment, MessageAnnotation, ModelSelection, ProviderConfigurationInput, RpcMethod, RpcParams, UserQuestionAnswer, ViewportAnchor } from './protocol'
import { selectedSessionPhase } from './sessionState'
import { acceptModelCatalog } from './sync'

type SessionActionMethod = {
  [Method in RpcMethod]: RpcParams<Method> extends { sessionId: string } ? Method : never
}[RpcMethod]

class AppStore extends SessionStore {
  private anchorRevision = 0

  get viewportAnchorRevision(): number {
    return this.anchorRevision
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

  setFiles(id: string, files: string[], append = false): void {
    const draft = this.drafts.get(id)
    void this.drafts.set(id, { ...draft, files: append ? [...new Set([...draft.files, ...files])] : files })
  }

  addAnnotation(annotation: Omit<DraftAnnotation, 'id'>): void {
    const key = this.draftKey()
    if (key === null || this.state.selectedSessionId === null) return
    const draft = this.inputDraft()
    void this.drafts.set(key, {
      ...draft,
      annotations: [...draft.annotations, { ...annotation, id: crypto.randomUUID() }],
    })
  }

  updateAnnotation(id: string, comment: string): void {
    const key = this.draftKey()
    if (key === null) return
    const draft = this.inputDraft()
    const annotations = draft.annotations.map(item => (item.id === id ? { ...item, comment } : item))
    void this.drafts.set(key, { ...draft, annotations })
  }

  removeAnnotation(id: string): void {
    const key = this.draftKey()
    if (key === null) return
    const draft = this.inputDraft()
    void this.drafts.set(key, {
      ...draft,
      annotations: draft.annotations.filter(item => item.id !== id),
    })
  }

  clearAnnotations(): void {
    const key = this.draftKey()
    if (key === null) return
    const draft = this.inputDraft()
    if (draft.annotations.length === 0) return
    void this.drafts.set(key, { ...draft, annotations: [] })
  }

  setModuleEnabled(id: string, enabled: boolean): void {
    const key = this.draftKey()
    if (key === null) return
    const draft = this.inputDraft()
    const rest = (draft.disabledModules ?? []).filter(disabled => disabled !== id)
    void this.drafts.set(key, { ...draft, disabledModules: enabled ? rest : [...rest, id] })
  }

  modelSelection(): ModelSelection | null {
    if (this.state.selectedSessionId !== null) return this.state.session?.runtime.selection ?? null
    return this.inputDraft().selection ?? this.state.bootstrap?.modelCatalog.defaultSelection ?? null
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
    let blockedReason: string | null = null
    if (state.selectedWorkspaceRoot === null) blockedReason = '请先选择项目'
    else if (state.bootstrapLoad.status !== 'idle') blockedReason = state.bootstrapLoad.status === 'error' ? '项目读取失败' : '正在读取项目'
    else if (state.sessionLoad.status === 'loading' || (state.selectedSessionId !== null && state.session === null && state.sessionLoad.status !== 'error')) blockedReason = '正在读取任务'
    else if (state.selectedSessionId !== null && state.session === null) blockedReason = '任务读取失败'
    else if (phase === 'stopping') blockedReason = '正在停止'
    else if (state.session?.runtime.pendingQuestion) blockedReason = '请先回答问题'
    else if (submitPending) blockedReason = '正在发送…'
    else if (!active && !this.modelAvailable()) blockedReason = '请选择模型'
    return {
      canSubmit: state.drafts !== null
        && blockedReason === null
        && hasDraftContent(draft),
      blockedReason,
    } as const
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
      } else {
        const annotations = draft.annotations.map(({ messageId, quote, comment }) => ({ messageId, quote, comment }))
        await this.transport.rpc('session.submit', { ...input, sessionId, annotations })
      }
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

  async answerQuestion(itemId: string, answers: UserQuestionAnswer[]): Promise<boolean> {
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
    return this.action('session.delete', actionOrigin.inline('session.delete', sessionId), async () => {
      await this.transport.rpc('session.delete', { sessionId })
      this.clearSessionAnchors([sessionId])
      this.clearQuestionDrafts([sessionId])
      await this.drafts.remove([sessionId])
    })
  }

  async editMessage(
    sessionId: string,
    message: { id: string; images?: ImageAttachment[]; files?: string[]; annotations?: MessageAnnotation[] },
    nextText: string,
  ): Promise<boolean> {
    const hasNonText = (message.images?.length ?? 0) > 0
      || (message.files?.length ?? 0) > 0
      || (message.annotations?.length ?? 0) > 0
    if (nextText.trim() === '' && !hasNonText) return false
    return this.action('session.rewind', actionOrigin.inline('session.rewind', sessionId), async () => {
      if (!this.modelAvailable()) throw new Error('请选择模型')
      const images = message.images?.length
        ? await Promise.all(message.images.map(async image => ({
            name: image.name,
            dataUrl: await this.transport.rpc('session.imageRead', { sessionId, imageId: image.id }),
          })))
        : []
      const files = [...(message.files ?? [])]
      const annotations = (message.annotations ?? []).map(({ messageId, quote, comment }) => ({ messageId, quote, comment }))
      const rewound = await this.transport.rpc('session.rewind', {
        sessionId,
        messageId: message.id,
        text: nextText,
        images,
        files,
        annotations,
      })
      this.applyRewoundSession(rewound)
      if (this.state.selectedSessionId === sessionId) this.setViewportAnchor(defaultAnchor())
      else this.clearSessionAnchors([sessionId])
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
      const nextTabs = closeWorkspaceTabs(this.state, workspaceRoot)
      if (nextTabs) this.patch(nextTabs)
      this.clearSessionAnchors(removedDrafts)
      this.clearQuestionDrafts(removedDrafts)
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

  async saveWebSearch(baseUrl: string, apiKey?: string): Promise<boolean> {
    return this.action('model.saveWebSearch', actionOrigin.inline('model.saveWebSearch', 'web'),
      async () => {
        const params = apiKey === undefined ? { baseUrl } : { baseUrl, apiKey }
        const result = await this.transport.rpc('model.saveWebSearch', params)
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
    this.anchorRevision++
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

  private clearQuestionDrafts(ids: string[]): void {
    const remove = new Set(ids)
    const entries = Object.entries(this.state.questionDrafts).filter(([id]) => !remove.has(id))
    if (entries.length !== Object.keys(this.state.questionDrafts).length) {
      this.patch({ questionDrafts: Object.fromEntries(entries) })
    }
  }

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


  setMessageFontSize(value: number): number {
    const messageFontSize = normalizeMessageFontSize(value)
    this.saveView({ messageFontSize })
    return messageFontSize
  }

  setTheme(theme: PersistedView['theme']): void {
    this.saveView({ theme })
  }

  setStepsDefaultExpanded(stepsDefaultExpanded: boolean): void {
    this.saveView({ stepsDefaultExpanded })
  }

  openFile(path: string, cwd: string, title?: string): void {
    const next = openFileTab(this.state, path, cwd, this.state.bootstrap?.userHome, title)
    if (!next) return
    this.patch(this.state.settingsOpen ? { ...next, settingsOpen: false } : next)
  }

  openChangedFiles(cwd: string, focusPath?: string): void {
    const next = openChangedFilesTab(this.state, cwd, focusPath)
    this.patch(this.state.settingsOpen ? { ...next, settingsOpen: false } : next)
  }

  closeFile(id: string): void {
    const next = closeFileTab(this.state, id)
    if (next) this.patch(next)
  }

  selectFile(id: string): void {
    this.patch(selectFileTab(this.state, id))
  }

  setFilePanelWidth(value: number): void {
    this.saveView({ filePanelWidth: clampFilePanelWidth(value) }, true)
  }

  setAccentColor(accentColor: string): void {
    this.saveView({ accentColor })
  }
}


export const appStore = new AppStore()

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
