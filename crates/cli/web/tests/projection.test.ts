import { diffContext } from '../src/diffView'
import { createPatch } from 'diff'
import { EventLog, appendEvent, eventsSince } from '../src/eventLog'
import assert from 'node:assert/strict'
import { test } from 'node:test'
import { buildTimeline } from '../src/timeline'
import { buildTrajectory } from '../src/trajectory'
import { userMessageItemId } from '../src/protocol'
import { contextOccupancy } from '../src/contextUsage'
import { reasoningChoices } from '../src/modelChoices'
import { inputTrigger } from '../src/inputTrigger'
import { session as wireSession, runtime, control, bootstrap, model, event, observation as makeObservation, startedAt, requestSnapshot } from './fixtures'
import type { SessionReadResult, HistoryItem } from '../src/protocol'
const session = (): SessionReadResult => wireSession()

test('caret triggers preserve command boundaries and ignore paths or URLs', () => {
  assert.deepEqual(inputTrigger('/', 1), { kind: 'skill', start: 0, end: 1, query: '' })
  assert.deepEqual(inputTrigger('/rev trailing', 4), { kind: 'skill', start: 0, end: 4, query: 'rev' })
  assert.equal(inputTrigger('https://example', 15), null)
  assert.equal(inputTrigger('src/file', 8), null)
  assert.equal(inputTrigger('//comment', 9), null)
  assert.deepEqual(inputTrigger('read @src', 9), { kind: 'file', start: 5, end: 9, query: 'src' })
  assert.deepEqual(inputTrigger('@src/lib.rs', 11), { kind: 'file', start: 0, end: 11, query: 'src/lib.rs' })
  assert.deepEqual(inputTrigger('(/review', 8), { kind: 'skill', start: 1, end: 8, query: 'review' })
})

test('long tool progress is bounded and incremental projections match refreshed snapshots', () => {
  const value = session()
  const args = { command: 'build' }
  const start = event({ method: 'tool/execution/start', params: { turnId: 't', toolCallId: 'tool', toolName: 'bash', args } })
  value.runtime.activeTurn!.events = appendEvent([], start)
  const original = value.runtime.activeTurn!.events
  for (let index = 0; index < 10000; index++) {
    value.runtime.activeTurn!.events = appendEvent(value.runtime.activeTurn!.events, event({
      method: 'tool/execution/update', params: { turnId: 't', toolCallId: 'tool', toolName: 'bash', args, partialResult: `output ${index}: ${'x'.repeat(4096)}` },
    }))
    buildTimeline(value)
    buildTrajectory(value)
  }
  assert.equal(value.runtime.activeTurn!.events.length, 2)
  assert.deepEqual([...original], [start])
  const compare = () => {
    const fresh = structuredClone({ ...value, runtime: { ...value.runtime, activeTurn: { ...value.runtime.activeTurn!, events: [...value.runtime.activeTurn!.events] } } })
    assert.deepEqual(buildTimeline(fresh), buildTimeline(value))
    assert.deepEqual(buildTrajectory(fresh), buildTrajectory(value))
  }
  compare()
  value.runtime.activeTurn!.events = appendEvent(value.runtime.activeTurn!.events, event({ method: 'tool/execution/end', params: { turnId: 't', toolCallId: 'tool', toolName: 'bash', result: { content: [{ type: 'text', text: 'complete' }], isError: false } } }))
  assert.equal(value.runtime.activeTurn!.events.length, 2)
  compare()
  assert.equal(buildTimeline(value)[0].tool!.output, 'complete')
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
  value.history.turns = [{ turnId: 't', status: 'completed', items: [
    { type: 'message', id: 'u', role: 'user', text: 'inspect' },
    { type: 'request', id: 'r', timestamp: 'now', observation: makeObservation({ ordinal: 1, attempt: 1, provider: 'p', model: 'm', status: 'ok', durationMs: 350, inputTokens: null, outputTokens: 12, cachedInputTokens: null, error: null }) },
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

test('individual tools preserve order and failure across history recovery', () => {
  const value = session()
  const events = [...value.runtime.activeTurn!.events]
  const attempt = (ordinal: number) => (event({ method: 'provider/attempt', params: { turnId: 't', modelTurnOrdinal: ordinal, attempt: 1, status: 'started' } }))
  const start = (id: string) => (event({ method: 'tool/execution/start', params: { turnId: 't', toolCallId: id, toolName: 'read', args: { path: id } } }))
  const end = (id: string) => (event({ method: 'tool/execution/end', params: { turnId: 't', toolCallId: id, toolName: 'read', result: { content: [{ type: 'text', text: id }], isError: id === 'b' } } }))
  events.push(attempt(1), start('a'), end('a'))
  value.runtime.activeTurn!.events = events
  const first = buildTimeline(value)[0]
  assert.equal(first.status, 'completed')
  assert.deepEqual(first.tool!.args, { path: 'a' })
  assert.equal(first.tool!.output, 'a')
  value.runtime.activeTurn!.events = [...events, start('b'), end('b'), attempt(2), start('c')]
  const live = buildTimeline(value)
  assert.deepEqual(live.map(tool => tool.body), ['a', 'b', 'c'])
  assert.equal(live[1].status, 'failed')
  assert.equal(live[2].status, 'running')
  const toolEntries = buildTrajectory(value).flatMap(turn => turn.entries).filter(item => item.kind === 'tool')
  assert.equal(toolEntries.length, 3)
  assert.deepEqual(toolEntries[0].input, { path: 'a' })
  assert.equal(toolEntries[0].text, 'a')
  value.runtime.activeTurn!.events = [...value.runtime.activeTurn!.events, event({ method: 'turn/completed', params: { turn: { turnId: 't', status: 'interrupted' } } })]
  assert.equal(buildTimeline(value).at(-1)!.kind, 'terminal')
  assert.equal(buildTrajectory(value).flatMap(turn => turn.entries).find(item => item.id === 'c')!.status, 'cancelled')
  value.runtime.activeTurn = null
  value.history.turns = [{ turnId: 't', status: 'interrupted', items: [
    { timestamp: startedAt, type: 'request', id: 'r1', observation: makeObservation({ ordinal: 1, attempt: 1 }) },
    { type: 'tool_call', id: 'a', name: 'read', args: { path: 'a' } },
    { type: 'tool_call', id: 'b', name: 'read', args: { path: 'b' } },
    { type: 'tool_result', id: 'a', output: 'a', isError: false },
    { type: 'tool_result', id: 'b', output: 'b', isError: true },
    { timestamp: startedAt, type: 'request', id: 'r2', observation: makeObservation({ ordinal: 2, attempt: 1 }) },
    { type: 'tool_call', id: 'c', name: 'read', args: { path: 'c' } },
  ] }]
  const recovered = buildTimeline(value)
  assert.deepEqual(recovered.slice(0, 3).map(tool => tool.key), live.map(tool => tool.key))
  assert.equal(recovered[1].status, 'failed')
})

test('request lookup and prompt head survive completion and history reload without full context', () => {
  const snapshot = requestSnapshot({
    request_id: 'request', messages: [{ tool_call_id: null, role: 'system', content: 'system prompt' }],
    tools: [{ name: 'read', description: 'Read a file', parameters_schema: { type: 'object' } }], model_preferences: {},
  })
  const value = session()
  value.runtime.activeTurn!.events = [
    event({ method: 'provider/attempt', params: { turnId: 't', modelTurnOrdinal: 1, attempt: 1, status: 'started', provider: 'p', model: 'm', requestId: 'lookup-request', requestHead: snapshot } }),
    event({ method: 'item/agentMessage/delta', params: { turnId: 't', item: { itemId: 'answer' }, delta: 'answer' } }),
    event({ method: 'provider/attempt', params: { turnId: 't', modelTurnOrdinal: 1, attempt: 1, requestId: 'lookup-request', status: 'ok', provider: 'p', model: 'm', attemptDurationMs: 100 } }),
  ]
  const live = buildTrajectory(value)[0].entries
  assert.equal(live[0].kind, 'system')
  assert.deepEqual(live[1].request!.requestHead, snapshot)
  assert.equal(live[1].text, 'answer')
  assert.equal(live[1].request!.requestId, 'lookup-request')
  assert.equal(live[1].duration, 100)
  value.runtime.activeTurn = null
  value.history.turns = [{ status: 'completed', turnId: 't', items: [
    { timestamp: startedAt, type: 'request', id: 'persisted', observation: live[1].request! },
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
  value.history.turns = [{ status: 'completed', turnId: 't', items: [{ type: 'request', id: 'r', timestamp: '2026-09-05T00:00:01Z', observation }] }]
  const request = () => buildTrajectory(value)[0].entries[0]
  assert.equal(request().status, 'running')
  assert.equal(request().duration, null)
  value.runtime.activeTurn = null
  assert.equal(request().status, 'cancelled')
  assert.equal(observation.status, 'started')
  value.runtime.activeTurn = { startedAt, turnId: 'next', events: [] }
  assert.equal(request().status, 'cancelled')
  value.runtime.activeTurn = null
  value.runtime.activeCompaction = { startedAt: '2026-09-05T00:00:00Z' }
  assert.equal(request().status, 'cancelled')
  value.history.turns = [{ status: 'completed', turnId: null, items: [{ type: 'request', id: 'c', timestamp: '2026-09-05T00:00:01Z', observation: { ...observation, purpose: 'compaction' } }] }]
  assert.equal(request().status, 'running')
  value.runtime.activeCompaction = { startedAt: '2026-09-05T00:00:02Z' }
  assert.equal(request().status, 'cancelled')
  value.runtime.activeCompaction = null
  assert.equal(request().status, 'cancelled')
})

test('runtime-only updates reuse stable historical trajectory objects', () => {
  const value = session()
  value.history.turns = [{ status: 'completed', turnId: 'history', items: [
    { type: 'message', id: 'answer', role: 'assistant', text: 'durable answer' },
  ] }]
  value.runtime.activeTurn = { startedAt, turnId: 'active', events: [] }
  const first = buildTrajectory(value)
  value.runtime.activeTurn.events = appendEvent(value.runtime.activeTurn.events, event({
    method: 'item/agentMessage/delta',
    params: { turnId: 'active', item: { itemId: 'live' }, delta: 'streaming' },
  }))
  const second = buildTrajectory(value)
  assert.strictEqual(second[0], first[0])
  assert.strictEqual(second[0].entries, first[0].entries)
  assert.strictEqual(second[0].entries[0], first[0].entries[0])
  assert.equal(second[1].entries[0].text, 'streaming')
})

test('structured file changes share statistics and rendered hunks across live and recovered views', () => {
  const value = session()
  const diff = createPatch('file.txt', '--old\n', '++new\n')
  const output = 'Successfully edited file.txt'
  value.runtime.activeTurn!.events = [
    event({ method: 'tool/execution/start', params: { turnId: 't', toolCallId: 'edit', toolName: 'edit', args: { path: 'file.txt' } } }),
    event({ method: 'tool/execution/end', params: { turnId: 't', toolCallId: 'edit', toolName: 'edit', result: { content: [{ type: 'text', text: output }], diff, isError: false } } }),
  ]
  const live = buildTimeline(value)[0]
  assert.deepEqual([live.addedLines, live.removedLines], [1, 1])
  assert.deepEqual(live.tool!.patches[0].hunks[0].lines, ['---old', '+++new'])
  assert.equal(live.tool!.output, output)
  const inspected = buildTrajectory(value)[0].entries[0].text
  assert.equal(inspected, `${output}\n\n${diff}`)
  value.runtime.activeTurn = null
  value.history.turns = [{ turnId: 't', status: 'completed', items: [
    { type: 'tool_call', id: 'edit', name: 'edit', args: { path: 'file.txt' } },
    { type: 'tool_result', id: 'edit', output, diff, isError: false },
  ] }]
  const recovered = buildTimeline(value)[0]
  assert.deepEqual([recovered.addedLines, recovered.removedLines], [1, 1])
  assert.deepEqual(recovered.tool, live.tool)
  assert.equal(buildTrajectory(value)[0].entries[0].text, inspected)
  value.history.turns = [{ ...value.history.turns[0], items: [value.history.turns[0].items[0],
    { type: 'tool_result', id: 'edit', output: `${output}\n\n${diff}`, isError: false },
  ] }]
  assert.equal(buildTimeline(value)[0].tool!.diff, '', 'ordinary output never impersonates structured file changes')
})

test('runtime failure does not add a conversation banner', () => {
  const value = session()
  value.runtime.activeTurn = null
  value.runtime.terminal = { status: 'failed', message: 'Provider rejected the request' }
  value.history.turns = [{ turnId: 't', status: 'failed', items: [] }]
  const terminals = buildTimeline(value).filter(item => item.kind === 'terminal')
  assert.equal(terminals.length, 0)
  assert.equal(buildTrajectory(value).at(-1)!.entries.at(-1)!.text, 'Provider rejected the request')
})

test('live diagnostics and request failures remain in trajectory only', () => {
  const value = session()
  value.runtime.activeTurn!.events = [
    event({ method: 'agent/diagnostic', params: { turnId: 't', severity: 'warning', message: 'Retrying request' } }),
    event({ method: 'agent/diagnostic', params: { turnId: 't', severity: 'error', message: 'Provider failed' } }),
    event({ method: 'provider/attempt', params: { turnId: 't', modelTurnOrdinal: 0, attempt: 1, provider: 'fixture', model: 'test', status: 'error', errorCategory: 'connection' } }),
    event({ method: 'turn/error', params: { turnId: 't', error: { stage: 'agent_loop', cause: 'provider_network', message: 'Request failed' } } }),
  ]
  assert.equal(buildTimeline(value).length, 0)
  const entries = buildTrajectory(value).flatMap(turn => turn.entries)
  assert.equal(entries.find(item => item.text === 'Retrying request')!.status, 'stable')
  assert.equal(entries.find(item => item.text === 'Provider failed')!.status, 'error')
  assert.equal(entries.find(item => item.request?.error === 'connection')!.status, 'error')
  assert.equal(entries.find(item => item.text.includes('Request failed'))!.status, 'error')
})

test('repeating an input in a new turn stays distinct across stream settlement', () => {
  const value = session()
  // History entries carry the producer's public content-block id: the durable
  // entry id plus the first text block suffix, via the same helper the live
  // projection applies to the userMessage event.
  const user = (entryId: string): HistoryItem => ({ type: 'message', id: userMessageItemId(entryId), role: 'user', text: 'hello' })
  value.history.turns = [{ turnId: 'previous', status: 'completed', items: [user('u1')] }]
  value.runtime.activeTurn!.events = [event({ method: 'turn/userMessage', params: { turnId: 't', entryId: 'u2', text: 'hello' } })]
  const check = () => {
    assert.equal(buildTimeline(value).filter(item => item.kind === 'user').length, 2)
    assert.deepEqual(buildTrajectory(value).map(turn => [turn.id, turn.entries.filter(item => item.kind === 'user').length]), [['previous', 1], ['t', 1]])
  }
  check()
  const liveKeys = buildTimeline(value).filter(item => item.kind === 'user').map(item => item.key)
  assert.equal(new Set(liveKeys).size, 2, 'identical text in different turns has distinct identity')
  value.runtime.activeTurn = null
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
  live.runtime.activeTurn!.events = [
    event({ method: 'turn/userMessage', params: { turnId: 'unique', entryId: 'u1', text: 'one input' } }),
    event({ method: 'item/started', params: { turnId: 'unique', item: { itemId: 'call' } } }),
    event({ method: 'tool/execution/start', params: { turnId: 'unique', toolCallId: 'call', toolName: 'write', args: { path: 'a.txt', content: 'saved' } } }),
    event({ method: 'tool/execution/end', params: { turnId: 'unique', toolCallId: 'call', toolName: 'write', result: { content: [{ type: 'text', text: 'Successfully wrote a.txt' }], diff: '--- a.txt\n+++ a.txt\n@@ -0,0 +1 @@\n+saved\n', isError: false } } }),
    event({ method: 'item/completed', params: { turnId: 'unique', item: { itemId: 'call' } } }),
  ]
  const projected = buildTimeline(live)
  assert.equal(projected.filter(item => item.key === 'content:unique:call').length, 1)
  assert.equal(projected.filter(item => item.kind === 'unknown').length, 0)
  assert.equal(projected.find(item => item.kind === 'diff')!.addedLines, 1)
  assert.deepEqual(projected.find(item => item.kind === 'diff')!.tool!.args, { path: 'a.txt', content: 'saved' })
  assert.equal(projected.filter(item => item.kind === 'user').length, 1)
  live.runtime.activeTurn!.events = [...live.runtime.activeTurn!.events,
    event({ method: 'item/agentMessage/delta', params: { turnId: 'unique', item: { itemId: 'answer' }, delta: 'a' } }),
  ]
  assert.equal(buildTimeline(live).find(item => item.kind === 'assistant')!.body, 'a')
})

test('streaming thinking and separate model replies retain order and identity after history reload', () => {
  const value = session()
  value.runtime.activeTurn!.events = [
    event({ method: 'provider/attempt', params: { turnId: 't', modelTurnOrdinal: 1, attempt: 1, status: 'started', provider: 'p', model: 'm' } }),
    event({ method: 'item/started', params: { item: { itemId: 'm1:thinking:0' } } }),
    event({ method: 'item/agentThinking/delta', params: { item: { itemId: 'm1:thinking:0' }, delta: '先检查' } }),
    event({ method: 'item/agentThinking/delta', params: { item: { itemId: 'm1:thinking:0' }, delta: '实现' } }),
  ]
  const liveThinking = buildTimeline(value)[0]
  assert.equal(liveThinking.body, '先检查实现')
  assert.equal(liveThinking.status, 'running')
  assert.equal(buildTrajectory(value)[0].entries[0].thinking, '先检查实现')
  value.runtime.activeTurn!.events = [...value.runtime.activeTurn!.events,
    event({ method: 'item/started', params: { item: { itemId: 'm1:text:0' } } }),
    event({ method: 'item/agentMessage/delta', params: { item: { itemId: 'm1:text:0' }, delta: '读取文件。' } }),
    event({ method: 'item/agentThinking', params: { item: { itemId: 'm1:thinking:0' }, text: '先检查实现' } }),
    event({ method: 'item/completed', params: { item: { itemId: 'm1:thinking:0' } } }),
    event({ method: 'item/completed', params: { item: { itemId: 'm1:text:0' } } }),
    event({ method: 'tool/execution/start', params: { turnId: 't', toolCallId: 'call1', toolName: 'read', args: { path: 'a.txt' } } }),
    event({ method: 'tool/execution/end', params: { turnId: 't', toolCallId: 'call1', toolName: 'read', result: { content: [{ type: 'text', text: 'contents' }], isError: false } } }),
    event({ method: 'item/started', params: { item: { itemId: 'm2:text:0' } } }),
    event({ method: 'item/agentMessage/delta', params: { item: { itemId: 'm2:text:0' }, delta: '现在完成。' } }),
    event({ method: 'item/completed', params: { item: { itemId: 'm2:text:0' } } }),
  ]
  const live = buildTimeline(value)
  assert.deepEqual(live.map(item => item.kind), ['thinking', 'assistant', 'tool', 'assistant'])
  assert.equal(buildTrajectory(value)[0].entries[0].thinking, '先检查实现')
  value.runtime.activeTurn = null
  value.history.turns = [{ turnId: 't', status: 'completed', items: [
    { type: 'thinking', id: 'm1:thinking:0', text: '先检查实现' },
    { type: 'message', id: 'm1:text:0', role: 'assistant', text: '读取文件。' },
    { type: 'tool_call', id: 'call1', name: 'read', args: { path: 'a.txt' } },
    { type: 'tool_result', id: 'call1', output: 'contents', isError: false },
    { type: 'message', id: 'm2:text:0', role: 'assistant', text: '现在完成。' },
  ] }]
  assert.deepEqual(buildTimeline(value).filter(item => item.kind !== 'terminal').map(item => [item.key, item.kind, item.body]), live.map(item => [item.key, item.kind, item.body]))
})

test('context occupancy binds capacity to the executing snapshot, not the edit catalog', () => {
  const value = session()
  value.runtime.selector = 'p/m#high'
  value.runtime.modelContextWindow = 1000
  const catalog = { ...bootstrap().modelCatalog, providers: [{
    providerId: 'p', displayName: null, baseUrl: 'http://localhost', credentialConfigured: true,
    models: [{ modelId: 'm', displayName: null, apiProtocol: 'chat', maxContextTokens: 1000,
      maxOutputTokens: null, reasoningVariants: [], defaultVariant: null, thinkingWireFormat: null }],
  }] }
  assert.equal(contextOccupancy(value, catalog), null, 'no measurement yet')
  const request = { turnId: 't', provider: 'p', model: 'm', inputTokens: 120, outputTokens: 70, cachedInputTokens: 30, status: 'ok' as const, modelTurnOrdinal: 1, attempt: 1 }
  value.runtime.activeTurn!.events = appendEvent(value.runtime.activeTurn!.events, event({ method: 'provider/attempt', params: request }))
  assert.deepEqual(contextOccupancy(value, catalog), { used: 120, capacity: 1000, percent: 12 })
  assert.equal(buildTrajectory(value)[0].entries[0].request!.inputTokens, 120)
  value.runtime.activeTurn!.events = appendEvent(value.runtime.activeTurn!.events, event({ method: 'provider/attempt', params: { ...request, inputTokens: null, status: 'started', attempt: 2 } }))
  assert.equal(contextOccupancy(value, catalog)!.used, 120)
  value.runtime.activeTurn!.events = appendEvent(value.runtime.activeTurn!.events, event({ method: 'provider/attempt', params: { ...request, purpose: 'compaction', inputTokens: 900, attempt: 3 } }))
  assert.equal(contextOccupancy(value, catalog), null, 'summary input is not the active conversation size')
  value.runtime.activeTurn = null
  const observed = makeObservation({ provider: request.provider, model: request.model, status: request.status,
    inputTokens: request.inputTokens, outputTokens: request.outputTokens, cachedInputTokens: request.cachedInputTokens })
  value.history.turns = [{ turnId: 't', status: 'completed', items: [{ id: 'request', timestamp: startedAt, type: 'request', observation: observed }] }]
  assert.equal(contextOccupancy(value, catalog)!.used, 120)
  // The window belongs to the execution that produced the usage: editing the
  // catalog must not reinterpret the measured input.
  const editedCatalog = { ...catalog, providers: [{ ...catalog.providers[0], models: [
    { ...catalog.providers[0].models[0], maxContextTokens: 5000 },
  ] }] }
  assert.deepEqual(contextOccupancy(value, editedCatalog), { used: 120, capacity: 1000, percent: 12 })
  value.runtime.selector = 'p/other'
  assert.equal(contextOccupancy(value, editedCatalog), null)
  value.runtime.selector = 'p/m'
  value.history.turns = [{ ...value.history.turns[0], items: [...value.history.turns[0].items, { type: 'compaction', id: 'compact', summary: 'short' }] }]
  assert.equal(contextOccupancy(value, catalog), null)
  value.history.turns = [{ ...value.history.turns[0], items: [...value.history.turns[0].items, { id: 'next-request', timestamp: startedAt, type: 'request', observation: { ...observed, inputTokens: 50 } }] }]
  assert.equal(contextOccupancy(value, catalog)!.used, 50)
  // An unreported window stays unknown; the frontend never guesses defaults.
  value.runtime.modelContextWindow = null
  assert.equal(contextOccupancy(value, catalog), null)
})

test('a tool appears with its input before any result or update arrives', () => {
  const value = session()
  value.runtime.activeTurn!.events = [
    event({ method: 'provider/attempt', params: { turnId: 't', modelTurnOrdinal: 1, attempt: 1, status: 'started' } }),
    event({ method: 'item/started', params: { turnId: 't', item: { itemId: 'slow' } } }),
    event({ method: 'tool/execution/start', params: { turnId: 't', toolCallId: 'slow', toolName: 'bash', args: { command: 'sleep 30' } } }),
  ]
  const tools = buildTimeline(value)
  assert.equal(tools.length, 1)
  assert.equal(tools[0].title, 'bash')
  assert.equal(tools[0].body, 'sleep 30')
  assert.equal(tools[0].status, 'running')
})

test('event suffixes preserve previous snapshots and incremental projections', () => {
  const value = session()
  const first = event({ method: 'item/agentMessage/delta', params: { turnId: 't', item: { itemId: 'a' }, delta: 'first' } })
  value.runtime.activeTurn!.events = appendEvent([], first)
  const oldEvents = value.runtime.activeTurn!.events
  const oldTrajectory = JSON.stringify(buildTrajectory(value))
  const oldTimeline = JSON.stringify(buildTimeline(value))
  for (let i = 0; i < 10000; i++) {
    value.runtime.activeTurn!.events = appendEvent(value.runtime.activeTurn!.events, event({ method: 'item/agentMessage/delta', params: { turnId: 't', item: { itemId: 'a' }, delta: '.' } }))
    buildTrajectory(value)
    buildTimeline(value)
  }
  assert.equal(oldEvents.length, 1)
  assert.deepEqual([...oldEvents], [first])
  assert.equal([...eventsSince(value.runtime.activeTurn!.events, 9999)].length, 2)
  const incrementalTrajectory = buildTrajectory(value)
  assert.equal(incrementalTrajectory[0].entries.at(-1)!.text.length, 10005)
  const incrementalTimeline = buildTimeline(value)
  const fresh = structuredClone({ ...value, runtime: { ...value.runtime, activeTurn: { ...value.runtime.activeTurn!, events: [...value.runtime.activeTurn!.events] } } })
  assert.deepEqual(buildTrajectory(fresh), incrementalTrajectory)
  assert.deepEqual(buildTimeline(fresh), incrementalTimeline)
  value.runtime.activeTurn!.events = oldEvents
  assert.equal(JSON.stringify(buildTrajectory(value)), oldTrajectory)
  assert.equal(JSON.stringify(buildTimeline(value)), oldTimeline)
  assert.equal(JSON.parse(JSON.stringify(new EventLog([first]))).length, 1)
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
