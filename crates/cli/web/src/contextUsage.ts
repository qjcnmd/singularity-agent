import type { RedactedModelCatalog, SessionReadResult } from './protocol'

/** Latest measured input for the selected model, invalidated by compaction or model changes. */
export function contextOccupancy(session: SessionReadResult | null, catalog: RedactedModelCatalog | undefined) {
  if (!session) return null
  let latest: { provider: string; model: string; inputTokens: number } | undefined
  for (const turn of session.history.turns) for (const item of turn.items) {
    if (item.type === 'request' && item.observation.inputTokens != null) {
      latest = { ...item.observation, inputTokens: item.observation.inputTokens }
    }
    if (item.type === 'compaction' || item.type === 'settings' && latest && (item.provider !== latest.provider || item.model !== latest.model)) latest = undefined
  }
  for (const event of session.runtime.activeTurn?.events ?? []) {
    if (event.method !== 'provider/attempt') continue
    const p = event.params
    if (p.inputTokens != null) latest = { provider: p.provider, model: p.model, inputTokens: p.inputTokens }
  }
  const selector = session.runtime.selector ?? catalog?.defaultSelector
  if (!latest || selector?.split('#')[0] !== `${latest.provider}/${latest.model}`) return null
  const capacity = catalog?.providers.find(provider => provider.providerId === latest.provider)?.models.find(model => model.modelId === latest.model)?.maxContextTokens
  if (!capacity || !Number.isFinite(latest.inputTokens) || latest.inputTokens < 0) return null
  return { used: latest.inputTokens, capacity, percent: Math.min(100, Math.round(latest.inputTokens / capacity * 100)) }
}
