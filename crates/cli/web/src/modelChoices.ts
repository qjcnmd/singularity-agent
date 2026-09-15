import type { ModelConfigurationInput } from './protocol'

const effortOrder = ['off', 'none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra']

/** 所有已配置 variant 都可选；enabled 本身控制 thinking。 */
export function reasoningChoices(model: ModelConfigurationInput | undefined) {
  return [...(model?.reasoningVariants ?? [])]
    .sort((left, right) => {
      const rank = (id: string) => {
        const index = effortOrder.indexOf(id)
        return index < 0 ? effortOrder.length : index
      }
      return rank(left.id) - rank(right.id)
    })
}
