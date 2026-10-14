import { useEffect, useRef, useState, type FormEvent } from 'react'
import type { DiscoveredModel, ModelConfigurationField, ProviderConfigurationInput } from '../protocol'
import { automaticFieldsFor, blankModel, formatCapacityInput, mergeDiscoveredModel, parseCapacity, reasoningConfigurationError, reasoningOptionsFor } from '../modelConfiguration'
import { Dialog } from './Dialog'
import { ReasoningOptions } from './ReasoningOptions'

type ModelInput = ProviderConfigurationInput['models'][number]
type ModelDraft = Omit<ModelInput, 'maxContextTokens' | 'maxOutputTokens'> & { contextText: string; outputText: string }
function toDraft({ maxContextTokens, maxOutputTokens, ...model }: ModelInput): ModelDraft {
  return { ...model, contextText: formatCapacityInput(maxContextTokens), outputText: formatCapacityInput(maxOutputTokens) }
}

/** 查询完成时采纳自动字段；保留手工容量的原始文本，供保存时校验。 */
function fillDraft(draft: ModelDraft, discovered: DiscoveredModel): ModelDraft {
  const model = {
    ...draft,
    maxContextTokens: parseCapacity(draft.contextText) ?? null,
    maxOutputTokens: parseCapacity(draft.outputText) ?? null,
  }
  const merged = mergeDiscoveredModel(model, discovered, draft.apiProtocol ?? 'chat')
  const resolved = toDraft(merged)
  return {
    ...resolved,
    contextText: draft.automaticFields?.includes('maxContextTokens') ? resolved.contextText : draft.contextText,
    outputText: draft.automaticFields?.includes('maxOutputTokens') ? resolved.outputText : draft.outputText,
  }
}

type ModelEditorProps = {
  index: number | null
  initial: ModelInput
  onConfirm: (model: ModelInput) => string | null
  onClose: () => void
  discover: (protocol: string) => Promise<DiscoveredModel[]>
}

/** 模型编辑弹窗：拥有自动能力、手工草稿与校验；父级仅确认模型身份和列表写入。 */
export function ModelEditor({ index, initial, onConfirm, onClose, discover }: ModelEditorProps) {
  const [draft, setDraft] = useState(() => toDraft({ ...initial, automaticFields: automaticFieldsFor(initial) }))
  const [error, setError] = useState<string | null>(null)
  const variants = draft.reasoningVariants ?? []
  const [options, setOptions] = useState(() => reasoningOptionsFor(variants, initial.apiProtocol ?? 'chat'))
  const draftRef = useRef(draft)
  draftRef.current = draft
  const revision = useRef(0)
  const [loading, setLoading] = useState(false)
  const canSave = !loading && Boolean(draft.modelId.trim() && draft.contextText.trim() && draft.outputText.trim())
  const [feedback, setFeedback] = useState<string | null>(null)
  useEffect(() => () => { revision.current++ }, [])

  function patch(change: Partial<ModelDraft>) {
    setError(null)
    setFeedback(null)
    setDraft(current => {
      const mapping: Record<string, ModelConfigurationField> = {
        contextText: 'maxContextTokens', outputText: 'maxOutputTokens', defaultVariant: 'reasoningVariants',
      }
      const changed = Object.keys(change).map(key => mapping[key] ?? key)
      const fields = current.automaticFields?.filter(field => !changed.includes(field)) ?? []
      if (change.contextText === '') fields.push('maxContextTokens')
      if (change.outputText === '') fields.push('maxOutputTokens')
      return { ...current, ...change, automaticFields: fields }
    })
  }

  function changeModelId(modelId: string) {
    revision.current++
    setLoading(false)
    setFeedback(null)
    setError(null)
    const current = draftRef.current
    const blank = toDraft({ ...blankModel(), modelId, apiProtocol: current.apiProtocol })
    const next = { ...current, modelId }
    for (const field of current.automaticFields ?? []) {
      if (field === 'maxContextTokens') next.contextText = ''
      else if (field === 'maxOutputTokens') next.outputText = ''
      else Object.assign(next, { [field]: blank[field] })
      if (field === 'reasoningVariants') next.defaultVariant = null
    }
    setDraft(next)
    setOptions(reasoningOptionsFor(next.reasoningVariants ?? [], next.apiProtocol ?? 'chat'))
  }

  const lookup = async () => {
    const id = draft.modelId.trim()
    if (!id) return
    const current = ++revision.current
    setLoading(true)
    setFeedback(null)
    try {
      const found = (await discover(draft.apiProtocol ?? 'chat')).find(model => model.modelId === id)
      if (current !== revision.current) return
      if (!found) {
        setFeedback('未获取模型参数')
        return
      }
      setError(null)
      const previous = draftRef.current
      const next = fillDraft(previous, found)
      setDraft(next)
      setOptions(reasoningOptionsFor([...found.reasoningVariants, ...(next.reasoningVariants ?? [])], next.apiProtocol ?? 'chat'))
      if (found.maxContextTokens === null && found.maxOutputTokens === null && found.reasoningVariants.length === 0) setFeedback('未获取模型参数')
    } catch {
      if (current === revision.current) setFeedback('未获取模型参数')
    } finally {
      if (current === revision.current) setLoading(false)
    }
  }
  const validateAndCommit = (): string | null => {
    const context = parseCapacity(draft.contextText)
    const output = parseCapacity(draft.outputText)
    if (context == null || output == null) return '上下文窗口和最大输出 Token 应为正整数，可使用 K / M。'
    if (output >= context) return '最大输出 Token 必须小于上下文窗口。'
    const reasoningError = reasoningConfigurationError(variants, draft.apiProtocol ?? 'chat')
    if (reasoningError !== null) return reasoningError
    // 文本容量只在此解析一次；列表此后保存数值，展示使用紧凑 token 格式。
    const { contextText, outputText, ...model } = draft
    const next: ModelInput = { ...model, maxContextTokens: context, maxOutputTokens: output }
    return onConfirm(next)
  }
  function submit(event: FormEvent) {
    event.preventDefault()
    if (!canSave) return
    const message = validateAndCommit()
    setError(message)
  }
  return (
    <Dialog open onClose={onClose} labelledBy="model-editor-title" className="confirm-modal model-editor-modal">
      <header className="modal-header">
        <h2 id="model-editor-title">{index === null ? '添加模型' : '编辑模型'}</h2>
        <button type="button" className="icon-button" onClick={onClose} aria-label="关闭模型编辑">×</button>
      </header>
      <form className="model-editor-form" onSubmit={submit}>
        <div className="confirm-body model-editor-scroll">
          <div className="model-identity-row">
            <label className="sg-field">
              <span>模型 ID</span>
              <input className="sg-input" data-autofocus value={draft.modelId} onChange={event => changeModelId(event.target.value)} />
            </label>
            <button type="button" className="sg-secondary-btn model-config-fetch" disabled={loading || !draft.modelId.trim()} aria-busy={loading} onClick={() => void lookup()}>获取模型参数{loading && <span className="model-fetch-spinner" aria-hidden="true" />}</button>
          </div>
          {feedback && <p className="model-fetch-status" role="status">{feedback}</p>}
          <div className="model-capacity-row"><label className="sg-field">
            <span>上下文窗口</span>
            <input aria-label="上下文窗口" className="sg-input" aria-required="true" value={draft.contextText} placeholder="例如 256K" onChange={event => patch({ contextText: event.target.value })} />
          </label>
          <label className="sg-field">
            <span>最大输出 Token</span>
            <input aria-label="最大输出 Token" className="sg-input" aria-required="true" value={draft.outputText} placeholder="例如 32K" onChange={event => patch({ outputText: event.target.value })} />
          </label></div>
          <ReasoningOptions options={options} selected={variants} onChange={reasoningVariants => patch({ reasoningVariants,
            defaultVariant: reasoningVariants.some(variant => variant.id === draft.defaultVariant) ? draft.defaultVariant : null })} />
        </div>
        {error && <p className="form-error model-editor-error" role="alert">{error}</p>}
        <footer className="sg-editor-actions">
          <button type="button" className="sg-secondary-btn" onClick={onClose}>取消</button>
          <button type="submit" disabled={!canSave} className="sg-primary-btn">保存</button>
        </footer>
      </form>
    </Dialog>
  )
}
