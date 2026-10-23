import { useEffect, useRef, useState, type FormEvent } from 'react'
import type { DiscoveredModel, ProviderApiProtocol, ProviderConfigurationInput } from '../protocol'
import { capacityConfigurationError, formatCapacityInput, mergeDiscoveredModel, parseCapacity, reasoningConfigurationError, reasoningOptionsFor } from '../modelConfiguration'
import { Dialog } from './Dialog'
import { ReasoningOptions } from './ReasoningOptions'

type ModelInput = ProviderConfigurationInput['models'][number]
type ModelDraft = Omit<ModelInput, 'maxContextTokens' | 'maxOutputTokens'> & { contextText: string; outputText: string }
function toDraft({ maxContextTokens, maxOutputTokens, ...model }: ModelInput): ModelDraft {
  return { ...model, contextText: formatCapacityInput(maxContextTokens), outputText: formatCapacityInput(maxOutputTokens) }
}

/** 查询只补空字段；非空容量保留原始文本，供保存时校验。 */
function fillDraft(draft: ModelDraft, discovered: DiscoveredModel, protocol: ProviderApiProtocol): ModelDraft {
  const model = {
    ...draft,
    maxContextTokens: parseCapacity(draft.contextText),
    maxOutputTokens: parseCapacity(draft.outputText),
  }
  const merged = mergeDiscoveredModel(model, discovered, protocol)
  const resolved = toDraft(merged)
  return {
    ...resolved,
    contextText: draft.contextText.trim() ? draft.contextText : resolved.contextText,
    outputText: draft.outputText.trim() ? draft.outputText : resolved.outputText,
  }
}

type ModelEditorProps = {
  index: number | null
  initial: ModelInput
  protocol: ProviderApiProtocol
  onConfirm: (model: ModelInput) => string | null
  onClose: () => void
  discover: () => Promise<DiscoveredModel[]>
}

/** 单个模型的添加与编辑弹窗。 */
export function ModelEditor({ index, initial, protocol, onConfirm, onClose, discover }: ModelEditorProps) {
  const [draft, setDraft] = useState(() => toDraft(initial))
  const [error, setError] = useState<string | null>(null)
  const variants = draft.reasoningVariants ?? []
  const [options, setOptions] = useState(() => reasoningOptionsFor(draft, protocol))
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
    setDraft(current => ({ ...current, ...change }))
  }

  function changeModelId(modelId: string) {
    revision.current++
    setLoading(false)
    setFeedback(null)
    setError(null)
    setDraft(current => ({ ...current, modelId }))
  }

  const lookup = async () => {
    const id = draft.modelId.trim()
    if (!id) return
    const current = ++revision.current
    setLoading(true)
    setFeedback(null)
    setError(null)
    try {
      const found = (await discover()).find(model => model.modelId === id)
      if (current !== revision.current) return
      if (!found) {
        setFeedback('未获取模型参数')
        return
      }
      const previous = draftRef.current
      const next = fillDraft(previous, found, protocol)
      setDraft(next)
      setOptions(reasoningOptionsFor(next, protocol, found.reasoningVariants))
      if (found.maxContextTokens === null && found.maxOutputTokens === null && found.reasoningVariants.length === 0) setFeedback('未获取模型参数')
    } catch (error) {
      if (current === revision.current) setError(error instanceof Error ? error.message : String(error))
    } finally {
      if (current === revision.current) setLoading(false)
    }
  }
  const validateAndCommit = (): string | null => {
    const context = parseCapacity(draft.contextText)
    const output = parseCapacity(draft.outputText)
    const capacityError = capacityConfigurationError(context, output)
    if (capacityError !== null) return capacityError
    const reasoningError = reasoningConfigurationError(draft, protocol)
    if (reasoningError !== null) return reasoningError
    // 将容量输入解析为数值后再传给上层保存。
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
          <ReasoningOptions options={options} selected={variants} onChange={reasoningVariants => patch({ reasoningVariants })} />
        </div>
        {error && <p className="form-error model-editor-error" role="alert">{error}</p>}
        <footer className="sg-editor-actions">
          <button type="button" className="sg-secondary-btn" onClick={onClose}>取消</button>
          <button type="submit" disabled={!canSave} className="sg-primary-btn">确定</button>
        </footer>
      </form>
    </Dialog>
  )
}
