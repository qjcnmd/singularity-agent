import { useLayoutEffect, useRef, useState, type FormEvent } from 'react'
import { formatTokenCount } from '../copy'
import { actionOrigin, appStore, pendingKey, type AppState } from '../appStore'
import type { DiscoveredModel, ProviderConfigurationInput, RedactedProvider } from '../protocol'
import { applyProtocol, blankModel, isValidCapacity, mergeDiscoveredModels } from '../modelConfiguration'
import { ModelEditor } from './ModelEditor'
import { Dialog } from './Dialog'

type ModelInput = ProviderConfigurationInput['models'][number]
type ProviderEditorProps = {
  state: Pick<AppState, 'bootstrap' | 'actionErrors' | 'pendingActions'>
  provider?: RedactedProvider
  onDone: () => void
}

export function ProviderEditor({ state, provider, onDone }: ProviderEditorProps) {
  const [providerId, setProviderId] = useState(provider?.providerId ?? '')
  const [name, setName] = useState(provider?.displayName ?? '')
  const [baseUrl, setBaseUrl] = useState(provider?.baseUrl ?? '')
  const [apiKey, setApiKey] = useState('')
  const [protocol, setProtocol] = useState(provider?.apiProtocol ?? provider?.models[0]?.apiProtocol ?? 'chat')
  const [models, setModels] = useState<ModelInput[]>(() => provider?.models ?? [])
  const [modelEditor, setModelEditor] = useState<{ index: number | null; draft: ModelInput } | null>(null)
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
  const commitModel = (index: number | null, draft: ModelInput): string | null => {
    if (invalidModelId(models, index, draft.modelId)) return '请输入有效且不重复的模型 ID。'
    setModels(rows => index === null ? [...rows, draft] : rows.map((row, at) => at === index ? draft : row))
    setModelEditor(null)
    return null
  }

  function queryModels(apiProtocol: string) {
    return appStore.transport.rpc('model.discover', {
      providerId: providerId.trim(), baseUrl, apiKey: apiKey.trim() || null, apiProtocol,
    })
  }

  function changeProtocol(next: string) {
    setProtocol(next)
    setModels(rows => rows.map(row => applyProtocol(row, next)))
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
      // 局部查询直接复用 Store 持有的同一条连接；空密钥映射留在调用边界，
      // 请求仍走既有 RPC envelope/版本/错误处理。
      const found = await queryModels(protocol)
      if (revision !== discoveryRevision.current) return
      if (found.length === 0) {
        setFailure('提供方没有返回可用模型，仍可手动添加。')
        return
      }
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
    event.preventDefault()
    setFailure(null)
    if (!/^[^\s/#]+$/.test(providerId.trim())) {
      setFailure('请输入不含空格、/ 或 # 的提供方 ID。')
      return
    }
    if (!provider && !saved && state.bootstrap?.modelCatalog.providers.some(p => p.providerId === providerId.trim())) {
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
      // 列表里的容量已由行编辑解析或来自提供方发现结果；这里只校验数值域，
      // 因为 ModelInput 是类型，不证明外部给的数值一定合法。
      if (model.maxContextTokens === null || model.maxOutputTokens === null || !isValidCapacity(model.maxContextTokens) || !isValidCapacity(model.maxOutputTokens)) {
        setFailure(`请编辑第 ${index + 1} 行模型，补齐有效的上下文窗口和最大输出 Token。`)
        return
      }
      submitted.push({ ...model, modelId: id, displayName: model.displayName?.trim() || null })
    }
    if (await appStore.saveProvider({ providerId: providerId.trim(), displayName: name.trim() || null, baseUrl, apiProtocol: protocol, models: submitted }, apiKey.trim())) {
      setApiKey('')
      onDone()
    } else if (appStore.getSnapshot().actionErrors[origin]?.code === 'configuration_partially_saved') {
      setSaved(true)
    }
  }
  return (
    <>
      <form className="sg-editor" onSubmit={event => void save(event)} noValidate>
        <fieldset disabled={busy} className="provider-fields">
          {provider || saved ? <strong>{name || providerId} <small className="provider-id">{providerId}</small></strong>
            : <label className="sg-field">
              <span>提供方 ID</span>
              <input className="sg-input" autoFocus value={providerId} onChange={event => setProviderId(event.target.value)} placeholder="例如 my-provider" />
            </label>}
          <label className="sg-field">
            <span>API 密钥</span>
            <input className="sg-input" type="password" autoComplete="off" value={apiKey} onChange={event => setApiKey(event.target.value)} placeholder={provider?.credentialConfigured ? '已配置——输入新值可替换' : '输入 API 密钥'} />
          </label>
          <div className="sg-customized-body">
            <label className="sg-field">
              <span>显示名称</span>
              <input className="sg-input" value={name} onChange={event => setName(event.target.value)} placeholder={providerId} />
            </label>
            <label className="sg-field">
              <span>API 地址</span>
              <input className="sg-input" value={baseUrl} onChange={event => setBaseUrl(event.target.value)} placeholder="https://api.example.com/v1" />
            </label>
            <label className="sg-field">
              <span>API 协议</span>
              <select aria-label="API 协议" className="sg-input" value={protocol} onChange={event => changeProtocol(event.target.value)}>
                {!['chat', 'responses'].includes(protocol) && <option value={protocol}>{protocol ? `无效协议：${protocol}` : '请选择协议'}</option>}
                <option value="chat">Chat Completions</option>
                <option value="responses">Responses</option>
              </select>
            </label>
            <section className="sg-model-catalog" aria-label="模型目录">
              <div className="sg-model-catalog-head">
                <span className="sg-model-catalog-title">模型目录</span>
                <span className="sg-row-actions">
                  <button type="button" className="quiet-button" disabled={fetching || !baseUrl.trim()} onClick={() => void discover()}>{fetching ? '正在询问提供方…' : '获取可用模型'}</button>
                </span>
              </div>
              {models.length === 0 && <p className="sg-model-empty">尚无模型。获取可用模型或手动添加后，即可在任务中选择。</p>}
              <div className="sg-model-list">
                {models.map((model, index) => (
                  <div key={index} className="sg-model-entry">
                    <div className="sg-model-row">
                      <span>{model.displayName || model.modelId}</span>
                      <small>{model.maxContextTokens === null ? '' : `${formatTokenCount(model.maxContextTokens)} 上下文`}</small>
                      <button type="button" className="quiet-button" onClick={() => setModelEditor({ index, draft: applyProtocol(model, protocol) })}>编辑</button>
                      <button type="button" className="sg-icon-btn sg-icon-btn-danger" aria-label={`删除模型 ${index + 1}`} onClick={() => setModels(rows => rows.filter((_, at) => at !== index))}>×</button>
                    </div>
                  </div>
                ))}
              </div>
              <button type="button" className="sg-add-model-btn" onClick={() => setModelEditor({ index: null, draft: { ...blankModel(), apiProtocol: protocol } })}>＋ 添加模型</button>
            </section>
          </div>
          {failure && <p className="form-error" role="alert">{failure}</p>}
          {saveError && <p className="form-error" role="alert">{saveError.message}</p>}
          <footer className="sg-editor-actions">
            <button type="button" className="sg-secondary-btn" onClick={onDone}>取消</button>
            <button type="submit" className="sg-primary-btn">{busy ? '保存中…' : '保存'}</button>
          </footer>
        </fieldset>
      </form>
      {modelEditor && <ModelEditor
        index={modelEditor.index} initial={modelEditor.draft} discover={queryModels}
        onConfirm={draft => commitModel(modelEditor.index, draft)} onClose={() => setModelEditor(null)}
      />}
      <Dialog open={candidates !== null} onClose={() => setCandidates(null)} labelledBy="discovered-models-title" className="confirm-modal model-discovery-modal">
        <header className="modal-header">
          <h2 id="discovered-models-title">选择可用模型</h2>
          <button type="button" className="icon-button" onClick={() => setCandidates(null)} aria-label="关闭模型列表">×</button>
        </header>
        <div className="model-candidates">
          {candidates?.map(candidate => (
            <label key={candidate.modelId}>
              <input type="checkbox" checked={picked.has(candidate.modelId)} onChange={() => toggleCandidate(candidate.modelId)} />
              <span>
                {candidate.modelId}
                {(candidate.reasoningVariants.length > 0 || candidate.maxContextTokens !== null) && <small>
                  {candidate.reasoningVariants.map(variant => variant.id).join(' / ')}
                  {candidate.maxContextTokens ? `${candidate.reasoningVariants.length ? ' · ' : ''}${formatTokenCount(candidate.maxContextTokens)}` : ''}
                </small>}
              </span>
              {models.some(model => model.modelId.trim() === candidate.modelId) && <small>更新配置</small>}
            </label>
          ))}
        </div>
        <footer className="sg-editor-actions">
          <button type="button" className="sg-secondary-btn" onClick={() => setCandidates(null)}>取消</button>
          <button type="button" className="sg-primary-btn" onClick={adopt}>添加模型</button>
        </footer>
      </Dialog>
    </>
  )
}

function invalidModelId(models: ModelInput[], index: number | null, value: string): boolean {
  const id = value.trim()
  return !id || /\s|#/.test(id) || models.some((model, at) => at !== index && model.modelId.trim() === id)
}
