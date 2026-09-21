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

interface SelectorParts {
  providerId: string
  modelId: string
  effort: string | null
}

export function parseSelector(selector: string | null): SelectorParts | null {
  if (selector === null) return null
  const slash = selector.indexOf('/')
  if (slash <= 0 || slash === selector.length - 1) return null
  const hash = selector.lastIndexOf('#')
  return {
    providerId: selector.slice(0, slash),
    modelId: selector.slice(slash + 1, hash > slash ? hash : undefined),
    effort: hash > slash ? selector.slice(hash + 1) || null : null,
  }
}

export function composeSelector(providerId: string, modelId: string, effort: string | null): string {
  return `${providerId}/${modelId}${effort === null ? '' : `#${effort}`}`
}
