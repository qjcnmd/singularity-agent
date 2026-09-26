import type { DiscoveredModel, ModelConfigurationInput } from './protocol'
import { modelConfigurationFields } from './protocol.generated'

/** 未记录字段归属的配置保留已有值，仅补齐空字段。 */
export function automaticFieldsFor(model: ModelConfigurationInput) {
  return model.automaticFields ?? modelConfigurationFields.filter(field => model[field] === null)
}

/**
 * 采纳提供方发现结果的纯规则：草稿行的新建与合并。
 *
 * 设置编辑器把选中集合交给这里，导入规则由这一处维护。
 */

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
    inputModalities: null,
    outputModalities: null,
    requiresReasoningContentForToolCalls: null,
  }
}

/** 模型协议决定线上参数的适用范围；编辑和发现导入共用此边界。 */
export function applyProtocol<T extends Pick<ModelConfigurationInput, 'apiProtocol' | 'thinkingWireFormat' | 'chatOutputTokensField' | 'requiresReasoningContentForToolCalls'>>(model: T, apiProtocol: string): T {
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
    const current = next[at]
    const merged = { ...current, automaticFields: automaticFieldsFor(current) }
    for (const field of merged.automaticFields) {
      if (current.apiProtocol !== protocol && ['reasoningVariants', 'thinkingWireFormat', 'chatOutputTokensField', 'requiresReasoningContentForToolCalls'].includes(field)) continue
      const value = candidate[field]
      // 未声明的推荐不擦掉上次已确认的值；显式手动清空的字段已不在自动集合中。
      if (value === null || (field === 'reasoningVariants' && candidate.reasoningVariants.length === 0)) continue
      Object.assign(merged, { [field]: value })
      if (field === 'reasoningVariants') merged.defaultVariant = candidate.defaultVariant
    }
    next[at] = merged
  }
  return next
}
