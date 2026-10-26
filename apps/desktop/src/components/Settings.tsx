import { useState } from 'react'
import { actionOrigin, appStore, pendingKey, useAppStore } from '../appStore'
import type { RedactedProvider } from '../protocol'
import { messageFontSize } from '../viewPersistence'
import { ProviderEditor } from './ProviderEditor'
import { Dialog } from './Dialog'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'
import { X } from 'lucide-react'

type ProviderEditorState = null | 'new' | RedactedProvider

export function Settings() {
  const state = useAppStore(['theme', 'bootstrap', 'settingsOpen', 'messageFontSize', 'actionErrors', 'pendingActions', 'selectedWorkspaceRoot'])
  const [editor, setEditor] = useState<ProviderEditorState>(null)
  const [editorVersion, setEditorVersion] = useState(0)
  const [removing, setRemoving] = useState<RedactedProvider | null>(null)
  const [refreshing, setRefreshing] = useState(false)
  const busy = [...state.pendingActions].some(key =>
    ['model.saveProvider', 'model.removeProvider'].some(method => key.startsWith(pendingKey(method, ''))))
  const catalog = state.bootstrap?.modelCatalog
  const providers = catalog?.providers ?? []
  // 正在编辑的 Provider 优先显示在列表中，保存或读取失败时表单不被新数据冲掉
  const editingProvider = editor === 'new' ? null : editor
  const providerRows = editingProvider && !providers.some(provider => provider.providerId === editingProvider.providerId)
    ? [...providers, editingProvider] : providers
  const unreadable = Boolean(catalog?.error && providers.length === 0)
  function openEditor(next: Exclude<ProviderEditorState, null>) {
    // 收起动画尚未结束就重新打开时，也从已保存配置开始编辑。
    setEditorVersion(version => version + 1)
    setEditor(next)
  }
  function close() {
    if (busy) return
    setEditor(null)
    setRemoving(null)
    appStore.setSettingsOpen(false)
  }

  function requestRemoval(provider: RedactedProvider) {
    appStore.clearError(actionOrigin.inline('model.removeProvider', provider.providerId))
    setRemoving(provider)
  }

  async function removeProvider() {
    if (removing && await appStore.removeProvider(removing.providerId)) {
      setEditor(current => current !== 'new' && current?.providerId === removing.providerId ? null : current)
      setRemoving(null)
    }
  }

  function changeMessageFontSize(value: string) {
    if (value !== '') appStore.setMessageFontSize(Number(value))
  }

  async function rereadModelConfiguration() {
    if (refreshing) return
    setRefreshing(true)
    try { await appStore.refreshModels() }
    finally { setRefreshing(false) }
  }
  return (
    <Dialog open={state.settingsOpen} onClose={close} labelledBy="settings-title" className="sg-settings-modal">
      <header className="modal-header">
        <h2 id="settings-title">设置</h2>
        <button type="button" className="icon-button" data-autofocus disabled={busy} onClick={close} aria-label="关闭设置"><X size={18} /></button>
      </header>
      <div className="sg-settings-content">
        <section className="settings-section" aria-labelledby="settings-appearance">
          <h3 id="settings-appearance">外观</h3>
          <div className="settings-group">
            <div className="settings-row"><span>主题</span>
              <div className="segmented" role="radiogroup" aria-label="主题">
                {([['light', '浅色'], ['dark', '深色']] as const).map(([theme, label]) => <button key={theme} type="button" role="radio" aria-checked={state.theme === theme} onClick={() => appStore.setTheme(theme)}>{label}</button>)}
              </div>
            </div>
            <div className="settings-row"><span>消息字号</span>
              <div className="stepper">
                <button type="button" className="icon-button small" aria-label="减小字号" disabled={state.messageFontSize <= messageFontSize.min} onClick={() => appStore.setMessageFontSize(state.messageFontSize - .5)}>−</button>
                <input className="input" type="number" aria-label="消息字号" min={messageFontSize.min} max={messageFontSize.max} step=".5" value={state.messageFontSize} onChange={event => changeMessageFontSize(event.target.value)} />
                <span>px</span>
                <button type="button" className="icon-button small" aria-label="增大字号" disabled={state.messageFontSize >= messageFontSize.max} onClick={() => appStore.setMessageFontSize(state.messageFontSize + .5)}>+</button>
              </div>
            </div>
          </div>
        </section>
        <section className="settings-section" aria-labelledby="settings-models">
          <h3 id="settings-models">模型</h3>
            {catalog?.error && editor === null && removing === null && <div className="sg-model-config-error" role="alert">
              <strong>{unreadable ? '无法读取模型配置' : '默认模型不可用'}</strong>
              <details>
                <summary>查看错误详情</summary>
                <pre>{catalog.error}</pre>
              </details>
              <button type="button" className="secondary-button" disabled={refreshing} onClick={() => void rereadModelConfiguration()}>{refreshing ? '读取中…' : '重新读取'}</button>
            </div>}
            <div className="settings-group">
              {providerRows.map(provider => {
                const expanded = editingProvider?.providerId === provider.providerId
                return (
                  <div key={provider.providerId} className={`settings-provider${expanded ? ' is-open' : ''}`}>
                    <div className="sg-row-head">
                      <button type="button" className="sg-provider-toggle" disabled={busy} aria-expanded={expanded} aria-controls={`provider-editor-${provider.providerId}`} onClick={() => {
                        appStore.clearError(actionOrigin.inline('model.saveProvider', provider.providerId))
                        if (expanded) setEditor(null)
                        else openEditor(provider)
                      }}>
                        <span className="provider-avatar" aria-hidden="true">{(provider.displayName || provider.providerId).slice(0, 1)}</span>
                        <span className="row-card-title"><strong>{provider.displayName || provider.providerId}</strong><small>{provider.models.length} 个模型 · {provider.baseUrl}</small></span>
                        <span className={`credential${provider.credentialConfigured ? ' on' : ''}`}>{provider.credentialConfigured ? '已配置密钥' : '无密钥'}</span>
                        <ExpandChevron expanded={expanded} size={16} />
                      </button>
                      <span className="sg-row-actions">
                        <button type="button" className="quiet-button danger" disabled={busy} aria-label={`删除提供方 ${provider.displayName || provider.providerId}`} onClick={() => requestRemoval(provider)}>删除</button>
                      </span>
                    </div>
                    <Disclosure open={expanded}>
                      <div id={`provider-editor-${provider.providerId}`}>
                        <ProviderEditor key={`${provider.providerId}:${editorVersion}`} state={state} provider={provider} active={state.settingsOpen && expanded} onDone={() => setEditor(null)} />
                      </div>
                    </Disclosure>
                  </div>
              )})}
              {editor !== 'new' && <button type="button" className="settings-row settings-add" disabled={unreadable || busy} onClick={() => openEditor('new')}>添加提供方</button>}
              <Disclosure open={editor === 'new'}><div className="settings-provider is-open new-provider"><ProviderEditor key={editorVersion} state={state} active={state.settingsOpen && editor === 'new'} onDone={() => setEditor(null)} /></div></Disclosure>
            </div>
        </section>
      </div>
      <Dialog open={removing !== null} onClose={() => { if (!busy) setRemoving(null) }} labelledBy="remove-provider-title" className="confirm-modal">
        <header className="modal-header"><h2 id="remove-provider-title">删除提供方</h2></header>
        <div className="confirm-body">
          <p>删除“{removing?.displayName || removing?.providerId}”及其模型和 API 密钥？</p>
          {removing && state.actionErrors[actionOrigin.inline('model.removeProvider', removing.providerId)] && <p role="alert" className="form-error">{state.actionErrors[actionOrigin.inline('model.removeProvider', removing.providerId)].message}</p>}
          <footer>
            <button type="button" className="secondary-button" data-autofocus disabled={busy} onClick={() => setRemoving(null)}>取消</button>
            <button type="button" className="danger-button" disabled={busy} onClick={() => void removeProvider()}>删除</button>
          </footer>
        </div>
      </Dialog>
    </Dialog>
  )
}
