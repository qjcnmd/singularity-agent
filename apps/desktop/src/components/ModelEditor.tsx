import { X } from 'lucide-react'
import { useState, type FormEvent } from 'react'
import type { ProviderApiProtocol, ProviderConfigurationInput } from '../protocol'
import { capacityConfigurationError, formatCapacityInput, parseCapacity, reasoningConfigurationError, reasoningOptionsFor } from '../modelConfiguration'
import { Dialog } from './Dialog'
import { ReasoningOptions } from './ReasoningOptions'

type ModelInput = ProviderConfigurationInput['models'][number]
type ModelDraft = Omit<ModelInput, 'maxContextTokens' | 'maxOutputTokens'> & { contextText: string; outputText: string }
function toDraft({ maxContextTokens, maxOutputTokens, ...model }: ModelInput): ModelDraft {
  return { ...model, contextText: formatCapacityInput(maxContextTokens), outputText: formatCapacityInput(maxOutputTokens) }
}

type ModelEditorProps = {
  index: number | null
  initial: ModelInput
  protocol: ProviderApiProtocol
  onConfirm: (model: ModelInput) => string | null
  onClose: () => void
}

/** 单个模型的添加与编辑弹窗。 */
export function ModelEditor({ index, initial, protocol, onConfirm, onClose }: ModelEditorProps) {
  const [draft, setDraft] = useState(() => toDraft(initial))
  const [error, setError] = useState<string | null>(null)
  const variants = draft.reasoningVariants ?? []
  const options = reasoningOptionsFor(initial, protocol)
  const canSave = Boolean(draft.modelId.trim() && draft.contextText.trim() && draft.outputText.trim())

  function patch(change: Partial<ModelDraft>) {
    setError(null)
    setDraft(current => ({ ...current, ...change }))
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
    <Dialog open onClose={onClose} labelledBy="model-editor-title" className="model-editor-modal">
      <header className="modal-header">
        <h2 id="model-editor-title">{index === null ? '添加模型' : '编辑模型'}</h2>
        <button type="button" className="icon-button" onClick={onClose} aria-label="关闭模型编辑"><X size={18} /></button>
      </header>
      <form className="model-editor-form" onSubmit={submit}>
        <div className="model-editor-scroll">
          <label className="sg-field">
            <span>模型 ID</span>
            <input className="sg-input" data-autofocus value={draft.modelId} onChange={event => patch({ modelId: event.target.value })} />
          </label>
          <div className="model-capacity-row"><label className="sg-field">
            <span>上下文窗口</span>
            <input aria-label="上下文窗口" className="sg-input" aria-required="true" value={draft.contextText} onChange={event => patch({ contextText: event.target.value })} />
          </label>
          <label className="sg-field">
            <span>最大输出 Token</span>
            <input aria-label="最大输出 Token" className="sg-input" aria-required="true" value={draft.outputText} onChange={event => patch({ outputText: event.target.value })} />
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
