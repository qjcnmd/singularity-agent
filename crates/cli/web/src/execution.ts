import { eventTurnId } from './protocol'
import type { HistoryItem, ReadSource, RequestObservation, SessionReadResult, SessionRuntime as WireSessionRuntime, ThreadReadPage, ThreadSummary, TurnErrorDetail, TurnEventEnvelope, TurnStatus } from './protocol'

export type FactStatus = 'stable' | 'running' | 'ok' | 'error' | 'cancelled'
interface FactBase { id: string; status: FactStatus; startedAt: string | null; error?: string }
export type ExecutionItem = FactBase & (
  | { kind: 'user' | 'assistant' | 'thinking'; text: string; requestId?: string }
  | { kind: 'tool'; name: string; args: unknown; output: string; diff?: string; duration?: number; readSource?: ReadSource }
  | { kind: 'request'; observation: RequestObservation }
  | { kind: 'settings'; provider: string; model: string; reasoning: string | null }
  | { kind: 'compaction' | 'event' | 'unknown'; text: string }
)
/** `null` 保留 wire 的含义：记录被归组到首次真实运行之前。 */
export interface ExecutionTurn { id: string | null; status: TurnStatus | null; error?: TurnErrorDetail; items: ExecutionItem[] }
type Measurement = { provider: string; model: string; inputTokens: number } | undefined
export interface ExecutionFacts { history: ExecutionTurn[]; active: ExecutionTurn[]; latest: Measurement }
export type SessionRuntime = WireSessionRuntime
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
    startedAt: startedAt ?? previous?.startedAt ?? null }
}

/** 工具调用条目：名称与参数归调用所有，结果随后按同一 id 就地替换。 */
function toolCallItem(id: string, name: string, args: unknown): ExecutionItem {
  return { ...base(id), kind: 'tool', name, args, output: '' }
}

/** 工具结果条目：名称与参数沿用已配对的调用，失败标志与 diff 由结果决定。 */
function toolResultItem(item: Extract<HistoryItem, { type: 'tool_result' }>, previous: ExecutionItem | undefined): ExecutionItem {
  return { ...base(item.id, item.isError ? 'error' : 'ok'), kind: 'tool',
    name: previous?.kind === 'tool' ? previous.name : '工具输出', args: previous?.kind === 'tool' ? previous.args : {},
    output: item.output, diff: item.isError ? undefined : item.diff, duration: item.durationMs,
    readSource: item.readSource }
}

function request(turn: ExecutionTurn, observation: RequestObservation, startedAt: string | null): ExecutionTurn {
  return upsert(turn, requestItem(observation, turn.items.find(item => item.id === observation.requestId), startedAt))
}

function historyItem(turn: ExecutionTurn, item: HistoryItem): ExecutionTurn {
  switch (item.type) {
    case 'request': return request(turn, item.observation, item.startedAt ?? null)
    case 'message': return upsert(turn, { ...base(item.id), kind: item.role === 'user' ? 'user' : 'assistant', text: item.text,
      requestId: item.role === 'assistant' ? lastRequest(turn) : undefined })
    case 'thinking': return upsert(turn, { ...base(item.id), kind: 'thinking', text: item.text, requestId: lastRequest(turn) })
    case 'tool_call': return upsert(turn, toolCallItem(item.id, item.name, item.args))
    case 'tool_result': return upsert(turn, toolResultItem(item, turn.items.find(value => value.id === item.id)))
    case 'compaction': return upsert(turn, { ...base(item.id), kind: 'compaction', text: item.summary })
    case 'settings': return upsert(turn, { ...base(item.id), kind: 'settings', provider: item.provider, model: item.model, reasoning: item.reasoning })
  }
}

function measure(latest: Measurement, observation: RequestObservation): Measurement {
  if (observation.purpose === 'compaction') return observation.status === 'ok' ? undefined : latest
  return observation.inputTokens == null ? latest : { provider: observation.provider, model: observation.model, inputTokens: observation.inputTokens }
}

/** input 测量由事实本身派生；raw items 之上不存在第二个来源。 */
function measureTurns(turns: ExecutionTurn[]): Measurement {
  let latest: Measurement
  for (const turn of turns) for (const item of turn.items) {
    if (item.kind === 'request') latest = measure(latest, item.observation)
    else if (item.kind === 'compaction') latest = undefined
    else if (item.kind === 'settings' && latest && (item.provider !== latest.provider || item.model !== latest.model)) latest = undefined
  }
  return latest
}

/**
 * 唯一的转换边界：一个 wire page 一次局部构建成 execution turns。
 * id→位置与当前 request 关联只存在于这次构建内：同 id 的条目就地替换
 * （request 的 start/end 合并、tool call/result 配对），逐项不再复制整段
 * items；结束时发布一次 ExecutionTurn，并在此接上助手终态归约（S04/S12）
 * 与请求开始时间（S11）。
 */
function pageTurns(page: ThreadReadPage): ExecutionTurn[] {
  return page.turns.map(turn => {
    const items: ExecutionItem[] = []
    const positions = new Map<string, number>()
    const place = (item: ExecutionItem) => {
      const position = positions.get(item.id)
      if (position === undefined) positions.set(item.id, items.push(item) - 1)
      else items[position] = item
    }
    let currentRequest: string | undefined
    for (const wire of turn.items) {
      switch (wire.type) {
        case 'request':
          currentRequest = wire.observation.requestId
          place(requestItem(wire.observation, items[positions.get(currentRequest) ?? -1], wire.startedAt ?? null))
          break
        case 'message':
          place({ ...base(wire.id), kind: wire.role === 'user' ? 'user' : 'assistant', text: wire.text,
            requestId: wire.role === 'assistant' ? currentRequest : undefined })
          break
        case 'thinking': place({ ...base(wire.id), kind: 'thinking', text: wire.text, requestId: currentRequest }); break
        case 'tool_call': place(toolCallItem(wire.id, wire.name, wire.args)); break
        case 'tool_result': place(toolResultItem(wire, items[positions.get(wire.id) ?? -1])); break
        case 'compaction': place({ ...base(wire.id), kind: 'compaction', text: wire.summary }); break
        case 'settings': place({ ...base(wire.id), kind: 'settings', provider: wire.provider, model: wire.model, reasoning: wire.reasoning }); break
      }
    }
    return settleAssistantItems({ id: turn.turnId, status: turn.status, error: turn.error, items })
  })
}

/** 被杀死进程持久化的 request start 不能证明当前仍存活。 */
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
    return changed ? settleAssistantItems({ ...turn, items }) : settleAssistantItems(turn)
  })
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

/** 更早的 page 只转换一次并前插；已加载 turns 保持其身份。 */
export function prependExecutionHistory(session: SessionView, page: ThreadReadPage): SessionView {
  const turns = [...pageTurns(page), ...session.facts.history]
  return { ...session, nextCursor: page.nextCursor, facts: { ...session.facts,
    history: settleRequests(turns, session.runtime), latest: session.facts.active.length ? session.facts.latest : measureTurns(turns) } }
}

function finishTurn(turn: ExecutionTurn, status: TurnStatus): ExecutionTurn {
  return settleAssistantItems({ ...turn, status, items: turn.items.map(item => item.status === 'running'
    ? { ...item, status: status === 'interrupted' ? 'cancelled' : status === 'failed' ? 'error' : 'ok' } : item) })
}

/** delta 工作量以可见 items 为界，绝不取决于更早 delta 的数量。 */
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
        duration: event.method === 'tool/execution/end' ? event.params.durationMs : undefined,
        // 真实读取范围只由 end 携带；start/update 保留已建立的值。
        readSource: event.method === 'tool/execution/end' ? event.params.readSource : tool?.readSource })
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
      // 失败细节是类型化事实：只有关联的 turn 保存它，不编码成说明文本。
      turn = { ...finishTurn(turn, 'failed'), error: event.params.error }
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
