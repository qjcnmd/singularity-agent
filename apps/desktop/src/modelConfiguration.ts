import type { ModelConfigurationInput, ProviderApiProtocol } from './protocol'
import { reasoningGrades } from './modelChoices'

export function parseCapacity(value: string): number | null {
  const match = /^(\d+(?:\.\d+)?)\s*([km])?$/i.exec(value.trim())
  if (!match) return null
  const parsed = Number(match[1]) * (match[2]?.toLowerCase() === 'm' ? 1_000_000 : match[2] ? 1_000 : 1)
  return isValidCapacity(parsed) ? parsed : null
}

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

type ReasoningConfiguration = Pick<ModelConfigurationInput, 'reasoningVariants'>

export function isConfigurationIdentifier(value: string): boolean {
  if (value === '' || /\s/.test(value)) return false
  for (const character of value) {
    const code = character.charCodeAt(0)
    if (code <= 0x1f || code === 0x7f) return false
  }
  return true
}

function supportsThinkingToggle(protocol: ProviderApiProtocol): boolean {
  return protocol === 'messages'
}

export function reasoningConfigurationError(model: ReasoningConfiguration, protocol: ProviderApiProtocol): string | null {
  const declared = model.reasoningVariants ?? []
  if (declared.some(variant => !isConfigurationIdentifier(variant))) return '思考档位无效，请取消选择此档位。'
  if (new Set(declared).size !== declared.length) return '思考选项不能重名。'
  if (protocol === 'responses' && declared.includes('on')) return 'Responses 须选择具体思考档位，不支持 on 开关。'
  return null
}

export function blankModel(): ModelConfigurationInput {
  return {
    modelId: '',
    maxContextTokens: null,
    maxOutputTokens: null,
    reasoningVariants: null,
  }
}

export function reasoningOptionsFor(model: ReasoningConfiguration, protocol: ProviderApiProtocol): string[] {
  const supportsToggle = supportsThinkingToggle(protocol)
  const variants = model.reasoningVariants ?? []
  const toggle = supportsToggle && variants.includes('on')
    && variants.every(variant => variant === 'off' || variant === 'on')
  const options = toggle ? ['off', 'on'] : ['off', ...reasoningGrades, ...(supportsToggle ? ['on'] : [])]
  return [...new Set([...options, ...variants])]
}
