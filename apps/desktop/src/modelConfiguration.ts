import type { ModelConfigurationInput, ProviderApiProtocol } from './protocol'
import { reasoningGrades } from './modelChoices'

/** 解析支持 `K`/`M` 后缀的 Token 容量输入；空值或非法值返回 `null`。 */
export function parseCapacity(value: string): number | null {
  const match = /^(\d+(?:\.\d+)?)\s*([km])?$/i.exec(value.trim())
  if (!match) return null
  const parsed = Number(match[1]) * (match[2]?.toLowerCase() === 'm' ? 1_000_000 : match[2] ? 1_000 : 1)
  return isValidCapacity(parsed) ? parsed : null
}

/** 校验容量是否为 `u32` 范围内的正整数。 */
function isValidCapacity(value: number): boolean {
  return Number.isSafeInteger(value) && value > 0 && value <= 0xffffffff
}

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

type ReasoningConfiguration = Pick<ModelConfigurationInput, 'reasoningVariants' | 'thinkingWireFormat'>

function supportsThinkingToggle(model: ReasoningConfiguration, protocol: ProviderApiProtocol): boolean {
  return protocol === 'chat' && (model.thinkingWireFormat === 'thinking_type' || model.thinkingWireFormat === 'enable_thinking')
}

/** 模型弹窗和提供方保存共用思考选项规则，切换协议后也能直接指出不适用的档位。 */
export function reasoningConfigurationError(model: ReasoningConfiguration, protocol: ProviderApiProtocol): string | null {
  const declared = model.reasoningVariants ?? []
  if (declared.some(variant => !variant || /[\s\x00-\x1f\x7f]/.test(variant))) return '思考档位无效，请取消选择此档位。'
  if (new Set(declared).size !== declared.length) return '思考选项不能重名。'
  if (protocol === 'responses' && declared.includes('on')) return 'Responses 须选择具体思考档位，不支持 on 开关。'
  if (declared.includes('on') && !supportsThinkingToggle(model, protocol)) return 'reasoning_effort 须选择具体思考档位，不支持 on 开关。'
  return null
}

export function blankModel(): ModelConfigurationInput {
  return {
    modelId: '',
    maxContextTokens: null,
    maxOutputTokens: null,
    reasoningVariants: null,
    thinkingWireFormat: null,
    chatOutputTokensField: null,
  }
}

/** 切换到 `responses` 协议时清空仅 `chat` 协议使用的字段。 */
export function applyProtocol(model: ModelConfigurationInput, apiProtocol: ProviderApiProtocol): ModelConfigurationInput {
  return { ...model,
    thinkingWireFormat: apiProtocol === 'responses' ? null : model.thinkingWireFormat,
    chatOutputTokensField: apiProtocol === 'responses' ? null : model.chatOutputTokensField }
}

/** 只提供能发送的开关；不适用的已选值仍显示，供用户取消并修正配置。 */
export function reasoningOptionsFor(model: ReasoningConfiguration, protocol: ProviderApiProtocol): string[] {
  const supportsToggle = supportsThinkingToggle(model, protocol)
  const variants = model.reasoningVariants ?? []
  const toggle = supportsToggle && variants.includes('on')
    && variants.every(variant => variant === 'off' || variant === 'on')
  const options = toggle ? ['off', 'on'] : ['off', ...reasoningGrades, ...(supportsToggle ? ['on'] : [])]
  return [...new Set([...options, ...variants])]
}
