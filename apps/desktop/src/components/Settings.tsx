import { useEffect, useRef, useState, type CSSProperties, type FormEvent } from 'react'
import { motion, useReducedMotion } from 'motion/react'
import { CircleCheck, X } from 'lucide-react'
import { actionOrigin, appStore, pendingKey, useAppStore } from '../appStore'
import { palette } from '../bubbleColors'
import { smooth } from '../motion'
import type { RedactedProvider } from '../protocol'
import { messageFontSize } from '../viewPersistence'
import { ProviderEditor } from './ProviderEditor'
import { Dialog } from './Dialog'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'
import { SelectionSlider } from './SelectionSlider'
import { InstructionsPage, McpPage, PageFrame, SkillsPage } from './ManagePages'

type SettingsTab = 'appearance' | 'models' | 'web-search' | 'instructions' | 'skills' | 'mcp'
type ProviderEditorState = null | 'new' | RedactedProvider

const settingsTabs: ReadonlyArray<{ id: SettingsTab; label: string }> = [
  { id: 'appearance', label: '外观' },
  { id: 'models', label: '模型' },
  { id: 'web-search', label: '联网搜索' },
  { id: 'instructions', label: '自定义指令' },
  { id: 'skills', label: 'Skills' },
  { id: 'mcp', label: 'MCP' },
]

export function Settings() {
  const { settingsOpen, pendingActions } = useAppStore(['settingsOpen', 'pendingActions'])
  const [tab, setTab] = useState<SettingsTab>('appearance')
  const navRef = useRef<HTMLElement>(null)
  const reduced = useReducedMotion()
  const busy = [...pendingActions].some(key =>
    ['model.saveProvider', 'model.removeProvider', 'model.saveWebSearch'].some(method => key.startsWith(pendingKey(method, ''))))

  function close() {
    if (busy) return
    appStore.setSettingsOpen(false)
  }

  return (
    <Dialog open={settingsOpen} onClose={close} labelledBy="settings-title" className="sg-settings-modal">
      <div className="settings-layout">
        <aside className="settings-sidebar">
          <h2 id="settings-title" className="settings-modal-title">设置</h2>
          <nav ref={navRef} className="settings-nav" aria-label="设置分类">
            <SelectionSlider container={navRef} activeKey={tab} />
            {settingsTabs.map(item => (
              <button
                key={item.id}
                type="button"
                data-selected={tab === item.id}
                aria-current={tab === item.id ? 'page' : undefined}
                className={`settings-nav-item${tab === item.id ? ' is-selected' : ''}`}
                onClick={() => setTab(item.id)}
              >
                {item.label}
              </button>
            ))}
          </nav>
        </aside>
        <div className="settings-main">
          <button type="button" className="icon-button settings-close" data-autofocus disabled={busy} onClick={close} aria-label="关闭设置">
            <X size={18} />
          </button>
          <div className="settings-pane">
            <motion.div
              key={tab}
              className="settings-pane-inner"
              initial={reduced ? false : { opacity: 0, y: 6 }}
              animate={{ opacity: 1, y: 0 }}
              transition={reduced ? { duration: 0 } : smooth}
            >
              {tab === 'appearance' && <AppearanceSection />}
              {tab === 'models' && <ModelsSection />}
              {tab === 'web-search' && <WebSearchSection />}
              {tab === 'instructions' && <InstructionsPage />}
              {tab === 'skills' && <SkillsPage />}
              {tab === 'mcp' && <McpPage />}
            </motion.div>
          </div>
        </div>
      </div>
    </Dialog>
  )
}

function AppearanceSection() {
  const state = useAppStore(['theme', 'accentColor', 'messageFontSize', 'stepsDefaultExpanded'])
  const [fontSizeDraft, setFontSizeDraft] = useState(() => String(state.messageFontSize))
  const skipBlurCommitRef = useRef(false)
  useEffect(() => {
    setFontSizeDraft(String(state.messageFontSize))
  }, [state.messageFontSize])

  function commitFontSizeDraft() {
    if (skipBlurCommitRef.current) {
      skipBlurCommitRef.current = false
      return
    }
    const trimmed = fontSizeDraft.trim()
    const parsed = Number(trimmed)
    if (trimmed === '' || !Number.isFinite(parsed)) {
      setFontSizeDraft(String(state.messageFontSize))
      return
    }
    setFontSizeDraft(String(appStore.setMessageFontSize(parsed)))
  }

  return (
    <PageFrame title="外观">
      <div className="settings-group">
        <div className="settings-row"><span>主题</span>
          <div className="segmented" role="radiogroup" aria-label="主题">
            {([['light', '浅色'], ['dark', '深色']] as const).map(([theme, label]) => (
              <button key={theme} type="button" role="radio" aria-checked={state.theme === theme} onClick={() => appStore.setTheme(theme)}>{label}</button>
            ))}
          </div>
        </div>
        <div className="settings-row"><span>强调色</span>
          <div className="accent-swatches" role="radiogroup" aria-label="强调色">
            {palette.map(color => (
              <button
                key={color.id}
                type="button"
                className="accent-swatch"
                role="radio"
                aria-checked={state.accentColor === color.id}
                aria-label={color.label}
                title={color.label}
                style={{ '--swatch': color.solid } as CSSProperties}
                onClick={() => appStore.setAccentColor(color.id)}
              />
            ))}
          </div>
        </div>
        <div className="settings-row"><span>消息字号</span>
          <div className="stepper">
            <button type="button" className="icon-button small" aria-label="减小字号" disabled={state.messageFontSize <= messageFontSize.min} onClick={() => appStore.setMessageFontSize(state.messageFontSize - .5)}>−</button>
            <input
              className="input"
              type="number"
              aria-label="消息字号"
              min={messageFontSize.min}
              max={messageFontSize.max}
              step=".5"
              value={fontSizeDraft}
              onFocus={() => { skipBlurCommitRef.current = false }}
              onChange={event => setFontSizeDraft(event.target.value)}
              onBlur={commitFontSizeDraft}
              onKeyDown={event => {
                if (event.key === 'Enter') event.currentTarget.blur()
                else if (event.key === 'Escape') {
                  event.preventDefault()
                  event.stopPropagation()
                  skipBlurCommitRef.current = true
                  setFontSizeDraft(String(state.messageFontSize))
                  event.currentTarget.blur()
                }
              }}
            />
            <span>px</span>
            <button type="button" className="icon-button small" aria-label="增大字号" disabled={state.messageFontSize >= messageFontSize.max} onClick={() => appStore.setMessageFontSize(state.messageFontSize + .5)}>+</button>
          </div>
        </div>
        <div className="settings-row"><span>执行步骤</span>
          <div className="segmented" role="radiogroup" aria-label="执行步骤默认状态">
            {([[true, '默认展开'], [false, '默认收起']] as const).map(([expanded, label]) => (
              <button key={label} type="button" role="radio" aria-checked={state.stepsDefaultExpanded === expanded} onClick={() => appStore.setStepsDefaultExpanded(expanded)}>{label}</button>
            ))}
          </div>
        </div>
      </div>
    </PageFrame>
  )
}

function ModelConfigErrorAlert({ title, error }: { title: string; error: string }) {
  const [refreshing, setRefreshing] = useState(false)
  async function reread() {
    if (refreshing) return
    setRefreshing(true)
    try { await appStore.refreshModels() }
    finally { setRefreshing(false) }
  }
  return (
    <div className="sg-model-config-error" role="alert">
      <strong>{title}</strong>
      <details>
        <summary>查看错误详情</summary>
        <pre>{error}</pre>
      </details>
      <button type="button" className="secondary-button" disabled={refreshing} onClick={() => void reread()}>
        {refreshing ? '读取中…' : '重新读取'}
      </button>
    </div>
  )
}

function ModelsSection() {
  const state = useAppStore(['bootstrap', 'actionErrors', 'pendingActions'])
  const [editor, setEditor] = useState<ProviderEditorState>(null)
  const [editorVersion, setEditorVersion] = useState(0)
  const [removing, setRemoving] = useState<RedactedProvider | null>(null)
  const busy = [...state.pendingActions].some(key =>
    ['model.saveProvider', 'model.removeProvider'].some(method => key.startsWith(pendingKey(method, ''))))
  const catalog = state.bootstrap?.modelCatalog
  const providers = catalog?.providers ?? []
  const editingProvider = editor === 'new' ? null : editor
  const providerRows = editingProvider && !providers.some(provider => provider.providerId === editingProvider.providerId)
    ? [...providers, editingProvider] : providers
  const unreadable = Boolean(catalog?.error && providers.length === 0)

  function openEditor(next: Exclude<ProviderEditorState, null>) {
    setEditorVersion(version => version + 1)
    setEditor(next)
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

  return (
    <PageFrame title="模型">
      {catalog?.error && editor === null && removing === null && (
        <ModelConfigErrorAlert title={unreadable ? '无法读取模型配置' : '默认模型不可用'} error={catalog.error} />
      )}
      <div className="settings-group">
        {providerRows.map(provider => {
          const expanded = editingProvider?.providerId === provider.providerId
          return (
            <div key={provider.providerId}>
              <div className="sg-row-head">
                <button type="button" className="sg-provider-toggle" disabled={busy} aria-expanded={expanded} aria-controls={`provider-editor-${provider.providerId}`} onClick={() => {
                  appStore.clearError(actionOrigin.inline('model.saveProvider', provider.providerId))
                  if (expanded) setEditor(null)
                  else openEditor(provider)
                }}>
                  <span className="row-card-title">
                    <span className="row-card-head-line">
                      <strong>{provider.displayName || provider.providerId}</strong>
                      <span className="proto-tag">{provider.apiProtocol === 'responses' ? 'Responses' : 'Messages'}</span>
                    </span>
                    <small>{provider.models.length} 个模型 · {provider.baseUrl}</small>
                  </span>
                  <span className={`credential${provider.credentialConfigured ? ' on' : ''}`}>{provider.credentialConfigured ? '已配置密钥' : '无密钥'}</span>
                  <ExpandChevron expanded={expanded} size={16} />
                </button>
                {expanded && <span className="sg-row-actions">
                  <button type="button" className="quiet-button danger" disabled={busy} aria-label={`删除提供方 ${provider.displayName || provider.providerId}`} onClick={() => requestRemoval(provider)}>删除</button>
                </span>}
              </div>
              <Disclosure open={expanded}>
                <div id={`provider-editor-${provider.providerId}`}>
                  <ProviderEditor key={`${provider.providerId}:${editorVersion}`} state={state} provider={provider} active={expanded} onDone={() => setEditor(null)} />
                </div>
              </Disclosure>
            </div>
          )
        })}
        {editor !== 'new' && <button type="button" className="settings-row settings-add" disabled={unreadable || busy} onClick={() => openEditor('new')}>添加提供方</button>}
        <Disclosure open={editor === 'new'}><div><ProviderEditor key={editorVersion} state={state} active={editor === 'new'} onDone={() => setEditor(null)} /></div></Disclosure>
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
    </PageFrame>
  )
}

function WebSearchSection() {
  const state = useAppStore(['bootstrap', 'actionErrors', 'pendingActions'])
  const catalog = state.bootstrap?.modelCatalog
  const saved = catalog?.webSearch
  const savedUrl = saved?.baseUrl ?? ''
  const credentialConfigured = saved?.credentialConfigured ?? false
  const unreadable = Boolean(catalog?.error && catalog.providers.length === 0)
  const [baseUrl, setBaseUrl] = useState(savedUrl)
  const [apiKey, setApiKey] = useState('')
  const [clearKey, setClearKey] = useState(false)
  const [savedNotice, setSavedNotice] = useState(false)
  const origin = actionOrigin.inline('model.saveWebSearch', 'web')
  const busy = state.pendingActions.has(pendingKey('model.saveWebSearch', origin))
  const saveError = state.actionErrors[origin]

  useEffect(() => () => appStore.clearError(origin), [origin])

  useEffect(() => {
    setBaseUrl(savedUrl)
  }, [savedUrl])

  useEffect(() => {
    if (!savedNotice) return
    const timer = window.setTimeout(() => setSavedNotice(false), 2400)
    return () => window.clearTimeout(timer)
  }, [savedNotice])

  async function submit(event: FormEvent) {
    event.preventDefault()
    if (busy) return
    setSavedNotice(false)
    appStore.clearError(origin)
    const nextKey = clearKey ? '' : apiKey.trim() !== '' ? apiKey.trim() : undefined
    const ok = await appStore.saveWebSearch(baseUrl.trim(), nextKey)
    if (ok) {
      setApiKey('')
      setClearKey(false)
      setSavedNotice(true)
    }
  }

  return (
    <PageFrame title="联网搜索">
      {unreadable && catalog?.error && (
        <ModelConfigErrorAlert title="无法读取模型配置" error={catalog.error} />
      )}
      <div className="settings-group">
        <form className="sg-editor web-search-form" onSubmit={event => void submit(event)} noValidate>
          <fieldset disabled={busy || unreadable} className="provider-fields">
            <div className="web-search-status-line">
              <span className="web-search-service">DeepSeek Search &amp; Fetch API</span>
              <span className={`credential${credentialConfigured && !clearKey ? ' on' : ''}`}>
                {clearKey ? '保存后清除密钥' : credentialConfigured ? '已配置密钥' : '无密钥'}
              </span>
            </div>
            <label className="sg-field">
              <span>API 地址</span>
              <input
                className="input"
                value={baseUrl}
                placeholder={saved?.baseUrl}
                onChange={event => { setBaseUrl(event.target.value); setSavedNotice(false) }}
              />
            </label>
            <div className="sg-field">
              <label htmlFor="web-search-api-key">API 密钥</label>
              <div className="web-search-key-row">
                <input
                  id="web-search-api-key"
                  className="input"
                  type="password"
                  autoComplete="off"
                  value={apiKey}
                  placeholder={credentialConfigured && !clearKey ? '留空保留现有密钥' : '输入 DeepSeek API Key'}
                  onChange={event => { setApiKey(event.target.value); setClearKey(false); setSavedNotice(false) }}
                />
                {credentialConfigured && !clearKey && (
                  <button type="button" className="quiet-button danger" onClick={() => { setClearKey(true); setApiKey(''); setSavedNotice(false) }}>清除密钥</button>
                )}
                {clearKey && (
                  <button type="button" className="quiet-button" onClick={() => setClearKey(false)}>撤销清除</button>
                )}
              </div>
            </div>
            {saveError && <p className="form-error" role="alert">{saveError.message}</p>}
            <footer className="sg-editor-actions web-search-actions">
              <span className="web-search-saved" role="status" aria-live="polite">
                {savedNotice && <><CircleCheck size={14} aria-hidden="true" /><span>已保存</span></>}
              </span>
              <button type="submit" className="primary-button">{busy ? '保存中…' : '保存配置'}</button>
            </footer>
          </fieldset>
        </form>
      </div>
    </PageFrame>
  )
}
