import type { ModelConfigurationInput } from './protocol'

const effortOrder = ['off', 'none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra']

/** All configured variants are selectable; enabled controls thinking itself. */
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
