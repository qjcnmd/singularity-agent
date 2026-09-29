import { eventTurnId } from './protocol'
import type { HistoryItem, ReadSource, RequestObservation, SessionReadResult, SessionRuntime, ThreadReadPage, ThreadSummary, TurnErrorDetail, TurnEventEnvelope, TurnStatus } from './protocol'

export type FactStatus = 'stable' | 'running' | 'ok' | 'error' | 'cancelled'
interface FactBase { id: string; status: FactStatus; startedAt: string | null; error?: string }
export type ExecutionItem = { id: string; kind: 'unknown' } | FactBase & (
  | { kind: 'user' | 'assistant' | 'thinking'; text: string; requestId?: string }
  | { kind: 'tool'; name: string; args: unknown; output: string; diff?: string; duration?: number; readSource?: ReadSource }
  | { kind: 'request'; observation: RequestObservation }
  | { kind: 'settings'; provider: string; model: string; reasoning: string | null }
  | { kind: 'compaction' | 'compaction_result' | 'event'; text: string }
)
/** `null` 保留 wire 的含义：记录被归组到首次真实运行之前。 */
export interface ExecutionTurn { startedAt?: string; finishedAt?: string; id: string | null; status: TurnStatus | null; error?: TurnErrorDetail; items: ExecutionItem[] }
interface ExecutionFacts { history: ExecutionTurn[]; active: ExecutionTurn[] }
/** 已加载的 history 只以事实形式存在；wire page 是读取边界，而非常驻状态。 */
export interface SessionView {
  summary: ThreadSummary
  nextCursor: string | null
  runtime: SessionRuntime
  facts: ExecutionFacts
}
const base = (id: string, status: FactStatus = 'stable'): FactBase => ({ id, status, startedAt: null })
const lastRequest = (turn: ExecutionTurn) => turn.items.findLast(item => item.kind === 'request')?.id

/** 在任何显示投影之前，先一次性配对身份并结算执行状态。 */
function upsert(turn: ExecutionTurn, item: ExecutionItem): ExecutionTurn {
  const index = turn.items.findIndex(previous => previous.id === item.id)
  const items = [...turn.items]
  if (index < 0) items.push(item)
  else items[index] = item
  return { ...turn, items }
}

/**
 * 助手/思考片段的状态由产出它的请求的失败终态决定：请求取消得到 cancelled、
 * 真正失败得到 error，实时与历史因此一致。请求成功或仍在进行时不改写片段
 * 自己的终态，独立失败（存储、工具、未关联请求的片段）不被覆盖。
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

/** 请求条目：开始观测是 running；已知的开始时间与请求头不因终态观测被清空，
 *  没有开始记录时保持未知，不用结束时间补。实时与批量构建共用这一条规则。 */
function requestItem(observation: RequestObservation, previous: ExecutionItem | undefined, startedAt: string | null): ExecutionItem {
  const prior = previous?.kind === 'request' ? previous.observation : undefined
  return { ...base(observation.requestId, observation.status === 'started' ? 'running' : observation.status),
    kind: 'request', observation: { ...observation, requestHead: observation.requestHead ?? prior?.requestHead },
    startedAt: startedAt ?? (previous?.kind === 'request' ? previous.startedAt : null) }
}

/** 工具调用条目：名称与参数归调用所有，结果随后按同一 id 就地替换。 */
function toolCallItem(id: string, name: string, args: unknown): Extract<ExecutionItem, { kind: 'tool' }> {
  return { ...base(id), kind: 'tool', name, args, output: '' }
}

/** 工具结果条目：名称与参数沿用已配对的调用，失败标志与 diff 由结果决定。 */
function toolResultItem(item: Extract<HistoryItem, { type: 'tool_result' }>, previous: ExecutionItem | undefined): ExecutionItem {
  const tool = previous as Extract<ExecutionItem, { kind: 'tool' }>
  return { ...base(item.id, item.isError ? 'error' : 'ok'), kind: 'tool',
    name: tool.name, args: tool.args,
    output: item.output, diff: item.isError ? undefined : item.diff, duration: item.durationMs,
    readSource: item.readSource }
}

/** 单条 wire HistoryItem → ExecutionItem 的唯一字段映射：批量页与实时
 *  content 共用同一份字段归属规则。previous 是同 id 的既有条目（请求沿用
 *  开始时间与请求头，工具结果沿用已配对的调用），requestId 是助手/思考
 *  片段归属的请求；更新算法由调用方决定。 */
function historyItemToExecution(item: HistoryItem, previous: ExecutionItem | undefined, requestId: string | undefined): ExecutionItem {
  switch (item.type) {
    case 'request': return requestItem(item.observation, previous, item.startedAt ?? null)
    case 'message': return { ...base(item.id), kind: item.role === 'user' ? 'user' : 'assistant', text: item.text,
      requestId: item.role === 'assistant' ? requestId : undefined }
    case 'thinking': return { ...base(item.id), kind: 'thinking', text: item.text, requestId }
    case 'tool_call': return toolCallItem(item.id, item.name, item.args)
    case 'tool_result': return toolResultItem(item, previous)
    case 'compaction': return { ...base(item.id), kind: 'compaction', text: item.summary }
    case 'compaction_result': return { ...base(item.id, item.status === 'failed' ? 'error' : item.status === 'interrupted' ? 'cancelled' : 'stable'),
      kind: 'compaction_result', text: item.message ?? (item.status === 'completed' ? '没有可压缩的内容' : item.status === 'interrupted' ? '已停止' : '压缩失败') }
    case 'settings': return { ...base(item.id), kind: 'settings', provider: item.provider, model: item.model, reasoning: item.reasoning }
  }
}

/**
 * 唯一的转换边界：一个 wire page 一次局部构建成 execution turns。
 * id→位置与当前 request 关联只存在于这次构建内：同 id 的条目就地替换
 * （request 的 start/end 合并、tool call/result 配对），逐项不再复制整段
 * items；结束时发布一次 ExecutionTurn，并在此接上助手终态归约
 * 与请求开始时间。字段映射复用单条转换，这里只保留位置与
 * 当前 request 的更新算法。
 */
function pageTurns(page: ThreadReadPage): ExecutionTurn[] {
  return page.turns.map(turn => {
    const items: ExecutionItem[] = []
    const positions = new Map<string, number>()
    let currentRequest: string | undefined
    for (const wire of turn.items) {
      if (wire.type === 'request') currentRequest = wire.observation.requestId
      // 请求条目的身份就是其观测的 request id，其余条目自带 id。
      const id = wire.type === 'request' ? wire.observation.requestId : wire.id
      const position = positions.get(id)
      const previous = position === undefined ? undefined : items[position]
      const item = historyItemToExecution(wire, previous, currentRequest)
      if (position === undefined) {
        positions.set(id, items.length)
        items.push(item)
      } else {
        items[position] = item
      }
    }
    return settleAssistantItems({ id: turn.turnId, status: turn.status, error: turn.error, startedAt: turn.startedAt, finishedAt: turn.finishedAt, items })
  })
}

/** 被杀死进程持久化的 request start 不能证明当前仍存活。 */
function settleRequests(turn: ExecutionTurn, runtime: SessionRuntime): ExecutionTurn {
  let changed = false
  const items = turn.items.map(item => {
    if (item.kind !== 'request' || item.observation.status !== 'started') return item
    const compaction = runtime.activeCompaction
    const belongsToTurn = turn.id === runtime.activeTurn?.turnId
    const belongsToCompaction = item.observation.purpose === 'compaction'
      && compaction !== null && item.startedAt !== null
      && Date.parse(item.startedAt) >= Date.parse(compaction.startedAt)
    const live = runtime.phase !== 'idle' && (belongsToTurn || belongsToCompaction)
    const status: FactStatus = live ? 'running' : 'cancelled'
    if (item.status === status) return item
    changed = true
    return { ...item, status }
  })
  return settleAssistantItems(changed ? { ...turn, items } : turn)
}

/**
 * 把一个 session page 读成事实。新的尾部只在与其重叠时
 * 保留已加载的前缀；无重叠时该 page 自带 cursor，缺口因此可见。
 */
export function readExecution(source: SessionReadResult, previous: SessionView | null = null): SessionView {
  const first = source.history.turns[0]
  const overlap = previous === null || first === undefined ? -1 : previous.facts.history.findIndex(turn => turn.id === first.turnId)
  const prefix = previous !== null && overlap >= 0 ? previous.facts.history.slice(0, overlap) : []
  const turns = [...prefix, ...pageTurns(source.history)]
  let facts: ExecutionFacts = { history: turns, active: [] }
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
  return {
    history: facts.history.map(turn => settleRequests(turn, runtime)),
    active: facts.active.map(turn => {
      if (turn.status !== null) return turn
      if (runtime.phase === 'idle' && runtime.terminal?.source === 'turn' && turn.id === facts.active.at(-1)?.id) return finishTurn(turn, runtime.terminal.status)
      return settleRequests(turn, runtime)
    }),
  }
}

/** 更早的 page 只转换一次并前插；已加载 turns 保持其身份。 */
export function prependExecutionHistory(session: SessionView, page: ThreadReadPage): SessionView {
  const turns = [...pageTurns(page), ...session.facts.history]
  return { ...session, nextCursor: page.nextCursor, facts: { ...session.facts,
    history: turns.map(turn => settleRequests(turn, session.runtime)) } }
}

function finishTurn(turn: ExecutionTurn, status: TurnStatus): ExecutionTurn {
  return settleAssistantItems({ ...turn, status, items: turn.items.map(item => item.kind !== 'unknown' && item.status === 'running'
    ? { ...item, status: status === 'interrupted' ? 'cancelled' : status === 'failed' ? 'error' : 'ok' } : item) })
}

/** delta 工作量以可见 items 为界，绝不取决于更早 delta 的数量。 */
export function acceptExecutionEvent(facts: ExecutionFacts, event: TurnEventEnvelope): ExecutionFacts {
  const id = eventTurnId(event)
  const index = facts.active.findIndex(turn => turn.id === id)
  let turn: ExecutionTurn = index < 0 ? { id, status: null, items: [] } : facts.active[index]
  switch (event.method) {
    case 'turn/started':
    case 'turn/controlChanged':
      break
    case 'turn/userMessage': turn = upsert(turn, { ...base(event.params.item.itemId), kind: 'user', text: event.params.text }); break
    case 'provider/attempt': {
      const observation = event.params.observation
      turn = upsert(turn, requestItem(observation, turn.items.find(item => item.id === observation.requestId), null))
      break
    }
    case 'item/started':
      if (!turn.items.some(item => item.id === event.params.item.itemId)) turn = upsert(turn, { id: event.params.item.itemId, kind: 'unknown' })
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
    // 开始事件建立工具定义；实时流和恢复快照都保留它，后续只更新输出。
    case 'tool/execution/start': {
      const p = event.params
      turn = upsert(turn, { ...toolCallItem(p.item.itemId, p.toolName, p.args),
        status: 'running', startedAt: p.startedAt })
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
      const tool = turn.items.find(item => item.id === p.item.itemId) as Extract<ExecutionItem, { kind: 'tool' }>
      turn = upsert(turn, { ...tool, status: p.isError ? 'error' : 'ok',
        output: p.output, diff: p.isError ? undefined : p.diff, duration: p.durationMs,
        readSource: p.readSource })
      break
    }
    case 'item/discarded':
      turn = { ...turn, items: turn.items.filter(item => item.id !== event.params.item.itemId) }
      break
    case 'item/completed':
    case 'item/failed': {
      if (event.params.content) {
        const content = event.params.content
        const id = content.type === 'request' ? content.observation.requestId : content.id
        turn = upsert(turn, historyItemToExecution(content, turn.items.find(item => item.id === id), lastRequest(turn)))
      }
      const previous = turn.items.find(item => item.id === event.params.item.itemId)
      if (!previous || previous.kind === 'unknown' || previous.kind === 'tool') break
      turn = upsert(turn, { ...previous,
        status: event.method === 'item/failed' ? 'error' : 'ok',
        error: event.method === 'item/failed' ? event.params.error : undefined })
      break
    }
    case 'agent/diagnostic': turn = upsert(turn, { ...base(`event-${event.sessionRevision}`, event.params.severity === 'error' ? 'error' : 'stable'), kind: 'event', text: event.params.message }); break
    case 'turn/error':
      // 失败细节是类型化事实：只有关联的 turn 保存它，不编码成说明文本。
      turn = { ...finishTurn(turn, 'failed'), error: event.params.error }
      break
    case 'turn/completed': {
      const status = event.params.turn.status
      turn = finishTurn(turn, status)
      break
    }
    default: {
      event satisfies never
    }
  }
  const active = [...facts.active]
  if (index < 0) active.push(turn)
  else active[index] = turn
  return { ...facts, active }
}
