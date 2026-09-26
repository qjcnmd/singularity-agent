import { useEffect, useRef, useState } from 'react'
import type { DiscoveredModel, ModelConfigurationField, ProviderConfigurationInput } from '../protocol'
import { automaticFieldsFor, blankModel, mergeDiscoveredModels } from '../modelImport'
import { modelModalities } from '../protocol.generated'
import { Dialog } from './Dialog'

type ModelInput = ProviderConfigurationInput['models'][number]
type ModelDraft = Omit<ModelInput, 'maxContextTokens' | 'maxOutputTokens'> & { contextText: string; outputText: string }
const toDraft = ({ maxContextTokens, maxOutputTokens, ...model }: ModelInput): ModelDraft => ({ ...model, contextText: capacityInput(maxContextTokens), outputText: capacityInput(maxOutputTokens) })

/** 查询完成时采纳自动字段；保留手工容量的原始文本，供保存时校验。 */
function fillDraft(draft: ModelDraft, discovered: DiscoveredModel): ModelDraft {
  const model = { ...draft, maxContextTokens: parseCapacity(draft.contextText) ?? null,
    maxOutputTokens: parseCapacity(draft.outputText) ?? null }
  const merged = mergeDiscoveredModels([model], [discovered], new Set([discovered.modelId]), draft.apiProtocol ?? 'chat')[0]
  const resolved = toDraft(merged)
  return { ...resolved, contextText: draft.automaticFields?.includes('maxContextTokens') ? resolved.contextText : draft.contextText, outputText: draft.automaticFields?.includes('maxOutputTokens') ? resolved.outputText : draft.outputText }
}

/** 模型编辑弹窗：拥有自动能力、手工草稿与校验；父级仅确认模型身份和列表写入。 */
export function ModelEditor({ index, initial, onConfirm, onClose, discover }: { index: number | null; initial: ModelInput; onConfirm: (model: ModelInput) => string | null; onClose: () => void; discover: (protocol: string) => Promise<DiscoveredModel[]> }) {
  const [draft, setDraft] = useState(() => toDraft({ ...initial, automaticFields: automaticFieldsFor(initial) }))
  const [error, setError] = useState<string | null>(null)
  const variants = draft.reasoningVariants ?? []
  const patch = (change: Partial<ModelDraft>) => { setError(null); setDraft(current => {
    const mapping: Record<string, ModelConfigurationField> = { contextText: 'maxContextTokens', outputText: 'maxOutputTokens', defaultVariant: 'reasoningVariants' }
    const changed = Object.keys(change).map(key => mapping[key] ?? key)
    const fields = current.automaticFields?.filter(field => !changed.includes(field)) ?? []
    if (change.contextText === '') fields.push('maxContextTokens')
    if (change.outputText === '') fields.push('maxOutputTokens')
    return { ...current, ...change, automaticFields: fields }
  }) }
  const revision = useRef(0)
  const advanced = useRef<HTMLDetailsElement>(null)
  const [loading, setLoading] = useState(false)
  const canSave = !loading && Boolean(draft.modelId.trim() && draft.contextText.trim() && draft.outputText.trim())
  const [feedback, setFeedback] = useState<string | null>(null)
  useEffect(() => () => { revision.current++ }, [])
  const lookup = async () => {
    const id = draft.modelId.trim()
    if (!id) return
    const current = ++revision.current
    setLoading(true); setFeedback(null)
    try {
      const found = (await discover(draft.apiProtocol ?? 'chat')).find(model => model.modelId === id)
      if (current !== revision.current) return
      if (!found) { setFeedback('目录中未找到此模型，可继续手动填写。'); return }
      setError(null)
      setDraft(current => fillDraft(current, found))
      setFeedback(`已获取模型能力 · ${found.metadataSource ?? '提供方模型目录'}`)
    } catch (error) {
      if (current === revision.current) setFeedback(`${error instanceof Error ? error.message : '获取失败'} 可手动填写或重新获取。`)
    } finally { if (current === revision.current) setLoading(false) }
  }
  const validateAndCommit = (): string | null => {
    const context = parseCapacity(draft.contextText), output = parseCapacity(draft.outputText)
    if (context == null || output == null) return '上下文窗口和最大输出 Token 应为正整数，可使用 K / M。'
    if (output >= context) return '最大输出 Token 必须小于上下文窗口。'
    const variants = draft.reasoningVariants ?? []
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
  const submit = () => {
    if (!canSave) return
    const message = validateAndCommit()
    setError(message)
    if (message && advanced.current) advanced.current.open = true
  }
  return (
    <Dialog open onClose={onClose} labelledBy="model-editor-title" className="confirm-modal model-editor-modal">
      <header className="modal-header"><h2 id="model-editor-title">{index === null ? '添加模型' : '编辑模型'}</h2><button type="button" className="icon-button" onClick={onClose} aria-label="关闭模型编辑">×</button></header>
      <form className="model-editor-form" onSubmit={event => { event.preventDefault(); submit() }}>
        <div className="confirm-body model-editor-scroll">
        <div className="model-identity-row"><label className="sg-field"><span>模型 ID</span><input className="sg-input" data-autofocus value={draft.modelId} onChange={e => { revision.current++; setLoading(false); setFeedback(null); setDraft(current => {
            const blank = toDraft({ ...blankModel(), modelId: e.target.value, apiProtocol: current.apiProtocol })
            const next = { ...current, modelId: e.target.value }
            for (const field of current.automaticFields ?? []) {
              if (field === 'maxContextTokens') next.contextText = ''
              else if (field === 'maxOutputTokens') next.outputText = ''
              else Object.assign(next, { [field]: blank[field] })
              if (field === 'reasoningVariants') next.defaultVariant = null
            }
            return next
          }) }}  /></label>
        <button type="button" className="sg-secondary-btn model-config-fetch" disabled={loading || !draft.modelId.trim()} onClick={() => void lookup()}>智能配置</button></div>
        {(loading || feedback) && <p className="model-fetch-status" role="status">{loading ? '正在获取模型能力…' : feedback}</p>}

        <label className="sg-field"><span>上下文窗口</span><input aria-label="上下文窗口" className="sg-input" aria-required="true" value={draft.contextText} placeholder="例如 256K" onChange={e => patch({ contextText: e.target.value })} /></label>
        <label className="sg-field"><span>最大输出 Token</span><input aria-label="最大输出 Token" className="sg-input" aria-required="true" value={draft.outputText} placeholder="例如 32K" onChange={e => patch({ outputText: e.target.value })} /></label>
        <details ref={advanced}><summary>高级配置</summary>
        <div className="model-advanced-fields">
        <label className="sg-field"><span>显示名称（选填）</span><input aria-label="显示名称" className="sg-input" value={draft.displayName ?? ''} onChange={e => patch({ displayName: e.target.value })} /></label>
        <ModalityOptions value={draft.inputModalities} onChange={inputModalities => patch({ inputModalities })} />
        <fieldset className="sg-reasoning-config"><legend title="按提供方支持的值填写；开关模型使用 off / on，线上档位留空。">思考选项（选填）</legend>
          {variants.map((variant, at) => <div className="sg-reasoning-row" key={at}>
            <label className="sg-field"><span>选项 {at + 1}</span><input className="sg-input" value={variant.id} onChange={e => {
              const id = e.target.value
              patch({ reasoningVariants: variants.map((v, i) => i === at ? { ...v, id } : v), defaultVariant: draft.defaultVariant === variant.id ? id : draft.defaultVariant })
            }} /></label>
            <label className="sg-field"><span>线上档位 {at + 1}</span><input className="sg-input" value={variant.wireEffort ?? ''} placeholder="开关留空" onChange={e => patch({ reasoningVariants: variants.map((v, i) => i === at ? { ...v, wireEffort: e.target.value || null } : v) })} /></label>
            <button type="button" className="quiet-button" aria-label={`删除思考选项 ${at + 1}`} onClick={() => {
              const remaining = variants.filter((_, i) => i !== at)
              patch({ reasoningVariants: remaining, defaultVariant: remaining.some(v => v.id === draft.defaultVariant) ? draft.defaultVariant : remaining[0]?.id ?? null })
            }}>删除</button>
          </div>)}
          <button type="button" className="quiet-button" onClick={() => patch({ reasoningVariants: [...variants, { id: '', wireEffort: null }] })}>添加思考选项</button>
          {variants.length > 0 && <label className="sg-field"><span>默认思考选项</span><select aria-label="默认思考选项" className="sg-input" value={draft.defaultVariant ?? ''} onChange={e => patch({ defaultVariant: e.target.value || null })}><option value="">请选择</option>{variants.map((variant, index) => <option key={index} value={variant.id}>{variant.id || '未命名'}</option>)}</select></label>}
        </fieldset>
        </div></details>
        </div>
        {error && <p className="form-error model-editor-error" role="alert">{error}</p>}
        <footer className="sg-editor-actions"><button type="button" className="quiet-button model-restore" disabled={loading} onClick={() => { setFeedback(null); setError(null); setDraft(toDraft({ ...blankModel(), modelId: draft.modelId, apiProtocol: draft.apiProtocol })) }}>重置表单</button><button type="button" className="sg-secondary-btn" onClick={onClose}>取消</button><button type="submit" disabled={!canSave} className="sg-primary-btn">保存</button></footer>
      </form>
    </Dialog>
  )
}

/** 空输入为 null，非法输入为 undefined；保存要求两项容量均为有效数值。 */
function parseCapacity(value: string): number | null | undefined {
  if (!value.trim()) return null
  const match = /^(\d+(?:\.\d+)?)\s*([km])?$/i.exec(value.trim())
  if (!match) return undefined
  const parsed = Number(match[1]) * (match[2]?.toLowerCase() === 'm' ? 1_000_000 : match[2] ? 1_000 : 1)
  return validCapacity(parsed) ? parsed : undefined
}
/** 容量数值的唯一合法域；null 仅用于未完成的表单草稿。 */
export function validCapacity(value: number | null): boolean { return value === null || (Number.isSafeInteger(value) && value > 0 && value <= 0xffffffff) }
function capacityInput(value: number | null): string { return value === null ? '' : value % 1_000_000 === 0 ? `${value / 1_000_000}M` : value % 1_000 === 0 ? `${value / 1_000}K` : String(value) }

const modalityLabels: Record<string, string> = { text: '文本', image: '图片', audio: '音频', video: '视频', pdf: 'PDF' }
function ModalityOptions({ value, onChange }: { value: string[] | null; onChange: (value: string[]) => void }) {
  return <fieldset className="model-modalities"><legend>输入模态</legend><div>
    {modelModalities.map(modality => <label key={modality}>
      <input type="checkbox" checked={modality === 'text' || Boolean(value?.includes(modality))} disabled={modality === 'text'} onChange={event => {
        const selected = new Set(value ?? ['text'])
        selected.add('text')
        if (event.target.checked) selected.add(modality); else selected.delete(modality)
        onChange(modelModalities.filter(item => selected.has(item)))
      }} />{modalityLabels[modality]}
    </label>)}
  </div></fieldset>
}
