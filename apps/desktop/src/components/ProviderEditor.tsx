import { Plus, X } from 'lucide-react'
import { useLayoutEffect, useRef, useState, type FormEvent } from 'react'
import { formatTokenCount } from '../copy'
import { actionOrigin, appStore, pendingKey, type AppState } from '../appStore'
import type { ProviderConfigurationInput, RedactedProvider } from '../protocol'
import { applyProtocol, blankModel, capacityConfigurationError, reasoningConfigurationError } from '../modelConfiguration'
import { ModelEditor } from './ModelEditor'
import { Dialog } from './Dialog'
import { ProtocolPicker } from './ProtocolPicker'

type ModelInput = ProviderConfigurationInput['models'][number]
type ProviderEditorProps = {
  state: Pick<AppState, 'bootstrap' | 'actionErrors' | 'pendingActions'>
  provider?: RedactedProvider
  active: boolean
  onDone: () => void
}

export function ProviderEditor({ state, provider, active, onDone }: ProviderEditorProps) {
  const [providerId, setProviderId] = useState(provider?.providerId ?? '')
  const [name, setName] = useState(provider?.displayName ?? '')
  const [baseUrl, setBaseUrl] = useState(provider?.baseUrl ?? '')
  const [apiKey, setApiKey] = useState('')
  const [protocol, setProtocol] = useState<ProviderConfigurationInput['apiProtocol']>(provider?.apiProtocol ?? 'chat')
  const [models, setModels] = useState<ModelInput[]>(() => provider?.models ?? [])
  const [modelEditor, setModelEditor] = useState<{ index: number | null; draft: ModelInput } | null>(null)
  const origin = actionOrigin.inline('model.saveProvider', providerId.trim())
  const busy = state.pendingActions.has(pendingKey('model.saveProvider', origin))
  const [fetching, setFetching] = useState(false)
  const [failure, setFailure] = useState<string | null>(null)
  const [candidates, setCandidates] = useState<string[] | null>(null)
  const [picked, setPicked] = useState<Set<string>>(new Set())
  const discoveryRevision = useRef(0)
  useLayoutEffect(() => {
    setFetching(false)
    setCandidates(null)
    setFailure(null)
    if (!active) setModelEditor(null)
    // 收起动画仍保留组件；关闭编辑器时就丢弃未返回的查询。
    return () => { discoveryRevision.current += 1 }
  }, [providerId, baseUrl, apiKey, active])
  const saveError = state.actionErrors[origin]
  /** 校验并提交模型草稿。返回错误提示字符串表示校验失败，null 表示已成功加入列表。 */
  const commitModel = (index: number | null, draft: ModelInput): string | null => {
    if (invalidModelId(models, index, draft.modelId)) return '请输入有效且不重复的模型 ID。'
    setModels(rows => index === null ? [...rows, draft] : rows.map((row, at) => at === index ? draft : row))
    setModelEditor(null)
    return null
  }

  function toggleCandidate(modelId: string) {
    setPicked(current => {
      const next = new Set(current)
      if (!next.delete(modelId)) next.add(modelId)
      return next
    })
  }

  const discover = async () => {
    const revision = ++discoveryRevision.current
    setFailure(null)
    setFetching(true)
    try {
      // 通过后端查询提供方的可用模型列表
      const found = await appStore.transport.rpc('model.discover', {
        providerId: providerId.trim(), baseUrl, apiKey: apiKey.trim() || null,
      })
      if (revision !== discoveryRevision.current) return
      if (found.length === 0) {
        setFailure('提供方没有返回可用模型。')
        return
      }
      setCandidates(found)
      setPicked(new Set(found.filter(id => models.some(model => model.modelId.trim() === id))))
    } catch (error) {
      if (revision === discoveryRevision.current) setFailure(error instanceof Error ? error.message : '获取模型失败。')
    } finally {
      if (revision === discoveryRevision.current) setFetching(false)
    }
  }
  const adopt = () => {
    setModels(rows => [...rows, ...(candidates ?? [])
      .filter(id => picked.has(id) && !rows.some(model => model.modelId.trim() === id))
      .map(modelId => ({ ...blankModel(), modelId }))])
    setCandidates(null)
  }
  const save = async (event: FormEvent) => {
    event.preventDefault()
    setFailure(null)
    appStore.clearError(origin)
    if (!/^[^\s\x00-\x1f\x7f]+$/.test(providerId.trim())) {
      setFailure('请输入不含空白和控制字符的提供方 ID。')
      return
    }
    if (!provider && state.bootstrap?.modelCatalog.providers.some(p => p.providerId === providerId.trim())) {
      setFailure('该提供方 ID 已存在，请编辑已有提供方。')
      return
    }
    let url: URL
    try {
      url = new URL(baseUrl)
    } catch {
      setFailure('请输入完整的 API 地址。')
      return
    }
    if (!['https:', 'http:'].includes(url.protocol) || url.username || url.password || url.search || url.hash) {
      setFailure('API 地址必须为 http 或 https 地址，不含凭据、查询或片段。')
      return
    }
    const submitted: ModelInput[] = []
    for (const [index, model] of models.entries()) {
      const id = model.modelId.trim()
      if (invalidModelId(models, index, id)) {
        setFailure(`第 ${index + 1} 行模型 ID 为空、重复或包含无效字符。`)
        return
      }
      const capacityError = capacityConfigurationError(model.maxContextTokens, model.maxOutputTokens)
      if (capacityError !== null) {
        setFailure(`模型 ${id}：${capacityError}`)
        return
      }
      const reasoningError = reasoningConfigurationError(model, protocol)
      if (reasoningError !== null) {
        setFailure(`模型 ${id}：${reasoningError}`)
        return
      }
      submitted.push({ ...model, modelId: id })
    }
    const result = await appStore.saveProvider({ providerId: providerId.trim(), displayName: name.trim() || null, baseUrl, apiProtocol: protocol, models: submitted }, apiKey.trim())
    if (result) {
      setApiKey('')
      onDone()
    }
  }
  return (
    <>
      <form className="sg-editor" onSubmit={event => void save(event)} noValidate>
        <fieldset disabled={busy} className="provider-fields">
          {!provider && <label className="sg-field">
              <span>提供方 ID</span>
              <input className="sg-input" autoFocus value={providerId} onChange={event => setProviderId(event.target.value)} />
            </label>}
          <label className="sg-field">
            <span>API 密钥</span>
            <input className="sg-input" type="password" autoComplete="off" value={apiKey} onChange={event => setApiKey(event.target.value)} />
          </label>
          <div className="field-row">
            <label className="sg-field"><span>显示名称</span><input className="sg-input" value={name} onChange={event => setName(event.target.value)} /></label>
            <div className="sg-field"><span>API 协议</span><ProtocolPicker value={protocol} onChange={setProtocol} /></div>
          </div>
          <label className="sg-field"><span>API 地址</span><input className="sg-input" value={baseUrl} onChange={event => setBaseUrl(event.target.value)} /></label>
            <section className="sg-model-catalog" aria-label="模型目录">
              <div className="sg-model-catalog-head">
                <strong>模型目录</strong>
                <span className="sg-row-actions">
                  <button type="button" className="quiet-button" disabled={fetching || !baseUrl.trim()} onClick={() => void discover()}>{fetching ? '正在询问提供方…' : '获取可用模型'}</button>
                </span>
              </div>
              {models.length === 0 && <p className="sg-model-empty">尚无模型。</p>}
              <div className="sg-model-list">
                {models.map((model, index) => {
                  const reasoningError = reasoningConfigurationError(model, protocol)
                  return (
                  <div key={index}>
                    <div className="sg-model-row">
                      <span className="catalog-id">{model.modelId}</span>
                      <small>{model.maxContextTokens === null ? '' : `${formatTokenCount(model.maxContextTokens)} 上下文`}{model.reasoningVariants?.length ? ` · ${model.reasoningVariants.join(' / ')}` : ''}</small>
                      <button type="button" className="quiet-button" onClick={() => setModelEditor({ index, draft: applyProtocol(model, protocol) })}>编辑</button>
                      <button type="button" className="sg-icon-btn danger" aria-label={`删除模型 ${model.modelId}`} onClick={() => setModels(rows => rows.filter((_, at) => at !== index))}><X size={14} /></button>
                    </div>
                    {reasoningError && <p className="form-error" role="alert">{reasoningError}</p>}
                  </div>
                  )
                })}
              </div>
              <button type="button" className="sg-add-model-btn" onClick={() => setModelEditor({ index: null, draft: blankModel() })}><Plus size={14} />添加模型</button>
            </section>
          {failure && <p className="form-error" role="alert">{failure}</p>}
          {saveError && <p className="form-error" role="alert">{saveError.message}</p>}
          <footer className="sg-editor-actions">
            <button type="button" className="sg-secondary-btn" onClick={onDone}>取消</button>
            <button type="submit" className="sg-primary-btn">{busy ? '保存中…' : '保存配置'}</button>
          </footer>
        </fieldset>
      </form>
      {active && modelEditor && <ModelEditor
        index={modelEditor.index} initial={modelEditor.draft} protocol={protocol}
        onConfirm={draft => commitModel(modelEditor.index, draft)} onClose={() => setModelEditor(null)}
      />}
      <Dialog open={active && candidates !== null} onClose={() => setCandidates(null)} labelledBy="discovered-models-title" className="model-discovery-modal">
        <header className="modal-header">
          <h2 id="discovered-models-title">选择可用模型</h2>
          <button type="button" className="icon-button" onClick={() => setCandidates(null)} aria-label="关闭模型列表"><X size={18} /></button>
        </header>
        <div className="model-candidates">
          {candidates?.map(id => (
            <label key={id} className={`candidate-row${picked.has(id) ? ' checked' : ''}`}>
              <input type="checkbox" checked={picked.has(id)} onChange={() => toggleCandidate(id)} />
              <span>
                <strong>{id}</strong>
              </span>
              {models.some(model => model.modelId.trim() === id) && <em>已添加</em>}
            </label>
          ))}
        </div>
        <footer className="sg-editor-actions">
          <button type="button" className="sg-secondary-btn" onClick={() => setCandidates(null)}>取消</button>
          <button type="button" className="sg-primary-btn" disabled={picked.size === 0} onClick={adopt}>添加模型</button>
        </footer>
      </Dialog>
    </>
  )
}

function invalidModelId(models: ModelInput[], index: number | null, value: string): boolean {
  const id = value.trim()
  return !id || /[\s\x00-\x1f\x7f]/.test(id) || models.some((model, at) => at !== index && model.modelId.trim() === id)
}
