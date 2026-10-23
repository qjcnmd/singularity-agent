import type { HistoryItem, SessionReadResult, SessionRuntime, ThreadSummary, ThreadTurn, TurnErrorDetail, TurnEventEnvelope, TurnStatus } from './protocol'

export interface SessionView {
  summary: ThreadSummary
  runtime: SessionRuntime
  facts: ThreadTurn[]
  context: SessionReadResult['context']
}
export function hasErrorItem(turn: ThreadTurn, error: TurnErrorDetail | null | undefined): boolean {
  return error?.itemId !== undefined && turn.items.some(item => item.id === error.itemId && 'status' in item && item.status === 'failed')
}
function upsert(turn: ThreadTurn, item: HistoryItem): ThreadTurn {
  const index = turn.items.findIndex(previous => previous.id === item.id)
  const items = [...turn.items]
  if (index < 0) items.push(item)
  else items[index] = item
  return { ...turn, items }
}
export function readExecution(source: SessionReadResult): SessionView {
  return { summary: source.history.summary, runtime: source.runtime, facts: source.history.turns, context: source.context }
}
export function finishTurn(turn: ThreadTurn, status: TurnStatus, manuallyStopped = false): ThreadTurn {
  return { ...turn, status, manuallyStopped, items: turn.items.map(item => 'status' in item && (item.status === 'running' || item.status === 'pending') ? { ...item, status } : item) }
}
function replaceMessage(turn: ThreadTurn, messageId: string, completed: HistoryItem[] = []): ThreadTurn {
  const belongsToMessage = (item: HistoryItem) => item.id.startsWith(`${messageId}:`)
  const position = turn.items.findIndex(belongsToMessage)
  const items = turn.items.filter(item => !belongsToMessage(item))
  items.splice(position < 0 ? items.length : position, 0, ...completed)
  return { ...turn, items }
}
/** 历史读取与流式更新直接操作协议条目，不另建一套执行对象。 */
export function acceptExecutionEvent(facts: ThreadTurn[], event: TurnEventEnvelope): ThreadTurn[] {
  if (event.method === 'context/loaded') return facts
  const id = event.turnId ?? facts.at(-1)?.turnId
  // 尚无回合时，独立压缩没有可显示的历史。
  if (id === undefined) return facts
  const index = facts.findIndex(turn => turn.turnId === id)
  if (event.method === 'turn/started' && index < 0) {
    return [...facts, { turnId: id, status: 'running', startedAt: event.params.startedAt, manuallyStopped: false, items: [] }]
  }
  let turn = facts[index]
  switch (event.method) {
    case 'turn/started': turn = { ...turn, status: 'running', startedAt: event.params.startedAt }; break
    case 'item/updated': turn = upsert(turn, event.params.item); break
    case 'item/textDelta': {
      const { itemId: id, thinking, delta } = event.params
      const previous = turn.items.find(item => item.id === id)
      const text = (previous && 'text' in previous ? previous.text : '') + delta
      turn = upsert(turn, thinking ? { type: 'thinking', id, status: 'running', text }
        : { type: 'message', id, status: 'running', role: 'assistant', text })
      break
    }
    case 'message/discarded': turn = replaceMessage(turn, event.params.messageId); break
    case 'message/finished':
      turn = replaceMessage(turn, event.params.messageId, event.params.items)
      break
    case 'tool/output':
    case 'tool/finished': {
      const tool = turn.items.find(item => item.id === event.params.itemId)
      if (tool?.type !== 'tool') break
      const result = event.method === 'tool/finished' ? event.params.result : { output: event.params.output, isError: false }
      turn = upsert(turn, { ...tool, result, status: event.method === 'tool/output' ? 'running' : result.isError ? 'failed' : 'completed' })
      break
    }
    case 'turn/error': turn = { ...finishTurn(turn, 'failed'), error: event.params.error, finishedAt: event.params.finishedAt }; break
    case 'turn/completed': turn = { ...finishTurn(turn, event.params.turn.status, event.params.manuallyStopped), finishedAt: event.params.finishedAt }; break
    default: event satisfies never
  }
  const next = [...facts]
  next[index] = turn
  return next
}
