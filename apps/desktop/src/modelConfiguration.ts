import type { DiscoveredModel, ModelConfigurationInput, ProviderApiProtocol } from './protocol'
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
  return null
}

export function formatCapacityInput(value: number | null): string {
  if (value === null) return ''
  if (value % 1_000_000 === 0) return `${value / 1_000_000}M`
  if (value % 1_000 === 0) return `${value / 1_000}K`
  return String(value)
}

/** 模型弹窗和提供方保存共用思考选项规则，切换协议后也能直接指出不适用的档位。 */
export function reasoningConfigurationError(variants: readonly string[] | null | undefined, protocol: ProviderApiProtocol): string | null {
  const declared = variants ?? []
  if (declared.some(variant => !variant || /[\s\x00-\x1f\x7f]/.test(variant))) return '思考档位无效，请重新获取模型参数或取消选择此档位。'
  if (new Set(declared).size !== declared.length) return '思考选项不能重名。'
  if (protocol === 'responses' && declared.includes('on')) return 'Responses 须选择具体思考档位，不支持 on 开关。'
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
  }
}

/** 提供方协议决定模型参数的适用范围；编辑和发现导入共用此边界。 */
export function applyProtocol(model: ModelConfigurationInput, apiProtocol: ProviderApiProtocol): ModelConfigurationInput {
  return { ...model,
    thinkingWireFormat: apiProtocol === 'responses' ? null : model.thinkingWireFormat,
    chatOutputTokensField: apiProtocol === 'responses' ? null : model.chatOutputTokensField }
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
  for (const field of ['maxContextTokens', 'maxOutputTokens', 'reasoningVariants'] as const) {
    if (current[field] !== null) continue
    const value = candidate[field]
    // 未返回的值保持为空。
    if (value === null || (field === 'reasoningVariants' && candidate.reasoningVariants.length === 0)) continue
    Object.assign(merged, { [field]: value })
    if (field === 'reasoningVariants' && !candidate.reasoningVariants.some(variant => variant === merged.defaultVariant)) merged.defaultVariant = null
  }
  return applyProtocol(merged, protocol)
}

/** 开关模型只提供 off / on，其他模型保留供应商声明的具体档位。 */
export function reasoningOptionsFor(variants: readonly string[], protocol: ProviderApiProtocol): string[] {
  const toggle = protocol === 'chat' && variants.includes('on')
    && variants.every(variant => variant === 'off' || variant === 'on')
  const options = toggle ? ['off', 'on'] : ['off', ...reasoningGrades, ...(protocol === 'chat' ? ['on'] : [])]
  return [...new Set([...options, ...variants])]
}
