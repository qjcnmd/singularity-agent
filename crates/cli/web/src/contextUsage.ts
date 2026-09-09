import { eventsSince, isEventPrefix, type EventSequence } from './eventLog'
import type { RedactedModelCatalog, SessionReadResult } from './protocol'

type Measurement = { provider: string; model: string; inputTokens: number } | undefined
const historicalMeasurements = new WeakMap<SessionReadResult['history']['turns'], { latest: Measurement }>()
let activeMeasurement: { history: SessionReadResult['history']['turns'] | null; events: EventSequence; latest: Measurement } = { history: null, events: [], latest: undefined }

/** Latest measured input for the selected model, invalidated by compaction or model changes. */
export function contextOccupancy(session: SessionReadResult | null, catalog: RedactedModelCatalog | undefined) {
  if (!session) return null
  let latest: Measurement
  const history = historicalMeasurements.get(session.history.turns)
  if (history) latest = history.latest
  else {
    for (const turn of session.history.turns) for (const item of turn.items) {
      if (item.type === 'request' && item.observation.purpose !== 'compaction' && item.observation.inputTokens != null) {
        latest = { ...item.observation, inputTokens: item.observation.inputTokens }
      }
      if (item.type === 'compaction' || item.type === 'settings' && latest && (item.provider !== latest.provider || item.model !== latest.model)) latest = undefined
    }
    historicalMeasurements.set(session.history.turns, { latest })
  }
  const events = session.runtime.activeTurn?.events ?? []
  const appended = activeMeasurement.history === session.history.turns
    && isEventPrefix(activeMeasurement.events, events)
  const start = appended ? activeMeasurement.events.length : 0
  if (appended) latest = activeMeasurement.latest
  for (const event of eventsSince(events, start, appended ? activeMeasurement.events : undefined)) {
    if (event.method !== 'provider/attempt') continue
    const p = event.params
    if (p.purpose === 'compaction') { latest = undefined; continue }
    if (p.inputTokens != null) latest = { provider: p.provider, model: p.model, inputTokens: p.inputTokens }
  }
  activeMeasurement = { history: session.history.turns, events, latest }
  const selector = session.runtime.selector ?? catalog?.defaultSelector
  if (!latest || selector?.split('#')[0] !== `${latest.provider}/${latest.model}`) return null
  const capacity = catalog?.providers.find(provider => provider.providerId === latest.provider)?.models.find(model => model.modelId === latest.model)?.maxContextTokens
  if (!capacity || !Number.isFinite(latest.inputTokens) || latest.inputTokens < 0) return null
  return { used: latest.inputTokens, capacity, percent: Math.min(100, Math.round(latest.inputTokens / capacity * 100)) }
}
