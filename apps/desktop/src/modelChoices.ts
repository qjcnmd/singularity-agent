import type { ModelSelection, RedactedModelCatalog } from './protocol'

/** 当前任务的选择优先于全局默认值。 */
export function effectiveSelection(selection: ModelSelection | null | undefined, catalog: RedactedModelCatalog | undefined): ModelSelection | null {
  return selection ?? catalog?.defaultSelection ?? null
}

/** 编辑器的常用档位与选择器的展示顺序共用；目录提供的特殊档位另行保留。 */
export const reasoningGrades = [
  'minimal', 'low', 'medium', 'high', 'xhigh', 'max',
] as const
const effortOrder: readonly string[] = ['off', 'none', ...reasoningGrades, 'ultra', 'on']

/** 所有已配置 variant 都可选；off 控制 thinking 关闭。排序只消费变体数组本身：
 *  返回排序后的副本，不改动输入，未知档位按它们在输入中的稳定次序排在已知档位之后。 */
export function sortReasoningVariants(variants: readonly string[] | null | undefined): string[] {
  const rank = (id: string) => {
    const index = effortOrder.indexOf(id)
    return index < 0 ? effortOrder.length : index
  }
  return [...(variants ?? [])].sort((left, right) => rank(left) - rank(right))
}

/** 在当前模型目录里查找所选模型，选择器和发送前检查共用；找不到返回 undefined。 */
export function selectedModel(catalog: RedactedModelCatalog | undefined, selection: ModelSelection | null) {
  const provider = catalog?.providers.find(provider => provider.providerId === selection?.provider)
  const model = provider?.models.find(model => model.modelId === selection?.model)
  if (!provider || !model) return undefined
  const effort = selection?.reasoning ?? model.defaultVariant
  if (effort !== null && !model.reasoningVariants?.some(variant => variant === effort)) return undefined
  return { provider, model, effort }
}
