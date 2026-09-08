import assert from 'node:assert/strict'
import { beforeEach, test } from 'node:test'
import { workbenchStore } from '../src/store.ts'
import { buildTimeline, groupTimelineTools } from '../src/timeline.ts'
import { buildTrajectory } from '../src/trajectory.ts'
import { contextOccupancy } from '../src/contextUsage.ts'
import { reasoningChoices } from '../src/modelChoices.ts'
import { WorkbenchConnection, RpcFailure } from '../src/connection.ts'
import { protocolVersion } from '../src/protocol.ts'

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

test('active trajectory updates each request and tool in place', () => {
  const value = session()
  value.runtime.activeTurn.events = [
    { method: 'provider/attempt', params: { modelTurnOrdinal: 1, attempt: 1, status: 'started', provider: 'p', model: 'm' } },
    { method: 'provider/attempt', params: { modelTurnOrdinal: 1, attempt: 1, status: 'ok', provider: 'p', model: 'm', attemptDurationMs: 100 } },
    { method: 'tool/execution/start', params: { toolCallId: 'c', toolName: 'read', args: { path: 'a' } } },
    { method: 'tool/execution/end', params: { toolCallId: 'c', toolName: 'read', result: { content: [{ text: 'contents' }], isError: false } } },
  ]
  const entries = buildTrajectory(value)[0].entries
  assert.equal(entries.length, 2)
  assert.equal(entries[0].duration, 100)
  assert.deepEqual(entries[1].input, { path: 'a' })
  assert.match(entries[1].text, /contents/)
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

test('tool batches use request boundaries without thinking and survive history recovery', () => {
  const value = session()
  const events = value.runtime.activeTurn.events
  const attempt = ordinal => ({ method: 'provider/attempt', params: { turnId: 't', modelTurnOrdinal: ordinal, attempt: 1, status: 'started' } })
  const start = id => ({ method: 'tool/execution/start', params: { turnId: 't', toolCallId: id, toolName: 'read', args: { path: id } } })
  const end = id => ({ method: 'tool/execution/end', params: { turnId: 't', toolCallId: id, toolName: 'read', result: { content: [{ text: id }], isError: id === 'b' } } })
  events.push(attempt(1), start('a'), end('a'))
  assert.equal(groupTimelineTools(buildTimeline(value))[0].tools[0].requestRunning, true)
  value.runtime.activeTurn.events = [...events, start('b'), end('b'), attempt(2), start('c')]
  const live = groupTimelineTools(buildTimeline(value))
  assert.deepEqual(live.map(group => group.tools.map(tool => tool.body)), [['a', 'b'], ['c']])
  assert.equal(live[0].tools[0].requestRunning, false)
  assert.equal(live[0].tools[1].status, 'failed')
  assert.equal(live[1].tools[0].requestRunning, true)
  value.runtime.activeTurn.events = [...value.runtime.activeTurn.events, { method: 'turn/completed', params: { turn: { turnId: 't', status: 'interrupted' } } }]
  assert.equal(groupTimelineTools(buildTimeline(value))[1].tools[0].requestRunning, false)
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
  const recovered = groupTimelineTools(buildTimeline(value))
  assert.deepEqual(recovered.slice(0, 2).map(group => group.key), live.map(group => group.key))
  assert.deepEqual(recovered[0].tools.map(tool => tool.toolRequest), live[0].tools.map(tool => tool.toolRequest))
  assert.equal(recovered[0].tools[1].status, 'failed')
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

test('request context survives completion and history reload with stable selection IDs', () => {
  const snapshot = {
    request_id: 'request', messages: [{ role: 'system', content: 'system prompt' }, { role: 'user', content: 'inspect' }],
    tools: [{ name: 'read', description: 'Read a file', parameters_schema: { type: 'object' } }], model_preferences: {},
  }
  const value = session()
  value.runtime.activeTurn.events = [
    { method: 'provider/attempt', params: { modelTurnOrdinal: 1, attempt: 1, status: 'started', provider: 'p', model: 'm', request: snapshot } },
    { method: 'item/agentMessage/delta', params: { delta: 'answer' } },
    { method: 'provider/attempt', params: { modelTurnOrdinal: 1, attempt: 1, status: 'ok', provider: 'p', model: 'm', attemptDurationMs: 100 } },
  ]
  const live = buildTrajectory(value)[0].entries
  assert.equal(live[0].kind, 'system')
  assert.deepEqual(live[1].request.request, snapshot)
  assert.equal(live[1].text, 'answer')
  value.runtime.activeTurn = null
  value.history.turns = [{ turnId: 't', items: [
    { type: 'request', id: 'persisted', observation: live[1].request },
    { type: 'message', id: 'assistant', role: 'assistant', text: 'answer' },
    { type: 'tool_call', id: 'tool', name: 'read', args: {} },
    { type: 'tool_result', id: 'tool', output: 'file', isError: false, durationMs: 23 },
  ] }]
  const restored = buildTrajectory(value)[0].entries
  assert.equal(restored[1].id, live[1].id)
  assert.deepEqual(restored[1].request.request, snapshot)
  assert.deepEqual(restored[2].schema, snapshot.tools[0])
  assert.equal(restored[2].duration, 23)
  assert.deepEqual(buildTrajectory(value), buildTrajectory(value))
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
    return { selector: params.selector, applyTiming: 'immediate' }
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
    { method: 'turn/error', params: { turnId: 't', message: 'Request failed' } },
  ]
  assert.equal(buildTimeline(value).length, 0)
  const entries = buildTrajectory(value).flatMap(turn => turn.entries)
  assert.ok(entries.some(item => item.text === 'Retrying request'))
  assert.ok(entries.some(item => item.request?.error === 'connection'))
  assert.ok(entries.some(item => item.text === 'Request failed'))
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
  overlap.runtime.activeTurn.events = [{ method: 'turn/started', params: { turnId: 't', input: 'hello' } }]
  assert.equal(buildTrajectory(overlap)[0].entries.filter(item => item.kind === 'user').length, 1)
})

test('interrupted trajectory does not present an unfinished tool as completed', () => {
  const value = session()
  value.runtime.activeTurn.events = [
    { method: 'tool/execution/start', params: { toolCallId: 'c', toolName: 'bash', args: {} } },
    { method: 'turn/completed', params: { turn: { status: 'interrupted' } } },
  ]
  assert.equal(buildTrajectory(value)[0].entries[0].status, 'cancelled')
})

test('failed write does not fabricate an applied diff', () => {
  const failed = session()
  failed.runtime.activeTurn = null
  failed.history.turns = [{ turnId: 't', status: 'failed', items: [
    { type: 'tool_call', id: 'call', name: 'write', args: { path: 'a.txt', content: 'never written' } },
    { type: 'tool_result', id: 'call', output: 'Permission denied', isError: true },
  ] }]
  const item = buildTimeline(failed).find(item => item.kind === 'diff')
  assert.equal(item.sections.some(section => section.kind === 'diff' && section.content.includes('never written')), false)
})

test('streamed tool lifecycle coalesces into one item and projection is repeatable', () => {
  const live = session()
  live.runtime.activeTurn.events = [
    { method: 'turn/started', params: { turnId: 'unique', input: 'one input' } },
    { method: 'item/started', params: { turnId: 'unique', item: { itemId: 'call' } } },
    { method: 'tool/execution/start', params: { turnId: 'unique', toolCallId: 'call', toolName: 'write', args: { path: 'a.txt', content: 'saved' } } },
    { method: 'tool/execution/end', params: { turnId: 'unique', toolCallId: 'call', toolName: 'write', result: { content: [{ text: '--- a.txt\n+++ a.txt\n@@ -0,0 +1 @@\n+saved\n' }], isError: false } } },
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
  assert.deepEqual(buildTimeline(live), buildTimeline(live))
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
  const request = { provider: 'p', model: 'm', inputTokens: 120, outputTokens: 70, cachedInputTokens: 30, status: 'ok', modelTurnOrdinal: 1, attempt: 1 }
  value.runtime.activeTurn.events.push({ method: 'provider/attempt', params: request })
  assert.deepEqual(contextOccupancy(value, catalog), { used: 120, capacity: 1000, percent: 12 })
  assert.equal(buildTrajectory(value)[0].entries[0].request.inputTokens, 120)
  value.runtime.activeTurn.events.push({ method: 'provider/attempt', params: { ...request, inputTokens: null, status: 'started', attempt: 2 } })
  assert.equal(contextOccupancy(value, catalog).used, 120)
  value.runtime.activeTurn = null
  value.history.turns = [{ items: [{ type: 'request', observation: request }] }]
  assert.equal(contextOccupancy(value, catalog).used, 120)
  value.runtime.selector = 'p/other'
  assert.equal(contextOccupancy(value, catalog), null)
  value.runtime.selector = 'p/m'
  value.history.turns[0].items.push({ type: 'compaction', summary: 'short' })
  assert.equal(contextOccupancy(value, catalog), null)
  value.history.turns[0].items.push({ type: 'request', observation: { ...request, inputTokens: 50 } })
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
  const groups = groupTimelineTools(buildTimeline(value))
  assert.equal(groups.length, 1)
  assert.equal(groups[0].tools[0].title, 'bash')
  assert.equal(groups[0].tools[0].body, 'sleep 30')
  assert.equal(groups[0].tools[0].status, 'running')
  assert.equal(groups[0].tools[0].requestRunning, true)
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
