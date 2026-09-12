import { eventsSince, isEventPrefix, type EventSequence } from './eventLog'
import { eventTurnId, userMessageItemId } from './protocol'
import type { HistoryItem, ModelRequestSnapshot, RequestObservation, SessionReadResult, ThreadTurn, TurnEventEnvelope } from './protocol'

export type TrajectoryKind = 'system' | 'user' | 'assistant' | 'tool' | 'compaction' | 'settings' | 'event'
export interface TrajectoryEntry {
  id: string
  kind: TrajectoryKind
  title: string
  text: string
  thinking: string
  input?: unknown
  schema?: unknown
  request?: RequestObservation
  prompt?: ModelRequestSnapshot
  previousPrompt?: ModelRequestSnapshot
  duration: number | null
  startedAt: string | null
  status: 'stable' | 'running' | 'ok' | 'error' | 'cancelled'
}
export interface TrajectoryTurn { id: string; title: string; entries: TrajectoryEntry[] }

function entry(id: string, kind: TrajectoryKind, title: string, text = ''): TrajectoryEntry {
  return { id, kind, title, text, thinking: '', duration: null, startedAt: null, status: 'stable' }
}

/** The inspector presents the complete result; no consumer parses this display text. */
function finishTool(item: TrajectoryEntry, output: string, diff: string | undefined, failed: boolean, duration: number | undefined) {
  item.text = diff ? `${output}\n\n${diff}` : output
  item.status = failed ? 'error' : 'ok'
  item.duration = duration ?? null
}
const lastRequest = (entries: TrajectoryEntry[]) => entries.findLast(item => item.request !== undefined)
const requestTitle = (r: RequestObservation) => `${r.purpose === 'compaction' ? '摘要请求' : '请求'} #${r.attempt}`
const historyEntryProjection = new WeakMap<ThreadTurn, TrajectoryEntry[]>()
const promptSignatures = new WeakMap<ModelRequestSnapshot, string>()
interface HistoricalProjection {
  turns: TrajectoryTurn[]
  unfinished: Array<{ turnIndex: number; entryIndex: number; startedAt: string | null }>
  previousPrompt: ModelRequestSnapshot | undefined
  ordinal: number
}
const historicalProjection = new WeakMap<ThreadTurn[], HistoricalProjection>()
let activeProjection: { events: EventSequence; turns: Map<string, TrajectoryTurn> } = { events: [], turns: new Map() }

/** Inspect durable history and the active stream without another copy of runtime state. */
export function buildTrajectory(session: SessionReadResult | null): TrajectoryTurn[] {
  if (!session) return []
  const historical = projectHistoricalTurns(session.history.turns)
  const turns = [...applyHistoricalLiveness(historical, session)]
  let ordinal = historical.ordinal
  let previousPrompt = historical.previousPrompt
  const active = session.runtime.activeTurn
  if (active) {
    const appended = isEventPrefix(activeProjection.events, active.events)
    const start = appended ? activeProjection.events.length : 0
    if (!appended) activeProjection = { events: [], turns: new Map() }
    let index = start
    for (const event of eventsSince(active.events, start, appended ? activeProjection.events : undefined)) {
      const id = eventTurnId(event)
      let turn = activeProjection.turns.get(id)
      if (!turn) {
        turn = { id, title: '', entries: [] }
        activeProjection.turns.set(id, turn)
      }
      projectActive(turn.entries, event, index++)
    }    activeProjection.events = active.events
    // The server freezes history before this chain; active turns only come from its events.
    for (const activeTurn of activeProjection.turns.values()) {
      const projected = decorateEntries(
        activeTurn.entries.map(item => ({ ...item })),
        activeTurn.id,
        session,
        previousPrompt,
      )
      previousPrompt = projected.previousPrompt
      turns.push({
        id: activeTurn.id,
        title: activeTurn.id.startsWith('leading-') ? '会话设置' : `第 ${++ordinal} 轮`,
        entries: projected.entries,
      })
    }
  } else {
    activeProjection = { events: [], turns: new Map() }
  }
  const terminal = session.runtime.terminal
  if (!active && terminal?.status === 'failed' && terminal.message) {
    const failure = { ...entry('runtime-error', 'event', '运行错误', terminal.message), status: 'error' as const }
    const index = turns.length - 1
    if (index >= 0) {
      const turn = turns[index]
      turns[index] = { ...turn, entries: [...turn.entries, failure] }
    } else {
      turns.push({ id: 'runtime', title: `第 ${++ordinal} 轮`, entries: [failure] })
    }
  }
  return turns.filter(turn => turn.entries.length)
}

function projectHistoricalTurns(history: ThreadTurn[]): HistoricalProjection {
  const cached = historicalProjection.get(history)
  if (cached) return cached
  const turns: TrajectoryTurn[] = []
  const unfinished: HistoricalProjection['unfinished'] = []
  let previousPrompt: ModelRequestSnapshot | undefined
  let ordinal = 0
  for (const [index, turn] of history.entries()) {
    let stable = historyEntryProjection.get(turn)
    if (!stable) {
      stable = []
      for (const item of turn.items) projectHistory(stable, item)
      historyEntryProjection.set(turn, stable)
    }
    const id = turn.turnId ?? `leading-${index}`
    const projected = decorateEntries(stable.map(item => ({ ...item })), id, null, previousPrompt)
    previousPrompt = projected.previousPrompt
    const turnIndex = turns.length
    for (const [entryIndex, item] of projected.entries.entries()) {
      if (item.request?.status === 'started') {
        unfinished.push({ turnIndex, entryIndex, startedAt: item.startedAt })
      }
    }
    turns.push({
      id,
      title: id.startsWith('leading-') ? '会话设置' : `第 ${++ordinal} 轮`,
      entries: projected.entries,
    })
  }
  const projection = { turns, unfinished, previousPrompt, ordinal }
  historicalProjection.set(history, projection)
  return projection
}

function applyHistoricalLiveness(
  projection: HistoricalProjection,
  session: SessionReadResult,
): TrajectoryTurn[] {
  let turns: TrajectoryTurn[] | null = null
  for (const pending of projection.unfinished) {
    const turn = projection.turns[pending.turnIndex]
    const item = turn.entries[pending.entryIndex]
    const compaction = session.runtime.activeCompaction
    const liveCompaction = item.request?.purpose === 'compaction' && compaction && pending.startedAt
      && Date.parse(pending.startedAt) >= Date.parse(compaction.startedAt)
    if (turn.id !== session.runtime.activeTurn?.turnId && !liveCompaction) continue
    turns ??= [...projection.turns]
    const current = turns[pending.turnIndex]
    const entries = current === turn ? [...turn.entries] : current.entries
    entries[pending.entryIndex] = { ...item, status: 'running', duration: null }
    turns[pending.turnIndex] = { ...current, entries }
  }
  return turns ?? projection.turns
}

function decorateEntries(
  entries: TrajectoryEntry[],
  turnId: string,
  session: SessionReadResult | null,
  initialPrompt: ModelRequestSnapshot | undefined,
): { entries: TrajectoryEntry[]; previousPrompt: ModelRequestSnapshot | undefined } {
  let previousPrompt = initialPrompt
  const withPrompts: TrajectoryEntry[] = []
  for (const item of entries) {
    if (item.request?.status === 'started') {
      const compaction = session?.runtime.activeCompaction
      const liveCompaction = item.request.purpose === 'compaction' && compaction && item.startedAt
        && Date.parse(item.startedAt) >= Date.parse(compaction.startedAt)
      // A durable start survives a killed process; only runtime state proves it is still running.
      item.status = turnId === session?.runtime.activeTurn?.turnId || liveCompaction ? 'running' : 'cancelled'
      item.duration = null
    }
    const prompt = item.request?.requestHead
    if (prompt && promptSignature(prompt) !== (previousPrompt && promptSignature(previousPrompt))) {
      const system = entry(`system-${item.id}`, 'system', previousPrompt ? '系统提示词更新' : '初始系统提示词', systemText(prompt))
      system.prompt = prompt
      system.previousPrompt = previousPrompt
      withPrompts.push(system)
    }
    if (prompt) previousPrompt = prompt
    if (item.kind === 'tool') item.schema = previousPrompt?.tools.find(tool => tool.name === item.title)
    withPrompts.push(item)
  }
  return { entries: withPrompts, previousPrompt }
}

export function systemText(request: ModelRequestSnapshot): string {
  return request.messages.filter(message => message.role === 'system' || message.role === 'developer').map(message => message.content).join('\n\n')
}
function promptSignature(request: ModelRequestSnapshot): string {
  let signature = promptSignatures.get(request)
  if (signature === undefined) {
    signature = JSON.stringify([systemText(request), request.tools])
    promptSignatures.set(request, signature)
  }
  return signature
}

function projectHistory(entries: TrajectoryEntry[], item: HistoryItem): void {
  switch (item.type) {
    case 'request': {
      const r = item.observation
      entries.push({ ...entry(r.requestId, 'assistant', requestTitle(r)), request: r, startedAt: r.status === 'started' ? item.timestamp : null, duration: r.durationMs, status: r.status === 'started' ? 'running' : r.status })
      break
    }
    case 'message': {
      const request = item.role === 'assistant' ? lastRequest(entries) : undefined
      if (request) request.text += (request.text ? '\n\n' : '') + item.text
      else entries.push(entry(item.id, item.role === 'user' ? 'user' : 'assistant', item.role === 'user' ? '用户' : '助手', item.text))
      break
    }
    case 'thinking': {
      const request = lastRequest(entries) ?? entries.findLast(value => value.kind === 'assistant')
      if (request) request.thinking += (request.thinking ? '\n\n' : '') + item.text
      else entries.push({ ...entry(item.id, 'assistant', '助手'), thinking: item.text })
      break
    }
    case 'tool_call': entries.push({ ...entry(item.id, 'tool', item.name), input: item.args }); break
    case 'tool_result': {
      const call = entries.find(value => value.id === item.id && value.kind === 'tool')
      const result = call ?? entry(item.id, 'tool', '工具输出')
      finishTool(result, item.output, item.diff, item.isError, item.durationMs)
      if (!call) entries.push(result)
      break
    }
    case 'compaction': entries.push(entry(item.id, 'compaction', '上下文压缩', item.summary)); break
    case 'settings': entries.push(entry(item.id, 'settings', '模型设置', `${item.provider}/${item.model}${item.reasoning ? ` · ${item.reasoning}` : ''}`)); break
  }
}

function projectActive(entries: TrajectoryEntry[], event: TurnEventEnvelope, index: number): void {
  switch (event.method) {
    case 'turn/userMessage':
      entries.push(entry(userMessageItemId(event.params.entryId), 'user', '用户', event.params.text))
      break
    case 'provider/attempt': {
      const r: RequestObservation = event.params.observation
      let item = entries.find(value => value.id === r.requestId)
      if (!item) { item = entry(r.requestId, 'assistant', requestTitle(r)); entries.push(item) }
      item.request = { ...r, requestHead: r.requestHead ?? item.request?.requestHead }
      item.duration = r.status === 'started' ? null : r.durationMs
      item.status = r.status === 'started' ? 'running' : r.status
      break
    }
    case 'item/agentMessage/delta': {
      const id = event.params.item.itemId
      let item = lastRequest(entries) ?? entries.find(value => value.id === id)
      if (!item) { item = { ...entry(id, 'assistant', '助手'), status: 'running' }; entries.push(item) }
      item.text += event.params.delta
      break
    }
    case 'item/agentThinking/delta':
    case 'item/agentThinking': {
      const item = lastRequest(entries) ?? entries.findLast(value => value.kind === 'assistant')
      if (item) item.thinking = event.method === 'item/agentThinking/delta' ? item.thinking + event.params.delta : event.params.text
      else entries.push({ ...entry(`thinking-${index}`, 'assistant', '助手'), thinking: event.method === 'item/agentThinking/delta' ? event.params.delta : event.params.text })
      break
    }
    case 'tool/execution/start':
    case 'tool/execution/update':
    case 'tool/execution/end': {
      const id = event.params.toolCallId
      let item = entries.find(value => value.id === id)
      if (!item) { item = entry(id, 'tool', event.params.toolName); entries.push(item) }
      if ('args' in event.params) item.input = event.params.args
      if (event.method === 'tool/execution/start') item.startedAt = event.params.startedAt ?? item.startedAt
      item.status = 'running'
      if (event.method === 'tool/execution/update') item.text = event.params.partialResult
      if (event.method === 'tool/execution/end') {
        const { result, durationMs } = event.params
        finishTool(item, result.content.map(part => part.text).join('\n'), result.diff, result.isError, durationMs)
      }
      break
    }
    case 'agent/diagnostic':
      entries.push({ ...entry(`event-${index}`, 'event', '运行信息', event.params.message), status: event.params.severity === 'error' ? 'error' : 'stable' })
      break
    case 'turn/error':
      entries.push({ ...entry(`event-${index}`, 'event', '运行信息', JSON.stringify(event.params)), status: 'error' })
      break
    case 'turn/completed': {
      const status = event.params.turn.status
      for (const item of entries) if (item.status === 'running') {
        item.status = status === 'interrupted' ? 'cancelled' : status === 'failed' ? 'error' : 'ok'
      }
      break
    }
  }
}
