import type { DiscoveredModel, ModelConfigurationInput, ProviderApiProtocol, ReasoningVariant } from './protocol'
import { reasoningGrades } from './modelChoices'

/** 空输入和非法输入都没有可保存的容量；表单保留原始文字用于编辑。 */
export function parseCapacity(value: string): number | null {
  const match = /^(\d+(?:\.\d+)?)\s*([km])?$/i.exec(value.trim())
  if (!match) return null
  const parsed = Number(match[1]) * (match[2]?.toLowerCase() === 'm' ? 1_000_000 : match[2] ? 1_000 : 1)
  return isValidCapacity(parsed) ? parsed : null
}

/** 配置容量的数值域；未填写的表单由调用方单独处理。 */
function isValidCapacity(value: number): boolean {
  return Number.isSafeInteger(value) && value > 0 && value <= 0xffffffff
}

/** 单模型编辑与批量保存共用容量规则；错误由表单补充模型名称。 */
export function capacityConfigurationError(context: number | null, output: number | null): string | null {
  if (context === null || output === null || !isValidCapacity(context) || !isValidCapacity(output)) return '上下文窗口和最大输出 Token 应为正整数，可使用 K / M。'
  if (output >= context) return '最大输出 Token 必须小于上下文窗口。'
  return null
}

export function formatCapacityInput(value: number | null): string {
  if (value === null) return ''
  if (value % 1_000_000 === 0) return `${value / 1_000_000}M`
  if (value % 1_000 === 0) return `${value / 1_000}K`
  return String(value)
}

/** 模型弹窗和提供方保存共用思考选项规则，切换协议后也能直接指出不适用的档位。 */
export function reasoningConfigurationError(variants: readonly ReasoningVariant[] | null | undefined, protocol: ProviderApiProtocol): string | null {
  const declared = variants ?? []
  if (declared.some(variant => !variant.id || /[\s/#]/.test(variant.id) || (variant.wireEffort !== null && /[\s/#]/.test(variant.wireEffort)))) return '思考档位无效，请重新获取模型参数或取消选择此档位。'
  if (new Set(declared.map(variant => variant.id)).size !== declared.length) return '思考选项不能重名。'
  if (declared.some(variant => variant.id === 'off' && variant.wireEffort !== null)) return '关闭思考的参数映射无效，请重新获取模型参数。'
  if (declared.some(variant => variant.id !== 'off' && variant.wireEffort === null && (protocol === 'responses' || variant.id !== 'on'))) return protocol === 'responses'
    ? 'Responses 不支持此思考开关，请编辑模型并重新选择思考档位。'
    : '此协议不支持所选思考开关，请重新获取模型参数或取消选择。'
  return null
}

/** 新建模型草稿的初始值；协议由所属提供方统一设置。 */
export function blankModel(): ModelConfigurationInput {
  return {
    modelId: '',
    maxContextTokens: null,
    maxOutputTokens: null,
    reasoningVariants: null,
    defaultVariant: null,
    thinkingWireFormat: null,
    chatOutputTokensField: null,
    requiresReasoningContentForToolCalls: null,
  }
}

/** 提供方协议决定模型参数的适用范围；编辑和发现导入共用此边界。 */
export function applyProtocol(model: ModelConfigurationInput, apiProtocol: ProviderApiProtocol): ModelConfigurationInput {
  return { ...model,
    thinkingWireFormat: apiProtocol === 'responses' ? null : model.thinkingWireFormat,
    chatOutputTokensField: apiProtocol === 'responses' ? null : model.chatOutputTokensField,
    requiresReasoningContentForToolCalls: apiProtocol === 'responses' ? null : model.requiresReasoningContentForToolCalls }
}

/** 只补空字段，保留已有设置。 */
export function mergeDiscoveredModels(
  rows: ModelConfigurationInput[],
  candidates: DiscoveredModel[],
  picked: ReadonlySet<string>,
  protocol: ProviderApiProtocol,
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

/** 单模型编辑与批量导入共用补全规则。 */
export function mergeDiscoveredModel(current: ModelConfigurationInput, candidate: DiscoveredModel, protocol: ProviderApiProtocol): ModelConfigurationInput {
  const merged = { ...current }
  for (const field of ['maxContextTokens', 'maxOutputTokens', 'reasoningVariants', 'requiresReasoningContentForToolCalls'] as const) {
    if (current[field] !== null) continue
    if (!(field in candidate)) continue
    const value = candidate[field]
    // 未返回的值保持为空。
    if (value === null || (field === 'reasoningVariants' && candidate.reasoningVariants.length === 0)) continue
    Object.assign(merged, { [field]: value })
    if (field === 'reasoningVariants' && !candidate.reasoningVariants.some(variant => variant.id === merged.defaultVariant)) merged.defaultVariant = null
  }
  return applyProtocol(merged, protocol)
}

/** 保留目录和已有配置的线上映射；开关模型只提供 off / on。 */
export function reasoningOptionsFor(variants: readonly ReasoningVariant[], protocol: ProviderApiProtocol): ReasoningVariant[] {
  const toggle = protocol === 'chat' && variants.some(variant => variant.id === 'on')
    && variants.every(variant => variant.id === 'off' || variant.id === 'on')
  // on 不随选择状态消失：保存 off 后还要能重新开启。
  const ids = toggle ? ['off', 'on'] : ['off', ...reasoningGrades, ...(protocol === 'chat' ? ['on'] : [])]
  const options = new Map(ids.map(id => [id, { id, wireEffort: id === 'off' || id === 'on' ? null : id }]))
  for (const variant of variants) options.set(variant.id, variant)
  return [...options.values()]
}
