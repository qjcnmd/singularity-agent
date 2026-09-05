import assert from 'node:assert/strict'
import { beforeEach, test } from 'node:test'
import { workbenchStore } from '../src/store.ts'
import { buildTimeline } from '../src/timeline.ts'

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
  assert.equal(projected.filter(item => item.key === 'active:unique:call').length, 1)
  assert.equal(projected.filter(item => item.kind === 'unknown').length, 0)
  assert.equal(projected.find(item => item.kind === 'diff').addedLines, 1)
  assert.equal(projected.filter(item => item.kind === 'user').length, 1)
  live.runtime.activeTurn.events = [...live.runtime.activeTurn.events,
    { method: 'item/agentMessage/delta', params: { turnId: 'unique', item: { itemId: 'answer' }, delta: 'a' } },
  ]
  assert.equal(buildTimeline(live).find(item => item.kind === 'assistant').body, 'a')
  assert.deepEqual(buildTimeline(live), buildTimeline(live))
})
