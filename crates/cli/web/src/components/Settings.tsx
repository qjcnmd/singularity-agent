import { useLayoutEffect, useRef, useState, type FormEvent } from 'react'
import { workbenchStore, type WorkbenchState } from '../store'
import type { DiscoveredModel, ProviderConfigurationInput, RedactedProvider } from '../protocol'
import { messageFontSize } from '../viewPersistence'
import { Dialog } from './Dialog'

type ModelInput = ProviderConfigurationInput['models'][number]
type ModelDraft = ModelInput & { contextText: string; outputText: string; expanded: boolean }
const blankModel = (): ModelInput => ({ modelId: '', displayName: null, apiProtocol: 'chat', maxContextTokens: null, maxOutputTokens: null, reasoningVariants: [], defaultVariant: null, thinkingWireFormat: null })
const toDraft = (model: ModelInput): ModelDraft => ({ ...model, contextText: capacity(model.maxContextTokens), outputText: capacity(model.maxOutputTokens), expanded: false })

export function Settings({ state, initialSetup = false, onSetupDone }: { state: WorkbenchState; initialSetup?: boolean; onSetupDone?: () => void }) {
  const [editing, setEditing] = useState<string | null>(null)
  const [adding, setAdding] = useState<'preset' | 'custom' | null>(null)
  const [removing, setRemoving] = useState<RedactedProvider | null>(null)
  const catalog = state.bootstrap?.modelCatalog
  const close = () => { onSetupDone?.(); setEditing(null); setAdding(null); setRemoving(null); workbenchStore.setSettingsOpen(false) }
  const presets = catalog?.presets ?? []
  if (initialSetup) {
    const missing = catalog?.providers.find(provider => !provider.credentialConfigured)
    return <Dialog open={state.settingsOpen} onClose={close} labelledBy="initial-setup-title" className="settings-modal">
      <header className="modal-header"><h2 id="initial-setup-title">{missing ? '填写 API 密钥' : '添加模型提供方'}</h2><button type="button" className="quiet-button" onClick={close}>稍后配置</button></header>
      {missing ? <CredentialSetup key={missing.providerId} provider={missing} state={state} onDone={close} /> : <ProviderEditor state={state} presetMode={presets.length > 0} onDone={close} />}
    </Dialog>
  }
  return (
    <Dialog open={state.settingsOpen} onClose={close} labelledBy="settings-title" className="settings-modal dsh-settings-modal">
      <header className="modal-header dsh-modal-header">
        <h2 id="settings-title">设置</h2>
        <div className="dsh-modal-actions">
          <button type="button" className="icon-button" data-autofocus onClick={close} aria-label="关闭设置">×</button>
        </div>
      </header>
      <main className="dsh-settings-content">
        <header className="dsh-view-header"><h3>消息</h3><p>调整你发送的消息和模型最终回复的字号。</p></header>
        <label className="message-font-setting"><span>消息字号</span><input type="range" min={messageFontSize.min} max={messageFontSize.max} step="1" value={state.messageFontSize} onChange={event => workbenchStore.setMessageFontSize(Number(event.target.value))} /><output>{state.messageFontSize} px</output></label>
        <header className="dsh-view-header"><h3>模型</h3><p>填入各提供方的 API 密钥即可使用其模型。</p></header>
        <div className="dsh-provider-list">
          {catalog?.providers.map(provider => (
            <div key={provider.providerId} className="dsh-row-card">
              <div className="dsh-row-head">
                <span className="dsh-row-identity"><strong>{provider.displayName || provider.providerId}</strong>
                  {!presets.some(preset => preset.providerId === provider.providerId) && <span className="dsh-row-tag">自定义</span>}
                  <span className={`dsh-credential-dot dsh-credential-dot-${provider.credentialConfigured ? 'configured' : 'missing'}`} role="img" aria-label={provider.credentialConfigured ? 'API 密钥已配置' : 'API 密钥缺失'} />
                </span>
                <span className="dsh-row-actions">
                  <button type="button" className="dsh-secondary-btn" aria-expanded={editing === provider.providerId} onClick={() => { setAdding(null); setEditing(editing === provider.providerId ? null : provider.providerId) }}>编辑</button>
                  <button type="button" className="quiet-button danger" aria-label={`删除提供方 ${provider.displayName || provider.providerId}`} onClick={() => { workbenchStore.clearError(`provider:${provider.providerId}`); setRemoving(provider) }}>删除</button>
                </span>
              </div>
              {editing === provider.providerId && <ProviderEditor key={provider.providerId} state={state} provider={provider} onDone={() => setEditing(null)} />}
            </div>
          ))}
          {catalog?.providers.length === 0 && <p className="dsh-provider-empty">尚未配置模型提供方。</p>}
        </div>
        {adding === null ? <div className="dsh-add-actions">
          <button type="button" className="dsh-add-card-btn" onClick={() => { setEditing(null); setAdding('preset') }}>＋ 添加提供方</button>
          <button type="button" className="dsh-add-card-btn" onClick={() => { setEditing(null); setAdding('custom') }}>＋ 添加自定义提供方</button>
        </div> : <ProviderEditor key={adding} state={state} presetMode={adding === 'preset'} onDone={() => setAdding(null)} />}
      </main>
      <Dialog open={removing !== null} onClose={() => setRemoving(null)} labelledBy="remove-provider-title" className="confirm-modal">
        <header className="modal-header"><h2 id="remove-provider-title">删除提供方</h2></header>
        <div className="confirm-body">
          <p>删除“{removing?.displayName || removing?.providerId}”及其模型配置和 API 密钥？已经运行的回合会继续；使用它的任务下次发送前需要重新选择模型。</p>
          {removing && state.actionErrors[`provider:${removing.providerId}`] && <p role="alert" className="form-error">{state.actionErrors[`provider:${removing.providerId}`].message}</p>}
          <footer><button type="button" className="secondary-button" data-autofocus onClick={() => setRemoving(null)}>取消</button>
            <button type="button" className="danger-button" disabled={removing !== null && workbenchStore.isPending('model.removeProvider', `provider:${removing.providerId}`)} onClick={async () => { if (removing && await workbenchStore.removeProvider(removing.providerId)) setRemoving(null) }}>删除</button></footer>
        </div>
      </Dialog>
    </Dialog>
  )
}

function CredentialSetup({ provider, state, onDone }: { provider: RedactedProvider; state: WorkbenchState; onDone: () => void }) {
  const [apiKey, setApiKey] = useState('')
  const origin = `provider-key:${provider.providerId}`
  const busy = workbenchStore.isPending('model.setApiKey', origin)
  return <form className="dsh-editor" onSubmit={async event => { event.preventDefault(); if (!busy && apiKey.trim() && await workbenchStore.setApiKey(provider.providerId, apiKey.trim())) { setApiKey(''); onDone() } }}>
    <label className="dsh-field"><span>{provider.displayName || provider.providerId} API 密钥</span><input className="dsh-input" type="password" autoFocus autoComplete="off" value={apiKey} onChange={event => setApiKey(event.target.value)} /></label>
    {state.actionErrors[origin] && <p role="alert">{state.actionErrors[origin].message}</p>}
    <button type="submit" className="primary-button" disabled={busy || !apiKey.trim()}>保存</button>
  </form>
}

function ProviderEditor({ state, provider, presetMode = false, onDone }: { state: WorkbenchState; provider?: RedactedProvider; presetMode?: boolean; onDone: () => void }) {
  const presets = state.bootstrap?.modelCatalog.presets ?? []
  const available = presets.filter(preset => !state.bootstrap?.modelCatalog.providers.some(p => p.providerId === preset.providerId))
  const initial = provider ?? (presetMode ? available[0] : undefined)
  const [providerId, setProviderId] = useState(initial?.providerId ?? '')
  const [name, setName] = useState(initial?.displayName ?? '')
  const [baseUrl, setBaseUrl] = useState(initial?.baseUrl ?? '')
  const [apiKey, setApiKey] = useState('')
  const [protocol, setProtocol] = useState<'chat' | 'responses'>((initial?.models[0]?.apiProtocol as 'chat' | 'responses') ?? 'chat')
  const [models, setModels] = useState<ModelDraft[]>(() => (initial?.models ?? []).map(model => toDraft({ ...model, apiProtocol: model.apiProtocol as 'chat' | 'responses' })))
  const [busy, setBusy] = useState(false)
  const [saved, setSaved] = useState(false)
  const [fetching, setFetching] = useState(false)
  const [failure, setFailure] = useState<string | null>(null)
  const [candidates, setCandidates] = useState<DiscoveredModel[] | null>(null)
  const [picked, setPicked] = useState<Set<string>>(new Set())
  const discoveryRevision = useRef(0)
  useLayoutEffect(() => {
    setFetching(false)
    setCandidates(null)
    setFailure(null)
    return () => { discoveryRevision.current += 1 }
  }, [providerId, baseUrl, apiKey, protocol])
  const preset = presets.find(p => p.providerId === providerId)
  const saveError = state.actionErrors[`provider:${providerId.trim()}`] ?? state.actionErrors[`provider-key:${providerId.trim()}`]
  const patchModel = (index: number, patch: Partial<ModelDraft>) => setModels(rows => rows.map((row, at) => at === index ? { ...row, ...patch } : row))

  const selectPreset = (id: string) => {
    const next = available.find(p => p.providerId === id)
    if (!next) return
    setProviderId(id); setName(next.displayName ?? ''); setBaseUrl(next.baseUrl)
    setProtocol(next.models[0]?.apiProtocol ?? 'chat'); setModels(next.models.map(toDraft)); setFailure(null)
  }
  const discover = async () => {
    const revision = ++discoveryRevision.current
    setFailure(null); setFetching(true)
    try {
      const found = await workbenchStore.discoverModels(providerId.trim(), normalizeBaseUrl(baseUrl), apiKey.trim())
      if (revision !== discoveryRevision.current) return
      if (found.length === 0) { setFailure('提供方没有返回可用模型，仍可手动添加。'); return }
      setCandidates(found)
      setPicked(new Set(found.filter(candidate => models.some(model => model.modelId.trim() === candidate.modelId)).map(model => model.modelId)))
    } catch (error) {
      if (revision === discoveryRevision.current) setFailure(error instanceof Error ? error.message : '获取模型失败，仍可手动添加。')
    } finally {
      if (revision === discoveryRevision.current) setFetching(false)
    }
  }
  const adopt = () => {
    setModels(rows => {
      const next = [...rows]
      for (const candidate of candidates ?? []) {
        if (!picked.has(candidate.modelId)) continue
        const index = next.findIndex(row => row.modelId.trim() === candidate.modelId)
        if (index < 0) next.push(toDraft({ ...blankModel(), ...candidate, apiProtocol: protocol }))
        else {
          const current = next[index]
          const variants = candidate.reasoningVariants.length
            ? candidate.reasoningVariants.map(variant => current.reasoningVariants.find(existing => existing.enabled && existing.wireEffort === variant.wireEffort) ?? variant)
            : current.reasoningVariants
          const importedDefault = candidate.reasoningVariants.find(variant => variant.id === candidate.defaultVariant)
          next[index] = { ...current,
            displayName: current.displayName || candidate.displayName,
            contextText: candidate.maxContextTokens === null ? current.contextText : capacity(candidate.maxContextTokens),
            outputText: candidate.maxOutputTokens === null ? current.outputText : capacity(candidate.maxOutputTokens),
            reasoningVariants: variants,
            defaultVariant: variants.some(variant => variant.id === current.defaultVariant) ? current.defaultVariant : variants.find(variant => variant.wireEffort === importedDefault?.wireEffort)?.id ?? variants[0]?.id ?? null,
            thinkingWireFormat: current.thinkingWireFormat ?? candidate.thinkingWireFormat,
          }
        }
      }
      return next
    })
    setCandidates(null)
  }
  const save = async (event: FormEvent) => {
    event.preventDefault(); setFailure(null)
    if (!/^[^\s/#]+$/.test(providerId.trim())) { setFailure('请输入不含空格、/ 或 # 的提供方 ID。'); return }
    if (!provider && !saved && state.bootstrap?.modelCatalog.providers.some(p => p.providerId === providerId.trim())) { setFailure('该提供方 ID 已存在，请编辑已有提供方。'); return }
    let url: URL
    try { url = new URL(normalizeBaseUrl(baseUrl)) } catch { setFailure('请输入完整的 API 地址。'); return }
    if (!['https:', 'http:'].includes(url.protocol) || url.username || url.password || url.search || url.hash) { setFailure('API 地址必须为 http 或 https 地址，不含凭据、查询或片段。'); return }
    const ids = new Set<string>()
    const submitted: ModelInput[] = []
    for (const [index, model] of models.entries()) {
      const id = model.modelId.trim()
      const context = parseCapacity(model.contextText), output = parseCapacity(model.outputText)
      if (!id || /\s|#/.test(id) || ids.has(id)) { setFailure(`第 ${index + 1} 行模型 ID 为空、重复或包含无效字符。`); return }
      if (Number.isNaN(context) || Number.isNaN(output)) { setFailure(`第 ${index + 1} 行容量应为空或正整数，可使用 K / M。`); return }
      ids.add(id)
      submitted.push({ modelId: id, displayName: model.displayName?.trim() || null, apiProtocol: model.apiProtocol, maxContextTokens: context, maxOutputTokens: output, reasoningVariants: model.reasoningVariants, defaultVariant: model.defaultVariant, thinkingWireFormat: model.thinkingWireFormat })
    }
    setBusy(true)
    try {
      if (!await workbenchStore.saveProvider({ providerId: providerId.trim(), displayName: name.trim() || null, baseUrl: normalizeBaseUrl(baseUrl), models: submitted, makeDefault: false })) return
      setSaved(true)
      if (apiKey.trim() && !await workbenchStore.setApiKey(providerId.trim(), apiKey.trim())) return
      setApiKey(''); onDone()
    } finally { setBusy(false) }
  }
  return (
    <form className="dsh-editor" onSubmit={event => void save(event)} noValidate>
      <fieldset disabled={busy} className="provider-fields">
        {presetMode && !saved ? <label className="dsh-field"><span>提供方</span><select className="dsh-input" value={providerId} onChange={event => selectPreset(event.target.value)}>{available.map(p => <option value={p.providerId} key={p.providerId}>{p.displayName || p.providerId}</option>)}</select>{available.length === 0 && <small>内置提供方均已添加。可在上方编辑或添加自定义提供方。</small>}</label>
          : provider || saved ? <strong>{name || providerId} <small className="provider-id">{providerId}</small></strong>
          : <label className="dsh-field"><span>提供方 ID</span><input className="dsh-input" autoFocus value={providerId} onChange={e => setProviderId(e.target.value)} placeholder="例如 my-provider" /></label>}
        <label className="dsh-field"><span>API 密钥</span><input className="dsh-input" type="password" autoComplete="off" value={apiKey} onChange={e => setApiKey(e.target.value)} placeholder={provider?.credentialConfigured ? '已配置——输入新值可替换' : '输入 API 密钥'} /></label>
        <details className="dsh-customized" open={!provider && !presetMode ? true : undefined}>
          <summary className="dsh-customized-summary">自定义设置</summary>
          <div className="dsh-customized-body">
            <label className="dsh-field"><span>显示名称</span><input className="dsh-input" value={name} onChange={e => setName(e.target.value)} placeholder={providerId} /></label>
            <label className="dsh-field"><span>API 地址</span><input className="dsh-input" value={baseUrl} onChange={e => setBaseUrl(e.target.value)} placeholder="https://api.example.com/v1" /></label>
            <label className="dsh-field"><span>API 协议</span><select className="dsh-input" value={protocol} onChange={e => { const value = e.target.value as 'chat' | 'responses'; setProtocol(value); setModels(rows => rows.map(row => ({ ...row, apiProtocol: value }))) }}><option value="chat">Chat Completions</option><option value="responses">Responses</option></select></label>
            <section className="dsh-model-catalog" aria-label="模型目录">
              <div className="dsh-model-catalog-head"><span className="dsh-model-catalog-title">模型目录</span><span className="dsh-row-actions">
                {preset && <button type="button" className="quiet-button" onClick={() => setModels(preset.models.map(toDraft))}>恢复默认模型</button>}
                <button type="button" className="quiet-button" disabled={fetching || !baseUrl.trim()} onClick={() => void discover()}>{fetching ? '正在询问提供方…' : '获取可用模型'}</button>
              </span></div>
              {models.length === 0 && <p className="dsh-model-empty">尚无模型。获取可用模型或手动添加后，即可在任务中选择。</p>}
              <div className="dsh-model-list">{models.map((model, index) => <div key={index} className="dsh-model-entry">
                <div className="dsh-model-row">
                  <input className="dsh-input" aria-label={`模型 ID ${index + 1}`} placeholder="模型 ID" value={model.modelId} onChange={e => patchModel(index, { modelId: e.target.value })} />
                  <input className="dsh-input" aria-label={`模型显示名称 ${index + 1}`} placeholder="显示名称" value={model.displayName ?? ''} onChange={e => patchModel(index, { displayName: e.target.value })} />
                  <button type="button" className="dsh-icon-btn" aria-label={`容量 ${index + 1}`} aria-expanded={model.expanded} onClick={() => patchModel(index, { expanded: !model.expanded })}>{model.expanded ? '⌄' : '›'}</button>
                  <button type="button" className="dsh-icon-btn dsh-icon-btn-danger" aria-label={`删除模型 ${index + 1}`} onClick={() => setModels(rows => rows.filter((_, at) => at !== index))}>×</button>
                </div>
                {model.expanded && <div className="dsh-model-advanced">
                  <label className="dsh-model-field"><span>上下文窗口</span><input className="dsh-input" aria-label={`上下文窗口 ${index + 1}`} value={model.contextText} placeholder="提供方默认，可填 256K" onChange={e => patchModel(index, { contextText: e.target.value })} /></label>
                  <label className="dsh-model-field"><span>最大输出 token 数</span><input className="dsh-input" aria-label={`最大输出 ${index + 1}`} value={model.outputText} placeholder="提供方默认，可填 32K" onChange={e => patchModel(index, { outputText: e.target.value })} /></label>
                  <label className="dsh-model-field"><span>该模型协议</span><select className="dsh-input" value={model.apiProtocol} onChange={e => patchModel(index, { apiProtocol: e.target.value as 'chat' | 'responses' })}><option value="chat">Chat Completions</option><option value="responses">Responses</option></select></label>
                </div>}
              </div>)}</div>
              <button type="button" className="dsh-add-model-btn" onClick={() => setModels(rows => [...rows, toDraft({ ...blankModel(), apiProtocol: protocol })])}>＋ 添加模型</button>
            </section>
          </div>
        </details>
        {failure && <p className="form-error" role="alert">{failure}</p>}
        {saveError && <p className="form-error" role="alert">{saveError.message}</p>}
        <footer className="dsh-editor-actions"><button type="button" className="dsh-secondary-btn" onClick={onDone}>取消</button><button type="submit" className="dsh-primary-btn" disabled={presetMode && available.length === 0 && !saved}>{busy ? '保存中…' : '保存'}</button></footer>
      </fieldset>
      <Dialog open={candidates !== null} onClose={() => setCandidates(null)} labelledBy="discovered-models-title" className="confirm-modal model-discovery-modal">
        <header className="modal-header"><h2 id="discovered-models-title">选择可用模型</h2><button type="button" className="icon-button" onClick={() => setCandidates(null)} aria-label="关闭模型列表">×</button></header>
        <div className="model-candidates">{candidates?.map(candidate => <label key={candidate.modelId}><input type="checkbox" checked={picked.has(candidate.modelId)} onChange={() => setPicked(current => { const next = new Set(current); if (!next.delete(candidate.modelId)) next.add(candidate.modelId); return next })} /><span>{candidate.modelId}<small>{candidate.reasoningVariants.length ? candidate.reasoningVariants.map(variant => variant.id).join(' / ') : '未获取思考档位'}{candidate.maxContextTokens ? ` · ${capacity(candidate.maxContextTokens)}` : ''}</small></span>{models.some(model => model.modelId.trim() === candidate.modelId) && <small>更新配置</small>}</label>)}</div>
        <footer className="dsh-editor-actions"><button type="button" className="dsh-secondary-btn" onClick={() => setCandidates(null)}>取消</button><button type="button" className="dsh-primary-btn" onClick={adopt}>应用所选模型</button></footer>
      </Dialog>
    </form>
  )
}

function normalizeBaseUrl(value: string): string {
  return value.trim().replace(/\/+$/, '').replace(/\/(chat\/completions|responses|models)$/, '')
}
function parseCapacity(value: string): number | null {
  if (!value.trim()) return null
  const match = /^(\d+(?:\.\d+)?)\s*([km])?$/i.exec(value.trim())
  if (!match) return NaN
  const parsed = Number(match[1]) * (match[2]?.toLowerCase() === 'm' ? 1_000_000 : match[2] ? 1_000 : 1)
  return Number.isSafeInteger(parsed) && parsed > 0 && parsed <= 0xffffffff ? parsed : NaN
}
function capacity(value: number | null): string { return value === null ? '' : value % 1_000_000 === 0 ? `${value / 1_000_000}M` : value % 1_000 === 0 ? `${value / 1_000}K` : String(value) }
