import { diffContext } from '../src/diffView'
import { createPatch } from 'diff'
import { readExecution, acceptExecutionEvent } from '../src/execution'
import type { TurnEventEnvelope } from '../src/protocol'
const appendEvent = (events: TurnEventEnvelope[], event: TurnEventEnvelope) => [...events, event]
import assert from 'node:assert/strict'
import { test } from 'node:test'
import { buildTimeline as projectTimeline, timelineBody, timelineStatus } from '../src/timeline'
import type { TimelineItemModel } from '../src/timeline'
import { buildTrajectory as projectTrajectory } from '../src/trajectory'
import { contextOccupancy as projectOccupancy } from '../src/contextUsage'
import { reasoningChoices } from '../src/modelChoices'
import { inputTrigger } from '../src/inputTrigger'
import { session as wireSession, runtime, control, bootstrap, model, event, observation as makeObservation, startedAt, requestSnapshot } from './fixtures'
import type { SessionReadResult, HistoryItem, RequestObservation } from '../src/protocol'
import type { ExecutionItem } from '../src/execution'
const session = (): SessionReadResult => wireSession()
const buildTimeline = (value: SessionReadResult) => projectTimeline(readExecution(value))
const buildTrajectory = (value: SessionReadResult) => projectTrajectory(readExecution(value))
const contextOccupancy = (value: SessionReadResult, catalog: Parameters<typeof projectOccupancy>[1]) => projectOccupancy(readExecution(value), catalog)
/** 工具运行事实只由顶层 fact 持有；测试经同一路径读取并断言其类型。 */
function toolFact(item: TimelineItemModel): Extract<ExecutionItem, { kind: 'tool' }> {
  if (item.fact?.kind !== 'tool') throw new Error('expected a tool fact')
  return item.fact
}

test('request start time survives the terminal observation and stays unknown without a start record', () => {
  const observed = (overrides: Partial<RequestObservation> = {}) =>
    makeObservation({ requestId: 'r', ordinal: 1, attempt: 1, provider: 'p', model: 'm', ...overrides })
  const requestOf = (value: SessionReadResult, where: 'active' | 'history') =>
    readExecution(value).facts[where].flatMap(turn => turn.items).find(item => item.kind === 'request')!

  const live = session()
  live.activeEvents = [
    event({ method: 'provider/attempt', params: { observation: observed({ status: 'started' }) } }),
    event({ method: 'provider/attempt', params: { observation: observed({ status: 'ok', durationMs: 120 }) } }),
  ]
  // 实时观测不带开始时间：保持未知，也不因终态清空或按到达顺序推算。
  assert.equal(requestOf(live, 'active').startedAt, null)

  const restored = session()
  restored.runtime = { ...restored.runtime, activeTurn: null }
  restored.activeEvents = []
  restored.history.turns = [{ turnId: 't', status: 'completed', items: [
    { type: 'request', startedAt: '2026-09-05T00:00:01Z', observation: observed({ status: 'ok', durationMs: 120 }) },
  ] }]
  const fact = requestOf(restored, 'history')
  assert.equal(fact.startedAt, '2026-09-05T00:00:01Z', 'the terminal observation does not clear the known start')
  assert.equal(buildTrajectory(restored)[0].entries.find(item => item.request?.requestId === 'r')!.startedAt, '2026-09-05T00:00:01Z')

  // 没有开始记录的旧日志保持未知：结束记录时间不冒充开始时间。
  restored.history.turns = [{ turnId: 't', status: 'completed', items: [
    { type: 'request', observation: observed({ status: 'ok', durationMs: 120 }) },
  ] }]
  assert.equal(requestOf(restored, 'history').startedAt, null)
})

test('a failed or cancelled request settles the assistant fragments it produced', () => {
  const fragments = (requestId: string, status: 'ok' | 'error' | 'cancelled') => [
    { type: 'request' as const, startedAt, observation: makeObservation({ requestId, ordinal: 1, attempt: 1, status, durationMs: 10 }) },
    { type: 'thinking' as const, id: `${requestId}-thinking`, text: '考虑中' },
    { type: 'message' as const, id: `${requestId}-answer`, role: 'assistant' as const, text: '部分输出' },
  ]
  const historyStatuses = (status: 'ok' | 'error' | 'cancelled') => {
    const value = session()
    value.runtime = { ...value.runtime, activeTurn: null }
    value.activeEvents = []
    value.history.turns = [{ turnId: 't', status: status === 'ok' ? 'completed' : status === 'error' ? 'failed' : 'interrupted', items: fragments('r', status) }]
    return readExecution(value).facts.history[0].items
      .filter(item => item.kind === 'assistant' || item.kind === 'thinking').map(item => item.status)
  }
  assert.deepEqual(historyStatuses('error'), ['error', 'error'], '网络失败的部分输出读回后仍是失败')
  assert.deepEqual(historyStatuses('cancelled'), ['cancelled', 'cancelled'], '正常取消不是执行失败')
  assert.deepEqual(historyStatuses('ok'), ['stable', 'stable'], '成功请求的片段保持普通历史状态')

  // 实时路径用同一规则结算：取消观测之后的 item/failed 不能把它降格成失败。
  const live = session()
  live.activeEvents = [
    event({ method: 'provider/attempt', params: { observation: makeObservation({ requestId: 'r', status: 'started' }) } }),
    event({ method: 'item/agentThinking/delta', params: { turnId: 't', item: { itemId: 'think' }, delta: '考虑' } }),
    event({ method: 'item/agentMessage/delta', params: { turnId: 't', item: { itemId: 'answer' }, delta: '部分' } }),
    event({ method: 'provider/attempt', params: { observation: makeObservation({ requestId: 'r', status: 'cancelled' }) } }),
    event({ method: 'item/failed', params: { turnId: 't', item: { itemId: 'answer' }, error: 'assistant response failed' } }),
    event({ method: 'turn/completed', params: { turn: { turnId: 't', status: 'interrupted' } } }),
  ]
  assert.deepEqual(readExecution(live).facts.active[0].items
    .filter(item => item.kind === 'assistant' || item.kind === 'thinking').map(item => item.status),
    ['cancelled', 'cancelled'], '实时与历史给出同一个取消终态')

  // 有输出后网络失败：实时与 reload 后都是失败，且 stage/cause 仍可访问。
  const failedLive = session()
  failedLive.activeEvents = [
    event({ method: 'provider/attempt', params: { observation: makeObservation({ requestId: 'r', status: 'started' }) } }),
    event({ method: 'item/agentMessage/delta', params: { turnId: 't', item: { itemId: 'answer' }, delta: '部分' } }),
    event({ method: 'provider/attempt', params: { observation: makeObservation({ requestId: 'r', status: 'error', error: 'connection reset' }) } }),
    event({ method: 'item/failed', params: { turnId: 't', item: { itemId: 'answer' }, error: 'assistant response failed' } }),
    event({ method: 'turn/error', params: { turnId: 't', error: { stage: 'agent_loop', cause: 'provider_network', message: 'connection reset' } } }),
  ]
  const failedLiveTurn = readExecution(failedLive).facts.active[0]
  assert.deepEqual(failedLiveTurn.items
    .filter(item => item.kind === 'assistant' || item.kind === 'thinking').map(item => item.status),
    ['error'], '实时网络失败的片段是失败')
  assert.equal(failedLiveTurn.error?.cause, 'provider_network', 'stage/cause 直接可读，不编码成说明文本')

  // 只有关联到失败请求的片段被涂色；早前 attempt 的错误不串联到后来的输出。
  const attempts = session()
  attempts.runtime = { ...attempts.runtime, activeTurn: null }
  attempts.activeEvents = []
  attempts.history.turns = [{ turnId: 't', status: 'completed', items: [
    ...fragments('attempt-1', 'error'),
    ...fragments('attempt-2', 'ok'),
  ] }]
  assert.deepEqual(readExecution(attempts).facts.history[0].items
    .filter(item => item.kind === 'assistant' || item.kind === 'thinking').map(item => item.status),
    ['error', 'error', 'stable', 'stable'], '第二次 attempt 的输出不被上一次的错误染色')

  // 工具自身的失败独立于请求：请求成功也不改写工具结果的状态。
  const tool = session()
  tool.runtime = { ...tool.runtime, activeTurn: null }
  tool.activeEvents = []
  tool.history.turns = [{ turnId: 't', status: 'completed', items: [
    { type: 'request', startedAt, observation: makeObservation({ requestId: 'r', ordinal: 1, attempt: 1, status: 'ok', durationMs: 5 }) },
    { type: 'tool_call', id: 'call', name: 'bash', args: { command: 'false' } },
    { type: 'tool_result', id: 'call', output: 'exit 1', isError: true },
  ] }]
  assert.equal(readExecution(tool).facts.history[0].items.find(item => item.id === 'call')!.status, 'error',
    '工具失败不被请求终态覆盖')
})

test('a large history page builds each turn once instead of copying its prefix per item', () => {
  const value = session()
  value.runtime = { ...value.runtime, activeTurn: null }
  value.activeEvents = []
  const items: HistoryItem[] = Array.from({ length: 40_000 }, (_, index) =>
    ({ type: 'tool_call', id: `tool-${index}`, name: 'read', args: { path: `file-${index}` } }))
  value.history.turns = [{ turnId: 't', status: 'completed', items }]

  const started = performance.now()
  const turns = readExecution(value).facts.history
  const elapsed = performance.now() - started
  assert.equal(turns[0].items.length, items.length)
  // 逐项 findIndex + 复制整段 items 是二次量级：同样 20000 项实测约 1.6s，40000 项
  // 约 6s；线性构建在 40000 项上只有几十毫秒。余量足够大，只用来发现复杂度回归。
  assert.ok(elapsed < 1500, `building ${items.length} items took ${Math.round(elapsed)}ms`)
})

test('the read source range reaches the tool fact from live events and history', () => {
  const readSource = { startLine: 1, lineCount: 2 }
  const output = 'a\nb\n\n[Showing lines 1-2. File continues; use offset=3 to continue.]'
  const toolOf = (value: SessionReadResult, where: 'active' | 'history') => {
    const item = readExecution(value).facts[where].flatMap(turn => turn.items).find(entry => entry.id === 'r')!
    assert.equal(item.kind, 'tool')
    return item
  }

  const live = session()
  live.activeEvents = [
    event({ method: 'tool/execution/start', params: { turnId: 't', item: { itemId: 'r' }, toolName: 'read', args: { path: 'a.txt', offset: 0 } } }),
    event({ method: 'tool/execution/end', params: { turnId: 't', item: { itemId: 'r' }, output, isError: false, readSource } }),
  ]
  assert.deepEqual(toolOf(live, 'active').readSource, readSource)

  const restored = session()
  restored.runtime = { ...restored.runtime, activeTurn: null }
  restored.activeEvents = []
  restored.history.turns = [{ turnId: 't', status: 'completed', items: [
    { type: 'tool_call', id: 'r', name: 'read', args: { path: 'a.txt', offset: 0 } },
    { type: 'tool_result', id: 'r', output, isError: false, readSource },
  ] }]
  assert.deepEqual(toolOf(restored, 'history').readSource, readSource)

  // 旧记录与其它工具没有这份数据：展示层不猜范围。
  restored.history.turns = [{ turnId: 't', status: 'completed', items: [
    { type: 'tool_call', id: 'r', name: 'read', args: { path: 'a.txt', offset: 0 } },
    { type: 'tool_result', id: 'r', output, isError: false },
  ] }]
  assert.equal(toolOf(restored, 'history').readSource, undefined)
  assert.equal(toolOf(restored, 'history').args !== undefined, true)
})

test('completed content restores without deltas and each queued turn keeps its own outcome', () => {
  for (const status of ['completed', 'interrupted'] as const) {
    const source = session()
    source.activeEvents = [
      event({ method: 'turn/started', params: { turn: { turnId: 'first' } } }),
      event({ method: 'item/completed', params: { turnId: 'first', item: { itemId: 'm1:text:0' }, content: { type: 'message', id: 'm1:text:0', role: 'assistant', text: '完整响应' } } }),
      event({ method: 'turn/error', params: { turnId: 'first' } }),
      event({ method: 'turn/started', params: { turn: { turnId: 'second' } } }),
      event({ method: 'item/agentMessage/delta', params: { turnId: 'second', item: { itemId: 'm2:text:0' }, delta: '部分' } }),
      event({ method: 'item/completed', params: { turnId: 'second', item: { itemId: 'm2:text:0' }, content: { type: 'message', id: 'm2:text:0', role: 'assistant', text: '最终响应' } } }),
      event({ method: 'turn/completed', params: { turn: { turnId: 'second', status } } }),
    ]
    source.runtime = { ...source.runtime, phase: 'idle', activeTurn: null, terminal: { status, message: null } }
    const view = readExecution(source)
    assert.deepEqual(view.facts.active.map(turn => turn.status), ['failed', status])
    const timeline = projectTimeline(view)
    assert.deepEqual(timeline.filter(item => item.kind === 'assistant').map(timelineBody), ['完整响应', '最终响应'])
    assert.deepEqual(timeline.filter(item => item.kind === 'terminal').map(item => item.key), status === 'interrupted' ? ['content:second:terminal'] : [])
  }
})

test('caret triggers preserve command boundaries and ignore paths or URLs', () => {
  assert.deepEqual(inputTrigger('/', 1), { kind: 'skill', start: 0, end: 1, query: '' })
  assert.deepEqual(inputTrigger('/rev trailing', 4), { kind: 'skill', start: 0, end: 4, query: 'rev' })
  assert.equal(inputTrigger('https://example', 15), null)
  assert.equal(inputTrigger('src/file', 8), null)
  assert.equal(inputTrigger('//comment', 9), null)
  assert.deepEqual(inputTrigger('read @src', 9), { kind: 'file', start: 5, end: 9, query: 'src' })
  assert.deepEqual(inputTrigger('@src/lib.rs', 11), { kind: 'file', start: 0, end: 11, query: 'src/lib.rs' })
  assert.deepEqual(inputTrigger('(/review', 8), { kind: 'skill', start: 1, end: 8, query: 'review' })
  // 相对路径与家目录路径是路径，不是技能触发；`@` 引用不受影响。
  assert.equal(inputTrigger('./read', 6), null)
  assert.equal(inputTrigger('../read', 7), null)
  assert.equal(inputTrigger('~/read', 6), null)
  assert.equal(inputTrigger('C:/Users', 8), null)
  assert.deepEqual(inputTrigger('see ./x and /rev', 16), { kind: 'skill', start: 12, end: 16, query: 'rev' })
  assert.deepEqual(inputTrigger('read @./src', 11), { kind: 'file', start: 5, end: 11, query: './src' })
})

test('long tool progress is bounded and incremental projections match refreshed snapshots', () => {
  const args = { command: 'build' }
  const start = event({ method: 'tool/execution/start', params: { turnId: 't', item: { itemId: 'tool' }, toolName: 'bash', args } })
  let value = readExecution(session())
  value = { ...value, facts: acceptExecutionEvent(value.facts, start) }
  const original = value
  let latest = start as TurnEventEnvelope
  for (let index = 0; index < 10000; index++) {
    latest = event({ method: 'tool/execution/update', params: { turnId: 't', item: { itemId: 'tool' }, partialResult: `output ${index}: ${'x'.repeat(4096)}` } })
    value = { ...value, facts: acceptExecutionEvent(value.facts, latest) }
    projectTimeline(value)
    projectTrajectory(value)
  }
  assert.equal(value.facts.active[0].items.length, 1)
  assert.equal(toolFact(projectTimeline(original)[0]).output, '')
  const fresh = session()
  fresh.activeEvents = [start, latest]
  assert.deepEqual(buildTimeline(fresh), projectTimeline(value))
  assert.deepEqual(buildTrajectory(fresh), projectTrajectory(value))
  value = { ...value, facts: acceptExecutionEvent(value.facts, event({ method: 'tool/execution/end', params: { turnId: 't', item: { itemId: 'tool' }, output: 'complete', isError: false } })) }
  assert.equal(value.facts.active[0].items.length, 1)
  assert.equal(toolFact(projectTimeline(value)[0]).output, 'complete')
})

test('a streaming delta never replaces the identity of unchanged timeline items', () => {
  // TimelineItem 是 memo 组件：它跳过重渲染的前提是投影给未变化的事实复用同一对象。
  const source = session()
  source.activeEvents = [
    event({ method: 'turn/started', params: { turn: { turnId: 't' } } }),
    event({ method: 'item/completed', params: { turnId: 't', item: { itemId: 'm1:text:0' }, content: { type: 'message', id: 'm1:text:0', role: 'assistant', text: '稳定历史' } } }),
    event({ method: 'tool/execution/start', params: { turnId: 't', item: { itemId: 'tool' }, toolName: 'bash', args: { command: 'build' } } }),
  ]
  let value = readExecution(source)
  const before = projectTimeline(value)
  assert.deepEqual(before.map(item => item.kind), ['assistant', 'tool'])

  const update = event({ method: 'tool/execution/update', params: { turnId: 't', item: { itemId: 'tool' }, partialResult: 'output 1' } })
  value = { ...value, facts: acceptExecutionEvent(value.facts, update) }
  const after = projectTimeline(value)
  assert.equal(after[0], before[0], '未变化的历史项复用同一对象，memo 才能跳过它的重渲染')
  assert.notEqual(after[1], before[1], '发生变化的流式项是新对象，必须重新渲染')
  assert.equal(after[1].key, before[1].key)
  assert.equal(toolFact(after[1]).output, 'output 1')
})

test('reasoning slider orders configured levels and retains thinking-off choices', () => {
  const variants = [
    { id: 'high', enabled: true, wireEffort: null }, { id: 'low', enabled: true, wireEffort: null },
    { id: 'off', enabled: false, wireEffort: null }, { id: 'medium', enabled: true, wireEffort: null },
  ]
  assert.deepEqual(reasoningChoices(model({ reasoningVariants: variants })).map(value => value.id), ['off', 'low', 'medium', 'high'])
  assert.deepEqual(variants.map(value => value.id), ['high', 'low', 'off', 'medium'])
  assert.equal(reasoningChoices(model({ reasoningVariants: [{ id: 'high', enabled: true, wireEffort: null }] })).length, 1)
  assert.deepEqual(reasoningChoices(undefined), [])
})

test('trajectory preserves request statistics and coalesces tool result without polluting chat', () => {
  const value = session()
  value.runtime.activeTurn = null
  value.activeEvents = []
  value.history.turns = [{ turnId: 't', status: 'completed', items: [
    { type: 'message', id: 'u', role: 'user', text: 'inspect' },
    { type: 'request', startedAt: 'now', observation: makeObservation({ ordinal: 1, attempt: 1, provider: 'p', model: 'm', status: 'ok', durationMs: 350, inputTokens: null, outputTokens: 12, cachedInputTokens: null, error: null }) },
    { type: 'tool_call', id: 'c', name: 'read', args: { path: 'a' } },
    { type: 'tool_result', id: 'c', output: 'missing', isError: true },
  ] }]
  const trajectory = buildTrajectory(value)[0].entries
  assert.equal(trajectory.length, 3)
  assert.equal(trajectory[1].duration, 350)
  assert.equal(trajectory[1].request!.inputTokens, null)
  assert.equal(trajectory[1].request!.outputTokens, 12)
  assert.equal(trajectory[2].status, 'error')
  assert.match(trajectory[2].text, /missing/)
  assert.deepEqual(buildTimeline(value).map(item => item.kind), ['user', 'tool'])
})

test('the current stop shows even when an earlier turn already stopped', () => {
  const value = session()
  value.activeEvents = []
  value.runtime = { ...value.runtime, phase: 'idle', activeTurn: null, terminal: { status: 'interrupted', message: null } }
  value.history.turns = [
    { turnId: 'first', status: 'interrupted', items: [{ type: 'message', id: 'm1', role: 'user', text: '被停止的问题' }] },
    { turnId: 'second', status: 'completed', items: [{ type: 'message', id: 'm2', role: 'assistant', text: '后来成功的回复' }] },
  ]

  const stopped = buildTimeline(value)
  assert.deepEqual(stopped.map(item => item.kind), ['user', 'terminal', 'assistant', 'terminal'],
    '旧的停止保留在原位，本次停止出现在当前尾部')
  assert.equal(stopped.at(-1)!.key, 'terminal:interrupted')

  // 普通失败不是这次停止的会话级提示。
  value.runtime = { ...value.runtime, terminal: { status: 'failed', message: 'boom' } }
  assert.equal(buildTimeline(value).at(-1)!.kind, 'assistant')

  // 最新一轮自己就是被停止的那一轮：尾部已表达停止，不再重复一条。
  value.runtime = { ...value.runtime, terminal: { status: 'interrupted', message: null } }
  value.history.turns = [value.history.turns[0]]
  const single = buildTimeline(value)
  assert.deepEqual(single.map(item => item.kind), ['user', 'terminal'])
})

test('individual tools preserve order and failure across history recovery', () => {
  const value = session()
  const events = [...value.activeEvents]
  const attempt = (ordinal: number) => event({ method: 'provider/attempt', params: { observation: makeObservation({ ordinal, attempt: 1, status: 'started' }) } })
  const start = (id: string) => (event({ method: 'tool/execution/start', params: { turnId: 't', item: { itemId: id }, toolName: 'read', args: { path: id } } }))
  const end = (id: string) => (event({ method: 'tool/execution/end', params: { turnId: 't', item: { itemId: id }, output: id, isError: id === 'b' } }))
  events.push(attempt(1), start('a'), end('a'))
  value.activeEvents = events
  const first = buildTimeline(value)[0]
  assert.equal(timelineStatus(first), 'ok')
  assert.deepEqual(toolFact(first).args, { path: 'a' })
  assert.equal(toolFact(first).output, 'a')
  value.activeEvents = [...events, start('b'), end('b'), attempt(2), start('c')]
  const live = buildTimeline(value)
  assert.deepEqual(live.map(tool => timelineBody(tool)), ['a', 'b', 'c'])
  assert.equal(timelineStatus(live[1]), 'error')
  assert.equal(timelineStatus(live[2]), 'running')
  const toolEntries = buildTrajectory(value).flatMap(turn => turn.entries).filter(item => item.kind === 'tool')
  assert.equal(toolEntries.length, 3)
  assert.deepEqual(toolEntries[0].input, { path: 'a' })
  assert.equal(toolEntries[0].text, 'a')
  value.activeEvents = [...value.activeEvents, event({ method: 'turn/completed', params: { turn: { turnId: 't', status: 'interrupted' } } })]
  assert.equal(buildTimeline(value).at(-1)!.kind, 'terminal')
  assert.equal(buildTrajectory(value).flatMap(turn => turn.entries).find(item => item.id === 'c')!.status, 'cancelled')
  value.runtime.activeTurn = null
  value.activeEvents = []
  value.history.turns = [{ turnId: 't', status: 'interrupted', items: [
    { startedAt, type: 'request', observation: makeObservation({ ordinal: 1, attempt: 1 }) },
    { type: 'tool_call', id: 'a', name: 'read', args: { path: 'a' } },
    { type: 'tool_call', id: 'b', name: 'read', args: { path: 'b' } },
    { type: 'tool_result', id: 'a', output: 'a', isError: false },
    { type: 'tool_result', id: 'b', output: 'b', isError: true },
    { startedAt, type: 'request', observation: makeObservation({ ordinal: 2, attempt: 1 }) },
    { type: 'tool_call', id: 'c', name: 'read', args: { path: 'c' } },
  ] }]
  const recovered = buildTimeline(value)
  assert.deepEqual(recovered.slice(0, 3).map(tool => tool.key), live.map(tool => tool.key))
  assert.equal(timelineStatus(recovered[1]), 'error')
})

test('request lookup and prompt head survive completion and history reload without full context', () => {
  const snapshot = requestSnapshot({
    messages: [{ role: 'system', content: 'system prompt' }],
    tools: [{ name: 'read', description: 'Read a file', parametersSchema: { type: 'object' } }], modelPreferences: {},
  })
  const value = session()
  value.activeEvents = [
    event({ method: 'provider/attempt', params: { observation: makeObservation({ requestId: 'lookup-request', ordinal: 1, attempt: 1, status: 'started', provider: 'p', model: 'm', requestHead: snapshot }) } }),
    event({ method: 'item/agentMessage/delta', params: { turnId: 't', item: { itemId: 'answer' }, delta: 'answer' } }),
    event({ method: 'provider/attempt', params: { observation: makeObservation({ requestId: 'lookup-request', ordinal: 1, attempt: 1, status: 'ok', provider: 'p', model: 'm', durationMs: 100 }) } }),
  ]
  const live = buildTrajectory(value)[0].entries
  assert.equal(live[0].kind, 'system')
  assert.deepEqual(live[1].request!.requestHead, snapshot)
  assert.equal(live[1].text, 'answer')
  assert.equal(live[1].request!.requestId, 'lookup-request')
  assert.equal(live[1].duration, 100)
  value.runtime.activeTurn = null
  value.activeEvents = []
  value.history.turns = [{ status: 'completed', turnId: 't', items: [
    { startedAt, type: 'request', observation: live[1].request! },
    { type: 'message', id: 'assistant', role: 'assistant', text: 'answer' },
    { type: 'tool_call', id: 'tool', name: 'read', args: {} },
    { type: 'tool_result', id: 'tool', output: 'file', isError: false, durationMs: 23 },
  ] }]
  const restored = buildTrajectory(value)[0].entries
  assert.equal(restored[1].id, live[1].id)
  assert.deepEqual(restored[1].request!.requestHead, snapshot)
  assert.deepEqual(restored[2].schema, snapshot.tools[0])
  assert.equal(restored[2].duration, 23)
})

test('unfinished historical requests follow runtime liveness without changing durable observations', () => {
  const value = session()
  const observation = makeObservation({ requestId: 'unfinished', ordinal: 1, attempt: 1, provider: 'p', model: 'm', status: 'started', durationMs: 0 })
  value.history.turns = [{ status: 'completed', turnId: 't', items: [{ type: 'request', startedAt: '2026-09-05T00:00:01Z', observation }] }]
  const request = () => buildTrajectory(value)[0].entries[0]
  assert.equal(request().status, 'running')
  assert.equal(request().duration, null)
  value.runtime.activeTurn = null
  value.activeEvents = []
  assert.equal(request().status, 'cancelled')
  assert.equal(observation.status, 'started')
  value.runtime.activeTurn = { startedAt, turnId: 'next' }
  assert.equal(request().status, 'cancelled')
  value.runtime.activeTurn = null
  value.activeEvents = []
  value.runtime.activeCompaction = { startedAt: '2026-09-05T00:00:00Z' }
  assert.equal(request().status, 'cancelled')
  value.history.turns = [{ status: 'completed', turnId: null, items: [{ type: 'request', startedAt: '2026-09-05T00:00:01Z', observation: { ...observation, purpose: 'compaction' } }] }]
  assert.equal(request().status, 'running')
  value.runtime.activeCompaction = { startedAt: '2026-09-05T00:00:02Z' }
  assert.equal(request().status, 'cancelled')
  value.runtime.activeCompaction = null
  assert.equal(request().status, 'cancelled')
})

test('incremental activity reuses stable historical trajectory objects', () => {
  const value = session()
  value.history.turns = [{ status: 'completed', turnId: 'history', items: [
    { type: 'message', id: 'answer', role: 'assistant', text: 'durable answer' },
  ] }]
  value.runtime.activeTurn = { startedAt, turnId: 'active' }
  const view = readExecution(value)
  const first = projectTrajectory(view)
  // 实时 delta 是真正的增量路径：已加载 history 不会被重读，只有 activity 变动。
  const streamed = { ...view, facts: acceptExecutionEvent(view.facts, event({
    method: 'item/agentMessage/delta',
    params: { turnId: 'active', item: { itemId: 'live' }, delta: 'streaming' },
  })) }
  const second = projectTrajectory(streamed)
  assert.strictEqual(second[0], first[0])
  assert.strictEqual(second[0].entries, first[0].entries)
  assert.strictEqual(second[0].entries[0], first[0].entries[0])
  assert.equal(second[1].entries[0].text, 'streaming')
})

test('settings stay structured and the leading group is shown as session settings', () => {
  const value = session()
  value.runtime.activeTurn = null
  value.activeEvents = []
  value.history.turns = [
    { turnId: null, status: null, items: [{ type: 'settings', id: 'set', provider: 'p', model: 'm', reasoning: 'high' }] },
    { turnId: 't', status: 'completed', items: [{ type: 'message', id: 'u', role: 'user', text: 'go' }] },
  ]
  const turns = buildTrajectory(value)
  assert.deepEqual(turns.map(turn => turn.title), ['会话设置', '第 1 轮'])
  assert.equal(turns[0].entries[0].text, 'p/m · high')
  // 事实保留 provider/model/reasoning；只有投影负责格式化它们。
  assert.deepEqual(readExecution(value).facts.history[0].items[0],
    { id: 'set', status: 'stable', startedAt: null, kind: 'settings', provider: 'p', model: 'm', reasoning: 'high' })
})

test('structured file changes share statistics and rendered hunks across live and recovered views', () => {
  const value = session()
  const diff = createPatch('file.txt', '--old\n', '++new\n')
  const output = 'Successfully edited file.txt'
  value.activeEvents = [
    event({ method: 'tool/execution/start', params: { turnId: 't', item: { itemId: 'edit' }, toolName: 'edit', args: { path: 'file.txt' } } }),
    event({ method: 'tool/execution/end', params: { turnId: 't', item: { itemId: 'edit' }, output, diff, isError: false } }),
  ]
  const live = buildTimeline(value)[0]
  assert.deepEqual([live.addedLines, live.removedLines], [1, 1])
  assert.deepEqual(live.tool!.patches[0].hunks[0].lines, ['---old', '+++new'])
  assert.equal(toolFact(live).output, output)
  const inspected = buildTrajectory(value)[0].entries[0].text
  assert.equal(inspected, `${output}\n\n${diff}`)
  value.runtime.activeTurn = null
  value.activeEvents = []
  value.history.turns = [{ turnId: 't', status: 'completed', items: [
    { type: 'tool_call', id: 'edit', name: 'edit', args: { path: 'file.txt' } },
    { type: 'tool_result', id: 'edit', output, diff, isError: false },
  ] }]
  const recovered = buildTimeline(value)[0]
  assert.deepEqual([recovered.addedLines, recovered.removedLines], [1, 1])
  assert.deepEqual(recovered.tool?.patches, live.tool?.patches)
  assert.deepEqual(toolFact(recovered).output, toolFact(live).output)
  assert.equal(buildTrajectory(value)[0].entries[0].text, inspected)
  value.history.turns = [{ ...value.history.turns[0], items: [value.history.turns[0].items[0],
    { type: 'tool_result', id: 'edit', output: `${output}\n\n${diff}`, isError: false },
  ] }]
  assert.equal(buildTimeline(value)[0].tool!.diff, '', 'ordinary output never impersonates structured file changes')
})

test('runtime failure does not add a conversation banner', () => {
  const value = session()
  value.runtime.activeTurn = null
  value.activeEvents = []
  value.runtime.terminal = { status: 'failed', message: 'Provider rejected the request' }
  value.history.turns = [{ turnId: 't', status: 'failed', items: [] }]
  const terminals = buildTimeline(value).filter(item => item.kind === 'terminal')
  assert.equal(terminals.length, 0)
  assert.equal(buildTrajectory(value).at(-1)!.entries.at(-1)!.text, 'Provider rejected the request')
})

test('a persisted turn failure carries typed detail instead of an encoded envelope', () => {
  const value = session()
  const error = { stage: 'agent_loop' as const, cause: 'provider_network' as const, message: 'Request failed' }
  value.activeEvents = []
  value.runtime = { ...value.runtime, phase: 'idle', activeTurn: null, terminal: { status: 'failed', message: 'Request failed' } }
  value.history.turns = [{ turnId: 't', status: 'failed', error, items: [] }]
  const view = readExecution(value)
  assert.deepEqual(view.facts.history.map(turn => turn.error), [error])
  const turn = buildTrajectory(value).at(-1)!
  const failure = turn.entries.at(-1)!
  assert.equal(failure.text, 'Request failed')
  assert.equal(failure.status, 'error')
  assert.deepEqual(failure.error, error, 'stage/cause stay typed and inspectable')
  assert.ok(!failure.text.includes('turnId') && !failure.text.includes('{'), 'the protocol envelope never becomes the message')
  // runtime 终态只承载无法落盘的失败：已持久化该轮错误时不重复一条同样的说明。
  assert.equal(turn.entries.filter(item => item.text === 'Request failed').length, 1)
  // 实时到达的同一错误同样只留下类型化事实。
  const live = session()
  live.activeEvents = [event({ method: 'turn/error', params: { turnId: 't', error } })]
  const liveTurn = buildTrajectory(live).at(-1)!
  assert.equal(liveTurn.entries.at(-1)!.text, 'Request failed')
  assert.deepEqual(liveTurn.entries.at(-1)!.error, error)
})

test('live diagnostics and request failures remain in trajectory only', () => {
  const value = session()
  value.activeEvents = [
    event({ method: 'agent/diagnostic', params: { turnId: 't', severity: 'warning', message: 'Retrying request' } }),
    event({ method: 'agent/diagnostic', params: { turnId: 't', severity: 'error', message: 'Provider failed' } }),
    event({ method: 'provider/attempt', params: { observation: makeObservation({ ordinal: 0, attempt: 1, provider: 'fixture', model: 'test', status: 'error', error: 'connection' }) } }),
    event({ method: 'turn/error', params: { turnId: 't', error: { stage: 'agent_loop', cause: 'provider_network', message: 'Request failed' } } }),
  ]
  assert.equal(buildTimeline(value).length, 0)
  const entries = buildTrajectory(value).flatMap(turn => turn.entries)
  assert.equal(entries.find(item => item.text === 'Retrying request')!.status, 'stable')
  assert.equal(entries.find(item => item.text === 'Provider failed')!.status, 'error')
  assert.equal(entries.find(item => item.request?.error === 'connection')!.status, 'error')
  assert.equal(entries.find(item => item.text.includes('Request failed'))!.status, 'error')
  assert.ok(!entries.some(item => item.text.includes('"turnId"')), 'the error text is never the serialized event')
})

test('repeating an input in a new turn stays distinct across stream settlement', () => {
  const value = session()
  // 历史条目与实时事件共用生产者发布的同一个公开内容块身份。
  const user = (itemId: string): HistoryItem => ({ type: 'message', id: itemId, role: 'user', text: 'hello' })
  value.history.turns = [{ turnId: 'previous', status: 'completed', items: [user('u1')] }]
  value.activeEvents = [event({ method: 'turn/userMessage', params: { turnId: 't', item: { itemId: 'u2' }, text: 'hello' } })]
  const check = () => {
    assert.equal(buildTimeline(value).filter(item => item.kind === 'user').length, 2)
    assert.deepEqual(buildTrajectory(value).map(turn => [turn.id, turn.entries.filter(item => item.kind === 'user').length]), [['previous', 1], ['t', 1]])
  }
  check()
  const liveKeys = buildTimeline(value).filter(item => item.kind === 'user').map(item => item.key)
  assert.equal(new Set(liveKeys).size, 2, 'identical text in different turns has distinct identity')
  value.runtime.activeTurn = null
  value.activeEvents = []
  value.history.turns = [...value.history.turns, { turnId: 't', status: 'completed', items: [user('u2')] }]
  check()
  assert.deepEqual(buildTimeline(value).filter(item => item.kind === 'user').map(item => item.key), liveKeys)
  assert.deepEqual(buildTimeline(structuredClone(value)).filter(item => item.kind === 'user').map(item => item.key), liveKeys)
  value.history.turns = [
    { turnId: null, status: null, items: [user('leading')] },
    value.history.turns[0],
    { ...value.history.turns[1], items: [user('u2'), user('steer')] },
  ]
  const keys = buildTimeline(value).filter(item => item.kind === 'user').map(item => item.key)
  assert.equal(keys[1], liveKeys[0])
  assert.equal(keys[2], liveKeys[1])
  assert.match(keys[0], /leading/)
  assert.match(keys[3], /steer/)
  assert.equal(new Set(keys).size, 4)
})

test('failed write does not fabricate an applied diff', () => {
  const failed = session()
  failed.runtime.activeTurn = null
  failed.history.turns = [{ turnId: 't', status: 'failed', items: [
    { type: 'tool_call', id: 'call', name: 'write', args: { path: 'a.txt', content: 'never written' } },
    { type: 'tool_result', id: 'call', output: 'Permission denied', isError: true },
  ] }]
  const item = buildTimeline(failed).find(item => item.kind === 'diff')
  assert.ok(item)
  assert.equal(item.tool!.diff.includes('never written'), false)
})

test('streamed tool lifecycle coalesces into one item and projection is repeatable', () => {
  const live = session()
  live.activeEvents = [
    event({ method: 'turn/userMessage', params: { turnId: 'unique', item: { itemId: 'u1' }, text: 'one input' } }),
    event({ method: 'item/started', params: { turnId: 'unique', item: { itemId: 'call' } } }),
    event({ method: 'tool/execution/start', params: { turnId: 'unique', item: { itemId: 'call' }, toolName: 'write', args: { path: 'a.txt', content: 'saved' } } }),
    event({ method: 'tool/execution/end', params: { turnId: 'unique', item: { itemId: 'call' }, output: 'Successfully wrote a.txt', diff: '--- a.txt\n+++ a.txt\n@@ -0,0 +1 @@\n+saved\n', isError: false } }),
    event({ method: 'item/completed', params: { turnId: 'unique', item: { itemId: 'call' } } }),
  ]
  const projected = buildTimeline(live)
  assert.equal(projected.filter(item => item.key === 'content:unique:call').length, 1)
  assert.equal(projected.filter(item => item.kind === 'unknown').length, 0)
  assert.equal(projected.find(item => item.kind === 'diff')!.addedLines, 1)
  assert.deepEqual(toolFact(projected.find(item => item.kind === 'diff')!).args, { path: 'a.txt', content: 'saved' })
  assert.equal(projected.filter(item => item.kind === 'user').length, 1)
  live.activeEvents = [...live.activeEvents,
    event({ method: 'item/agentMessage/delta', params: { turnId: 'unique', item: { itemId: 'answer' }, delta: 'a' } }),
  ]
  assert.equal(timelineBody(buildTimeline(live).find(item => item.kind === 'assistant')!), 'a')
})

test('streaming thinking and separate model replies retain order and identity after history reload', () => {
  const value = session()
  value.activeEvents = [
    event({ method: 'provider/attempt', params: { observation: makeObservation({ ordinal: 1, attempt: 1, status: 'started', provider: 'p', model: 'm' }) } }),
    event({ method: 'item/started', params: { item: { itemId: 'm1:thinking:0' } } }),
    event({ method: 'item/agentThinking/delta', params: { item: { itemId: 'm1:thinking:0' }, delta: '先检查' } }),
    event({ method: 'item/agentThinking/delta', params: { item: { itemId: 'm1:thinking:0' }, delta: '实现' } }),
  ]
  const liveThinking = buildTimeline(value)[0]
  assert.equal(timelineBody(liveThinking), '先检查实现')
  assert.equal(timelineStatus(liveThinking), 'running')
  assert.equal(buildTrajectory(value)[0].entries[0].thinking, '先检查实现')
  value.activeEvents = [...value.activeEvents,
    event({ method: 'item/started', params: { item: { itemId: 'm1:text:0' } } }),
    event({ method: 'item/agentMessage/delta', params: { item: { itemId: 'm1:text:0' }, delta: '读取文件。' } }),
    event({ method: 'item/completed', params: { item: { itemId: 'm1:thinking:0' }, content: { type: 'thinking', id: 'm1:thinking:0', text: '先检查实现' } } }),
    event({ method: 'item/completed', params: { item: { itemId: 'm1:thinking:0' } } }),
    event({ method: 'item/completed', params: { item: { itemId: 'm1:text:0' } } }),
    event({ method: 'tool/execution/start', params: { turnId: 't', item: { itemId: 'call1' }, toolName: 'read', args: { path: 'a.txt' } } }),
    event({ method: 'tool/execution/end', params: { turnId: 't', item: { itemId: 'call1' }, output: 'contents', isError: false } }),
    event({ method: 'item/started', params: { item: { itemId: 'm2:text:0' } } }),
    event({ method: 'item/agentMessage/delta', params: { item: { itemId: 'm2:text:0' }, delta: '现在完成。' } }),
    event({ method: 'item/completed', params: { item: { itemId: 'm2:text:0' } } }),
  ]
  const live = buildTimeline(value)
  assert.deepEqual(live.map(item => item.kind), ['thinking', 'assistant', 'tool', 'assistant'])
  assert.equal(buildTrajectory(value)[0].entries[0].thinking, '先检查实现')
  value.runtime.activeTurn = null
  value.activeEvents = []
  value.history.turns = [{ turnId: 't', status: 'completed', items: [
    { type: 'thinking', id: 'm1:thinking:0', text: '先检查实现' },
    { type: 'message', id: 'm1:text:0', role: 'assistant', text: '读取文件。' },
    { type: 'tool_call', id: 'call1', name: 'read', args: { path: 'a.txt' } },
    { type: 'tool_result', id: 'call1', output: 'contents', isError: false },
    { type: 'message', id: 'm2:text:0', role: 'assistant', text: '现在完成。' },
  ] }]
  assert.deepEqual(buildTimeline(value).filter(item => item.kind !== 'terminal').map(item => [item.key, item.kind, timelineBody(item)]), live.map(item => [item.key, item.kind, timelineBody(item)]))
})

test('context occupancy binds capacity to the executing snapshot, not the edit catalog', () => {
  const value = session()
  value.runtime.selector = 'p/m#high'
  value.runtime.modelContextWindow = 1000
  const catalog = { ...bootstrap().modelCatalog, providers: [{
    providerId: 'p', displayName: null, baseUrl: 'http://localhost', credentialConfigured: true,
    models: [{ modelId: 'm', displayName: null, apiProtocol: 'chat', maxContextTokens: 1000,
      maxOutputTokens: null, reasoningVariants: [], defaultVariant: null, thinkingWireFormat: null, chatOutputTokensField: null }],
  }] }
  assert.equal(contextOccupancy(value, catalog), null, 'no measurement yet')
  const request = makeObservation({ ordinal: 1, attempt: 1, provider: 'p', model: 'm', inputTokens: 120, outputTokens: 70, cachedInputTokens: 30, status: 'ok' })
  value.activeEvents = appendEvent(value.activeEvents, event({ method: 'provider/attempt', params: { observation: request } }))
  assert.deepEqual(contextOccupancy(value, catalog), { used: 120, capacity: 1000, percent: 12 })
  assert.equal(buildTrajectory(value)[0].entries[0].request!.inputTokens, 120)
  value.activeEvents = appendEvent(value.activeEvents, event({ method: 'provider/attempt', params: { observation: { ...request, inputTokens: null, status: 'started', attempt: 2 } } }))
  assert.equal(contextOccupancy(value, catalog)!.used, 120)
  const failedCompaction = { ...request, requestId: 'failed-compaction', purpose: 'compaction' as const, inputTokens: null, status: 'error' as const }
  value.activeEvents = appendEvent(value.activeEvents, event({ method: 'provider/attempt', params: { observation: failedCompaction } }))
  assert.equal(contextOccupancy(value, catalog)!.used, 120, 'a failed summary preserves the last measured input')
  const recoveredFailure = session()
  recoveredFailure.runtime = { ...value.runtime, activeTurn: null }
  // history 按 request id 合并观察结果，因此每个被测量的 request 在此保留自己的 id。
  recoveredFailure.history.turns = [{ turnId: 't', status: 'failed', items: [
    { type: 'request', startedAt, observation: { ...request, requestId: 'measured' } },
    { type: 'request', startedAt, observation: { ...failedCompaction, requestId: 'failed' } },
  ] }]
  assert.deepEqual(contextOccupancy(recoveredFailure, catalog), contextOccupancy(value, catalog))
  value.activeEvents = appendEvent(value.activeEvents, event({ method: 'provider/attempt', params: { observation: { ...request, purpose: 'compaction', inputTokens: 900, attempt: 3 } } }))
  assert.equal(contextOccupancy(value, catalog), null, 'summary input is not the active conversation size')
  value.runtime.activeTurn = null
  value.activeEvents = []
  const observed = makeObservation({ provider: request.provider, model: request.model, status: request.status,
    inputTokens: request.inputTokens, outputTokens: request.outputTokens, cachedInputTokens: request.cachedInputTokens })
  value.history.turns = [{ turnId: 't', status: 'completed', items: [{ startedAt, type: 'request', observation: observed }] }]
  assert.equal(contextOccupancy(value, catalog)!.used, 120)
  // window 属于产出该 usage 的那次执行：编辑
  // catalog 不得重新解释已测量的 input。
  const editedCatalog = { ...catalog, providers: [{ ...catalog.providers[0], models: [
    { ...catalog.providers[0].models[0], maxContextTokens: 5000 },
  ] }] }
  assert.deepEqual(contextOccupancy(value, editedCatalog), { used: 120, capacity: 1000, percent: 12 })
  value.runtime.selector = 'p/other'
  assert.equal(contextOccupancy(value, editedCatalog), null)
  value.runtime.selector = 'p/m'
  value.history.turns = [{ ...value.history.turns[0], items: [...value.history.turns[0].items, { type: 'compaction', id: 'compact', summary: 'short' }] }]
  assert.equal(contextOccupancy(value, catalog), null)
  value.history.turns = [{ ...value.history.turns[0], items: [...value.history.turns[0].items,
    { startedAt, type: 'request', observation: { ...observed, requestId: 'next-request', inputTokens: 50 } }] }]
  assert.equal(contextOccupancy(value, catalog)!.used, 50)
  value.history.turns = [{ ...value.history.turns[0], items: [...value.history.turns[0].items,
    { type: 'settings', id: 'switch', provider: 'other', model: 'x', reasoning: null }] }]
  assert.equal(contextOccupancy(value, catalog), null, 'switching provider or model invalidates the measured input')
  // 未上报的 window 保持未知；前端绝不猜测默认值。
  value.runtime.modelContextWindow = null
  assert.equal(contextOccupancy(value, catalog), null)
})

test('a tool appears with its input before any result or update arrives', () => {
  const value = session()
  value.activeEvents = [
    event({ method: 'provider/attempt', params: { observation: makeObservation({ ordinal: 1, attempt: 1, status: 'started' }) } }),
    event({ method: 'item/started', params: { turnId: 't', item: { itemId: 'slow' } } }),
    event({ method: 'tool/execution/start', params: { turnId: 't', item: { itemId: 'slow' }, toolName: 'bash', args: { command: 'sleep 30' } } }),
  ]
  const tools = buildTimeline(value)
  assert.equal(tools.length, 1)
  assert.equal(tools[0].title, 'bash')
  assert.equal(timelineBody(tools[0]), 'sleep 30')
  assert.equal(timelineStatus(tools[0]), 'running')
})

test('incremental facts preserve old views through ten thousand deltas', () => {
  const first = event({ method: 'item/agentMessage/delta', params: { turnId: 't', item: { itemId: 'a' }, delta: 'first' } })
  let value = readExecution(session())
  value = { ...value, facts: acceptExecutionEvent(value.facts, first) }
  const old = value
  const oldTrajectory = projectTrajectory(old)
  const oldTimeline = projectTimeline(old)
  const oldText = JSON.stringify([oldTrajectory, oldTimeline])
  for (let i = 0; i < 10000; i++) {
    value = { ...value, facts: acceptExecutionEvent(value.facts, event({ method: 'item/agentMessage/delta', params: { turnId: 't', item: { itemId: 'a' }, delta: '.' } })) }
    projectTrajectory(value)
    projectTimeline(value)
  }
  assert.equal(value.facts.active[0].items.length, 1)
  assert.equal(projectTrajectory(value)[0].entries.at(-1)!.text.length, 10005)
  const fresh = session()
  fresh.activeEvents = [event({ method: 'item/agentMessage/delta', params: { turnId: 't', item: { itemId: 'a' }, delta: 'first' + '.'.repeat(10000) } })]
  assert.deepEqual(buildTrajectory(fresh), projectTrajectory(value))
  assert.deepEqual(buildTimeline(fresh), projectTimeline(value))
  assert.equal(JSON.stringify([oldTrajectory, oldTimeline]), oldText)
  assert.equal(JSON.stringify([projectTrajectory(old), projectTimeline(old)]), oldText)
})

test('diff view keeps one surrounding line and preserves numbers when splitting blocks', () => {
  const hunks = diffContext([{oldStart: 10, newStart: 10, oldLines: 8, newLines: 9,
    lines: [' a', ' b', '-old', '+new', '+extra', ' d', ' e', ' f', '-old2', '+new2', ' h']}])
  assert.deepEqual(hunks.map(h => [h.oldStart, h.newStart, h.lines]), [
    [11, 11, [' b', '-old', '+new', '+extra', ' d']],
    [15, 16, [' f', '-old2', '+new2', ' h']],
  ])
  assert.deepEqual(hunks.map(h => [h.oldLines, h.newLines]), [[3, 4], [3, 3]])
  assert.deepEqual(diffContext([{oldStart: 1, newStart: 1, oldLines: 1, newLines: 1,
    lines: ['-old', '\\ No newline at end of file', '+new', '\\ No newline at end of file']}])[0].lines,
    ['-old', '\\ No newline at end of file', '+new', '\\ No newline at end of file'])
})
