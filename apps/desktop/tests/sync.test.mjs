import assert from 'node:assert/strict'
import { registerHooks } from 'node:module'
import { test } from 'node:test'

// Node 直接运行现有 TypeScript，只需补全应用使用的无扩展名导入。
const hooks = registerHooks({ resolve(specifier, context, next) {
  return next(specifier.startsWith('.') && context.parentURL?.endsWith('.ts') ? `${specifier}.ts` : specifier, context)
} })
const { acceptBootstrap, acceptSessionStart, acceptSessionRead, acceptModelCatalog, initialSyncState, pruneStaleSelection, reduceStream } =
  await import('../src/sync.ts')
const { buildTimeline } = await import('../src/timeline.ts')
hooks.deregister()

function bootstrap(revision, sessions = []) {
  return { revision, workspaces: [{ root: 'C:/project' }], sessionsByWorkspace: { 'c:/project': sessions },
    sessionStates: {}, modelCatalog: { providers: [], defaultSelection: null, error: null } }
}
const summary = { threadId: 'new-task', title: 'first message', cwd: 'C:/project', updatedAt: '2026-10-23T00:00:00Z' }
const started = { revision: 10, summary, status: { phase: 'running', failure: null } }

test('首次发送先返回时，较早的配置更新仍生效且新任务保持选中', () => {
  let state = acceptBootstrap(initialSyncState(), bootstrap(8))
  state = acceptSessionStart(state, started)
  const payload = { ...bootstrap(9), modelCatalog: { providers: [{ providerId: 'added-provider' }] } }
  state = reduceStream(state, { type: 'app_changed', revision: 11, payload }).state
  assert.equal(state.revision, 11)
  assert.equal(state.bootstrap.revision, 9)
  assert.equal(state.bootstrap.modelCatalog.providers[0].providerId, 'added-provider')
  assert.deepEqual(state.bootstrap.sessionsByWorkspace['c:/project'], [summary])
  const selection = pruneStaleSelection(state.bootstrap, { ...state,
    selectedWorkspaceRoot: 'c:/project', selectedSessionId: 'new-task' })
  assert.equal(selection.selectionDropped, false)
  assert.equal(selection.liveSessions['new-task'].phase, 'running')

  state = acceptBootstrap(state, bootstrap(11, [summary]))
  state = acceptBootstrap(state, bootstrap(12))
  assert.deepEqual(state.bootstrap.sessionsByWorkspace['c:/project'], [])
  assert.equal(pruneStaleSelection(state.bootstrap, { ...state,
    selectedWorkspaceRoot: 'c:/project', selectedSessionId: 'new-task' }).selectionDropped, true)
})

test('首次发送晚返回时，不覆盖已收到的新标题和运行状态', () => {
  const renamed = { ...summary, title: 'renamed' }
  let state = acceptBootstrap(initialSyncState(), bootstrap(12, [renamed]))
  state = reduceStream(state, { type: 'session_settled', revision: 12, sessionId: 'new-task',
    summary: renamed,
    payload: { phase: 'idle', failure: null } }).state
  state = acceptSessionStart(state, started)
  assert.deepEqual(state.bootstrap.sessionsByWorkspace['c:/project'], [renamed])
  assert.equal(state.liveSessions['new-task'].phase, 'idle')
  state = acceptBootstrap(state, bootstrap(13))
  state = acceptSessionStart(state, started)
  assert.deepEqual(state.bootstrap.sessionsByWorkspace['c:/project'], [])
})

test('保存失败结束临时输出，保留片段且只显示一次错误', () => {
  const runtime = { phase: 'running', failure: null, backgroundTasks: [], pendingMessages: [] }
  const failure = { source: 'turn', error: { cause: 'store', message: '保存执行结果失败' } }
  let state = { ...initialSyncState(), session: {
    summary: { ...summary, cwd: 'C:/project' }, runtime, context: null,
    facts: [{ turnId: 'turn', startedAt: '2026-10-23T00:00:00Z', status: 'running', manuallyStopped: false, items: [
      { type: 'message', id: 'user', role: 'user', text: '检查项目', status: 'completed' },
      { type: 'message', id: 'reply', role: 'assistant', text: '收到的正文', status: 'running' },
      { type: 'thinking', id: 'thinking', text: '收到的思考', status: 'running' },
      { type: 'tool', id: 'tool', name: 'read', args: { path: 'README.md' }, result: null, status: 'running' },
    ] }],
  } }
  const settled = { type: 'session_settled', revision: 1, sessionId: summary.threadId,
    summary,
    payload: { ...runtime, phase: 'idle', failure } }
  state = reduceStream(state, settled).state
  assert.equal(state.session.runtime.phase, 'idle')
  assert.equal(state.session.facts[0].status, 'failed')
  assert.deepEqual(state.session.facts[0].items.map(item => item.status), ['completed', 'failed', 'failed', 'failed'])
  assert.equal(state.session.facts[0].items[1].text, '收到的正文')
  assert.equal(state.session.facts[0].finishedAt, undefined)
  const timeline = buildTimeline(state.session)
  assert.ok(timeline.every(({ item }) => item.status !== 'running'))
  assert.deepEqual(timeline.filter(({ item }) => item.type === 'terminal').map(({ item }) => item.text), [failure.error.message])

  state = reduceStream(state, { type: 'turn_event', revision: 2, sessionId: summary.threadId,
    payload: { turnId: 'next-turn', phase: 'running', method: 'turn/started',
      params: { turn: { turnId: 'next-turn', status: 'running' }, startedAt: '2026-10-22T00:00:00Z' } } }).state
  assert.equal(reduceStream(state, settled).state, state)
  assert.equal(state.session.facts.at(-1).status, 'running')

  for (const source of ['turn', 'compaction']) {
    const previous = { ...state, session: { ...state.session, facts: [
      { turnId: 'completed-turn', startedAt: '2026-10-23T00:00:00Z', status: 'completed', manuallyStopped: false, items: [] },
    ] } }
    const preparedFailure = reduceStream(previous, { ...settled, revision: 3,
      payload: { ...settled.payload, failure: { ...failure, source } } }).state
    assert.equal(preparedFailure.session.facts, previous.session.facts)
  }
})

test('局部更新先到时，旧快照、创建响应和历史读取都不能撤销改名或模型保存', () => {
  let state = acceptBootstrap(initialSyncState(), bootstrap(8, [summary]))
  const catalog = { providers: [{ providerId: 'saved' }], defaultSelection: null, error: null }
  state = acceptModelCatalog(state, { revision: 11, catalog })
  const renamed = { ...summary, title: 'new title', updatedAt: '2026-10-23T00:01:00Z' }
  state = reduceStream(state, { type: 'session_summary_changed', revision: 12, summary: renamed }).state
  state = reduceStream(state, { type: 'app_changed', revision: 13, payload: bootstrap(9, [summary]) }).state
  state = acceptSessionStart(state, started)
  assert.equal(state.bootstrap.modelCatalog, catalog)
  assert.deepEqual(state.bootstrap.sessionsByWorkspace['c:/project'], [renamed])
  const oldRead = { revision: 10, history: { summary, turns: [] }, runtime: started.status, backgroundTasks: [], context: null }
  assert.equal(acceptSessionRead(state, oldRead), state)
  assert.equal(acceptModelCatalog(state, { revision: 10, catalog: {} }), state)

  const settled = { ...renamed, status: 'completed', updatedAt: '2026-10-23T00:02:00Z' }
  const result = reduceStream(state, { type: 'session_settled', revision: 14, sessionId: summary.threadId,
    summary: settled, payload: { phase: 'idle', failure: null } })
  assert.equal(result.effect, null)
  assert.deepEqual(result.state.bootstrap.sessionsByWorkspace['c:/project'], [settled])
  assert.equal(result.state.bootstrap.modelCatalog, catalog)
})
