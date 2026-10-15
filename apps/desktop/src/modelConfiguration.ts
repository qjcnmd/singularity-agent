import type { DiscoveredModel, ModelConfigurationInput, ReasoningVariant } from './protocol'
import { modelConfigurationFields } from './protocol.generated'
import { reasoningGrades } from './modelChoices'

/** 空输入为 null，非法输入为 undefined；保存要求两项容量均为有效数值。 */
export function parseCapacity(value: string): number | null | undefined {
  if (!value.trim()) return null
  const match = /^(\d+(?:\.\d+)?)\s*([km])?$/i.exec(value.trim())
  if (!match) return undefined
  const parsed = Number(match[1]) * (match[2]?.toLowerCase() === 'm' ? 1_000_000 : match[2] ? 1_000 : 1)
  return isValidCapacity(parsed) ? parsed : undefined
}

/** 配置容量的数值域；未填写的表单由调用方单独处理。 */
export function isValidCapacity(value: number): boolean {
  return Number.isSafeInteger(value) && value > 0 && value <= 0xffffffff
}

export function formatCapacityInput(value: number | null): string {
  if (value === null) return ''
  if (value % 1_000_000 === 0) return `${value / 1_000_000}M`
  if (value % 1_000 === 0) return `${value / 1_000}K`
  return String(value)
}

/** 未记录字段归属的配置保留已有值，仅补齐空字段。 */
export function automaticFieldsFor(model: ModelConfigurationInput) {
  return model.automaticFields ?? modelConfigurationFields.filter(field => model[field] === null)
}

/** 模型弹窗和提供方保存共用思考选项规则，切换协议后也能直接指出不适用的档位。 */
export function reasoningConfigurationError(variants: readonly ReasoningVariant[] | null | undefined, protocol: string): string | null {
  const declared = variants ?? []
  if (declared.some(variant => !variant.id || /[\s/#]/.test(variant.id) || (variant.wireEffort !== null && /[\s/#]/.test(variant.wireEffort)))) return '思考档位无效，请重新获取模型参数或取消选择此档位。'
  if (new Set(declared.map(variant => variant.id)).size !== declared.length) return '思考选项不能重名。'
  if (declared.some(variant => variant.id === 'off' && variant.wireEffort !== null)) return '关闭思考的参数映射无效，请重新获取模型参数。'
  if (declared.some(variant => variant.id !== 'off' && variant.wireEffort === null && (protocol === 'responses' || variant.id !== 'on'))) return protocol === 'responses'
    ? 'Responses 不支持此思考开关，请编辑模型并重新选择思考档位。'
    : '此协议不支持所选思考开关，请重新获取模型参数或取消选择。'
  return null
}

/** 新建草稿的初始值；提供方与协议由调用方补齐。 */
export function blankModel(): ModelConfigurationInput {
  return {
    modelId: '',
    automaticFields: [...modelConfigurationFields],
    displayName: null,
    apiProtocol: 'chat',
    maxContextTokens: null,
    maxOutputTokens: null,
    reasoningVariants: null,
    defaultVariant: null,
    thinkingWireFormat: null,
    chatOutputTokensField: null,
    requiresReasoningContentForToolCalls: null,
  }
}

/** 模型协议决定线上参数的适用范围；编辑和发现导入共用此边界。 */
export function applyProtocol(model: ModelConfigurationInput, apiProtocol: string): ModelConfigurationInput {
  return { ...model, apiProtocol,
    thinkingWireFormat: apiProtocol === 'responses' ? null : model.thinkingWireFormat,
    chatOutputTokensField: apiProtocol === 'responses' ? null : model.chatOutputTokensField,
    requiresReasoningContentForToolCalls: apiProtocol === 'responses' ? null : model.requiresReasoningContentForToolCalls }
}

/** 只更新自动字段；用户覆盖与最近有效值在再次导入时保留。 */
export function mergeDiscoveredModels(
  rows: ModelConfigurationInput[],
  candidates: DiscoveredModel[],
  picked: ReadonlySet<string>,
  protocol: string,
): ModelConfigurationInput[] {
  const next = [...rows]
  for (const candidate of candidates) {
    if (!picked.has(candidate.modelId)) continue
    const at = next.findIndex(row => row.modelId.trim() === candidate.modelId)
    const current = at < 0 ? { ...blankModel(), modelId: candidate.modelId } : next[at]
    const merged = mergeDiscoveredModel(current, candidate, protocol)
    if (at < 0) next.push(merged)
    else next[at] = merged
  }
  return next
}

/** 单模型编辑与批量导入共用自动字段合并规则。 */
export function mergeDiscoveredModel(current: ModelConfigurationInput, candidate: DiscoveredModel, protocol: string): ModelConfigurationInput {
  const merged = { ...current, automaticFields: automaticFieldsFor(current) }
  for (const field of merged.automaticFields) {
    if (!(field in candidate)) continue
    const value = candidate[field as keyof DiscoveredModel]
    // 未声明的推荐不擦掉上次已确认的值；手动字段不在自动集合中。
    if (value === null || (field === 'reasoningVariants' && candidate.reasoningVariants.length === 0)) continue
    Object.assign(merged, { [field]: value })
    if (field === 'reasoningVariants' && !candidate.reasoningVariants.some(variant => variant.id === merged.defaultVariant)) merged.defaultVariant = null
  }
  return applyProtocol(merged, protocol)
}

/** 保留目录和已有配置的线上映射；开关模型只提供 off / on。 */
export function reasoningOptionsFor(variants: readonly ReasoningVariant[], protocol: string): ReasoningVariant[] {
  const toggle = protocol === 'chat' && variants.some(variant => variant.id === 'on')
    && variants.every(variant => variant.id === 'off' || variant.id === 'on')
  // on 不能随选择状态消失，否则保存 off 后无法重新开启。
  const ids = toggle ? ['off', 'on'] : ['off', ...reasoningGrades, ...(protocol === 'chat' ? ['on'] : [])]
  const options = new Map(ids.map(id => [id, { id, wireEffort: id === 'off' || id === 'on' ? null : id }]))
  for (const variant of variants) options.set(variant.id, variant)
  return [...options.values()]
}
