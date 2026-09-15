import { eventTurnId } from './protocol'
import type { HistoryItem, RequestObservation, SessionReadResult, SessionRuntime as WireSessionRuntime, ThreadReadPage, ThreadSummary, TurnEventEnvelope, TurnStatus } from './protocol'

export type FactStatus = 'stable' | 'running' | 'ok' | 'error' | 'cancelled'
interface FactBase { id: string; status: FactStatus; startedAt: string | null; error?: string }
export type ExecutionItem = FactBase & (
  | { kind: 'user' | 'assistant' | 'thinking'; text: string; requestId?: string }
  | { kind: 'tool'; name: string; args: unknown; output: string; diff?: string; duration?: number }
  | { kind: 'request'; observation: RequestObservation }
  | { kind: 'settings'; provider: string; model: string; reasoning: string | null }
  | { kind: 'compaction' | 'event' | 'unknown'; text: string }
)
/** `null` keeps the wire's meaning: records grouped before the first real run. */
export interface ExecutionTurn { id: string | null; status: TurnStatus | null; items: ExecutionItem[] }
type Measurement = { provider: string; model: string; inputTokens: number } | undefined
export interface ExecutionFacts { history: ExecutionTurn[]; active: ExecutionTurn[]; latest: Measurement }
export type SessionRuntime = WireSessionRuntime
/** The loaded history lives only as facts; the wire page is a read boundary, never a resident state. */
export interface SessionView {
  summary: ThreadSummary
  nextCursor: string | null
  runtime: SessionRuntime
  facts: ExecutionFacts
}
const base = (id: string, status: FactStatus = 'stable'): FactBase => ({ id, status, startedAt: null })
const lastRequest = (turn: ExecutionTurn) => turn.items.findLast(item => item.kind === 'request')?.id

/** Pair identities and settle execution state once, before any display projection. */
function upsert(turn: ExecutionTurn, item: ExecutionItem): ExecutionTurn {
  const index = turn.items.findIndex(previous => previous.id === item.id)
  const items = [...turn.items]
  if (index < 0) items.push(item)
  else items[index] = item
  return { ...turn, items }
}

function request(turn: ExecutionTurn, observation: RequestObservation, startedAt: string | null): ExecutionTurn {
  const previous = turn.items.find(item => item.id === observation.requestId)
  const prior = previous?.kind === 'request' ? previous.observation : undefined
  return upsert(turn, { ...base(observation.requestId, observation.status === 'started' ? 'running' : observation.status),
    kind: 'request', observation: { ...observation, requestHead: observation.requestHead ?? prior?.requestHead },
    startedAt: observation.status === 'started' ? startedAt ?? previous?.startedAt ?? null : null,
  })
}

function historyItem(turn: ExecutionTurn, item: HistoryItem): ExecutionTurn {
  switch (item.type) {
    case 'request': return request(turn, item.observation, item.timestamp)
    case 'message': return upsert(turn, { ...base(item.id), kind: item.role === 'user' ? 'user' : 'assistant', text: item.text,
      requestId: item.role === 'assistant' ? lastRequest(turn) : undefined })
    case 'thinking': return upsert(turn, { ...base(item.id), kind: 'thinking', text: item.text, requestId: lastRequest(turn) })
    case 'tool_call': return upsert(turn, { ...base(item.id), kind: 'tool', name: item.name, args: item.args, output: '' })
    case 'tool_result': {
      const previous = turn.items.find(value => value.id === item.id)
      return upsert(turn, { ...base(item.id, item.isError ? 'error' : 'ok'), kind: 'tool',
        name: previous?.kind === 'tool' ? previous.name : '工具输出', args: previous?.kind === 'tool' ? previous.args : {},
        output: item.output, diff: item.isError ? undefined : item.diff, duration: item.durationMs })
    }
    case 'compaction': return upsert(turn, { ...base(item.id), kind: 'compaction', text: item.summary })
    case 'settings': return upsert(turn, { ...base(item.id), kind: 'settings', provider: item.provider, model: item.model, reasoning: item.reasoning })
  }
}

function measure(latest: Measurement, observation: RequestObservation): Measurement {
  if (observation.purpose === 'compaction') return observation.status === 'ok' ? undefined : latest
  return observation.inputTokens == null ? latest : { provider: observation.provider, model: observation.model, inputTokens: observation.inputTokens }
}

/** Input measurement is derived from the facts themselves; no second source over raw items exists. */
function measureTurns(turns: ExecutionTurn[]): Measurement {
  let latest: Measurement
  for (const turn of turns) for (const item of turn.items) {
    if (item.kind === 'request') latest = measure(latest, item.observation)
    else if (item.kind === 'compaction') latest = undefined
    else if (item.kind === 'settings' && latest && (item.provider !== latest.provider || item.model !== latest.model)) latest = undefined
  }
  return latest
}

/** The single conversion boundary: one wire page becomes execution turns. */
function pageTurns(page: ThreadReadPage): ExecutionTurn[] {
  return page.turns.map(source => source.items.reduce(historyItem, { id: source.turnId, status: source.status, items: [] }))
}

/** Request starts persisted by a killed process do not prove current liveness. */
function settleRequests(turns: ExecutionTurn[], runtime: SessionRuntime): ExecutionTurn[] {
  return turns.map(turn => {
    let changed = false
    const items = turn.items.map(item => {
      if (item.kind !== 'request' || item.observation.status !== 'started') return item
      const compaction = runtime.activeCompaction
      const live = runtime.phase !== 'idle' && (turn.id === runtime.activeTurn?.turnId
        || item.observation.purpose === 'compaction' && compaction && item.startedAt && Date.parse(item.startedAt) >= Date.parse(compaction.startedAt))
      const status = live ? 'running' : 'cancelled'
      if (item.status === status) return item
      changed = true
      return { ...item, status } as ExecutionItem
    })
    return changed ? { ...turn, items } : turn
  })
}

/**
 * Read a session page into facts. A fresh tail keeps the already loaded prefix only while it
 * overlaps it; without overlap the page carries its own cursor, so the gap stays visible.
 */
export function readExecution(source: SessionReadResult, previous: SessionView | null = null): SessionView {
  const first = source.history.turns[0]
  const overlap = previous === null || first === undefined ? -1 : previous.facts.history.findIndex(turn => turn.id === first.turnId)
  const prefix = previous !== null && overlap >= 0 ? previous.facts.history.slice(0, overlap) : []
  const turns = [...prefix, ...pageTurns(source.history)]
  let facts: ExecutionFacts = { history: turns, active: [], latest: measureTurns(turns) }
  for (const event of source.activeEvents) facts = acceptExecutionEvent(facts, event)
  const runtime = source.runtime
  return { summary: source.history.summary,
    nextCursor: previous !== null && overlap >= 0 ? previous.nextCursor : source.history.nextCursor,
    runtime, facts: settleFacts(facts, runtime) }
}

export function updateExecutionRuntime(session: SessionView, runtime: SessionRuntime): SessionView {
  if (session.runtime.phase === runtime.phase && session.runtime.activeTurn?.turnId === runtime.activeTurn?.turnId
    && session.runtime.activeCompaction?.startedAt === runtime.activeCompaction?.startedAt && session.runtime.terminal === runtime.terminal) {
    return { ...session, runtime }
  }
  return { ...session, runtime, facts: settleFacts(session.facts, runtime) }
}

function settleFacts(facts: ExecutionFacts, runtime: SessionRuntime): ExecutionFacts {
  return { ...facts,
    history: settleRequests(facts.history, runtime), active: facts.active.map(turn => {
      if (turn.status !== null) return turn
      if (runtime.phase === 'idle' && runtime.terminal && turn.id === facts.active.at(-1)?.id) return finishTurn(turn, runtime.terminal.status)
      return settleRequests([turn], runtime)[0]
    }) }
}

/** An earlier page is converted once and prepended; loaded turns keep their identity. */
export function prependExecutionHistory(session: SessionView, page: ThreadReadPage): SessionView {
  const turns = [...pageTurns(page), ...session.facts.history]
  return { ...session, nextCursor: page.nextCursor, facts: { ...session.facts,
    history: settleRequests(turns, session.runtime), latest: session.facts.active.length ? session.facts.latest : measureTurns(turns) } }
}

function finishTurn(turn: ExecutionTurn, status: TurnStatus): ExecutionTurn {
  return { ...turn, status, items: turn.items.map(item => item.status === 'running'
    ? { ...item, status: status === 'interrupted' ? 'cancelled' : status === 'failed' ? 'error' : 'ok' } : item) }
}

/** Delta work is bounded by visible items, never by the number of earlier deltas. */
export function acceptExecutionEvent(facts: ExecutionFacts, event: TurnEventEnvelope): ExecutionFacts {
  const id = eventTurnId(event)
  const index = facts.active.findIndex(turn => turn.id === id)
  let turn: ExecutionTurn = index < 0 ? { id, status: null, items: [] } : facts.active[index]
  let latest = facts.latest
  switch (event.method) {
    case 'turn/userMessage': turn = upsert(turn, { ...base(event.params.item.itemId), kind: 'user', text: event.params.text }); break
    case 'provider/attempt':
      turn = request(turn, event.params.observation, null)
      latest = measure(latest, event.params.observation)
      break
    case 'item/started':
      if (!turn.items.some(item => item.id === event.params.item.itemId)) turn = upsert(turn, { ...base(event.params.item.itemId, 'running'), kind: 'unknown', text: event.params.item.itemId })
      break
    case 'item/agentMessage/delta':
    case 'item/agentThinking/delta': {
      const itemId = event.params.item.itemId
      const previous = turn.items.find(item => item.id === itemId)
      const kind = event.method === 'item/agentMessage/delta' ? 'assistant' : 'thinking'
      const text = (previous?.kind === kind ? previous.text : '') + event.params.delta
      turn = upsert(turn, { ...base(itemId, 'running'), kind, text,
        requestId: previous && 'requestId' in previous ? previous.requestId : lastRequest(turn) })
      break
    }
    case 'tool/execution/start':
    case 'tool/execution/update':
    case 'tool/execution/end': {
      // Start 建立工具事实（名称/参数/开始时刻）；Update/End 只按同一 item
      // 身份更新该事实，不重复携带静态定义。
      const p = event.params
      const previous = turn.items.find(item => item.id === p.item.itemId)
      const tool = previous?.kind === 'tool' ? previous : undefined
      turn = upsert(turn, { ...base(p.item.itemId, event.method === 'tool/execution/end' ? event.params.isError ? 'error' : 'ok' : 'running'), kind: 'tool',
        name: event.method === 'tool/execution/start' ? event.params.toolName : tool?.name ?? '',
        args: event.method === 'tool/execution/start' ? event.params.args : tool?.args ?? {},
        startedAt: event.method === 'tool/execution/start' ? event.params.startedAt : tool?.startedAt ?? null,
        output: event.method === 'tool/execution/update' ? event.params.partialResult : event.method === 'tool/execution/end' ? event.params.output : tool?.output ?? '',
        diff: event.method === 'tool/execution/end' && !event.params.isError ? event.params.diff : undefined,
        duration: event.method === 'tool/execution/end' ? event.params.durationMs : undefined })
      break
    }
    case 'item/completed':
    case 'item/failed': {
      if (event.params.content) turn = historyItem(turn, event.params.content)
      const previous = turn.items.find(item => item.id === event.params.item.itemId)
      if (previous?.kind === 'tool') break // The tool result owns success and failure.
      turn = upsert(turn, { ...(previous ?? { ...base(event.params.item.itemId), kind: 'unknown', text: event.params.item.itemId }),
        status: event.method === 'item/failed' ? 'error' : 'ok',
        error: event.method === 'item/failed' ? event.params.error : undefined })
      break
    }
    case 'agent/diagnostic': turn = upsert(turn, { ...base(`event-${turn.items.length}`, event.params.severity === 'error' ? 'error' : 'stable'), kind: 'event', text: event.params.message }); break
    case 'turn/error':
      turn = finishTurn(upsert(turn, { ...base(`event-${turn.items.length}`, 'error'), kind: 'event', text: JSON.stringify(event.params) }), 'failed')
      break
    case 'turn/completed': {
      const status = event.params.turn.status
      turn = finishTurn(turn, status)
      break
    }
  }
  const active = [...facts.active]
  if (index < 0) active.push(turn)
  else active[index] = turn
  return { ...facts, active, latest }
}
