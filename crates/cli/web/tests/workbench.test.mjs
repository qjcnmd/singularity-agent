import { diffContext } from '../src/diffView.ts'
import { createPatch } from 'diff'
import { EventLog, appendEvent, eventsSince } from '../src/eventLog.ts'
import assert from 'node:assert/strict'
import { beforeEach, test } from 'node:test'
import { workbenchStore, sameWorkbenchFields } from '../src/store.ts'
import { buildTimeline } from '../src/timeline.ts'
import { buildTrajectory } from '../src/trajectory.ts'
import { contextOccupancy } from '../src/contextUsage.ts'
import { reasoningChoices } from '../src/modelChoices.ts'
import { WorkbenchConnection, RpcFailure } from '../src/connection.ts'
import { protocolVersion } from '../src/protocol.ts'
import { inputTrigger } from '../src/inputTrigger.ts'

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
  const start = { method: 'tool/execution/start', params: { turnId: 't', toolCallId: 'tool', toolName: 'bash', args } }
  value.runtime.activeTurn.events = appendEvent([], start)
  const original = value.runtime.activeTurn.events
  for (let index = 0; index < 10000; index++) {
    value.runtime.activeTurn.events = appendEvent(value.runtime.activeTurn.events, {
      method: 'tool/execution/update', params: { turnId: 't', toolCallId: 'tool', toolName: 'bash', args, partialResult: `output ${index}: ${'x'.repeat(4096)}` },
    })
    buildTimeline(value)
    buildTrajectory(value)
  }
  assert.equal(value.runtime.activeTurn.events.length, 2)
  assert.deepEqual([...original], [start])
  const compare = () => {
    const fresh = structuredClone({ ...value, runtime: { ...value.runtime, activeTurn: { ...value.runtime.activeTurn, events: [...value.runtime.activeTurn.events] } } })
    const now = Date.now()
    assert.deepEqual(buildTimeline(fresh, now), buildTimeline(value, now))
    assert.deepEqual(buildTrajectory(fresh), buildTrajectory(value))
  }
  compare()
  value.runtime.activeTurn.events = appendEvent(value.runtime.activeTurn.events, { method: 'tool/execution/end', params: { turnId: 't', toolCallId: 'tool', toolName: 'bash', result: { content: [{ text: 'complete' }], isError: false } } })
  assert.equal(value.runtime.activeTurn.events.length, 2)
  compare()
  assert.equal(buildTimeline(value)[0].tool.output, 'complete')
})

test('connection keeps retrying after a long outage and stops cleanly', t => {
  const previousWindow = globalThis.window
  const previousSocket = globalThis.WebSocket
  const timers = new Map()
  const sockets = []
  const statuses = []
  let timerId = 0
  globalThis.window = {
    location: { protocol: 'http:', host: '127.0.0.1:3081' },
    setTimeout: (callback, delay) => { timers.set(++timerId, { callback, delay }); return timerId },
    clearTimeout: id => timers.delete(id),
  }
  globalThis.WebSocket = class extends EventTarget {
    constructor() { super(); sockets.push(this) }
    close() { this.dispatchEvent(new Event('close')) }
  }
  t.after(() => { globalThis.window = previousWindow; globalThis.WebSocket = previousSocket })
  const connection = new WorkbenchConnection(() => {}, status => statuses.push(status))
  connection.start()
  for (let attempt = 0; attempt < 9; attempt++) {
    sockets.at(-1).close()
    assert.equal(timers.size, 1)
    const [id, timer] = [...timers][0]
    assert.ok(timer.delay <= 8000)
    timers.delete(id)
    timer.callback()
  }
  sockets.at(-1).dispatchEvent(new MessageEvent('message', { data: JSON.stringify({ version: protocolVersion, type: 'ready' }) }))
  assert.equal(statuses.at(-1), 'ready')
  assert.equal(statuses.includes('unavailable'), false)
  sockets.at(-1).close()
  assert.equal(timers.size, 1)
  connection.stop()
  assert.equal(timers.size, 0)
})

test('RPC transport failure reconnects once and never replays the mutation', async t => {
  const previousWindow = globalThis.window
  const previousSocket = globalThis.WebSocket
  const previousFetch = globalThis.fetch
  const timers = []
  const sockets = []
  const statuses = []
  const frames = []
  let calls = 0
  globalThis.window = {
    location: { protocol: 'http:', host: '127.0.0.1:3081' },
    setTimeout: callback => { timers.push(callback); return timers.length },
    clearTimeout: () => {},
  }
  globalThis.WebSocket = class extends EventTarget {
    constructor() { super(); sockets.push(this) }
    close() { this.dispatchEvent(new Event('close')) }
  }
  globalThis.fetch = async () => { calls++; throw new Error('connection reset') }
  t.after(() => { globalThis.window = previousWindow; globalThis.WebSocket = previousSocket; globalThis.fetch = previousFetch })
  const connection = new WorkbenchConnection(frame => frames.push(frame), status => statuses.push(status))
  const ready = socket => socket.dispatchEvent(new MessageEvent('message', { data: JSON.stringify({version:protocolVersion,type:'ready'}) }))
  connection.start()
  ready(sockets[0])
  await assert.rejects(connection.rpc('session.submit', {text:'hello'}), {code:'unavailable'})
  assert.equal(statuses.at(-1), 'recovering')
  assert.equal(timers.length, 1)
  ready(sockets[0])
  assert.equal(frames.length, 1, 'stale socket cannot restore readiness')
  timers.shift()()
  ready(sockets[1])
  assert.equal(statuses.at(-1), 'ready')
  assert.equal(frames.length, 2, 'new ready frame triggers the usual baseline sync')
  assert.equal(calls, 1)
  connection.stop()
})

test('reasoning slider orders configured levels and retains thinking-off choices', () => {
  const variants = [
    { id: 'high', enabled: true }, { id: 'low', enabled: true },
    { id: 'off', enabled: false }, { id: 'medium', enabled: true },
  ]
  assert.deepEqual(reasoningChoices({ reasoningVariants: variants }).map(value => value.id), ['off', 'low', 'medium', 'high'])
  assert.deepEqual(variants.map(value => value.id), ['high', 'low', 'off', 'medium'])
  assert.equal(reasoningChoices({ reasoningVariants: [{ id: 'high', enabled: true }] }).length, 1)
  assert.deepEqual(reasoningChoices(undefined), [])
})

let store
let bootstrap
const runtime = () => ({
  sessionRevision: 0, phase: 'running', selector: null, controls: [], pendingControls: [],
  activeCompaction: null, terminal: null,
  activeTurn: { turnId: 't', input: 'hello', status: 'running', events: [], startedAt: '2026-09-05T00:00:00Z' },
})
const session = () => ({ summary: { threadId: 's' }, history: { turns: [], nextCursor: null }, runtime: runtime() })
function historyPage(first, last, total = last) {
  const summary = { threadId: 's', turnCount: total }
  return { ...session(), summary,
    history: { summary, turns: Array.from({ length: last - first + 1 }, (_, offset) => ({ turnId: `t${first + offset}`, status: 'completed', items: [] })), nextCursor: first === 1 ? null : `turn:t${first}` },
    runtime: { ...runtime(), sessionRevision: total, phase: 'idle', activeTurn: null },
  }
}
const frame = (revision, delta) => ({
  generation: 'g', revision, sessionId: 's', type: 'turn_event',
  payload: {
    method: 'item/agentMessage/delta', sessionRevision: revision,
    params: { turnId: 't', item: { itemId: 'a' }, delta },
  },
})

test('trajectory preserves request statistics and coalesces tool result without polluting chat', () => {
  const value = session()
  value.runtime.activeTurn = null
  value.runtime.controls = [{ controlId: 'c', channel: 'follow_up', disposition: 'cancelled', text: 'withdrawn' }]
  value.history.turns = [{ turnId: 't', status: 'completed', items: [
    { type: 'message', id: 'u', role: 'user', text: 'inspect' },
    { type: 'request', id: 'r', timestamp: 'now', observation: { ordinal: 1, attempt: 1, provider: 'p', model: 'm', status: 'ok', durationMs: 350, inputTokens: null, outputTokens: 12, cachedInputTokens: null, error: null } },
    { type: 'tool_call', id: 'c', name: 'read', args: { path: 'a' } },
    { type: 'tool_result', id: 'c', output: 'missing', isError: true },
  ] }]
  const trajectory = buildTrajectory(value)[0].entries
  assert.equal(trajectory.length, 3)
  assert.equal(trajectory[1].duration, 350)
  assert.equal(trajectory[1].request.inputTokens, null)
  assert.equal(trajectory[1].request.outputTokens, 12)
  assert.equal(trajectory[2].failed, true)
  assert.match(trajectory[2].text, /missing/)
  assert.deepEqual(buildTimeline(value).map(item => item.kind), ['user', 'tool'])
})

beforeEach(() => {
  const storage = new Map()
  globalThis.localStorage = {
    getItem: key => storage.get(key) ?? null,
    setItem: (key, value) => storage.set(key, value),
    key: index => [...storage.keys()][index],
    get length() { return storage.size },
  }
  bootstrap = {
    generation: 'g', revision: 0, sessionPhases: {},
    workspaces: [{ workspaceId: 'w' }], sessionsByWorkspace: { w: [{ threadId: 's' }] },
  }
  store = new workbenchStore.constructor()
  store.generation = 'g'
  store.patch({ bootstrap, selectedWorkspaceId: 'w', selectedSessionId: 's', session: session(), liveSessions: {} }, false)
})

test('legacy drafts survive view persistence and newer per-task drafts take precedence', () => {
  const key = 'singularity.workbench.view.v1'
  localStorage.setItem(key, JSON.stringify({ version: 1, drafts: { old: 'unsent', newer: 'old text' } }))
  localStorage.setItem(`${key}:draft:newer`, 'new text')
  const restored = new workbenchStore.constructor()
  assert.deepEqual(restored.getSnapshot().drafts, { old: 'unsent', newer: 'new text' })
  restored.setTheme('dark')
  assert.equal(JSON.parse(localStorage.getItem(key)).drafts, undefined)
  assert.deepEqual(new workbenchStore.constructor().getSnapshot().drafts, { old: 'unsent', newer: 'new text' })
})

test('a failed draft migration leaves its original container recoverable', () => {
  const key = 'singularity.workbench.view.v1'
  const original = JSON.stringify({ version: 1, drafts: { old: 'unsent' } })
  localStorage.setItem(key, original)
  const restored = new workbenchStore.constructor()
  const setItem = localStorage.setItem
  localStorage.setItem = (name, value) => {
    if (name.startsWith(`${key}:draft:`)) throw new Error('storage full')
    setItem(name, value)
  }
  restored.setTheme('dark')
  assert.equal(localStorage.getItem(key), original)
  assert.equal(new workbenchStore.constructor().getSnapshot().drafts.old, 'unsent')
  localStorage.setItem = setItem
  restored.setTheme('light')
  assert.equal(new workbenchStore.constructor().getSnapshot().drafts.old, 'unsent')
})

test('individual tools preserve order and failure across history recovery', () => {
  const value = session()
  const events = value.runtime.activeTurn.events
  const attempt = ordinal => ({ method: 'provider/attempt', params: { turnId: 't', modelTurnOrdinal: ordinal, attempt: 1, status: 'started' } })
  const start = id => ({ method: 'tool/execution/start', params: { turnId: 't', toolCallId: id, toolName: 'read', args: { path: id } } })
  const end = id => ({ method: 'tool/execution/end', params: { turnId: 't', toolCallId: id, toolName: 'read', result: { content: [{ text: id }], isError: id === 'b' } } })
  events.push(attempt(1), start('a'), end('a'))
  const first = buildTimeline(value)[0]
  assert.equal(first.status, 'completed')
  assert.deepEqual(first.tool.args, { path: 'a' })
  assert.equal(first.tool.output, 'a')
  value.runtime.activeTurn.events = [...events, start('b'), end('b'), attempt(2), start('c')]
  const live = buildTimeline(value)
  assert.deepEqual(live.map(tool => tool.body), ['a', 'b', 'c'])
  assert.equal(live[1].status, 'failed')
  assert.equal(live[2].status, 'running')
  const toolEntries = buildTrajectory(value).flatMap(turn => turn.entries).filter(item => item.kind === 'tool')
  assert.equal(toolEntries.length, 3)
  assert.deepEqual(toolEntries[0].input, { path: 'a' })
  assert.equal(toolEntries[0].text, 'a')
  value.runtime.activeTurn.events = [...value.runtime.activeTurn.events, { method: 'turn/completed', params: { turn: { turnId: 't', status: 'interrupted' } } }]
  assert.equal(buildTimeline(value).at(-1).kind, 'terminal')
  assert.equal(buildTrajectory(value).flatMap(turn => turn.entries).find(item => item.id === 'c').status, 'cancelled')
  value.runtime.activeTurn = null
  value.history.turns = [{ turnId: 't', status: 'interrupted', items: [
    { type: 'request', id: 'r1', observation: { ordinal: 1, attempt: 1 } },
    { type: 'tool_call', id: 'a', name: 'read', args: { path: 'a' } },
    { type: 'tool_call', id: 'b', name: 'read', args: { path: 'b' } },
    { type: 'tool_result', id: 'a', output: 'a', isError: false },
    { type: 'tool_result', id: 'b', output: 'b', isError: true },
    { type: 'request', id: 'r2', observation: { ordinal: 2, attempt: 1 } },
    { type: 'tool_call', id: 'c', name: 'read', args: { path: 'c' } },
  ] }]
  const recovered = buildTimeline(value)
  assert.deepEqual(recovered.slice(0, 3).map(tool => tool.key), live.map(tool => tool.key))
  assert.equal(recovered[1].status, 'failed')
})

test('workspace appearance and trajectory panel survive reload independently of session content', () => {
  const originalSession = store.getSnapshot().session
  store.setWorkspaceAppearance('w', { icon: 'code', color: '#3b82f6' })
  store.setWorkspaceAppearance('another', { icon: 'book', color: '#ec72ad' })
  store.setTrajectoryOpen(true)
  store.setSidebarWidth(300)
  const restored = new workbenchStore.constructor().getSnapshot()
  assert.deepEqual(restored.workspaceAppearance, {
    w: { icon: 'code', color: '#3b82f6' },
    another: { icon: 'book', color: '#ec72ad' },
  })
  assert.equal(restored.trajectoryOpen, true)
  assert.equal(restored.sidebarWidth, 300)
  assert.equal(store.getSnapshot().session, originalSession)
})

test('request lookup and prompt head survive completion and history reload without full context', () => {
  const snapshot = {
    request_id: 'request', messages: [{ role: 'system', content: 'system prompt' }],
    tools: [{ name: 'read', description: 'Read a file', parameters_schema: { type: 'object' } }], model_preferences: {},
  }
  const value = session()
  value.runtime.activeTurn.events = [
    { method: 'provider/attempt', params: { turnId: 't', modelTurnOrdinal: 1, attempt: 1, status: 'started', provider: 'p', model: 'm', requestId: 'lookup-request', requestHead: snapshot } },
    { method: 'item/agentMessage/delta', params: { turnId: 't', item: { itemId: 'answer' }, delta: 'answer' } },
    { method: 'provider/attempt', params: { turnId: 't', modelTurnOrdinal: 1, attempt: 1, requestId: 'lookup-request', status: 'ok', provider: 'p', model: 'm', attemptDurationMs: 100 } },
  ]
  const live = buildTrajectory(value)[0].entries
  assert.equal(live[0].kind, 'system')
  assert.deepEqual(live[1].request.requestHead, snapshot)
  assert.equal(live[1].text, 'answer')
  assert.equal(live[1].request.request, undefined)
  assert.equal(live[1].request.requestId, 'lookup-request')
  assert.equal(live[1].duration, 100)
  value.runtime.activeTurn = null
  value.history.turns = [{ turnId: 't', items: [
    { type: 'request', id: 'persisted', observation: live[1].request },
    { type: 'message', id: 'assistant', role: 'assistant', text: 'answer' },
    { type: 'tool_call', id: 'tool', name: 'read', args: {} },
    { type: 'tool_result', id: 'tool', output: 'file', isError: false, durationMs: 23 },
  ] }]
  const restored = buildTrajectory(value)[0].entries
  assert.equal(restored[1].id, live[1].id)
  assert.deepEqual(restored[1].request.requestHead, snapshot)
  assert.deepEqual(restored[2].schema, snapshot.tools[0])
  assert.equal(restored[2].duration, 23)
})

test('unfinished historical requests follow runtime liveness without changing durable observations', () => {
  const value = session()
  const observation = { requestId: 'unfinished', ordinal: 1, attempt: 1, provider: 'p', model: 'm', status: 'started', durationMs: 0 }
  value.history.turns = [{ turnId: 't', items: [{ type: 'request', id: 'r', timestamp: '2026-09-05T00:00:01Z', observation }] }]
  const request = () => buildTrajectory(value)[0].entries[0]
  assert.equal(request().status, 'running')
  assert.equal(request().duration, null)
  value.runtime.activeTurn = null
  assert.equal(request().status, 'cancelled')
  assert.equal(observation.status, 'started')
  value.runtime.activeTurn = { turnId: 'next', events: [] }
  assert.equal(request().status, 'cancelled')
  value.runtime.activeTurn = null
  value.runtime.activeCompaction = { startedAt: '2026-09-05T00:00:00Z' }
  assert.equal(request().status, 'cancelled')
  value.history.turns = [{ turnId: null, items: [{ type: 'request', id: 'c', timestamp: '2026-09-05T00:00:01Z', observation: { ...observation, purpose: 'compaction' } }] }]
  assert.equal(request().status, 'running')
  value.runtime.activeCompaction = { startedAt: '2026-09-05T00:00:02Z' }
  assert.equal(request().status, 'cancelled')
  value.runtime.activeCompaction = null
  assert.equal(request().status, 'cancelled')
})

test('structured file changes share statistics and rendered hunks across live and recovered views', () => {
  const value = session()
  const diff = createPatch('file.txt', '--old\n', '++new\n')
  const output = 'Successfully edited file.txt'
  value.runtime.activeTurn.events = [
    { method: 'tool/execution/start', params: { turnId: 't', toolCallId: 'edit', toolName: 'edit', args: { path: 'file.txt' } } },
    { method: 'tool/execution/end', params: { turnId: 't', toolCallId: 'edit', toolName: 'edit', result: { content: [{ text: output }], diff, isError: false } } },
  ]
  const live = buildTimeline(value)[0]
  assert.deepEqual([live.addedLines, live.removedLines], [1, 1])
  assert.deepEqual(live.tool.patches[0].hunks[0].lines, ['---old', '+++new'])
  assert.equal(live.tool.output, output)
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
  assert.equal(buildTimeline(value)[0].tool.diff, '', 'ordinary output never impersonates structured file changes')
})

test('bootstrap refresh does not consume unapplied stream events', async () => {
  store.connection.rpc = async () => ({ ...bootstrap, revision: 1 })
  await store.refreshBootstrap()
  store.applyFrame(frame(1, 'new delta'))
  assert.equal(store.state.session.runtime.activeTurn.events.length, 1)
})

test('older session read cannot overwrite an already applied delta', async () => {
  let resolveRead
  store.connection.rpc = () => new Promise(resolve => { resolveRead = resolve })
  const reading = store.readSession('w', 's')
  store.applyFrame(frame(1, 'newer'))
  resolveRead(session())
  await reading
  assert.equal(store.state.session.runtime.activeTurn.events.length, 1)
  assert.equal(store.revision, 1)
})

test('snapshot watermark suppresses buffered events already covered by the read', async () => {
  let resolveRead
  store.connection.rpc = () => new Promise(resolve => { resolveRead = resolve })
  const reading = store.readSession('w', 's')
  store.onFrame(frame(1, 'covered by snapshot'))
  const newer = session()
  newer.runtime.sessionRevision = 2
  newer.runtime.phase = 'stopping'
  resolveRead(newer)
  await reading
  assert.equal(store.state.session.runtime.sessionRevision, 2)
  assert.equal(store.state.session.runtime.activeTurn.events.length, 0)
  assert.equal(store.state.session.runtime.phase, 'stopping')
  assert.equal(store.state.liveSessions.s.phase, 'stopping')
})

test('pagination stays continuous across a settled-turn refresh and keeps the loaded prefix', async () => {
  store.state.session = historyPage(41, 80)
  let resolveOlder
  store.connection.rpc = async (_method, params) => params.beforeTurn !== null
    ? new Promise(resolve => { resolveOlder = resolve }) : historyPage(42, 81)
  const loading = store.readOlder()
  await store.readSession('w', 's')
  resolveOlder(historyPage(1, 40, 80))
  await loading
  assert.deepEqual(store.state.session.history.turns.map(turn => turn.turnId), historyPage(1, 81).history.turns.map(turn => turn.turnId))
  assert.equal(store.state.session.history.nextCursor, null)
  assert.equal(store.state.session.summary.turnCount, 81)
  assert.equal(store.state.session.history.summary.turnCount, 81)
  store.connection.rpc = async () => historyPage(43, 82)
  await store.readSession('w', 's')
  assert.equal(store.state.session.history.turns.length, 82)
  assert.equal(store.state.session.history.nextCursor, null)
})

test('late pagination cannot hide a gap after the history window has moved beyond overlap', async () => {
  store.state.session = historyPage(41, 80)
  let resolveOlder
  store.connection.rpc = async (_method, params) => params.beforeTurn !== null
    ? new Promise(resolve => { resolveOlder = resolve }) : historyPage(101, 140)
  const loading = store.readOlder()
  await store.readSession('w', 's')
  resolveOlder(historyPage(1, 40, 80))
  await loading
  assert.deepEqual(store.state.session.history, historyPage(101, 140).history)
})

test('a late settings receipt cannot overwrite a newer authoritative model selection', async () => {
  let resolveSave
  store.connection.rpc = () => new Promise(resolve => { resolveSave = resolve })
  const saving = store.updateSettings('p/a')
  for (const [revision, selector] of [[1, 'p/a'], [2, 'p/b']]) {
    store.onFrame({ generation: 'g', revision, sessionId: 's', type: 'session_changed', payload: { ...runtime(), sessionRevision: revision, selector } })
  }
  resolveSave({ accepted: true, generation: 'g', revision: 1, sessionId: 's' })
  assert.equal(await saving, true)
  assert.equal(store.state.session.runtime.selector, 'p/b')
  assert.equal(store.state.session.runtime.sessionRevision, 2)
})

test('late bootstrap response cannot replace a newer streamed title', async () => {
  let resolveBootstrap
  store.connection.rpc = () => new Promise(resolve => { resolveBootstrap = resolve })
  const refreshing = store.refreshBootstrap()
  store.onFrame({
    generation: 'g', revision: 1, type: 'workbench_changed',
    payload: { ...bootstrap, revision: 0, sessionsByWorkspace: { w: [{ threadId: 's', title: 'new title' }] } },
  })
  resolveBootstrap({ ...bootstrap, sessionsByWorkspace: { w: [{ threadId: 's', title: 'old title' }] } })
  await refreshing
  assert.equal(store.state.bootstrap.sessionsByWorkspace.w[0].title, 'new title')
  assert.equal(store.revision, 1)
})

test('late deltas retain stopping in both selected session and sidebar', () => {
  store.state.session.runtime.phase = 'stopping'
  store.updateLiveSession('s', store.state.session.runtime)
  store.applyFrame(frame(1, 'late delta'))
  assert.equal(store.state.session.runtime.phase, 'stopping')
  assert.equal(store.state.liveSessions.s.phase, 'stopping')
})

test('background session retains stopping and rejects older snapshots', () => {
  store.updateLiveSession('other', { sessionRevision: 3, phase: 'stopping', terminal: null })
  store.applyFrame({ ...frame(1, 'late delta'), sessionId: 'other', payload: { ...frame(1, '').payload, sessionRevision: 4 } })
  store.applyFrame({ generation: 'g', revision: 2, sessionId: 'other', type: 'session_changed', payload: { ...runtime(), sessionRevision: 2 } })
  assert.equal(store.state.liveSessions.other.phase, 'stopping')
  assert.equal(store.state.liveSessions.other.sessionRevision, 4)
})

test('late session creation cannot change selection in another workspace', async () => {
  let resolveCreate
  store.connection.rpc = method => method === 'session.create'
    ? new Promise(resolve => { resolveCreate = resolve }) : Promise.resolve(bootstrap)
  store.patch({ session: null }, false)
  const creating = store.createSession()
  store.patch({ selectedWorkspaceId: 'another-workspace', selectedSessionId: 'another-session' }, false)
  resolveCreate(session())
  await creating
  assert.equal(store.state.selectedWorkspaceId, 'another-workspace')
  assert.equal(store.state.selectedSessionId, 'another-session')
})

test('typing during new-task creation stays in the new draft and preserves the previous task', async () => {
  let resolveCreate
  store.setDraft('previous task draft')
  store.connection.rpc = method => method === 'session.create'
    ? new Promise(resolve => { resolveCreate = resolve }) : Promise.resolve(bootstrap)
  const creating = store.createSession()
  assert.equal(store.state.selectedSessionId, null)
  store.setDraft('typed while creating')
  const created = session()
  created.summary.threadId = 'new-session'
  resolveCreate(created)
  assert.equal(await creating, true)
  assert.equal(store.draft(), 'typed while creating')
  assert.equal(store.state.drafts.s, 'previous task draft')
  assert.equal(store.state.drafts['new:w'], '')
})

test('sending from a workspace without a task creates it and sends the retained draft once', async () => {
  const calls = []
  const created = session()
  created.summary.threadId = 'created'
  created.runtime.phase = 'idle'
  store.patch({ selectedSessionId: null, session: null, connection: 'ready' }, false)
  store.setDraft('first message')
  store.connection.rpc = async (method, params) => {
    calls.push([method, params])
    if (method === 'session.create') return created
    if (method === 'workbench.bootstrap') return bootstrap
    return {}
  }
  assert.equal(await store.submitDraft(), true)
  assert.deepEqual(calls.filter(([method]) => method === 'session.submit'), [['session.submit', { workspaceId: 'w', sessionId: 'created', text: 'first message' }]])
  assert.equal(store.draft(), '')
})

test('stopping, compacting and reserved phases retain input without dispatching', async () => {
  store.patch({ connection: 'ready' }, false)
  store.setDraft('keep this draft')
  store.connection.rpc = async () => assert.fail('a blocked phase must not dispatch')
  for (const phase of ['stopping', 'compacting', 'reserved']) {
    store.state.session.runtime.phase = phase
    assert.equal(await store.submitDraft(), false)
    assert.equal(store.draft(), 'keep this draft')
  }
})

test('a disconnected action preserves the draft without a persistent error or replay', async () => {
  store.patch({ connection: 'ready' }, false)
  store.state.session.runtime.phase = 'idle'
  store.setDraft('keep this draft')
  let calls = 0
  store.connection.rpc = async () => { calls++; throw new RpcFailure('unavailable', 'offline', 'retry later') }
  assert.equal(await store.submitDraft(), false)
  assert.equal(store.draft(), 'keep this draft')
  assert.equal(store.state.actionError, null)
  assert.deepEqual(store.state.actionErrors, {})
  assert.equal(calls, 1)
})

test('model selection before the first message creates a task without losing its draft', async () => {
  const calls = []
  const created = session()
  created.runtime.phase = 'idle'
  created.summary.threadId = 'model-task'
  store.patch({ selectedSessionId: null, session: null }, false)
  store.setDraft('retain before model selection')
  store.connection.rpc = async (method, params) => {
    calls.push([method, params])
    if (method === 'session.create') return created
    if (method === 'workbench.bootstrap') return bootstrap
    store.onFrame({ generation: 'g', revision: 1, sessionId: 'model-task', type: 'session_changed', payload: { ...created.runtime, sessionRevision: 1, selector: params.selector } })
    return { accepted: true, generation: 'g', revision: 1, sessionId: 'model-task' }
  }
  assert.equal(await store.updateSettings('aliyun/qwen3.8-flash#medium'), true)
  assert.equal(store.draft(), 'retain before model selection')
  assert.equal(store.state.session.runtime.selector, 'aliyun/qwen3.8-flash#medium')
  assert.equal(calls.filter(([method]) => method === 'session.create').length, 1)
  assert.equal(calls.filter(([method]) => method === 'session.updateSettings').length, 1)
})

test('runtime failure does not add a conversation banner', () => {
  const value = session()
  value.runtime.activeTurn = null
  value.runtime.terminal = { status: 'failed', message: 'Provider rejected the request' }
  value.history.turns = [{ turnId: 't', status: 'failed', items: [] }]
  const terminals = buildTimeline(value).filter(item => item.kind === 'terminal')
  assert.equal(terminals.length, 0)
  assert.equal(buildTrajectory(value).at(-1).entries.at(-1).text, 'Provider rejected the request')
})

test('live diagnostics and request failures remain in trajectory only', () => {
  const value = session()
  value.runtime.activeTurn.events = [
    { method: 'agent/diagnostic', params: { turnId: 't', severity: 'warning', message: 'Retrying request' } },
    { method: 'provider/attempt', params: { turnId: 't', modelTurnOrdinal: 0, attempt: 1, provider: 'fixture', model: 'test', status: 'error', errorCategory: 'connection' } },
    { method: 'turn/error', params: { turnId: 't', error: { stage: 'agent_loop', cause: 'provider_network', message: 'Request failed' } } },
  ]
  assert.equal(buildTimeline(value).length, 0)
  const entries = buildTrajectory(value).flatMap(turn => turn.entries)
  assert.ok(entries.some(item => item.text === 'Retrying request'))
  assert.ok(entries.some(item => item.request?.error === 'connection'))
  assert.ok(entries.some(item => item.text.includes('Request failed')))
})

test('independent stores preserve each other\'s session drafts', () => {
  const other = new workbenchStore.constructor()
  store.setDraftFor('session-a', 'draft a')
  other.setDraftFor('session-b', 'draft b')
  const reopened = new workbenchStore.constructor()
  assert.equal(reopened.state.drafts['session-a'], 'draft a')
  assert.equal(reopened.state.drafts['session-b'], 'draft b')
})

test('history and active snapshot show overlapping user input only once', () => {
  const overlap = session()
  overlap.history.turns = [{ turnId: 't', status: 'running', items: [{ type: 'message', id: 'u', role: 'user', text: 'hello' }] }]
  assert.equal(buildTimeline(overlap).filter(item => item.kind === 'user' && item.body === 'hello').length, 1)
  overlap.runtime.activeTurn.events = [{ method: 'turn/started', params: { turn: { turnId: 't', status: 'running' }, input: 'hello' } }]
  assert.equal(buildTrajectory(overlap)[0].entries.filter(item => item.kind === 'user').length, 1)
})

test('failed write does not fabricate an applied diff', () => {
  const failed = session()
  failed.runtime.activeTurn = null
  failed.history.turns = [{ turnId: 't', status: 'failed', items: [
    { type: 'tool_call', id: 'call', name: 'write', args: { path: 'a.txt', content: 'never written' } },
    { type: 'tool_result', id: 'call', output: 'Permission denied', isError: true },
  ] }]
  const item = buildTimeline(failed).find(item => item.kind === 'diff')
  assert.equal(item.tool.diff.includes('never written'), false)
})

test('streamed tool lifecycle coalesces into one item and projection is repeatable', () => {
  const live = session()
  live.runtime.activeTurn.events = [
    { method: 'turn/started', params: { turnId: 'unique', input: 'one input' } },
    { method: 'item/started', params: { turnId: 'unique', item: { itemId: 'call' } } },
    { method: 'tool/execution/start', params: { turnId: 'unique', toolCallId: 'call', toolName: 'write', args: { path: 'a.txt', content: 'saved' } } },
    { method: 'tool/execution/end', params: { turnId: 'unique', toolCallId: 'call', toolName: 'write', result: { content: [{ text: 'Successfully wrote a.txt' }], diff: '--- a.txt\n+++ a.txt\n@@ -0,0 +1 @@\n+saved\n', isError: false } } },
    { method: 'item/completed', params: { turnId: 'unique', item: { itemId: 'call' } } },
  ]
  const projected = buildTimeline(live)
  assert.equal(projected.filter(item => item.key === 'content:unique:call').length, 1)
  assert.equal(projected.filter(item => item.kind === 'unknown').length, 0)
  assert.equal(projected.find(item => item.kind === 'diff').addedLines, 1)
  assert.equal(projected.filter(item => item.kind === 'user').length, 1)
  live.runtime.activeTurn.events = [...live.runtime.activeTurn.events,
    { method: 'item/agentMessage/delta', params: { turnId: 'unique', item: { itemId: 'answer' }, delta: 'a' } },
  ]
  assert.equal(buildTimeline(live).find(item => item.kind === 'assistant').body, 'a')
})


test('streaming thinking and separate model replies retain order and identity after history reload', () => {
  const value = session()
  const event = (method, itemId, data) => ({ method, params: { turnId: 't', item: { itemId }, ...data } })
  value.runtime.activeTurn.events = [
    { method: 'provider/attempt', params: { turnId: 't', modelTurnOrdinal: 1, attempt: 1, status: 'started', provider: 'p', model: 'm' } },
    event('item/started', 'm1:thinking:0', {}),
    event('item/agentThinking/delta', 'm1:thinking:0', { delta: '先检查' }),
    event('item/agentThinking/delta', 'm1:thinking:0', { delta: '实现' }),
  ]
  const liveThinking = buildTimeline(value)[0]
  assert.equal(liveThinking.body, '先检查实现')
  assert.equal(liveThinking.status, 'running')
  assert.equal(buildTrajectory(value)[0].entries[0].thinking, '先检查实现')
  value.runtime.activeTurn.events = [...value.runtime.activeTurn.events,
    event('item/started', 'm1:text:0', {}),
    event('item/agentMessage/delta', 'm1:text:0', { delta: '读取文件。' }),
    event('item/agentThinking', 'm1:thinking:0', { text: '先检查实现' }),
    event('item/completed', 'm1:thinking:0', {}),
    event('item/completed', 'm1:text:0', {}),
    { method: 'tool/execution/start', params: { turnId: 't', toolCallId: 'call1', toolName: 'read', args: { path: 'a.txt' } } },
    { method: 'tool/execution/end', params: { turnId: 't', toolCallId: 'call1', toolName: 'read', result: { content: [{ text: 'contents' }], isError: false } } },
    event('item/started', 'm2:text:0', {}),
    event('item/agentMessage/delta', 'm2:text:0', { delta: '现在完成。' }),
    event('item/completed', 'm2:text:0', {}),
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


test('context occupancy uses per-request measured input during a turn and after recovery', () => {
  const value = session()
  value.runtime.selector = 'p/m#high'
  const catalog = { providers: [{ providerId: 'p', models: [{ modelId: 'm', maxContextTokens: 1000 }] }] }
  assert.equal(contextOccupancy(value, catalog), null)
  const request = { turnId: 't', provider: 'p', model: 'm', inputTokens: 120, outputTokens: 70, cachedInputTokens: 30, status: 'ok', modelTurnOrdinal: 1, attempt: 1 }
  value.runtime.activeTurn.events = appendEvent(value.runtime.activeTurn.events, { method: 'provider/attempt', params: request })
  assert.deepEqual(contextOccupancy(value, catalog), { used: 120, capacity: 1000, percent: 12 })
  assert.equal(buildTrajectory(value)[0].entries[0].request.inputTokens, 120)
  value.runtime.activeTurn.events = appendEvent(value.runtime.activeTurn.events, { method: 'provider/attempt', params: { ...request, inputTokens: null, status: 'started', attempt: 2 } })
  assert.equal(contextOccupancy(value, catalog).used, 120)
  value.runtime.activeTurn.events = appendEvent(value.runtime.activeTurn.events, { method: 'provider/attempt', params: { ...request, purpose: 'compaction', inputTokens: 900, attempt: 3 } })
  assert.equal(contextOccupancy(value, catalog), null, 'summary input is not the active conversation size')
  value.runtime.activeTurn = null
  value.history.turns = [{ items: [{ type: 'request', observation: request }] }]
  assert.equal(contextOccupancy(value, catalog).used, 120)
  value.runtime.selector = 'p/other'
  assert.equal(contextOccupancy(value, catalog), null)
  value.runtime.selector = 'p/m'
  value.history.turns = [{ ...value.history.turns[0], items: [...value.history.turns[0].items, { type: 'compaction', summary: 'short' }] }]
  assert.equal(contextOccupancy(value, catalog), null)
  value.history.turns = [{ ...value.history.turns[0], items: [...value.history.turns[0].items, { type: 'request', observation: { ...request, inputTokens: 50 } }] }]
  assert.equal(contextOccupancy(value, catalog).used, 50)
  assert.equal(contextOccupancy(value, { providers: [] }), null)
})

test('background completion reminders clear on opening and do not mark old or current sessions', async () => {
  store.patch({ liveSessions: { s: { sessionRevision: 1, phase: 'idle', terminal: null }, other: { sessionRevision: 1, phase: 'idle', terminal: null } } }, false)
  assert.equal(store.state.unreadSessions.size, 0)
  store.updateLiveSession('other', { sessionRevision: 2, phase: 'running', terminal: null })
  store.updateLiveSession('other', { sessionRevision: 3, phase: 'idle', terminal: { status: 'failed' } })
  assert.equal(store.state.unreadSessions.has('other'), true)
  store.connection.rpc = async () => ({ ...session(), summary: { threadId: 'other' } })
  store.state.bootstrap.sessionsByWorkspace.w.push({ threadId: 'other' })
  await store.selectSession('other')
  assert.equal(store.state.unreadSessions.has('other'), false)
  store.updateLiveSession('other', { sessionRevision: 4, phase: 'running', terminal: null })
  store.updateLiveSession('other', { sessionRevision: 5, phase: 'idle', terminal: { status: 'completed' } })
  assert.equal(store.state.unreadSessions.size, 0)
  store.patch({ selectedSessionId: 's' }, false)
  store.updateLiveSession('other', { sessionRevision: 6, phase: 'running', terminal: null })
  store.updateLiveSession('other', { sessionRevision: 7, phase: 'idle', terminal: { status: 'completed' } })
  assert.equal(store.state.unreadSessions.has('other'), true)
  store.updateLiveSession('other', { sessionRevision: 8, phase: 'running', terminal: null })
  assert.equal(store.state.unreadSessions.has('other'), false)
})

test('a tool appears with its input before any result or update arrives', () => {
  const value = session()
  value.runtime.activeTurn.events = [
    { method: 'provider/attempt', params: { turnId: 't', modelTurnOrdinal: 1, attempt: 1, status: 'started' } },
    { method: 'item/started', params: { turnId: 't', item: { itemId: 'slow' } } },
    { method: 'tool/execution/start', params: { turnId: 't', toolCallId: 'slow', toolName: 'bash', args: { command: 'sleep 30' } } },
  ]
  const tools = buildTimeline(value)
  assert.equal(tools.length, 1)
  assert.equal(tools[0].title, 'bash')
  assert.equal(tools[0].body, 'sleep 30')
  assert.equal(tools[0].status, 'running')
})

test('draft transfer does not overwrite the draft of an existing empty task', async () => {
  store.state.bootstrap.sessionsByWorkspace.w = [{ threadId: 'target', turnCount: 0, status: null }]
  store.setDraftFor('target', 'valuable target draft')
  store.setDraft('source draft')
  const created = session()
  created.summary.threadId = 'created'
  const calls = []
  store.connection.rpc = async method => {
    calls.push(method)
    return method === 'session.create' ? created : bootstrap
  }
  assert.equal(await store.createSession('w', true), true)
  assert.ok(calls.includes('session.create'))
  assert.equal(store.state.drafts.target, 'valuable target draft')
  assert.equal(store.draft(), 'source draft')
  assert.equal(store.state.drafts.s, '')
})


test('running input defaults to queue after a one-off steer', async () => {
  store.state.connection = 'ready'
  const calls = []
  store.connection.rpc = async (method, params) => { calls.push({ method, params }); return {} }
  for (const intent of [undefined, 'steer', undefined]) {
    store.setDraft('message')
    assert.equal(await store.submitDraft(intent), true)
  }
  assert.deepEqual(calls.map(call => call.method), ['session.followUp', 'session.steer', 'session.followUp'])
})


test('Rust event goldens satisfy the frontend event contract', async () => {
  const { readFileSync, writeFileSync, unlinkSync } = await import('node:fs')
  const { execFileSync } = await import('node:child_process')
  const { fileURLToPath } = await import('node:url')
  const source = readFileSync(new URL('../../../protocol/tests/contract.rs', import.meta.url), 'utf8')
  const table = source.slice(source.indexOf('let cases:'), source.indexOf('for (method, event, jsonl_params)'))
  const events = [...table.matchAll(/\(\s*"([^"]+)",[\s\S]*?r#"([\s\S]*?)"#,/g)].map(([, method, raw]) => {
    const params = JSON.parse(raw)
    if (method === 'turn/started' || method === 'tool/execution/start') params.startedAt = '2026-09-08T00:00:00Z'
    return { method, params, sessionRevision: 1 }
  })
  assert.ok(events.length >= 14, 'all Rust golden cases must be read')
  const fixture = new URL(`../.protocol-contract-${process.pid}.ts`, import.meta.url)
  try {
    writeFileSync(fixture, `import type { TurnEventEnvelope } from './src/protocol'\nconst events = ${JSON.stringify(events)} satisfies TurnEventEnvelope[]\n`)
    execFileSync(process.execPath, [fileURLToPath(new URL('../node_modules/typescript/bin/tsc', import.meta.url)),
      '--ignoreConfig', '--noEmit', '--strict', '--skipLibCheck', '--target', 'esnext', '--module', 'esnext', '--moduleResolution', 'bundler', fileURLToPath(fixture)], { stdio: 'pipe', encoding: 'utf8' })
  } finally { unlinkSync(fixture) }
})

test('event suffixes preserve previous snapshots and incremental projections', () => {
  const value = session()
  const first = { method: 'item/agentMessage/delta', params: { turnId: 't', item: { itemId: 'a' }, delta: 'first' } }
  value.runtime.activeTurn.events = appendEvent([], first)
  const oldEvents = value.runtime.activeTurn.events
  const oldTrajectory = JSON.stringify(buildTrajectory(value))
  const oldTimeline = JSON.stringify(buildTimeline(value))
  for (let i = 0; i < 10000; i++) {
    value.runtime.activeTurn.events = appendEvent(value.runtime.activeTurn.events, { method: 'item/agentMessage/delta', params: { turnId: 't', item: { itemId: 'a' }, delta: '.' } })
    buildTrajectory(value)
    buildTimeline(value)
  }
  assert.equal(oldEvents.length, 1)
  assert.deepEqual([...oldEvents], [first])
  assert.equal([...eventsSince(value.runtime.activeTurn.events, 9999)].length, 2)
  const incrementalTrajectory = buildTrajectory(value)
  assert.equal(incrementalTrajectory[0].entries.at(-1).text.length, 10005)
  const incrementalTimeline = buildTimeline(value)
  const fresh = structuredClone({ ...value, runtime: { ...value.runtime, activeTurn: { ...value.runtime.activeTurn, events: [...value.runtime.activeTurn.events] } } })
  assert.deepEqual(buildTrajectory(fresh), incrementalTrajectory)
  assert.deepEqual(buildTimeline(fresh), incrementalTimeline)
  value.runtime.activeTurn.events = oldEvents
  assert.equal(JSON.stringify(buildTrajectory(value)), oldTrajectory)
  assert.equal(JSON.stringify(buildTimeline(value)), oldTimeline)
  assert.equal(JSON.parse(JSON.stringify(new EventLog([first]))).length, 1)
})

test('sidebar subscription ignores event revisions but observes lifecycle changes', () => {
  const initial = { ...store.state, liveSessions: { s: { sessionRevision: 1, phase: 'running', terminal: null } } }
  const delta = { ...initial, liveSessions: { s: { ...initial.liveSessions.s, sessionRevision: 2 } } }
  assert.equal(sameWorkbenchFields(initial, delta, ['liveSessions']), true)
  assert.equal(sameWorkbenchFields(initial, { ...delta, liveSessions: { s: { ...delta.liveSessions.s, phase: 'stopping' } } }, ['liveSessions']), false)
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
