import { useState, type FormEvent } from 'react'
import { actionOrigin, appStore, pendingKey, type AppState } from '../appStore'
import type { RedactedProvider } from '../protocol'
import { messageFontSize } from '../viewPersistence'
import { ProviderEditor } from './ProviderEditor'
import { Dialog } from './Dialog'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'

/** 设置面板只声明自己读取的字段：父级按同一份清单订阅。 */
type SettingsState = Pick<AppState, 'bootstrap' | 'settingsOpen' | 'messageFontSize' | 'actionErrors' | 'pendingActions'>

type ProviderEditorState = null | { kind: 'new' } | { kind: 'existing'; providerId: string }

export function Settings({ state, initialSetup = false, onSetupDone }: { state: SettingsState; initialSetup?: boolean; onSetupDone?: () => void }) {
  const [editor, setEditor] = useState<ProviderEditorState>(null)
  const [removing, setRemoving] = useState<RedactedProvider | null>(null)
  const catalog = state.bootstrap?.modelCatalog
  function close() {
    onSetupDone?.()
    setEditor(null)
    setRemoving(null)
    appStore.setSettingsOpen(false)
  }

  function requestRemoval(provider: RedactedProvider) {
    appStore.clearError(actionOrigin.provider(provider.providerId))
    setRemoving(provider)
  }

  async function removeProvider() {
    if (removing && await appStore.removeProvider(removing.providerId)) setRemoving(null)
  }

  function changeMessageFontSize(value: string) {
    if (value !== '') appStore.setMessageFontSize(Number(value))
  }
  if (initialSetup) {
    return state.settingsOpen ? <InitialSetup state={state} onClose={close} /> : null
  }
  return (
    <Dialog open={state.settingsOpen} onClose={close} labelledBy="settings-title" className="sg-settings-modal">
      <header className="modal-header sg-modal-header">
        <h2 id="settings-title">设置</h2>
        <div className="sg-modal-actions">
          <button type="button" className="icon-button" data-autofocus onClick={close} aria-label="关闭设置">×</button>
        </div>
      </header>
      <main className="sg-settings-content">
        <header className="sg-view-header">
          <h3>消息</h3>
          <p>调整你发送的消息和模型最终回复的字号。</p>
        </header>
        <label className="message-font-setting">
          <span>消息字号</span>
          <input type="number" aria-label="消息字号" min={messageFontSize.min} max={messageFontSize.max} step="1" value={state.messageFontSize} onChange={event => changeMessageFontSize(event.target.value)} />
          <span>px</span>
        </label>
        <header className="sg-view-header">
          <h3>模型</h3>
          <p>填入各提供方的 API 密钥即可使用其模型。</p>
        </header>
        {catalog?.message && <p role="alert" className="form-error">{catalog.message}</p>}
        <div className="sg-provider-list">
          {catalog?.providers.map(provider => {
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
                    <ProviderEditor key={provider.providerId} state={state} provider={provider} onDone={() => setEditor(null)} />
                  </div>
                </Disclosure>
              </div>
          )})}
          {catalog?.providers.length === 0 && <p className="sg-provider-empty">尚未配置模型提供方。</p>}
        </div>
        {editor?.kind !== 'new' ? <div className="sg-add-actions">
          <button type="button" className="sg-add-card-btn" onClick={() => setEditor({ kind: 'new' })}>＋ 添加提供方</button>
        </div> : <ProviderEditor state={state} onDone={() => setEditor(null)} />}
      </main>
      <Dialog open={removing !== null} onClose={() => setRemoving(null)} labelledBy="remove-provider-title" className="confirm-modal">
        <header className="modal-header"><h2 id="remove-provider-title">删除提供方</h2></header>
        <div className="confirm-body">
          <p>删除“{removing?.displayName || removing?.providerId}”及其模型配置和 API 密钥？已经运行的回合会继续；使用它的任务下次发送前需要重新选择模型。</p>
          {removing && state.actionErrors[actionOrigin.provider(removing.providerId)] && <p role="alert" className="form-error">{state.actionErrors[actionOrigin.provider(removing.providerId)].message}</p>}
          <footer>
            <button type="button" className="secondary-button" data-autofocus onClick={() => setRemoving(null)}>取消</button>
            <button type="button" className="danger-button" disabled={removing !== null && state.pendingActions.has(pendingKey('model.removeProvider', actionOrigin.provider(removing.providerId)))} onClick={() => void removeProvider()}>删除</button>
          </footer>
        </div>
      </Dialog>
    </Dialog>
  )
}

function InitialSetup({ state, onClose }: { state: SettingsState; onClose: () => void }) {
  // 在保存动作完成前保持所选编辑器不变。
  const [missing] = useState(() => state.bootstrap?.modelCatalog.providers.find(provider => !provider.credentialConfigured))
  return <Dialog open onClose={onClose} labelledBy="initial-setup-title">
    <header className="modal-header">
      <h2 id="initial-setup-title">{missing ? '填写 API 密钥' : '添加模型提供方'}</h2>
      <button type="button" className="quiet-button" onClick={onClose}>稍后配置</button>
    </header>
    {state.bootstrap?.modelCatalog.message && <p role="alert" className="form-error configuration-error">{state.bootstrap.modelCatalog.message}</p>}
    {missing ? <CredentialSetup provider={missing} state={state} onDone={onClose} /> : <ProviderEditor state={state} onDone={onClose} />}
  </Dialog>
}

function CredentialSetup({ provider, state, onDone }: { provider: RedactedProvider; state: SettingsState; onDone: () => void }) {
  const [apiKey, setApiKey] = useState('')
  const origin = actionOrigin.providerKey(provider.providerId)
  const busy = state.pendingActions.has(pendingKey('model.setApiKey', origin))
  async function save(event: FormEvent) {
    event.preventDefault()
    if (busy || !apiKey.trim()) return
    if (await appStore.setApiKey(provider.providerId, apiKey.trim())) {
      setApiKey('')
      onDone()
    }
  }

  return <form className="sg-editor" onSubmit={event => void save(event)}>
    <label className="sg-field">
      <span>{provider.displayName || provider.providerId} API 密钥</span>
      <input className="sg-input" type="password" autoFocus autoComplete="off" value={apiKey} onChange={event => setApiKey(event.target.value)} />
    </label>
    {state.actionErrors[origin] && <p role="alert">{state.actionErrors[origin].message}</p>}
    <button type="submit" className="primary-button" disabled={busy || !apiKey.trim()}>保存</button>
  </form>
}
