import { parseSelector } from './modelChoices'
import type { SessionView } from './execution'
import type { RedactedModelCatalog } from './protocol'

/** capacity 属于产出该测量值的执行。 */
export function contextOccupancy(session: SessionView | null, catalog: RedactedModelCatalog | undefined) {
  if (!session) return null
  const selector = session.runtime.selector ?? catalog?.defaultSelector
  const capacity = session.runtime.modelContextWindow
  const selected = parseSelector(selector ?? null)
  if (!selected || !capacity) return null
  // 从尾部读到最近测量或失效边界即停止，历史与实时事件共用一份规则。
  for (const turns of [session.facts.active, session.facts.history]) {
    for (let turn = turns.length - 1; turn >= 0; turn--) {
      const items = turns[turn].items
      for (let index = items.length - 1; index >= 0; index--) {
        const item = items[index]
        if (item.kind === 'compaction') return null
        if (item.kind === 'settings' && (item.provider !== selected.providerId || item.model !== selected.modelId)) return null
        if (item.kind !== 'request') continue
        const request = item.observation
        if (request.purpose === 'compaction') {
          if (request.status === 'ok') return null
          continue
        }
        const used = request.inputTokens
        if (used == null) continue
        if (request.provider !== selected.providerId || request.model !== selected.modelId || !Number.isFinite(used) || used < 0) return null
        return { used, capacity, percent: Math.min(100, Math.round(used / capacity * 100)) }
      }
    }
  }
  return null
}
