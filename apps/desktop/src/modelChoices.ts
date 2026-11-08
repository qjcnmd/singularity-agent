import type { ModelSelection, RedactedModelCatalog } from './protocol'

/** 编辑器的常用档位与选择器的展示顺序共用；目录提供的特殊档位另行保留。 */
export const reasoningGrades = [
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
