import { useState } from 'react'
import { actionOrigin, appStore, pendingKey, type AppState } from '../appStore'
import type { RedactedProvider } from '../protocol'
import { messageFontSize } from '../viewPersistence'
import { ProviderEditor } from './ProviderEditor'
import { Dialog } from './Dialog'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'
import { McpSettings } from './McpSettings'
import { Palette, Boxes, Plug } from 'lucide-react'

/** 设置面板只声明自己读取的字段：父级按同一份清单订阅。 */
type SettingsState = Pick<AppState, 'bootstrap' | 'settingsOpen' | 'messageFontSize' | 'actionErrors' | 'pendingActions' | 'selectedWorkspaceId'>

type ProviderEditorState = null | { kind: 'new' } | { kind: 'existing'; providerId: string }

export function Settings({ state }: { state: SettingsState }) {
  const [page, setPage] = useState<'appearance' | 'models' | 'mcp'>('appearance')
  const [editor, setEditor] = useState<ProviderEditorState>(null)
  const [removing, setRemoving] = useState<{ provider: RedactedProvider } | null>(null)
  const [refreshing, setRefreshing] = useState(false)
  const catalog = state.bootstrap?.modelCatalog
  const providers = catalog?.providers ?? []
  const unreadable = Boolean(catalog?.error && providers.length === 0)
  function close() {
    setEditor(null)
    setRemoving(null)
    appStore.setSettingsOpen(false)
  }

  function requestRemoval(provider: RedactedProvider) {
    appStore.clearError(actionOrigin.provider(provider.providerId))
    setRemoving({ provider })
  }

  async function removeProvider() {
    if (removing && await appStore.removeProvider(removing.provider.providerId)) {
      setRemoving(current => current === removing ? null : current)
    }
  }

  function changeMessageFontSize(value: string) {
    if (value !== '') appStore.setMessageFontSize(Number(value))
  }

  async function rereadModelConfiguration() {
    if (refreshing) return
    setRefreshing(true)
    try { await appStore.refreshBootstrap() }
    finally { setRefreshing(false) }
  }
  return (
    <Dialog open={state.settingsOpen} onClose={close} labelledBy="settings-title" className="sg-settings-modal">
      <header className="modal-header sg-modal-header">
        <h2 id="settings-title">设置</h2>
        <div className="sg-modal-actions">
          <button type="button" className="icon-button" data-autofocus onClick={close} aria-label="关闭设置">×</button>
        </div>
      </header>
      <div className="sg-settings-layout">
        <nav className="sg-settings-nav" aria-label="设置分类">
          <button type="button" aria-current={page === 'appearance' ? 'page' : undefined} onClick={() => setPage('appearance')}><Palette size={18} /><span>外观</span></button>
          <button type="button" aria-current={page === 'models' ? 'page' : undefined} onClick={() => setPage('models')}><Boxes size={18} /><span>模型</span></button>
          <button type="button" aria-current={page === 'mcp' ? 'page' : undefined} onClick={() => setPage('mcp')}><Plug size={18} /><span>MCP</span></button>
        </nav>
        <main className="sg-settings-content">
          <section className="sg-settings-page" aria-label="外观设置" hidden={page !== 'appearance'}>
            <header className="sg-view-header">
              <h3>外观</h3>
              <p>调整你发送的消息和模型最终回复的字号。</p>
            </header>
            <label className="message-font-setting">
              <span>消息字号</span>
              <input type="number" aria-label="消息字号" min={messageFontSize.min} max={messageFontSize.max} step="1" value={state.messageFontSize} onChange={event => changeMessageFontSize(event.target.value)} />
              <span>px</span>
            </label>
          </section>
          <section className="sg-settings-page" aria-label="模型设置" hidden={page !== 'models'}>
            <header className="sg-view-header">
              <h3>模型</h3>
              <p>填入各提供方的 API 密钥即可使用其模型。</p>
            </header>
            {catalog?.error && <div className="sg-model-config-error" role="alert">
              <strong>{unreadable ? '无法读取模型配置' : '默认模型不可用'}</strong>
              <p>{unreadable ? '请检查模型配置和密钥文件，修正后重新读取。' : '请检查默认模型对应的提供方、模型和密钥，修正后重新读取。'}</p>
              <details>
                <summary>查看错误详情</summary>
                <pre>{catalog.error}</pre>
              </details>
              <button type="button" className="secondary-button" disabled={refreshing} onClick={() => void rereadModelConfiguration()}>{refreshing ? '读取中…' : '重新读取'}</button>
            </div>}
            <div className="sg-provider-list">
              {providers.map(provider => {
                const expanded = editor?.kind === 'existing' && editor.providerId === provider.providerId
                return (
                  <div key={provider.providerId} className="sg-row-card">
                    <div className="sg-row-head">
                      <button type="button" className="sg-provider-toggle" aria-expanded={expanded} aria-controls={`provider-editor-${provider.providerId}`} onClick={() => setEditor(expanded ? null : { kind: 'existing', providerId: provider.providerId })}>
                        <span className="sg-row-identity">
                          <strong>{provider.displayName || provider.providerId}</strong>
                          <span className={`sg-credential-dot sg-credential-dot-${provider.credentialConfigured ? 'configured' : 'missing'}`} role="img" aria-label={provider.credentialConfigured ? 'API 密钥已配置' : 'API 密钥缺失'} />
                        </span>
                        <ExpandChevron expanded={expanded} size={16} />
                      </button>
                      <span className="sg-row-actions">
                        <button type="button" className="quiet-button danger" aria-label={`删除提供方 ${provider.displayName || provider.providerId}`} onClick={() => requestRemoval(provider)}>删除</button>
                      </span>
                    </div>
                    <Disclosure open={expanded}>
                      <div id={`provider-editor-${provider.providerId}`}>
                        <ProviderEditor key={provider.providerId} state={state} provider={provider} onDone={() => setEditor(current => current === editor ? null : current)} />
                      </div>
                    </Disclosure>
                  </div>
              )})}
              {catalog && !catalog.error && providers.length === 0 && <p className="sg-provider-empty">尚未配置模型提供方。</p>}
              {providers.length > 0 && providers.every(provider => provider.models.length === 0) && <p className="sg-model-empty">尚未配置模型。请展开提供方并添加模型。</p>}
            </div>
            {editor?.kind !== 'new' ? <div className="sg-add-actions">
              <button type="button" className="sg-add-card-btn" disabled={unreadable} onClick={() => setEditor({ kind: 'new' })}>＋ 添加提供方</button>
            </div> : null}
            <Disclosure open={editor?.kind === 'new'}><ProviderEditor state={state} onDone={() => setEditor(current => current === editor ? null : current)} /></Disclosure>
          </section>
          <section className="sg-settings-page" aria-label="MCP 设置" hidden={page !== 'mcp'}>
            {state.settingsOpen && <McpSettings workspaceId={state.selectedWorkspaceId} active={page === 'mcp'} />}
          </section>
        </main>
      </div>
      <Dialog open={removing !== null} onClose={() => setRemoving(null)} labelledBy="remove-provider-title" className="confirm-modal">
        <header className="modal-header"><h2 id="remove-provider-title">删除提供方</h2></header>
        <div className="confirm-body">
          <p>删除“{removing?.provider.displayName || removing?.provider.providerId}”及其模型配置和 API 密钥？已经运行的回合会继续；使用它的任务下次发送前需要重新选择模型。</p>
          {removing && state.actionErrors[actionOrigin.provider(removing.provider.providerId)] && <p role="alert" className="form-error">{state.actionErrors[actionOrigin.provider(removing.provider.providerId)].message}</p>}
          <footer>
            <button type="button" className="secondary-button" data-autofocus onClick={() => setRemoving(null)}>取消</button>
            <button type="button" className="danger-button" disabled={removing !== null && state.pendingActions.has(pendingKey('model.removeProvider', actionOrigin.provider(removing.provider.providerId)))} onClick={() => void removeProvider()}>删除</button>
          </footer>
        </div>
      </Dialog>
    </Dialog>
  )
}
