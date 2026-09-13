import type { SessionView } from './execution'
import type { RedactedModelCatalog } from './protocol'

/** Capacity belongs to the execution that produced the measurement. */
export function contextOccupancy(session: SessionView | null, catalog: RedactedModelCatalog | undefined) {
  if (!session) return null
  const latest = session.facts.latest
  const selector = session.runtime.selector ?? catalog?.defaultSelector
  const capacity = session.runtime.modelContextWindow
  if (!latest || selector?.split('#')[0] !== `${latest.provider}/${latest.model}`) return null
  if (!capacity || !Number.isFinite(latest.inputTokens) || latest.inputTokens < 0) return null
  return { used: latest.inputTokens, capacity, percent: Math.min(100, Math.round(latest.inputTokens / capacity * 100)) }
}
