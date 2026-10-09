import { useEffect, useRef, useState, type FormEvent } from 'react'
import type { DiscoveredModel, ModelConfigurationField, ProviderConfigurationInput } from '../protocol'
import { automaticFieldsFor, blankModel, formatCapacityInput, mergeDiscoveredModel, parseCapacity } from '../modelConfiguration'
import { Dialog } from './Dialog'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'

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
  const revision = useRef(0)
  const [advanced, setAdvanced] = useState(false)
  const [loading, setLoading] = useState(false)
  const canSave = !loading && Boolean(draft.modelId.trim() && draft.contextText.trim() && draft.outputText.trim())
  const [feedback, setFeedback] = useState<string | null>(null)
  useEffect(() => () => { revision.current++ }, [])

  function patch(change: Partial<ModelDraft>) {
    setError(null)
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
    setDraft(current => {
      const blank = toDraft({ ...blankModel(), modelId, apiProtocol: current.apiProtocol })
      const next = { ...current, modelId }
      for (const field of current.automaticFields ?? []) {
        if (field === 'maxContextTokens') next.contextText = ''
        else if (field === 'maxOutputTokens') next.outputText = ''
        else Object.assign(next, { [field]: blank[field] })
        if (field === 'reasoningVariants') next.defaultVariant = null
      }
      return next
    })
  }

  function renameVariant(index: number, id: string) {
    patch({
      reasoningVariants: variants.map((variant, at) => at === index ? { ...variant, id } : variant),
      defaultVariant: draft.defaultVariant === variants[index].id ? id : draft.defaultVariant,
    })
  }

  function changeVariantEffort(index: number, wireEffort: string) {
    patch({ reasoningVariants: variants.map((variant, at) => at === index ? { ...variant, wireEffort: wireEffort || null } : variant) })
  }

  function removeVariant(index: number) {
    const remaining = variants.filter((_, at) => at !== index)
    patch({
      reasoningVariants: remaining,
      defaultVariant: remaining.some(variant => variant.id === draft.defaultVariant) ? draft.defaultVariant : remaining[0]?.id ?? null,
    })
  }

  function resetDraft() {
    setFeedback(null)
    setError(null)
    setDraft(toDraft({ ...blankModel(), modelId: draft.modelId, apiProtocol: draft.apiProtocol }))
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
        setFeedback('目录中未找到此模型。')
        return
      }
      setError(null)
      setDraft(current => fillDraft(current, found))
    } catch (error) {
      if (current === revision.current) setFeedback(error instanceof Error ? error.message : '获取失败')
    } finally {
      if (current === revision.current) setLoading(false)
    }
  }
  const validateAndCommit = (): string | null => {
    const context = parseCapacity(draft.contextText)
    const output = parseCapacity(draft.outputText)
    if (context == null || output == null) return '上下文窗口和最大输出 Token 应为正整数，可使用 K / M。'
    if (output >= context) return '最大输出 Token 必须小于上下文窗口。'
    if (variants.some(v => !v.id || /[\s/#]/.test(v.id) || (v.wireEffort !== null && /[\s/#]/.test(v.wireEffort)))) return '思考选项及线上档位不能包含空格、/ 或 #。'
    if (new Set(variants.map(v => v.id)).size !== variants.length) return '思考选项不能重名。'
    if (variants.length && !variants.some(v => v.id === draft.defaultVariant)) return '请选择默认思考选项。'
    if (variants.some(v => v.id === 'off' && v.wireEffort !== null)) return 'off 的线上档位应留空。'
    if (variants.some(v => v.id !== 'off' && v.wireEffort === null && (draft.apiProtocol === 'responses' || v.id !== 'on'))) return '只有 Chat 的 on 开关可以不填写线上档位。'
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
    if (message) setAdvanced(true)
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
            <button type="button" className="sg-secondary-btn model-config-fetch" disabled={loading || !draft.modelId.trim()} onClick={() => void lookup()}>智能配置</button>
          </div>
          {(loading || feedback) && <p className="model-fetch-status" role="status">{loading ? '正在获取模型能力…' : feedback}</p>}

          <label className="sg-field">
            <span>上下文窗口</span>
            <input aria-label="上下文窗口" className="sg-input" aria-required="true" value={draft.contextText} placeholder="例如 256K" onChange={event => patch({ contextText: event.target.value })} />
          </label>
          <label className="sg-field">
            <span>最大输出 Token</span>
            <input aria-label="最大输出 Token" className="sg-input" aria-required="true" value={draft.outputText} placeholder="例如 32K" onChange={event => patch({ outputText: event.target.value })} />
          </label>
          <div>
            <button type="button" className="model-advanced-toggle" aria-expanded={advanced} aria-controls="model-advanced-fields" onClick={() => setAdvanced(value => !value)}>高级配置<ExpandChevron expanded={advanced} size={16} /></button>
            <Disclosure open={advanced}><div id="model-advanced-fields" className="model-advanced-fields">
              <label className="sg-field">
                <span>显示名称（选填）</span>
                <input aria-label="显示名称" className="sg-input" value={draft.displayName ?? ''} onChange={event => patch({ displayName: event.target.value })} />
              </label>
              <fieldset className="sg-reasoning-config">
                <legend title="按提供方支持的值填写；开关模型使用 off / on，线上档位留空。">思考选项（选填）</legend>
                {variants.map((variant, at) => (
                  <div className="sg-reasoning-row" key={at}>
                    <label className="sg-field">
                      <span>选项 {at + 1}</span>
                      <input className="sg-input" value={variant.id} onChange={event => renameVariant(at, event.target.value)} />
                    </label>
                    <label className="sg-field">
                      <span>线上档位 {at + 1}</span>
                      <input className="sg-input" value={variant.wireEffort ?? ''} placeholder="开关留空" onChange={event => changeVariantEffort(at, event.target.value)} />
                    </label>
                    <button type="button" className="quiet-button" aria-label={`删除思考选项 ${at + 1}`} onClick={() => removeVariant(at)}>删除</button>
                  </div>
                ))}
                <button type="button" className="quiet-button" onClick={() => patch({ reasoningVariants: [...variants, { id: '', wireEffort: null }] })}>添加思考选项</button>
                {variants.length > 0 && <label className="sg-field">
                  <span>默认思考选项</span>
                  <select aria-label="默认思考选项" className="sg-input" value={draft.defaultVariant ?? ''} onChange={event => patch({ defaultVariant: event.target.value || null })}>
                    <option value="">请选择</option>
                    {variants.map((variant, index) => <option key={index} value={variant.id}>{variant.id || '未命名'}</option>)}
                  </select>
                </label>}
              </fieldset>
            </div></Disclosure>
          </div>
        </div>
        {error && <p className="form-error model-editor-error" role="alert">{error}</p>}
        <footer className="sg-editor-actions">
          <button type="button" className="quiet-button model-restore" disabled={loading} onClick={resetDraft}>重置表单</button>
          <button type="button" className="sg-secondary-btn" onClick={onClose}>取消</button>
          <button type="submit" disabled={!canSave} className="sg-primary-btn">保存</button>
        </footer>
      </form>
    </Dialog>
  )
}
