import type { ModelSelection, RedactedModelCatalog } from './protocol'

export function effectiveSelection(selection: ModelSelection | null | undefined, catalog: RedactedModelCatalog | undefined): ModelSelection | null {
  return selection ?? catalog?.defaultSelection ?? null
}

/** 编辑器的常用档位与选择器的展示顺序共用；目录提供的特殊档位另行保留。 */
export const reasoningGrades = [
  'minimal', 'low', 'medium', 'high', 'xhigh', 'max',
] as const
const effortOrder: readonly string[] = ['off', 'none', ...reasoningGrades, 'ultra', 'on']

/** 按从弱到强的固定顺序排列思考档位；未知档位保持原顺序排在末尾。 */
export function sortReasoningVariants(variants: readonly string[] | null | undefined): string[] {
  const rank = (id: string) => {
    const index = effortOrder.indexOf(id)
    return index < 0 ? effortOrder.length : index
  }
  return [...(variants ?? [])].sort((left, right) => rank(left) - rank(right))
}

/** 在当前模型目录中校验并查找所选模型及思考档位，不合法或不存在时返回 `undefined`。 */
export function selectedModel(catalog: RedactedModelCatalog | undefined, selection: ModelSelection | null) {
  const provider = catalog?.providers.find(provider => provider.providerId === selection?.provider)
  const model = provider?.models.find(model => model.modelId === selection?.model)
  if (!provider || !model) return undefined
  const effort = selection?.reasoning ?? null
  if (effort !== null && !model.reasoningVariants?.some(variant => variant === effort)) return undefined
  return { provider, model, effort }
}
