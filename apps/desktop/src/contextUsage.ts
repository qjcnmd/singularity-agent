import { effectiveSelector, parseSelector } from './modelChoices'
import type { SessionView } from './execution'
import type { RedactedModelCatalog } from './protocol'

/** capacity 来自产出该测量值的执行。 */
export function contextOccupancy(session: SessionView | null, catalog: RedactedModelCatalog | undefined) {
  if (!session) return null
  const selector = effectiveSelector(session.runtime.selector, catalog)
  const capacity = session.runtime.modelContextWindow
  const selected = parseSelector(selector ?? null)
  if (!selected || !capacity) return null
  // 从尾部读到最近测量或失效边界即停止，历史与实时事件共用一份规则。
  const turns = session.facts
    for (let turn = turns.length - 1; turn >= 0; turn--) {
      const items = turns[turn].items
      for (let index = items.length - 1; index >= 0; index--) {
        const item = items[index]
        if (item.kind === 'compaction') return null
        if (item.kind === 'settings' && (item.provider !== selected.providerId || item.model !== selected.modelId)) return null
        if (item.kind !== 'request') continue
        const request = item.observation
        // 摘要请求成功后仍可能被拒绝；只有正式压缩条目使旧测量失效。
        if (request.purpose === 'compaction') continue
        const used = request.usage?.inputTokens
        if (used == null) continue
        if (request.provider !== selected.providerId || request.model !== selected.modelId) return null
        return { used, capacity, percent: Math.min(100, Math.round(used / capacity * 100)) }
      }
    }
  return null
}
