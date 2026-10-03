import type { DiscoveredModel, ModelConfigurationInput } from './protocol'
import { modelConfigurationFields } from './protocol.generated'

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

/** 投影推荐到仍由智能配置管理的字段；用户覆盖与最近有效值在再次导入时保留。 */
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
    if (at < 0) {
      const { metadataSource: _source, ...configuration } = candidate
      next.push(applyProtocol({ ...blankModel(), ...configuration,
        reasoningVariants: candidate.reasoningVariants.length ? candidate.reasoningVariants : null,
      }, protocol))
      continue
    }
    next[at] = mergeDiscoveredModel(next[at], candidate, protocol)
  }
  return next
}

/** 单模型编辑与批量导入共用自动字段合并规则。 */
export function mergeDiscoveredModel(current: ModelConfigurationInput, candidate: DiscoveredModel, protocol: string): ModelConfigurationInput {
  const merged = { ...current, automaticFields: automaticFieldsFor(current) }
  for (const field of merged.automaticFields) {
    if (current.apiProtocol !== protocol && (field === 'reasoningVariants' || field === 'requiresReasoningContentForToolCalls')) continue
    if (!(field in candidate)) continue
    const value = candidate[field as keyof DiscoveredModel]
    // 未声明的推荐不擦掉上次已确认的值；手动字段不在自动集合中。
    if (value === null || (field === 'reasoningVariants' && candidate.reasoningVariants.length === 0)) continue
    Object.assign(merged, { [field]: value })
    if (field === 'reasoningVariants') merged.defaultVariant = candidate.defaultVariant
  }
  return merged
}
