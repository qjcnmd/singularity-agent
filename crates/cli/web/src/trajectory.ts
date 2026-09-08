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
const requestId = (r: Pick<RequestObservation, 'ordinal' | 'attempt'>) => `request-${r.ordinal}-${r.attempt}`
const lastRequest = (entries: TrajectoryEntry[]) => entries.findLast(item => item.request !== undefined)
const requestTitle = (r: RequestObservation) => `请求 #${r.attempt}`
const historyProjection = new WeakMap<ThreadTurn, TrajectoryEntry[]>()
const promptSignatures = new WeakMap<ModelRequestSnapshot, string>()

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
    for (const [index, event] of active.events.entries()) {
      const id = String(event.params.turnId ?? (event.params.turn as { turnId?: string } | undefined)?.turnId ?? active.turnId)
      let turn = turns.find(item => item.id === id)
      if (!turn) { turn = { id, title: '', entries: [] }; turns.push(turn) }
      projectActive(turn.entries, event, index)
    }
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
      const prompt = item.request?.request
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
      entries.push({ ...entry(requestId(r), 'assistant', requestTitle(r)), request: r, duration: r.durationMs, status: r.status === 'started' ? 'running' : r.status, failed: r.status === 'error' })
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
      result.text = item.output
      result.failed = item.isError
      result.status = item.isError ? 'error' : 'ok'
      result.duration = item.durationMs ?? null
      if (!call) entries.push(result)
      break
    }
    case 'compaction': entries.push(entry(item.id, 'compaction', '上下文压缩', item.summary)); break
    case 'settings': entries.push(entry(item.id, 'settings', '模型设置', `${item.provider}/${item.model}${item.reasoning ? ` · ${item.reasoning}` : ''}`)); break
  }
}

function projectActive(entries: TrajectoryEntry[], event: TurnEventEnvelope, index: number): void {
  const p = event.params
  switch (event.method) {
    case 'turn/started':
      if (!entries.some(item => item.kind === 'user')) entries.push(entry(`user-${index}`, 'user', '用户', String(p.input ?? '')))
      break
    case 'provider/attempt': {
      const r: RequestObservation = {
        ordinal: Number(p.modelTurnOrdinal), attempt: Number(p.attempt), provider: String(p.provider), model: String(p.model),
        status: p.status as RequestObservation['status'], durationMs: typeof p.attemptDurationMs === 'number' ? p.attemptDurationMs : 0,
        inputTokens: typeof p.inputTokens === 'number' ? p.inputTokens : null,
        outputTokens: typeof p.outputTokens === 'number' ? p.outputTokens : null,
        cachedInputTokens: typeof p.cachedInputTokens === 'number' ? p.cachedInputTokens : null,
        error: p.errorCategory ? String(p.errorCategory) : null,
        request: p.request as ModelRequestSnapshot | undefined,
      }
      let item = entries.find(value => value.id === requestId(r))
      if (!item) { item = entry(requestId(r), 'assistant', requestTitle(r)); entries.push(item) }
      item.request = { ...r, request: r.request ?? item.request?.request }
      item.duration = p.status === 'started' ? null : r.durationMs
      item.status = r.status === 'started' ? 'running' : r.status
      item.failed = r.status === 'error'
      break
    }
    case 'item/agentMessage/delta': {
      const id = (p.item as { itemId?: string } | undefined)?.itemId ?? 'assistant'
      let item = lastRequest(entries) ?? entries.find(value => value.id === id)
      if (!item) { item = { ...entry(id, 'assistant', '助手'), status: 'running' }; entries.push(item) }
      item.text += String(p.delta ?? '')
      break
    }
    case 'item/agentThinking/delta':
    case 'item/agentThinking': {
      const item = lastRequest(entries) ?? entries.findLast(value => value.kind === 'assistant')
      if (item) item.thinking = event.method.endsWith('/delta') ? item.thinking + String(p.delta ?? '') : String(p.text ?? '')
      break
    }
    case 'tool/execution/start':
    case 'tool/execution/update':
    case 'tool/execution/end': {
      const id = String(p.toolCallId)
      let item = entries.find(value => value.id === id)
      if (!item) { item = entry(id, 'tool', String(p.toolName ?? '工具')); entries.push(item) }
      item.input = p.args ?? item.input
      item.startedAt = typeof p.startedAt === 'string' ? p.startedAt : item.startedAt
      item.status = 'running'
      if (event.method === 'tool/execution/update') item.text = String(p.partialResult ?? '')
      if (event.method === 'tool/execution/end') {
        const result = p.result as { content?: Array<{ text?: string }>; isError?: boolean } | undefined
        item.text = result?.content?.map(part => part.text ?? '').join('\n') ?? ''
        item.failed = result?.isError ?? false
        item.status = item.failed ? 'error' : 'ok'
        item.duration = typeof p.durationMs === 'number' ? p.durationMs : null
      }
      break
    }
    case 'agent/diagnostic':
    case 'turn/error': entries.push({ ...entry(`event-${index}`, 'event', '运行信息', String(p.message ?? JSON.stringify(p))), failed: p.severity === 'error' || event.method === 'turn/error' }); break
    case 'turn/completed': {
      const status = (p.turn as { status?: string } | undefined)?.status
      for (const item of entries) if (item.status === 'running') {
        item.status = status === 'interrupted' ? 'cancelled' : status === 'failed' ? 'error' : 'ok'
        item.failed = status === 'failed'
      }
      break
    }
  }
}
