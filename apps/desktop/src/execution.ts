import { eventTurnId } from './protocol'
import { compactionText } from './copy'
import type { HistoryItem, ImageAttachment, ReadSource, RequestObservation, SessionReadResult, SessionRuntime, ThreadReadPage, ThreadSummary, TurnErrorDetail, TurnEventEnvelope, TurnStatus, ToolResult } from './protocol'

export type FactStatus = 'stable' | 'running' | 'ok' | 'error' | 'cancelled'
interface FactBase { id: string; status: FactStatus; images?: ImageAttachment[] }
export type ExecutionItem = FactBase & (
  | { kind: 'user' | 'assistant' | 'thinking'; text: string; requestId?: string }
  | { kind: 'tool'; name: string; args: unknown; output: string; duration?: number; readSource?: ReadSource }
  | { kind: 'context'; name: string; text: string }
  | { kind: 'request'; observation: RequestObservation }
  | { kind: 'settings'; provider: string; model: string; reasoning: string | null }
  | { kind: 'compaction' | 'compaction_result'; text: string }
)
/** `null` 表示持久前导组，或不属于回合的活动压缩。 */
export interface ExecutionTurn { startedAt?: string; finishedAt?: string; id: string | null; status: TurnStatus | null; manuallyStopped: boolean; error?: TurnErrorDetail; items: ExecutionItem[] }
type ExecutionFacts = ExecutionTurn[]
/** 已加载的 history 只保存事实；wire page 是读取边界，不是常驻状态。 */
export interface SessionView {
  summary: ThreadSummary
  nextCursor: string | null
  runtime: SessionRuntime
  facts: ExecutionFacts
}

/** 回合错误关联到一个已有失败条目时，显示端只展示该条目的原因。 */
export function hasErrorItem(turn: ExecutionTurn, error: TurnErrorDetail | null | undefined): boolean {
  return error?.itemId !== undefined && turn.items.some(item => item.id === error.itemId && item.status === 'error')
}
const base = (id: string, status: FactStatus = 'stable'): FactBase => ({ id, status })
const lastRequest = (turn: ExecutionTurn) => turn.items.findLast(item => item.kind === 'request')?.id

/** 配对身份并结算执行状态，先于任何显示投影。 */
function upsert(turn: ExecutionTurn, item: ExecutionItem): ExecutionTurn {
  const index = turn.items.findIndex(previous => previous.id === item.id)
  const items = [...turn.items]
  if (index < 0) items.push(item)
  else items[index] = item
  return { ...turn, items }
}

/**
 * 助手/思考片段的状态跟随产出它的请求的失败终态：请求取消记 cancelled，失败记
 * error，实时与历史一致。请求成功或进行中时不改写片段自己的终态；存储、工具和
 * 未关联请求的片段这类独立失败也不改写。
 */
function settleAssistantItems(turn: ExecutionTurn): ExecutionTurn {
  let failures: Map<string, FactStatus> | undefined
  for (const item of turn.items) {
    if (item.kind === 'request' && (item.status === 'error' || item.status === 'cancelled')) {
      failures ??= new Map()
      failures.set(item.id, item.status)
    }
  }
  if (failures === undefined) return turn
  let changed = false
  const items = turn.items.map(item => {
    if (item.kind !== 'assistant' && item.kind !== 'thinking') return item
    const status = item.requestId === undefined ? undefined : failures.get(item.requestId)
    if (status === undefined || status === item.status) return item
    changed = true
    return { ...item, status }
  })
  return changed ? { ...turn, items } : turn
}

/** 请求只保留状态与计量，开始、结束使用同一个请求身份。 */
function requestItem(observation: RequestObservation): ExecutionItem {
  return { ...base(observation.requestId, observation.status === 'started' ? 'running' : observation.status),
    kind: 'request', observation }
}

/** 工具调用条目：名称与参数属于调用本身，结果随后按同一 id 就地替换。 */
function toolCallItem(id: string, name: string, args: unknown): Extract<ExecutionItem, { kind: 'tool' }> {
  return { ...base(id), kind: 'tool', name, args, output: '' }
}

/** 工具结果条目：名称与参数取自已配对的调用，输出与状态由结果决定。 */
function toolResultItem(item: ToolResult, previous: ExecutionItem | undefined): ExecutionItem {
  const tool = previous as Extract<ExecutionItem, { kind: 'tool' }>
  return { ...tool, status: item.isError ? 'error' : 'ok',
    output: item.output, duration: item.durationMs,
    readSource: item.readSource, images: item.images }
}

/** 历史和实时内容共用条目映射；助手片段关联产出它的请求。 */
function historyItemToExecution(item: HistoryItem, requestId: string | undefined): ExecutionItem {
  switch (item.type) {
    case 'request': return requestItem(item.observation)
    case 'context': return { ...base(item.id), kind: 'context', name: item.name, text: item.content }
    case 'message': return { ...base(item.id, item.running ? 'running' : 'stable'), kind: item.role === 'user' ? 'user' : 'assistant', text: item.text, images: item.images,
      requestId: item.role === 'assistant' ? requestId : undefined }
    case 'thinking': return { ...base(item.id, item.running ? 'running' : 'stable'), kind: 'thinking', text: item.text, requestId }
    case 'tool': {
      const tool = toolCallItem(item.id, item.name, item.args)
      const result = item.result ? toolResultItem(item.result, tool) : tool
      return item.running ? { ...result, status: 'running' } : result
    }
    case 'compaction': return { ...base(item.id), kind: 'compaction', text: item.summary }
    case 'compaction_result': return { ...base(item.id, item.status === 'running' ? 'running' : item.status === 'failed' ? 'error' : item.status === 'interrupted' ? 'cancelled' : 'stable'),
      kind: 'compaction_result', text: item.message ?? (item.status === 'running' ? compactionText.running : item.status === 'completed' ? compactionText.unchanged : item.status === 'interrupted' ? compactionText.unfinished : compactionText.failed) }
    case 'settings': return { ...base(item.id), kind: 'settings', provider: item.provider, model: item.model, reasoning: item.reasoning }
  }
}

/** 后端已经合并当前内容；前端转换条目并关联助手所属请求。 */
function pageTurns(page: ThreadReadPage): ExecutionTurn[] {
  return page.turns.map(turn => {
    const items: ExecutionItem[] = []
    let currentRequest: string | undefined
    for (const wire of turn.items) {
      if (wire.type === 'request') currentRequest = wire.observation.requestId
      const item = historyItemToExecution(wire, currentRequest)
      items.push(item)
    }
    return { id: turn.turnId, status: turn.status, manuallyStopped: turn.manuallyStopped, error: turn.error, startedAt: turn.startedAt, finishedAt: turn.finishedAt, items }
  })
}

/**
 * 把一个 session page 读成事实。新 page 的尾部与已加载内容重叠时保留原前缀，
 * 不重叠时该 page 自带 cursor，未加载的缺口会暴露出来。
 */
export function readExecution(source: SessionReadResult, previous: SessionView | null = null): SessionView {
  const first = source.history.turns[0]
  const overlap = previous === null || first === undefined ? -1 : previous.facts.findIndex(turn => turn.id === first.turnId)
  const prefix = previous !== null && overlap >= 0 ? previous.facts.slice(0, overlap) : []
  const turns = [...prefix, ...pageTurns(source.history)]
  const runtime = source.runtime
  return { summary: source.history.summary,
    nextCursor: previous !== null && overlap >= 0 ? previous.nextCursor : source.history.nextCursor,
    runtime, facts: settleFacts(turns, runtime) }
}

export function updateExecutionRuntime(session: SessionView, runtime: SessionRuntime): SessionView {
  if (session.runtime.phase === runtime.phase
    && session.runtime.terminal === runtime.terminal) {
    return { ...session, runtime }
  }
  return { ...session, runtime, facts: settleFacts(session.facts, runtime) }
}

function settleFacts(facts: ExecutionFacts, runtime: SessionRuntime): ExecutionFacts {
  return facts.map(turn => {
    if ((turn.status === null || turn.status === 'running') && turn.id !== null && runtime.phase === 'idle' && runtime.terminal?.source === 'turn') {
      return finishTurn(turn, runtime.terminal.status, runtime.terminal.manuallyStopped)
    }
    return settleAssistantItems(turn)
  })
}

/** 更早的 page 只转换一次并前插；已加载 turns 保持其身份。 */
export function prependExecutionHistory(session: SessionView, page: ThreadReadPage): SessionView {
  const turns = pageTurns(page).map(settleAssistantItems)
  return { ...session, nextCursor: page.nextCursor, facts: [...turns, ...session.facts] }
}

function finishTurn(turn: ExecutionTurn, status: TurnStatus, manuallyStopped = false): ExecutionTurn {
  return settleAssistantItems({ ...turn, status, manuallyStopped, items: turn.items.map(item => item.status === 'running'
    ? { ...item, status: status === 'interrupted' ? 'cancelled' : status === 'failed' ? 'error' : 'ok' } : item) })
}

/** delta 的工作量只按可见 items 计算，与更早 delta 的数量无关。 */
export function acceptExecutionEvent(facts: ExecutionFacts, event: TurnEventEnvelope): ExecutionFacts {
  const id = eventTurnId(event) ?? facts.at(-1)?.id ?? null
  const index = facts.findIndex(turn => turn.id === id)
  let turn: ExecutionTurn = index < 0 ? { id, status: null, manuallyStopped: false, items: [] } : facts[index]
  switch (event.method) {
    case 'turn/started':
      turn = { ...turn, status: 'running', startedAt: event.params.startedAt }
      break
    case 'turn/userMessage': turn = upsert(turn, { ...base(event.params.item.itemId), kind: 'user', text: event.params.text, images: event.params.images }); break
    case 'provider/attempt': {
      const observation = event.params.observation
      turn = upsert(turn, requestItem(observation))
      break
    }
    case 'context/injected':
    case 'session/settingsUpdated':
    case 'compaction/updated': {
      const item = event.params.content
      turn = upsert(turn, historyItemToExecution(item, undefined))
      break
    }
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
    // 开始事件建立工具定义；实时流和恢复快照都保留它，后续只更新输出。
    case 'tool/execution/start': {
      const p = event.params
      turn = upsert(turn, { ...toolCallItem(p.item.itemId, p.toolName, p.args),
        status: 'running' })
      break
    }
    case 'tool/execution/update': {
      const p = event.params
      const tool = turn.items.find(item => item.id === p.item.itemId) as Extract<ExecutionItem, { kind: 'tool' }>
      turn = upsert(turn, { ...tool, output: p.partialResult })
      break
    }
    case 'tool/execution/end': {
      const p = event.params
      turn = upsert(turn, toolResultItem(p, turn.items.find(item => item.id === p.item.itemId)))
      break
    }
    case 'item/discarded':
      turn = { ...turn, items: turn.items.filter(item => item.id !== event.params.item.itemId) }
      break
    case 'item/completed':
    case 'item/failed': {
      // 完成事件只来自助手正文与思考；完整内容和最终状态一起写回。
      const content = event.params.content
      const previous = turn.items.find(item => item.id === event.params.item.itemId)
      const item = content ? historyItemToExecution(content, lastRequest(turn)) : previous
      if (!item) break
      turn = upsert(turn, { ...item,
        status: event.method === 'item/failed' ? 'error' : 'ok' })
      break
    }
    case 'turn/error':
      // 失败细节是类型化事实：只有关联的 turn 保存它，不编码成说明文本。
      turn = { ...finishTurn(turn, 'failed'), error: event.params.error, finishedAt: event.params.finishedAt }
      break
    case 'turn/completed': {
      const status = event.params.turn.status
      turn = { ...finishTurn(turn, status, event.params.manuallyStopped), finishedAt: event.params.finishedAt }
      break
    }
    default: {
      event satisfies never
    }
  }
  const active = [...facts]
  if (index < 0) active.push(turn)
  else active[index] = turn
  return active
}
