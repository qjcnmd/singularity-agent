import assert from 'node:assert/strict'
import { beforeEach, test } from 'node:test'
import { WorkbenchStore, sameWorkbenchFields } from '../src/store'
import { RpcFailure } from '../src/connection'
import type { ActionReceipt, SessionReadResult, WorkbenchBootstrap } from '../src/protocol.generated'
import { bootstrap, bootstrapFrame, control, frame, historyPage, receipt, runtime, session, sessionFrame, summary } from './fixtures'
import { FakeTransport, MemoryStorage, deferred, harness, tick, waitFor } from './storeHarness'

beforeEach(() => {
  Object.defineProperty(globalThis, 'localStorage', { configurable: true, value: new MemoryStorage() })
  Object.defineProperty(globalThis, 'window', { configurable: true, value: new EventTarget() })
})

const emptyBootstrap = () => bootstrap({ sessionsByWorkspace: { w: [] }, sessionPhases: {} })
const idleSession = (id = 's') => session({ summary: summary({ threadId: id }), runtime: runtime({ phase: 'idle', activeTurn: null }) })
const unopenedStore = () => new WorkbenchStore({ createTransport: (frame, status) => new FakeTransport(frame, status) })

test('snapshot watermark suppresses events buffered during a read', async () => {
  const { store, transport } = await harness()
  const pending = deferred<SessionReadResult>()
  transport.respond('session.read', () => pending.promise)
  const reading = store.retrySession()
  transport.emit(frame(1, 'covered by snapshot'))
  pending.resolve(session({ runtime: runtime({ sessionRevision: 2, phase: 'stopping' }) }))
  await reading
  assert.equal(store.getSnapshot().session?.runtime.activeTurn?.events.length, 0)
  assert.equal(store.getSnapshot().liveSessions.s.phase, 'stopping')
  assert.equal(store.getSnapshot().revision, 1)
})

test('pagination remains continuous after settlement refresh; a late page cannot hide a new gap', async () => {
  for (const tail of [historyPage(42, 81), historyPage(101, 140)]) {
    const { store, transport } = await harness({ session: historyPage(41, 80) })
    const older = deferred<SessionReadResult>()
    transport.respond('session.read', params => params.beforeTurn ? older.promise : tail)
    const loading = store.readOlder()
    await store.retrySession()
    older.resolve(historyPage(1, 40, 80))
    await loading
    const expected = tail.summary.turnCount === 81 ? historyPage(1, 81) : tail
    assert.deepEqual(store.getSnapshot().session?.history, expected.history)
    if (tail.summary.turnCount === 81) {
      transport.respond('session.read', () => historyPage(43, 82))
      await store.retrySession()
      assert.equal(store.getSnapshot().session?.history.turns.length, 82)
      assert.equal(store.getSnapshot().session?.history.nextCursor, null)
    }
    store.stop()
  }
})

test('late action receipts do not overwrite authoritative selection and streamed titles', async () => {
  const { store, transport } = await harness()
  const save = deferred<ActionReceipt>()
  transport.respond('session.updateSettings', () => save.promise)
  const saving = store.updateSettings('p/a')
  transport.emit(sessionFrame(1, runtime({ sessionRevision: 1, selector: 'p/a' })))
  transport.emit(sessionFrame(2, runtime({ sessionRevision: 2, selector: 'p/b' })))
  save.resolve(receipt({ revision: 1 }))
  assert.equal(await saving, true)
  assert.equal(store.getSnapshot().session?.runtime.selector, 'p/b')
  const refresh = deferred<WorkbenchBootstrap>()
  transport.respond('workbench.bootstrap', () => refresh.promise)
  transport.respond('session.rename', () => summary({ title: 'old title' }))
  const renaming = store.renameSession('s', 'old title')
  await tick()
  transport.emit(bootstrapFrame(3, bootstrap({ sessionsByWorkspace: { w: [summary({ title: 'new title' })] } })))
  refresh.resolve(bootstrap({ sessionsByWorkspace: { w: [summary({ title: 'old title' })] } }))
  await renaming
  assert.equal(store.getSnapshot().bootstrap?.sessionsByWorkspace.w[0].title, 'new title')
  assert.equal(store.getSnapshot().revision, 3)
})

test('late creation and session reads cannot change a newer selection', async () => {
  const catalog = bootstrap({
    workspaces: [...bootstrap().workspaces, { workspaceId: 'another', name: 'Another', root: '/another' }],
    sessionsByWorkspace: { w: [summary()], another: [summary({ threadId: 'other' })] },
  })
  const { store, transport } = await harness({ bootstrap: catalog })
  const create = deferred<SessionReadResult>()
  const read = deferred<SessionReadResult>()
  transport.respond('session.create', () => create.promise)
  transport.respond('session.read', params => params.sessionId === 's' ? read.promise : idleSession('other'))
  const reading = store.retrySession()
  const creating = store.createSession()
  store.selectWorkspace('another')
  await waitFor(store, state => state.session?.summary.threadId === 'other')
  create.resolve(session())
  read.resolve(session())
  await Promise.all([reading, creating])
  assert.equal(store.getSnapshot().selectedWorkspaceId, 'another')
  assert.equal(store.getSnapshot().selectedSessionId, 'other')
  assert.equal(store.getSnapshot().session?.summary.threadId, 'other')
})

test('typing during creation belongs to the new task; buffered events are delivered after creation', async () => {
  const { store, transport } = await harness()
  const create = deferred<SessionReadResult>()
  store.setDraft('previous task draft')
  transport.respond('session.create', () => create.promise)
  const creating = store.createSession()
  assert.equal(store.getSnapshot().selectedSessionId, null)
  store.setDraft('typed while creating')
  transport.emit({ ...frame(1, 'arrived during creation'), sessionId: 'new-session' })
  create.resolve(session({ summary: summary({ threadId: 'new-session' }) }))
  assert.equal(await creating, true)
  assert.equal(store.draft(), 'typed while creating')
  assert.equal(store.getSnapshot().drafts.s, 'previous task draft')
  assert.equal(store.getSnapshot().drafts['new:w'], '')
  assert.equal(store.getSnapshot().session?.runtime.activeTurn?.events.length, 1)
})

test('failed creation releases buffered events, preserves the draft and allows another attempt', async () => {
  const { store, transport } = await harness()
  const create = deferred<SessionReadResult>()
  transport.respond('session.create', () => create.promise)
  const creating = store.createSession()
  store.setDraft('keep this input')
  transport.emit(frame(1, 'another task is running'))
  create.reject(new RpcFailure('internal', 'could not create', 'retry'))
  assert.equal(await creating, false)
  assert.equal(store.getSnapshot().sessionLoad.status, 'idle')
  assert.equal(store.getSnapshot().revision, 1)
  transport.respond('session.create', () => idleSession('retry-session'))
  assert.equal(await store.createSession(), true)
  assert.equal(store.getSnapshot().selectedSessionId, 'retry-session')
  assert.equal(store.draft(), 'keep this input')
})

test('first submission creates a task and sends its retained draft once', async () => {
  const { store, transport } = await harness({ bootstrap: emptyBootstrap(), selectedSessionId: null })
  transport.respond('session.create', () => idleSession('created'))
  transport.respond('session.submit', () => receipt({ sessionId: 'created' }))
  store.setDraft('first message')
  assert.equal(await store.submitDraft(), true)
  assert.deepEqual(transport.calls.filter(call => call.method === 'session.submit'), [
    { method: 'session.submit', params: { workspaceId: 'w', sessionId: 'created', text: 'first message' } },
  ])
  assert.equal(store.draft(), '')
})

test('switching tasks during creation refresh never submits or changes settings on the newly selected task', async () => {
  for (const action of ['submit', 'settings']) {
    const { store, transport } = await harness({ bootstrap: emptyBootstrap(), selectedSessionId: null })
    const refresh = deferred<WorkbenchBootstrap>()
    transport.respond('session.create', () => idleSession('created'))
    transport.respond('session.read', () => idleSession('other'))
    transport.respond('workbench.bootstrap', () => refresh.promise)
    store.setDraft('original input')
    const pending = action === 'submit' ? store.submitDraft() : store.updateSettings('p/model')
    await tick()
    transport.emit(bootstrapFrame(1, bootstrap({ sessionsByWorkspace: { w: [summary({ threadId: 'other' })] } })))
    await store.selectSession('other')
    store.setDraft('other task input')
    refresh.resolve(emptyBootstrap())
    assert.equal(await pending, false)
    assert.equal(store.getSnapshot().selectedSessionId, 'other')
    assert.equal(store.draft(), 'other task input')
    assert.equal(store.getSnapshot().drafts.created, 'original input')
    assert.equal(transport.calls.some(call => call.method === 'session.submit' || call.method === 'session.updateSettings'), false)
    store.stop()
  }
})

test('blocked phases and an unavailable connection retain the draft without replay', async () => {
  const { store, transport } = await harness()
  store.setDraft('keep this draft')
  for (const [index, phase] of (['stopping', 'compacting', 'reserved'] as const).entries()) {
    transport.emit(sessionFrame(index + 1, runtime({ sessionRevision: index + 1, phase })))
    assert.equal(await store.submitDraft(), false)
  }
  transport.emit(sessionFrame(4, runtime({ sessionRevision: 4, phase: 'idle' })))
  transport.respond('session.submit', () => { throw new RpcFailure('unavailable', 'offline', 'retry') })
  assert.equal(await store.submitDraft(), false)
  assert.equal(store.draft(), 'keep this draft')
  assert.equal(store.getSnapshot().actionError, null)
  assert.equal(transport.calls.filter(call => call.method === 'session.submit').length, 1)
  transport.status('recovering')
  assert.equal(await store.submitDraft(), false)
})

test('model selection before a first message creates a task without losing its draft', async () => {
  const { store, transport } = await harness({ bootstrap: emptyBootstrap(), selectedSessionId: null })
  transport.respond('session.create', () => idleSession('model-task'))
  transport.respond('session.updateSettings', params => {
    transport.emit(sessionFrame(1, runtime({ sessionRevision: 1, selector: params.selector, phase: 'idle' }), 'model-task'))
    return receipt({ sessionId: 'model-task' })
  })
  store.setDraft('pending input')
  assert.equal(await store.updateSettings('p/model#medium'), true)
  assert.equal(store.draft(), 'pending input')
  assert.equal(store.getSnapshot().session?.runtime.selector, 'p/model#medium')
})

test('running input returns to follow-up after a one-off steer', async () => {
  const { store, transport } = await harness()
  transport.respond('session.followUp', () => receipt())
  transport.respond('session.steer', () => receipt())
  for (const intent of [undefined, 'steer', undefined] as const) {
    store.setDraft('next')
    assert.equal(await store.submitDraft(intent), true)
  }
  assert.deepEqual(transport.calls.filter(call => ['session.followUp', 'session.steer'].includes(call.method)).map(call => call.method),
    ['session.followUp', 'session.steer', 'session.followUp'])
})

test('queued controls preserve their identity through replace, withdraw and send-now actions', async () => {
  const { store, transport } = await harness({ session: session({ runtime: runtime({ pendingControls: [control()] }) }) })
  transport.respond('session.queueReplace', () => receipt())
  transport.respond('session.queueWithdraw', () => receipt())
  transport.respond('session.queueSendNow', () => receipt())
  assert.equal(await store.replace('control', 'changed'), true)
  assert.equal(await store.withdraw('control'), true)
  assert.equal(await store.sendQueuedNow(), true)
  assert.deepEqual(transport.calls.filter(call => call.method.startsWith('session.queue')).map(call => call.params), [
    { workspaceId: 'w', sessionId: 's', controlId: 'control', text: 'changed' },
    { workspaceId: 'w', sessionId: 's', controlId: 'control' },
    { workspaceId: 'w', sessionId: 's', controlId: 'control' },
  ])
})

test('background completion reminders clear on opening and never mark current or old sessions', async () => {
  const { store, transport } = await harness({ bootstrap: bootstrap({
    sessionsByWorkspace: { w: [summary(), summary({ threadId: 'other' })] },
    sessionPhases: { s: 'idle', other: 'idle' },
  }) })
  assert.equal(store.getSnapshot().unreadSessions.size, 0)
  const send = (revision: number, phase: 'running' | 'idle') =>
    transport.emit(sessionFrame(revision, runtime({ sessionRevision: revision, phase,
      terminal: phase === 'idle' ? { status: 'completed', message: null } : null }), 'other'))
  send(1, 'running'); send(2, 'idle')
  assert.equal(store.getSnapshot().unreadSessions.has('other'), true)
  await store.selectSession('other')
  assert.equal(store.getSnapshot().unreadSessions.size, 0)
  send(3, 'running'); send(4, 'idle')
  assert.equal(store.getSnapshot().unreadSessions.size, 0)
  await store.selectSession('s')
  send(5, 'running'); send(6, 'idle')
  assert.equal(store.getSnapshot().unreadSessions.has('other'), true)
  send(7, 'running')
  assert.equal(store.getSnapshot().unreadSessions.size, 0)
})

test('draft transfer does not overwrite an existing empty task draft', async () => {
  const { store, transport } = await harness({ bootstrap: bootstrap({ sessionsByWorkspace: { w: [
    summary(), summary({ threadId: 'target', turnCount: 0, status: null }),
  ] } }) })
  await store.selectSession('target')
  store.setDraft('valuable target draft')
  await store.selectSession('s')
  store.setDraft('source draft')
  transport.respond('session.create', () => idleSession('new'))
  assert.equal(await store.createSession('w', true), true)
  assert.equal(store.getSnapshot().drafts.target, 'valuable target draft')
  assert.equal(store.getSnapshot().drafts.s, '')
  assert.equal(store.draft(), 'source draft')
})

test('independent stores preserve per-task drafts and view preferences across reload', async () => {
  const catalog = bootstrap({ sessionsByWorkspace: { w: [summary(), summary({ threadId: 'other' })] } })
  const first = await harness({ bootstrap: catalog })
  first.store.setDraft('draft a')
  first.store.setWorkspaceAppearance('w', { icon: 'star', color: '#ff0000' })
  first.store.setTrajectoryOpen(true)
  const second = await harness({ bootstrap: catalog, selectedSessionId: 'other' })
  second.store.setDraft('draft b')
  const restored = unopenedStore().getSnapshot()
  assert.equal(restored.drafts.s, 'draft a')
  assert.equal(restored.drafts.other, 'draft b')
  // The latest view write is shared; draft writes remain independent by session.
  first.store.setTheme('dark')
  assert.deepEqual(unopenedStore().getSnapshot().workspaceAppearance.w, { icon: 'star', color: '#ff0000' })
  assert.equal(unopenedStore().getSnapshot().trajectoryOpen, true)
})

test('legacy draft migration preserves newer entries and retains its original container on write failure', () => {
  const key = 'singularity.workbench.view.v1'
  const original = JSON.stringify({ version: 1, drafts: { old: 'unsent', newer: 'old text' } })
  localStorage.setItem(key, original)
  localStorage.setItem(`${key}:draft:newer`, 'new text')
  const restored = unopenedStore()
  assert.deepEqual(restored.getSnapshot().drafts, { old: 'unsent', newer: 'new text' })
  const setItem = localStorage.setItem.bind(localStorage)
  localStorage.setItem = (name, value) => {
    if (name.startsWith(`${key}:draft:`)) throw new Error('storage full')
    setItem(name, value)
  }
  restored.setTheme('dark')
  assert.equal(localStorage.getItem(key), original)
  assert.equal(unopenedStore().getSnapshot().drafts.old, 'unsent')
  localStorage.setItem = setItem
  restored.setTheme('light')
  assert.equal(JSON.parse(localStorage.getItem(key)!).drafts, undefined)
  assert.equal(unopenedStore().getSnapshot().drafts.old, 'unsent')
})

test('sidebar subscriptions ignore stream revisions but observe lifecycle changes', async () => {
  const { store } = await harness()
  const initial = store.getSnapshot()
  const next = { ...initial, liveSessions: { s: { ...initial.liveSessions.s, sessionRevision: 1 } } }
  assert.equal(sameWorkbenchFields(initial, next, ['bootstrap', 'liveSessions']), true)
  assert.equal(sameWorkbenchFields(initial, { ...next, liveSessions: { s: { ...next.liveSessions.s, phase: 'idle' } } }, ['liveSessions']), false)
})
