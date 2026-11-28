import type { ModelConfigurationInput, ModelSelection, ProviderApiProtocol, RedactedModelCatalog } from './protocol'

const reasoningGrades = [
  'minimal', 'low', 'medium', 'high', 'xhigh', 'max',
] as const
const effortOrder: readonly string[] = ['off', 'none', ...reasoningGrades, 'ultra', 'on']

export function sortReasoningVariants(variants: readonly string[] | null | undefined): string[] {
  const rank = (id: string) => {
    const index = effortOrder.indexOf(id)
    return index < 0 ? effortOrder.length : index
  }
  return [...(variants ?? [])].sort((left, right) => rank(left) - rank(right))
}

export function selectedModel(catalog: RedactedModelCatalog | undefined, selection: ModelSelection | null) {
  const provider = catalog?.providers.find(provider => provider.providerId === selection?.provider)
  const model = provider?.models.find(model => model.modelId === selection?.model)
  if (!provider || !model) return undefined
  const effort = selection?.reasoning ?? null
  if (effort !== null && !model.reasoningVariants?.some(variant => variant === effort)) return undefined
  return { provider, model, effort }
}

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
  if (output >= context) return '最大输出 Token 必须小于上下文窗口。'
  return null
}

export function formatCapacityInput(value: number | null): string {
  if (value === null) return ''
  if (value % 1_000_000 === 0) return `${value / 1_000_000}M`
  if (value % 1_000 === 0) return `${value / 1_000}K`
  return String(value)
}

type ReasoningConfiguration = Pick<ModelConfigurationInput, 'reasoningVariants'>

function supportsThinkingToggle(protocol: ProviderApiProtocol): boolean {
  return protocol === 'messages'
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
