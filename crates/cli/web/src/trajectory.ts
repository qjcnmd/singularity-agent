import { eventsSince, isEventPrefix, type EventSequence } from './eventLog'
import { eventTurnId } from './protocol'
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
  failed: boolean
}
export interface TrajectoryTurn { id: string; title: string; entries: TrajectoryEntry[] }

function entry(id: string, kind: TrajectoryKind, title: string, text = ''): TrajectoryEntry {
  return { id, kind, title, text, thinking: '', duration: null, startedAt: null, status: 'stable', failed: false }
}

/** The inspector presents the complete result; no consumer parses this display text. */
function finishTool(item: TrajectoryEntry, output: string, diff: string | undefined, failed: boolean, duration: number | undefined) {
  item.text = diff ? `${output}\n\n${diff}` : output
  item.failed = failed
  item.status = failed ? 'error' : 'ok'
  item.duration = duration ?? null
}
const requestId = (r: Pick<RequestObservation, 'requestId' | 'ordinal' | 'attempt'>) => r.requestId || `request-${r.ordinal}-${r.attempt}`
const lastRequest = (entries: TrajectoryEntry[]) => entries.findLast(item => item.request !== undefined)
const requestTitle = (r: RequestObservation) => `${r.purpose === 'compaction' ? '摘要请求' : '请求'} #${r.attempt}`
const historyProjection = new WeakMap<ThreadTurn, TrajectoryEntry[]>()
const promptSignatures = new WeakMap<ModelRequestSnapshot, string>()
let activeProjection: { history: ThreadTurn[] | null; events: EventSequence; turns: Map<string, TrajectoryTurn> } = { history: null, events: [], turns: new Map() }

/** Inspect durable history and the active stream without another copy of runtime state. */
export function buildTrajectory(session: SessionReadResult | null): TrajectoryTurn[] {
  if (!session) return []
  const turns: TrajectoryTurn[] = []
  for (const [index, turn] of session.history.turns.entries()) {
    let stable = historyProjection.get(turn)
    if (!stable) {
      stable = []
      for (const item of turn.items) projectHistory(stable, item)
      historyProjection.set(turn, stable)
    }
    // The active stream and prompt/schema decoration only mutate this view copy.
    const entries = stable.map(item => ({ ...item }))
    turns.push({ id: turn.turnId ?? `leading-${index}`, title: '', entries })
  }
  const active = session.runtime.activeTurn
  if (active) {
    const appended = activeProjection.history === session.history.turns && isEventPrefix(activeProjection.events, active.events)
    const start = appended ? activeProjection.events.length : 0
    if (!appended) activeProjection = { history: session.history.turns, events: [], turns: new Map() }
    let index = start
    for (const event of eventsSince(active.events, start, appended ? activeProjection.events : undefined)) {
      const id = eventTurnId(event)
      let turn = activeProjection.turns.get(id)
      if (!turn) {
        turn = { id, title: '', entries: turns.find(value => value.id === id)?.entries.map(item => ({ ...item })) ?? [] }
        activeProjection.turns.set(id, turn)
      }
      projectActive(turn.entries, event, index++)
    }
    activeProjection.events = active.events
    for (const activeTurn of activeProjection.turns.values()) {
      const value = { ...activeTurn, entries: activeTurn.entries.map(item => ({ ...item })) }
      const position = turns.findIndex(turn => turn.id === value.id)
      if (position < 0) turns.push(value)
      else turns[position] = value
    }
  } else {
    activeProjection = { history: null, events: [], turns: new Map() }
  }
  const terminal = session.runtime.terminal
  if (!active && terminal?.status === 'failed' && terminal.message) {
    let turn = turns.at(-1)
    if (!turn) { turn = { id: 'runtime', title: '', entries: [] }; turns.push(turn) }
    turn.entries.push({ ...entry('runtime-error', 'event', '运行错误', terminal.message), status: 'error', failed: true })
  }
  let ordinal = 0
  let previousPrompt: ModelRequestSnapshot | undefined
  for (const turn of turns) {
    turn.title = turn.id.startsWith('leading-') ? '会话设置' : `第 ${++ordinal} 轮`
    const withPrompts: TrajectoryEntry[] = []
    for (const item of turn.entries) {
      if (item.request?.status === 'started') {
        const compaction = session.runtime.activeCompaction
        const liveCompaction = item.request.purpose === 'compaction' && compaction && item.startedAt
          && Date.parse(item.startedAt) >= Date.parse(compaction.startedAt)
        // A durable start survives a killed process; only runtime state proves it is still running.
        item.status = turn.id === active?.turnId || liveCompaction ? 'running' : 'cancelled'
        item.duration = null
      }
      const prompt = item.request?.requestHead ?? item.request?.request
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
    turn.entries = withPrompts
  }
  return turns.filter(turn => turn.entries.length)
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
      entries.push({ ...entry(requestId(r), 'assistant', requestTitle(r)), request: r, startedAt: r.status === 'started' ? item.timestamp : null, duration: r.durationMs, status: r.status === 'started' ? 'running' : r.status, failed: r.status === 'error' })
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
    case 'turn/started':
      if (!entries.some(item => item.kind === 'user')) entries.push(entry(`user-${index}`, 'user', '用户', event.params.input))
      break
    case 'provider/attempt': {
      const p = event.params
      const r: RequestObservation = {
        requestId: p.requestId, requestHead: p.requestHead, purpose: p.purpose,
        ordinal: p.modelTurnOrdinal, attempt: p.attempt, provider: p.provider, model: p.model,
        status: p.status, durationMs: p.attemptDurationMs ?? 0,
        inputTokens: p.inputTokens, outputTokens: p.outputTokens, cachedInputTokens: p.cachedInputTokens,
        error: p.errorCategory,
      }
      let item = entries.find(value => value.id === requestId(r))
      if (!item) { item = entry(requestId(r), 'assistant', requestTitle(r)); entries.push(item) }
      item.request = { ...r, requestHead: r.requestHead ?? item.request?.requestHead }
      item.duration = p.status === 'started' ? null : r.durationMs
      item.status = r.status === 'started' ? 'running' : r.status
      item.failed = r.status === 'error'
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
      entries.push({ ...entry(`event-${index}`, 'event', '运行信息', event.params.message), failed: event.params.severity === 'error' })
      break
    case 'turn/error':
      entries.push({ ...entry(`event-${index}`, 'event', '运行信息', JSON.stringify(event.params)), failed: true })
      break
    case 'turn/completed': {
      const status = event.params.turn.status
      for (const item of entries) if (item.status === 'running') {
        item.status = status === 'interrupted' ? 'cancelled' : status === 'failed' ? 'error' : 'ok'
        item.failed = status === 'failed'
      }
      break
    }
  }
}
