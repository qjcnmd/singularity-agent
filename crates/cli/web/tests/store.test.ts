import assert from 'node:assert/strict'
import { beforeEach, test } from 'node:test'
import { WorkbenchStore, sameWorkbenchFields } from '../src/store'
import { RpcFailure } from '../src/connection'
import type { SessionReadResult } from '../src/protocol.generated'
import { protocolVersion } from '../src/protocol'
import { bootstrap, bootstrapFrame, control, frame, historyPage, readyFrame, runtime, session, sessionFrame, summary } from './fixtures'
import { FakeTransport, MemoryStorage, deferred, harness, tick, waitFor } from './storeHarness'
import { persistDraft, storageKey } from '../src/viewPersistence'

beforeEach(() => {
  Object.defineProperty(globalThis, 'localStorage', { configurable: true, value: new MemoryStorage() })
  Object.defineProperty(globalThis, 'window', { configurable: true, value: new EventTarget() })
})

const emptyBootstrap = () => bootstrap({ sessionsByWorkspace: { w: [] }, sessionPhases: {} })
const idleSession = (id = 's') => session({ history: { ...session().history, summary: summary({ threadId: id }) }, runtime: runtime({ phase: 'idle', activeTurn: null }) })
/** 已加载 history 只是事实；分页通过身份、summary 与 cursor 来断言。 */
const historyIds = (session: { facts: { history: Array<{ id: string | null }> } } | null) =>
  session?.facts.history.map(turn => turn.id)
const pageIds = (page: SessionReadResult) => page.history.turns.map(turn => turn.turnId)
const unopenedStore = () => new WorkbenchStore({ createTransport: (frame, status) => new FakeTransport(frame, status) })

test('snapshot watermark suppresses events buffered during a read', async () => {
  const { store, transport } = await harness()
  const pending = deferred<SessionReadResult>()
  transport.respond('session.read', () => pending.promise)
  const reading = store.retrySession()
  transport.emit(frame(1, 'covered by snapshot'))
  pending.resolve(session({ runtime: runtime({ sessionRevision: 2, phase: 'stopping' }) }))
  await reading
  assert.equal(store.getSnapshot().session?.facts.active.flatMap(turn => turn.items).length, 0)
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
    const expected = tail.history.summary.turnCount === 81 ? historyPage(1, 81) : tail
    assert.deepEqual(historyIds(store.getSnapshot().session), pageIds(expected))
    assert.deepEqual(store.getSnapshot().session?.summary, expected.history.summary)
    assert.equal(store.getSnapshot().session?.nextCursor, expected.history.nextCursor)
    if (tail.history.summary.turnCount === 81) {
      transport.respond('session.read', () => historyPage(43, 82))
      await store.retrySession()
      assert.equal(store.getSnapshot().session?.facts.history.length, 82)
      assert.equal(store.getSnapshot().session?.nextCursor, null)
    }
    store.stop()
  }
})

test('late command completion never overwrites authoritative selection, titles or model catalogs', async () => {
  const { store, transport } = await harness()
  const save = deferred<null>()
  transport.respond('session.updateSettings', () => save.promise)
  const saving = store.updateSettings('p/a')
  transport.emit(sessionFrame(1, runtime({ sessionRevision: 1, selector: 'p/a' })))
  transport.emit(sessionFrame(2, runtime({ sessionRevision: 2, selector: 'p/b' })))
  save.resolve(null)
  assert.equal(await saving, true)
  assert.equal(store.getSnapshot().session?.runtime.selector, 'p/b')
  const rename = deferred<null>()
  transport.respond('session.rename', () => rename.promise)
  const renaming = store.renameSession('s', 'old title')
  await tick()
  transport.emit(bootstrapFrame(3, bootstrap({ sessionsByWorkspace: { w: [summary({ title: 'new title' })] } })))
  rename.resolve(null)
  await renaming
  assert.equal(store.getSnapshot().bootstrap?.sessionsByWorkspace.w[0].title, 'new title')
  assert.equal(store.getSnapshot().revision, 3)
  const provider = { providerId: 'p', displayName: null, baseUrl: 'https://old.example', models: [] }
  const newCatalog = { ...bootstrap().modelCatalog, defaultSelector: 'p/new' }
  const providerSave = deferred<null>()
  transport.respond('model.saveProvider', () => providerSave.promise)
  const savingProvider = store.saveProvider(provider)
  await tick()
  transport.emit(bootstrapFrame(4, bootstrap({ modelCatalog: newCatalog })))
  providerSave.resolve(null)
  assert.equal(await savingProvider, true)
  assert.equal(store.getSnapshot().bootstrap?.modelCatalog.defaultSelector, 'p/new')
  assert.equal(store.getSnapshot().revision, 4)
  assert.equal(transport.calls.filter(call => call.method === 'workbench.bootstrap').length, 1)
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
  void store.selectSession('other')
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
  store.setSidebarView({ collapsed: ['w', 'other-project'] })
  transport.respond('session.create', () => create.promise)
  const creating = store.createSession()
  assert.equal(store.getSnapshot().selectedSessionId, null)
  assert.deepEqual(store.getSnapshot().sidebarView.collapsed, ['other-project'])
  store.setDraft('typed while creating')
  transport.emit(bootstrapFrame(1, bootstrap()))
  transport.emit(bootstrapFrame(2, bootstrap({
    sessionsByWorkspace: { w: [summary({ threadId: 'new-session' }), summary()] },
    sessionPhases: { s: 'running', 'new-session': 'idle' },
  })))
  transport.emit({ ...frame(3, 'arrived during creation'), sessionId: 'new-session' })
  create.resolve(session({ history: { ...session().history, summary: summary({ threadId: 'new-session' }) } }))
  assert.equal(await creating, true)
  assert.equal(store.draft(), 'typed while creating')
  assert.equal(store.getSnapshot().drafts.s, 'previous task draft')
  assert.equal(store.getSnapshot().drafts['new:w'], '')
  assert.equal(store.getSnapshot().session?.facts.active.flatMap(turn => turn.items).length, 1)
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
  transport.respond('session.submit', () => null)
  store.setDraft('first message')
  assert.equal(await store.submitDraft(), true)
  assert.deepEqual(transport.calls.filter(call => call.method === 'session.submit'), [
    { method: 'session.submit', params: { workspaceId: 'w', sessionId: 'created', text: 'first message' } },
  ])
  assert.equal(store.draft(), '')
})

test('switching tasks during a first action never redirects that action to the newer selection', async () => {
  for (const action of ['submit', 'settings']) {
    const catalog = bootstrap({
      workspaces: [...bootstrap().workspaces, { workspaceId: 'another', name: 'Another', root: '/another' }],
      sessionsByWorkspace: { w: [], another: [summary({ threadId: 'other' })] },
      sessionPhases: { other: 'idle' },
    })
    const { store, transport } = await harness({ bootstrap: catalog, selectedSessionId: null })
    const operation = deferred<null>()
    transport.respond('session.create', () => idleSession('created'))
    transport.respond('session.read', () => idleSession('other'))
    transport.respond(action === 'submit' ? 'session.submit' : 'session.updateSettings', () => operation.promise)
    store.setDraft('original input')
    const pending = action === 'submit' ? store.submitDraft() : store.updateSettings('p/model')
    await tick()
    assert.equal(transport.calls.some(call => call.method === (action === 'submit' ? 'session.submit' : 'session.updateSettings')), true)
    void store.selectSession('other')
    await waitFor(store, state => state.session?.summary.threadId === 'other')
    store.setDraft('other task input')
    operation.resolve(null)
    assert.equal(await pending, true)
    assert.equal(store.getSnapshot().selectedSessionId, 'other')
    assert.equal(store.draft(), 'other task input')
    assert.equal(store.getSnapshot().drafts.created, action === 'submit' ? '' : 'original input')
    const call = transport.calls.find(call => call.method === (action === 'submit' ? 'session.submit' : 'session.updateSettings'))
    assert.deepEqual(call?.params, action === 'submit'
      ? { workspaceId: 'w', sessionId: 'created', text: 'original input' }
      : { workspaceId: 'w', sessionId: 'created', selector: 'p/model' })
    store.stop()
  }
})

test('an unavailable connection retains the draft without replay', async () => {
  const { store, transport } = await harness()
  store.setDraft('keep this draft')
  transport.emit(sessionFrame(1, runtime({ sessionRevision: 1, phase: 'idle' })))
  transport.respond('session.submit', () => { throw new RpcFailure('unavailable', 'offline', 'retry') })
  assert.equal(await store.submitDraft(), false)
  assert.equal(store.draft(), 'keep this draft')
  assert.equal(store.getSnapshot().actionError, null)
  assert.equal(transport.calls.filter(call => call.method === 'session.submit').length, 1)
  transport.status('recovering')
  assert.equal(await store.submitDraft(), false)
})

test('the first applicable blocking reason wins and routing follows the phase', async () => {
  const { store, transport } = await harness({ bootstrap: bootstrap({ sessionPhases: { s: 'running' } }) })
  const blocked = (intent?: 'steer') => store.submissionState(intent)
  store.setDraft('queued input')
  assert.deepEqual(blocked(), { canSubmit: true, blockedReason: null, method: 'session.followUp' })
  assert.equal(blocked('steer').method, 'session.steer', 'running 时的 steer 意图仍走 steer')

  // 在途提交与 stopping 同时成立：phase 原因优先于“正在发送…”。
  const submission = deferred<null>()
  transport.respond('session.followUp', () => submission.promise)
  const sending = store.submitDraft()
  await tick()
  assert.equal(blocked().blockedReason, '正在发送…')
  transport.emit(sessionFrame(1, runtime({ sessionRevision: 1, phase: 'stopping' })))
  assert.equal(blocked().blockedReason, '正在停止当前任务，结束后即可发送。')
  submission.resolve(null)
  assert.equal(await sending, true)

  for (const [revision, phase, reason] of [
    [2, 'reserved', '正在启动任务，稍后可继续发送。'],
    [3, 'compacting', '上下文整理完成后即可发送，也可以先停止整理。'],
  ] as const) {
    transport.emit(sessionFrame(revision, runtime({ sessionRevision: revision, phase })))
    assert.equal(blocked().blockedReason, reason)
  }
  // 基线读取在途时先报告同步，而不是当前 phase。
  const reread = deferred<SessionReadResult>()
  transport.respond('session.read', () => reread.promise)
  const reading = store.retrySession()
  assert.equal(blocked().blockedReason, '正在同步任务状态，稍后即可发送。')
  reread.resolve(session({ runtime: runtime({ sessionRevision: 3, phase: 'compacting' }) }))
  await reading
  assert.equal(blocked().blockedReason, '上下文整理完成后即可发送，也可以先停止整理。')
  // 连接未就绪优先于当前 phase，且路由回到 submit。
  transport.status('recovering')
  assert.deepEqual(blocked(), { canSubmit: false, blockedReason: '连接恢复后即可发送，草稿会保留。', method: 'session.submit' })
  store.stop()
})

test('read failures and empty drafts report their own state', async () => {
  // 读取失败要求显式重试，而不是停在同步中。
  const failed = await harness()
  failed.transport.respond('session.read', () => { throw new RpcFailure('internal', '会话日志损坏。', '检查会话文件后重试。') })
  failed.store.setDraft('queued input')
  await failed.store.retrySession()
  assert.deepEqual(failed.store.submissionState(),
    { canSubmit: false, blockedReason: '任务读取失败，请点击上方“重试读取”。', method: 'session.submit' })
  failed.store.stop()

  // 空输入没有阻止原因，但不可提交；没有会话时首次提交由创建承接。
  const empty = await harness({ bootstrap: emptyBootstrap(), selectedSessionId: null })
  assert.deepEqual(empty.store.submissionState(), { canSubmit: false, blockedReason: null, method: 'session.submit' })
  empty.store.setDraft('first message')
  assert.equal(empty.store.submissionState().canSubmit, true)
  empty.store.stop()
})

test('a failed submission keeps whatever draft the user left during the wait', async () => {
  for (const duringWait of ['', 'replacement text', 'first message']) {
    const { store, transport } = await harness()
    const submission = deferred<null>()
    transport.respond('session.followUp', () => submission.promise)
    store.setDraft('first message')
    const sending = store.submitDraft()
    await tick()
    store.setDraft(duringWait)
    submission.reject(new RpcFailure('internal', '发送失败。', '请重试。'))
    assert.equal(await sending, false)
    assert.equal(store.draft(), duringWait, `draft left during the wait must survive a failure: ${JSON.stringify(duringWait)}`)
    assert.equal(store.getSnapshot().actionError?.message, '发送失败。')
    store.stop()
  }
})

test('a successful submission clears the draft only while it still matches what was sent', async () => {
  for (const [duringWait, expected] of [['first message', ''], ['edited while sending', 'edited while sending']] as const) {
    const { store, transport } = await harness()
    const submission = deferred<null>()
    transport.respond('session.followUp', () => submission.promise)
    store.setDraft('first message')
    const sending = store.submitDraft()
    await tick()
    if (duringWait !== 'first message') store.setDraft(duringWait)
    submission.resolve(null)
    assert.equal(await sending, true)
    assert.equal(store.draft(), expected)
    store.stop()
  }
})

test('model selection before a first message creates a task without losing its draft', async () => {
  const { store, transport } = await harness({ bootstrap: emptyBootstrap(), selectedSessionId: null })
  transport.respond('session.create', () => idleSession('model-task'))
  transport.respond('session.updateSettings', params => {
    transport.emit(sessionFrame(1, runtime({ sessionRevision: 1, selector: params.selector, phase: 'idle' }), 'model-task'))
    return null
  })
  store.setDraft('pending input')
  assert.equal(await store.updateSettings('p/model#medium'), true)
  assert.equal(store.draft(), 'pending input')
  assert.equal(store.getSnapshot().session?.runtime.selector, 'p/model#medium')
})

test('running input returns to follow-up after a one-off steer', async () => {
  const { store, transport } = await harness()
  transport.respond('session.followUp', () => null)
  transport.respond('session.steer', () => null)
  for (const intent of [undefined, 'steer', undefined] as const) {
    store.setDraft('next')
    assert.equal(await store.submitDraft(intent), true)
  }
  assert.deepEqual(transport.calls.filter(call => ['session.followUp', 'session.steer'].includes(call.method)).map(call => call.method),
    ['session.followUp', 'session.steer', 'session.followUp'])
})

test('queued controls preserve their identity through replace, withdraw and send-now actions', async () => {
  const { store, transport } = await harness({ session: session({ runtime: runtime({ pendingControls: [control()] }) }) })
  transport.respond('session.queueReplace', () => null)
  transport.respond('session.queueWithdraw', () => null)
  transport.respond('session.queueSendNow', () => null)
  assert.equal(await store.replace('control', 'changed'), true)
  assert.equal(await store.withdraw('control'), true)
  assert.equal(await store.sendQueuedNow(), true)
  assert.deepEqual(transport.calls.filter(call => call.method.startsWith('session.queue')).map(call => call.params), [
    { workspaceId: 'w', sessionId: 's', controlId: 'control', text: 'changed' },
    { workspaceId: 'w', sessionId: 's', controlId: 'control' },
    { workspaceId: 'w', sessionId: 's' },
  ])
})

test('send-now names the whole queue once instead of enumerating a snapshot', async () => {
  const { store, transport } = await harness({ session: session({ runtime: runtime({ pendingControls: [
    control({ controlId: 'steer', channel: 'steer', sequence: 1 }),
    control({ controlId: 'follow', channel: 'follow_up', sequence: 2 }),
  ] }) }) })
  transport.respond('session.queueSendNow', () => null)
  assert.equal(await store.sendQueuedNow(), true)
  // 目标集合由服务端在当前队列上确定：前端不发逐条请求，也就不存在按过期快照
  // 请求已被消费条目的路径。
  assert.deepEqual(transport.calls.filter(call => call.method === 'session.queueSendNow').map(call => call.params), [
    { workspaceId: 'w', sessionId: 's' },
  ])
  assert.equal(await store.sendNow('follow'), true)
  assert.deepEqual(transport.calls.filter(call => call.method === 'session.queueSendNow').map(call => call.params), [
    { workspaceId: 'w', sessionId: 's' },
    { workspaceId: 'w', sessionId: 's', controlId: 'follow' },
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
      terminal: phase === 'idle' ? { source: 'turn', status: 'completed', message: null } : null }), 'other'))
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
  // 最新的 view 写入是共享的；draft 写入按 session 保持独立。
  first.store.setTheme('dark')
  assert.deepEqual(unopenedStore().getSnapshot().workspaceAppearance.w, { icon: 'star', color: '#ff0000' })
  assert.equal(unopenedStore().getSnapshot().trajectoryOpen, true)
})

test('legacy draft migration preserves newer entries and retains its original container on write failure', () => {
  const key = storageKey
  const original = JSON.stringify({ version: 1, drafts: { old: 'unsent', newer: 'old text' } })
  localStorage.setItem(key, original)
  persistDraft('newer', 'new text')
  const restored = unopenedStore()
  assert.deepEqual(restored.getSnapshot().drafts, { old: 'unsent', newer: 'new text' })
  const setItem = localStorage.setItem.bind(localStorage)
  localStorage.setItem = (name, value) => {
    // 草稿迁移写入失败：容器此时尚未被覆盖。
    if (name !== key) throw new Error('storage full')
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

test('settings and drafts persist independently across reloads', () => {
  const store = unopenedStore()
  store.setWorkspaceAppearance('w', { icon: 'star', color: '#ff0000' })
  store.setTheme('dark')
  const view = localStorage.getItem(storageKey)
  store.setDraft('independent input')
  assert.equal(localStorage.getItem(storageKey), view)
  store.setWorkspaceAppearance('other', { icon: 'folder', color: '#0000ff' })
  const restored = unopenedStore().getSnapshot()
  assert.equal(restored.theme, 'dark')
  assert.deepEqual(restored.workspaceAppearance, {
    w: { icon: 'star', color: '#ff0000' }, other: { icon: 'folder', color: '#0000ff' },
  })
  assert.equal(restored.drafts['new:none'], 'independent input')
})

test('authoritative removal clears selection and runtime through stream, mutation and resync', async () => {
  for (const entry of ['stream', 'mutation', 'resync']) {
    const { store, transport } = await harness()
    store.setDraft('retained separately')
    const removed = bootstrap({ revision: 1, workspaces: [], sessionsByWorkspace: {}, sessionPhases: {} })
    if (entry === 'stream') transport.emit(bootstrapFrame(1, removed))
    else if (entry === 'mutation') {
      transport.respond('workspace.rename', () => {
        transport.emit(bootstrapFrame(1, removed))
        return null
      })
      await store.renameWorkspace('w', 'renamed elsewhere')
    } else {
      transport.respond('workbench.bootstrap', () => removed)
      transport.emit({ version: protocolVersion, generation: removed.generation, revision: 1, type: 'resync_required', payload: { reason: 'test' } })
      await waitFor(store, state => state.selectedWorkspaceId === null)
    }
    const state = store.getSnapshot()
    assert.equal(state.selectedWorkspaceId, null, entry)
    assert.equal(state.selectedSessionId, null, entry)
    assert.equal(state.session, null, entry)
    assert.deepEqual(state.liveSessions, {}, entry)
    assert.equal(state.sessionLoad.status, 'idle', entry)
    assert.equal(state.drafts.s, 'retained separately', entry)
    store.stop()
  }
})

test('removed tasks cannot be restored by a late read', async () => {
  const { store, transport } = await harness()
  const read = deferred<SessionReadResult>()
  transport.respond('session.read', () => read.promise)
  const reading = store.retrySession()
  transport.respond('session.rename', () => {
    transport.emit(bootstrapFrame(1, bootstrap({ revision: 1, sessionsByWorkspace: { w: [] }, sessionPhases: {} })))
    return null
  })
  await store.renameSession('s', 'removed elsewhere')
  read.resolve(session())
  await reading
  assert.equal(store.getSnapshot().selectedSessionId, null)
  assert.equal(store.getSnapshot().session, null)
  assert.equal(store.getSnapshot().sessionLoad.status, 'idle')
})

test('recovery selects the first available task while ordinary snapshots only clear removed selection', async () => {
  const { store, transport } = await harness({ selectedSessionId: null })
  assert.equal(store.getSnapshot().selectedSessionId, 's', 'initial recovery retains its default selection')
  const replacement = bootstrap({ revision: 1, sessionsByWorkspace: { w: [summary({ threadId: 'other' })] } })
  transport.emit(bootstrapFrame(1, replacement))
  assert.equal(store.getSnapshot().selectedSessionId, null, 'ordinary snapshot does not navigate to another task')
  transport.respond('workbench.bootstrap', () => replacement)
  transport.emit({ version: protocolVersion, generation: replacement.generation, revision: 1, type: 'resync_required', payload: { reason: 'reconnect' } })
  await waitFor(store, state => state.session?.summary.threadId === 'other')
  assert.equal(store.getSnapshot().selectedSessionId, 'other', 'reconnection retains its default selection')
})

test('submissions never route on the stale phase while a resync is pending', async () => {
  for (const start of [
    (transport: FakeTransport) => { transport.status('recovering'); transport.emit(readyFrame()) },
    (transport: FakeTransport) => transport.emit(frame(2, 'unseen revision')),
  ] as const) {
    const { store, transport } = await harness()
    store.setDraft('sent during resync')
    // resync 在所选 session 的读取挂起时重新执行 baseline sync：
    // 保留的快照可能显示过期的 phase。重连路径与
    // 仍为 ready 的连接上的逻辑缺口，都必须在等待
    // baseline 之前撤销提交就绪状态。
    const reads = deferred<SessionReadResult>()
    transport.respond('session.read', () => reads.promise)
    start(transport)
    await waitFor(store, state => state.sessionLoad.status === 'loading')
    assert.notEqual(store.getSnapshot().connection, 'ready', 'readiness waits for the baseline read')
    const routedCalls = () => transport.calls.filter(call => call.method.startsWith('session.')).length
    const before = routedCalls()
    assert.equal(await store.submitDraft(), false)
    assert.equal(routedCalls(), before, 'no phase-routed RPC fires on the unverified snapshot')
    assert.equal(store.getSnapshot().drafts.s, 'sent during resync', 'the draft survives the blocked window')
    // 收敛后的快照报告 running phase：同一 draft 按 follow-up 路由。
    reads.resolve(session({ runtime: runtime({ sessionRevision: 5, phase: 'running' }) }))
    await waitFor(store, state => state.connection === 'ready' && state.sessionLoad.status === 'idle')
    assert.equal(store.getSnapshot().session?.runtime.phase, 'running')
    transport.respond('session.followUp', () => null)
    assert.equal(await store.submitDraft(), true)
    assert.equal(transport.calls.at(-1)?.method, 'session.followUp')
    assert.equal(store.getSnapshot().drafts.s, '', 'the draft clears after acceptance')
    store.stop()
  }
})

test('a baseline read refused by the connection keeps its connection state instead of declaring readiness', async () => {
  // 三个连接级码都不得被读侧的 sessionLoad 吞成「读侧已处理」：forbidden 保留
  // 拒绝状态，unavailable 与本模块自己合成的 invalid_response 保留恢复中。
  // 后两者由 store 走 reconnect() 校准，forbidden 不重连。
  const mutations = ['session.submit', 'session.archive'] as const
  for (const [code, expected, reconnects] of [
    ['forbidden', 'forbidden', 0],
    ['unavailable', 'recovering', 1],
    ['invalid_response', 'recovering', 1],
  ] as const) {
    const { store, transport } = await harness({ session: session({ runtime: runtime({ phase: 'idle', activeTurn: null }) }) })
    // 先让一次变更的响应结果不可信：变更可能已在服务端生效，所以校准只重读
    // 基线，绝不重发这次变更。
    store.setDraft('sent before the connection failed')
    transport.respond('session.submit', () => { throw new RpcFailure(code, '变更结果不确定。', '稍后重试。') })
    assert.equal(await store.submitDraft(), false, code)
    transport.respond('session.read', () => { throw new RpcFailure(code, '基线读取失败。', '稍后重试。') })
    transport.emit(frame(2, 'unseen revision'))
    await waitFor(store, state => state.sessionLoad.status === 'error')
    await tick()
    // 连接级失败不能被读侧的 sessionLoad 吞掉后改写成就绪：forbidden 保留拒绝
    // 状态，unavailable 与 invalid_response 保留恢复中，三者都不宣告基线成功。
    const connection = store.getSnapshot().connection
    assert.notEqual(connection, 'ready', `${code} never declares readiness`)
    assert.equal(connection, expected, code)
    assert.equal(store.getSnapshot().sessionLoad.error?.code, code)
    assert.equal(store.getSnapshot().session, null)
    assert.equal(transport.reconnects, reconnects, `${code} reconciles through the reconnect path exactly as often as expected`)
    assert.deepEqual(
      transport.calls.filter(call => mutations.includes(call.method as typeof mutations[number])).map(call => call.method),
      ['session.submit'],
      `${code} replays no mutation beyond the single attempt`,
    )
    store.stop()
  }
})

test('a superseded baseline read never overwrites the newer read that replaced it', async () => {
  const catalog = bootstrap({
    sessionsByWorkspace: { w: [summary(), summary({ threadId: 'other' })] },
    sessionPhases: { s: 'idle', other: 'idle' },
  })
  const { store, transport } = await harness({ bootstrap: catalog })
  const baseline = deferred<SessionReadResult>()
  transport.respond('session.read', params => params.sessionId === 's'
    ? baseline.promise
    : Promise.reject(new RpcFailure('forbidden', '读取被拒绝。', '稍后重试。')))
  // 重同步的基线读取尚未返回时切换任务：新选择的读取先被拒绝，旧读取随后才落地。
  transport.emit(frame(2, 'unseen revision'))
  await waitFor(store, state => state.sessionLoad.status === 'loading')
  await store.selectSession('other')
  assert.equal(store.getSnapshot().sessionLoad.error?.code, 'forbidden')
  baseline.resolve(session())
  await tick()
  // 被取代的读取跟随取代者收敛，不能把新读取的连接级失败改写成就绪，
  // 也不能用旧选择的快照覆盖新选择。
  assert.equal(store.getSnapshot().connection, 'forbidden')
  assert.equal(store.getSnapshot().selectedSessionId, 'other')
  assert.equal(store.getSnapshot().session, null)
})

test('a corrupted session read stays a visible error without holding the connection in recovery', async () => {
  const { store, transport } = await harness()
  transport.respond('session.read', () => { throw new RpcFailure('internal', '会话日志损坏。', '检查会话文件后重试。') })
  store.setDraft('kept draft')
  transport.emit(frame(2, 'unseen revision'))
  await waitFor(store, state => state.sessionLoad.status === 'error')
  await tick()
  // 业务读失败既不被伪装成基线成功（sessionLoad 独立报错，且不按未经验证的
  // 快照路由提交），也不把整条连接卡在 recovering。
  assert.equal(store.getSnapshot().connection, 'ready')
  assert.equal(transport.reconnects, 0)
  assert.equal(store.getSnapshot().sessionLoad.error?.message, '会话日志损坏。')
  assert.equal(store.getSnapshot().session, null)
  assert.equal(store.submissionState().canSubmit, false)
})

test('a new generation ready during an in-flight resync still takes its own baseline', async () => {
  const { store, transport } = await harness()
  const bootstraps = [deferred<ReturnType<typeof bootstrap>>(), deferred<ReturnType<typeof bootstrap>>()]
  const reads = [deferred<SessionReadResult>(), deferred<SessionReadResult>()]
  let bootstrapCalls = 0
  let readCalls = 0
  transport.respond('workbench.bootstrap', () => bootstraps[Math.min(bootstrapCalls++, 1)].promise)
  transport.respond('session.read', () => reads[Math.min(readCalls++, 1)].promise)
  // 旧基线（bootstrap 与 session 读取）都尚未返回。
  transport.emit(frame(2, 'unseen revision'))
  assert.equal(store.getSnapshot().connection, 'recovering')
  // 新连接先宣告 g2 ready 并发出 g2 事件，随后旧基线才返回。
  const second = bootstrap({ generation: 'g2', sessionPhases: { s: 'running' } })
  transport.emit({ version: protocolVersion, generation: 'g2', revision: 0, type: 'ready', payload: {} })
  transport.emit({ ...frame(1, 'from g2'), generation: 'g2' })
  reads[0].resolve(session())
  bootstraps[0].resolve(bootstrap())
  await tick()
  assert.equal(bootstrapCalls, 2, 'the new generation is not swallowed by the in-flight resync')
  reads[1].resolve(session())
  bootstraps[1].resolve(second)
  await waitFor(store, state => state.generation === 'g2' && state.connection === 'ready')
  // g2 事件在 g2 基线之后由同一个 reducer 接纳，而不是被缓冲过滤丢掉。
  assert.equal(store.getSnapshot().revision, 1)
  assert.equal(store.getSnapshot().session?.facts.active.flatMap(turn => turn.items).length, 1)
})

test('sidebar subscriptions ignore stream revisions but observe lifecycle changes', async () => {
  const { store } = await harness()
  const initial = store.getSnapshot()
  const next = { ...initial, liveSessions: { s: { ...initial.liveSessions.s, sessionRevision: 1 } } }
  assert.equal(sameWorkbenchFields(initial, next, ['bootstrap', 'liveSessions']), true)
  assert.equal(sameWorkbenchFields(initial, { ...next, liveSessions: { s: { ...next.liveSessions.s, phase: 'idle' } } }, ['liveSessions']), false)
})
