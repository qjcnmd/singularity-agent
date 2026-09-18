import type { ReasoningVariant } from './protocol'

const effortOrder = ['off', 'none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra']

/** 所有已配置 variant 都可选；enabled 本身控制 thinking。排序只消费变体数组本身：
 *  返回排序后的副本，不改动输入，未知档位按它们在输入中的稳定次序排在已知档位之后。 */
export function sortReasoningVariants(variants: readonly ReasoningVariant[] | undefined): ReasoningVariant[] {
  const rank = (id: string) => {
    const index = effortOrder.indexOf(id)
    return index < 0 ? effortOrder.length : index
  }
  return [...(variants ?? [])].sort((left, right) => rank(left.id) - rank(right.id))
}
