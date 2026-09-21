import { useLayoutEffect, useRef, useState, type FormEvent } from 'react'
import { formatTokenCount } from '../copy'
import { actionOrigin, appStore, pendingKey, type AppState } from '../appStore'
import type { DiscoveredModel, ProviderConfigurationInput, RedactedProvider } from '../protocol'
import { messageFontSize } from '../viewPersistence'
import { blankModel, mergeDiscoveredModels } from '../modelImport'
import { Dialog } from './Dialog'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'

/** 设置面板只声明自己读取的字段：父级按同一份清单订阅。 */
type SettingsState = Pick<AppState, 'bootstrap' | 'settingsOpen' | 'messageFontSize' | 'actionErrors' | 'pendingActions'>

type ModelInput = ProviderConfigurationInput['models'][number]
type ModelDraft = Omit<ModelInput, 'maxContextTokens' | 'maxOutputTokens'> & { contextText: string; outputText: string }
const toDraft = ({ maxContextTokens, maxOutputTokens, ...model }: ModelInput): ModelDraft => ({ ...model, contextText: capacityInput(maxContextTokens), outputText: capacityInput(maxOutputTokens) })

function ProtocolOptions({ value }: { value: string | null }) {
  return <>
    {!['chat', 'responses'].includes(value ?? '') && <option value={value ?? ''}>{value ? `无效协议：${value}` : '请选择协议'}</option>}
    <option value="chat">Chat Completions</option>
    <option value="responses">Responses</option>
  </>
}

export function Settings({ state, initialSetup = false, onSetupDone }: { state: SettingsState; initialSetup?: boolean; onSetupDone?: () => void }) {
  const [editing, setEditing] = useState<string | null>(null)
  const [adding, setAdding] = useState(false)
  const [removing, setRemoving] = useState<RedactedProvider | null>(null)
  const catalog = state.bootstrap?.modelCatalog
  const close = () => { onSetupDone?.(); setEditing(null); setAdding(false); setRemoving(null); appStore.setSettingsOpen(false) }
  if (initialSetup) {
    return state.settingsOpen ? <InitialSetup state={state} onClose={close} /> : null
  }
  return (
    <Dialog open={state.settingsOpen} onClose={close} labelledBy="settings-title" className="dsh-settings-modal">
      <header className="modal-header dsh-modal-header">
        <h2 id="settings-title">设置</h2>
        <div className="dsh-modal-actions">
          <button type="button" className="icon-button" data-autofocus onClick={close} aria-label="关闭设置">×</button>
        </div>
      </header>
      <main className="dsh-settings-content">
        <header className="dsh-view-header"><h3>消息</h3><p>调整你发送的消息和模型最终回复的字号。</p></header>
        <label className="message-font-setting"><span>消息字号</span><input type="number" aria-label="消息字号" min={messageFontSize.min} max={messageFontSize.max} step="1" value={state.messageFontSize} onChange={event => { if (event.target.value !== '') appStore.setMessageFontSize(Number(event.target.value)) }} /><span>px</span></label>
        <header className="dsh-view-header"><h3>模型</h3><p>填入各提供方的 API 密钥即可使用其模型。</p></header>
        <div className="dsh-provider-list">
          {catalog?.providers.map(provider => (
            <div key={provider.providerId} className="dsh-row-card">
              <div className="dsh-row-head">
                <button type="button" className="dsh-provider-toggle" aria-expanded={editing === provider.providerId} aria-controls={`provider-editor-${provider.providerId}`} onClick={() => { setAdding(false); setEditing(editing === provider.providerId ? null : provider.providerId) }}><span className="dsh-row-identity"><strong>{provider.displayName || provider.providerId}</strong>
                  <span className={`dsh-credential-dot dsh-credential-dot-${provider.credentialConfigured ? 'configured' : 'missing'}`} role="img" aria-label={provider.credentialConfigured ? 'API 密钥已配置' : 'API 密钥缺失'} />
                </span><ExpandChevron expanded={editing === provider.providerId} size={16} /></button>
                <span className="dsh-row-actions">
                  <button type="button" className="quiet-button danger" aria-label={`删除提供方 ${provider.displayName || provider.providerId}`} onClick={() => { appStore.clearError(actionOrigin.provider(provider.providerId)); setRemoving(provider) }}>删除</button>
                </span>
              </div>
              <Disclosure open={editing === provider.providerId}><div id={`provider-editor-${provider.providerId}`}><ProviderEditor key={provider.providerId} state={state} provider={provider} onDone={() => setEditing(null)} /></div></Disclosure>
            </div>
          ))}
          {catalog?.providers.length === 0 && <p className="dsh-provider-empty">尚未配置模型提供方。</p>}
        </div>
        {!adding ? <div className="dsh-add-actions">
          <button type="button" className="dsh-add-card-btn" onClick={() => { setEditing(null); setAdding(true) }}>＋ 添加提供方</button>
        </div> : <ProviderEditor state={state} onDone={() => setAdding(false)} />}
      </main>
      <Dialog open={removing !== null} onClose={() => setRemoving(null)} labelledBy="remove-provider-title" className="confirm-modal">
        <header className="modal-header"><h2 id="remove-provider-title">删除提供方</h2></header>
        <div className="confirm-body">
          <p>删除“{removing?.displayName || removing?.providerId}”及其模型配置和 API 密钥？已经运行的回合会继续；使用它的任务下次发送前需要重新选择模型。</p>
          {removing && state.actionErrors[actionOrigin.provider(removing.providerId)] && <p role="alert" className="form-error">{state.actionErrors[actionOrigin.provider(removing.providerId)].message}</p>}
          <footer><button type="button" className="secondary-button" data-autofocus onClick={() => setRemoving(null)}>取消</button>
            <button type="button" className="danger-button" disabled={removing !== null && state.pendingActions.has(pendingKey('model.removeProvider', actionOrigin.provider(removing.providerId)))} onClick={async () => { if (removing && await appStore.removeProvider(removing.providerId)) setRemoving(null) }}>删除</button></footer>
        </div>
      </Dialog>
    </Dialog>
  )
}

function InitialSetup({ state, onClose }: { state: SettingsState; onClose: () => void }) {
  // 在保存动作完成前保持所选编辑器不变。
  const [missing] = useState(() => state.bootstrap?.modelCatalog.providers.find(provider => !provider.credentialConfigured))
  return <Dialog open onClose={onClose} labelledBy="initial-setup-title">
    <header className="modal-header"><h2 id="initial-setup-title">{missing ? '填写 API 密钥' : '添加模型提供方'}</h2><button type="button" className="quiet-button" onClick={onClose}>稍后配置</button></header>
    {missing ? <CredentialSetup provider={missing} state={state} onDone={onClose} /> : <ProviderEditor state={state} onDone={onClose} />}
  </Dialog>
}

function CredentialSetup({ provider, state, onDone }: { provider: RedactedProvider; state: SettingsState; onDone: () => void }) {
  const [apiKey, setApiKey] = useState('')
  const origin = actionOrigin.providerKey(provider.providerId)
  const busy = state.pendingActions.has(pendingKey('model.setApiKey', origin))
  return <form className="dsh-editor" onSubmit={async event => { event.preventDefault(); if (!busy && apiKey.trim() && await appStore.setApiKey(provider.providerId, apiKey.trim())) { setApiKey(''); onDone() } }}>
    <label className="dsh-field"><span>{provider.displayName || provider.providerId} API 密钥</span><input className="dsh-input" type="password" autoFocus autoComplete="off" value={apiKey} onChange={event => setApiKey(event.target.value)} /></label>
    {state.actionErrors[origin] && <p role="alert">{state.actionErrors[origin].message}</p>}
    <button type="submit" className="primary-button" disabled={busy || !apiKey.trim()}>保存</button>
  </form>
}

function ProviderEditor({ state, provider, onDone }: { state: SettingsState; provider?: RedactedProvider; onDone: () => void }) {
  const [providerId, setProviderId] = useState(provider?.providerId ?? '')
  const [name, setName] = useState(provider?.displayName ?? '')
  const [baseUrl, setBaseUrl] = useState(provider?.baseUrl ?? '')
  const [apiKey, setApiKey] = useState('')
  const [protocol, setProtocol] = useState(provider?.models[0] ? provider.models[0].apiProtocol ?? '' : 'chat')
  const [models, setModels] = useState<ModelInput[]>(() => provider?.models ?? [])
  const [modelEditor, setModelEditor] = useState<{ index: number | null; draft: ModelDraft } | null>(null)
  const [saved, setSaved] = useState(false)
  const origin = actionOrigin.provider(providerId.trim())
  const busy = state.pendingActions.has(pendingKey('model.saveProvider', origin))
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
  const saveError = state.actionErrors[origin]
  /** 校验并提交模型编辑草稿：返回错误消息表示未提交（编辑框保留），null 表示已写入列表。 */
  const commitModel = (index: number | null, draft: ModelDraft): string | null => {
    if (invalidModelId(models, index, draft.modelId)) return '请输入有效且不重复的模型 ID。'
    const context = parseCapacity(draft.contextText), output = parseCapacity(draft.outputText)
    if (context === undefined || output === undefined) return '容量应为空或正整数，可使用 K / M。'
    // 文本容量只在此解析一次；列表此后保存数值，展示使用紧凑 token 格式。
    const { contextText, outputText, ...model } = draft
    const next: ModelInput = { ...model, maxContextTokens: context, maxOutputTokens: output }
    setModels(rows => index === null ? [...rows, next] : rows.map((row, at) => at === index ? next : row))
    setModelEditor(null)
    return null
  }

  const discover = async () => {
    const revision = ++discoveryRevision.current
    setFailure(null); setFetching(true)
    try {
      // 局部查询直接复用 Store 持有的同一条连接；空密钥映射留在调用边界，
      // 请求仍走既有 RPC envelope/版本/错误处理。
      const found = await appStore.transport.rpc('model.discover', {
        providerId: providerId.trim(),
        baseUrl,
        apiKey: apiKey.trim() || null,
      })
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
    setModels(rows => mergeDiscoveredModels(rows, candidates ?? [], picked, protocol))
    setCandidates(null)
  }
  const save = async (event: FormEvent) => {
    event.preventDefault(); setFailure(null)
    if (!/^[^\s/#]+$/.test(providerId.trim())) { setFailure('请输入不含空格、/ 或 # 的提供方 ID。'); return }
    if (!provider && !saved && state.bootstrap?.modelCatalog.providers.some(p => p.providerId === providerId.trim())) { setFailure('该提供方 ID 已存在，请编辑已有提供方。'); return }
    let url: URL
    try { url = new URL(baseUrl) } catch { setFailure('请输入完整的 API 地址。'); return }
    if (!['https:', 'http:'].includes(url.protocol) || url.username || url.password || url.search || url.hash) { setFailure('API 地址必须为 http 或 https 地址，不含凭据、查询或片段。'); return }
    const submitted: ModelInput[] = []
    for (const [index, model] of models.entries()) {
      const id = model.modelId.trim()
      if (invalidModelId(models, index, id)) { setFailure(`第 ${index + 1} 行模型 ID 为空、重复或包含无效字符。`); return }
      // 列表里的容量已由行编辑解析或来自提供方发现结果；这里只校验数值域，
      // 因为 ModelInput 是类型，不证明外部给的数值一定合法。
      if (!validCapacity(model.maxContextTokens) || !validCapacity(model.maxOutputTokens)) { setFailure(`第 ${index + 1} 行容量应为空或正整数，可使用 K / M。`); return }
      submitted.push({ ...model, modelId: id, displayName: model.displayName?.trim() || null })
    }
    if (await appStore.saveProvider({ providerId: providerId.trim(), displayName: name.trim() || null, baseUrl, models: submitted }, apiKey.trim())) {
      setApiKey(''); onDone()
    } else if (appStore.getSnapshot().actionErrors[origin]?.code === 'configuration_partially_saved') setSaved(true)
  }
  return (
    <>
    <form className="dsh-editor" onSubmit={event => void save(event)} noValidate>
      <fieldset disabled={busy} className="provider-fields">
        {provider || saved ? <strong>{name || providerId} <small className="provider-id">{providerId}</small></strong>
          : <label className="dsh-field"><span>提供方 ID</span><input className="dsh-input" autoFocus value={providerId} onChange={e => setProviderId(e.target.value)} placeholder="例如 my-provider" /></label>}
        <label className="dsh-field"><span>API 密钥</span><input className="dsh-input" type="password" autoComplete="off" value={apiKey} onChange={e => setApiKey(e.target.value)} placeholder={provider?.credentialConfigured ? '已配置——输入新值可替换' : '输入 API 密钥'} /></label>
        <div className="dsh-customized-body">
            <label className="dsh-field"><span>显示名称</span><input className="dsh-input" value={name} onChange={e => setName(e.target.value)} placeholder={providerId} /></label>
            <label className="dsh-field"><span>API 地址</span><input className="dsh-input" value={baseUrl} onChange={e => setBaseUrl(e.target.value)} placeholder="https://api.example.com/v1" /></label>
            <label className="dsh-field"><span>API 协议</span><select className="dsh-input" value={protocol} onChange={e => { const value = e.target.value; setProtocol(value); setModels(rows => rows.map(row => ({ ...row, apiProtocol: value }))) }}><ProtocolOptions value={protocol} /></select></label>
            <section className="dsh-model-catalog" aria-label="模型目录">
              <div className="dsh-model-catalog-head"><span className="dsh-model-catalog-title">模型目录</span><span className="dsh-row-actions">
                <button type="button" className="quiet-button" disabled={fetching || !baseUrl.trim()} onClick={() => void discover()}>{fetching ? '正在询问提供方…' : '获取可用模型'}</button>
              </span></div>
              {models.length === 0 && <p className="dsh-model-empty">尚无模型。获取可用模型或手动添加后，即可在任务中选择。</p>}
              <div className="dsh-model-list">{models.map((model, index) => <div key={index} className="dsh-model-entry">
                <div className="dsh-model-row">
                  <span>{model.displayName || model.modelId}</span><small>{model.maxContextTokens === null ? '' : `${formatTokenCount(model.maxContextTokens)} 上下文`}</small>
                  <button type="button" className="quiet-button" onClick={() => setModelEditor({ index, draft: toDraft(model) })}>编辑</button>
                  <button type="button" className="dsh-icon-btn dsh-icon-btn-danger" aria-label={`删除模型 ${index + 1}`} onClick={() => setModels(rows => rows.filter((_, at) => at !== index))}>×</button>
                </div>
              </div>)}</div>
              <button type="button" className="dsh-add-model-btn" onClick={() => setModelEditor({ index: null, draft: toDraft({ ...blankModel(), apiProtocol: protocol }) })}>＋ 添加模型</button>
            </section>
        </div>
        {failure && <p className="form-error" role="alert">{failure}</p>}
        {saveError && <p className="form-error" role="alert">{saveError.message}</p>}
        <footer className="dsh-editor-actions"><button type="button" className="dsh-secondary-btn" onClick={onDone}>取消</button><button type="submit" className="dsh-primary-btn">{busy ? '保存中…' : '保存'}</button></footer>
      </fieldset>
    </form>
      {modelEditor && <ModelEditor index={modelEditor.index} initial={modelEditor.draft} onConfirm={draft => commitModel(modelEditor.index, draft)} onClose={() => setModelEditor(null)} />}
      <Dialog open={candidates !== null} onClose={() => setCandidates(null)} labelledBy="discovered-models-title" className="confirm-modal model-discovery-modal">
        <header className="modal-header"><h2 id="discovered-models-title">选择可用模型</h2><button type="button" className="icon-button" onClick={() => setCandidates(null)} aria-label="关闭模型列表">×</button></header>
        <div className="model-candidates">{candidates?.map(candidate => <label key={candidate.modelId}><input type="checkbox" checked={picked.has(candidate.modelId)} onChange={() => setPicked(current => { const next = new Set(current); if (!next.delete(candidate.modelId)) next.add(candidate.modelId); return next })} /><span>{candidate.modelId}<small>{candidate.reasoningVariants.length ? candidate.reasoningVariants.map(variant => variant.id).join(' / ') : '未获取思考档位'}{candidate.maxContextTokens ? ` · ${formatTokenCount(candidate.maxContextTokens)}` : ''}</small></span>{models.some(model => model.modelId.trim() === candidate.modelId) && <small>更新配置</small>}</label>)}</div>
        <footer className="dsh-editor-actions"><button type="button" className="dsh-secondary-btn" onClick={() => setCandidates(null)}>取消</button><button type="button" className="dsh-primary-btn" onClick={adopt}>应用所选模型</button></footer>
      </Dialog>
    </>
  )
}

/** 模型编辑弹窗：拥有本次编辑草稿与输入错误，校验与列表写入由父级的确认回调完成。 */
function ModelEditor({ index, initial, onConfirm, onClose }: { index: number | null; initial: ModelDraft; onConfirm: (draft: ModelDraft) => string | null; onClose: () => void }) {
  const [draft, setDraft] = useState(initial)
  const [error, setError] = useState<string | null>(null)
  const patch = (change: Partial<ModelDraft>) => setDraft(current => ({ ...current, ...change }))
  return (
    <Dialog open onClose={onClose} labelledBy="model-editor-title" className="confirm-modal">
      <header className="modal-header"><h2 id="model-editor-title">{index === null ? '添加模型' : '编辑模型'}</h2><button type="button" className="icon-button" onClick={onClose} aria-label="关闭模型编辑">×</button></header>
      <form className="confirm-body" onSubmit={event => { event.preventDefault(); setError(onConfirm(draft)) }}>
        <label className="dsh-field"><span>模型 ID</span><input className="dsh-input" data-autofocus value={draft.modelId} onChange={e => patch({ modelId: e.target.value })} /></label>
        <label className="dsh-field"><span>显示名称</span><input className="dsh-input" value={draft.displayName ?? ''} onChange={e => patch({ displayName: e.target.value })} /></label>
        <label className="dsh-field"><span>上下文窗口</span><input className="dsh-input" value={draft.contextText} placeholder="留空按 128K 估算，可填 500K" onChange={e => patch({ contextText: e.target.value })} /></label>
        <label className="dsh-field"><span>最大输出 Token</span><input className="dsh-input" value={draft.outputText} placeholder="留空按 4K 估算，可填 32K" onChange={e => patch({ outputText: e.target.value })} /></label>
        <details><summary>高级配置</summary><label className="dsh-field"><span>API 协议</span><select className="dsh-input" value={draft.apiProtocol ?? ''} onChange={e => patch({ apiProtocol: e.target.value })}><ProtocolOptions value={draft.apiProtocol} /></select></label></details>
        {error && <p className="form-error" role="alert">{error}</p>}
        <footer className="dsh-editor-actions"><button type="button" className="dsh-secondary-btn" onClick={onClose}>取消</button><button type="submit" className="dsh-primary-btn">保存</button></footer>
      </form>
    </Dialog>
  )
}

/** 空输入为 null（不声明容量，执行层按保守下界估算）；解析失败为 undefined。 */
function parseCapacity(value: string): number | null | undefined {
  if (!value.trim()) return null
  const match = /^(\d+(?:\.\d+)?)\s*([km])?$/i.exec(value.trim())
  if (!match) return undefined
  const parsed = Number(match[1]) * (match[2]?.toLowerCase() === 'm' ? 1_000_000 : match[2] ? 1_000 : 1)
  return validCapacity(parsed) ? parsed : undefined
}
/** 容量数值的唯一合法域；null 表示留空、按保守下界估算。 */
function validCapacity(value: number | null): boolean { return value === null || (Number.isSafeInteger(value) && value > 0 && value <= 0xffffffff) }
function capacityInput(value: number | null): string { return value === null ? '' : value % 1_000_000 === 0 ? `${value / 1_000_000}M` : value % 1_000 === 0 ? `${value / 1_000}K` : String(value) }

function invalidModelId(models: ModelInput[], index: number | null, value: string): boolean {
  const id = value.trim()
  return !id || /\s|#/.test(id) || models.some((model, at) => at !== index && model.modelId.trim() === id)
}
