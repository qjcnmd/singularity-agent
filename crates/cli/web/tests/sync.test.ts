import assert from 'node:assert/strict'
import { test } from 'node:test'
import { acceptBootstrap, acceptSessionRead, initialSyncState, reduceStream, resetBaseline, type SyncState } from '../src/sync'
import { protocolVersion } from '../src/protocol'
import { bootstrap, control, frame, historyPage, liveRuntime, runtime, session, sessionFrame } from './fixtures'

function baseline() { return acceptSessionRead(resetBaseline(initialSyncState(), bootstrap()), session()) }

test('bootstrap refresh leaves unseen stream events available and rejects older titles', () => {
  let state = acceptBootstrap(baseline(), bootstrap({ revision: 2 }))
  state = reduceStream(state, 's', frame(1, 'unseen'), '').state
  assert.equal(state.session?.facts.active.flatMap(turn => turn.items).length, 1)
  assert.equal(state.revision, 1)
  const same = acceptBootstrap(state, bootstrap({ revision: 1 }))
  assert.equal(same, state)
})

test('late reads cannot overwrite applied deltas; snapshot watermark suppresses covered events', () => {
  let state = reduceStream(baseline(), 's', frame(1, 'newer'), '').state
  assert.equal(acceptSessionRead(state, session()), state)
  state = acceptSessionRead(state, session({ runtime: runtime({ sessionRevision: 3, phase: 'stopping' }) }))
  state = reduceStream(state, 's', frame(2, 'covered by snapshot'), '').state
  assert.equal(state.session?.facts.active.flatMap(turn => turn.items).length, 0)
  assert.equal(state.liveSessions.s.phase, 'stopping')
  assert.equal(state.revision, 2)
})

test('gaps, host changes and lag signals request resync without consuming a partial stream', () => {
  const state = baseline()
  for (const incoming of [
    frame(2, 'gap'), { ...frame(1, 'new host'), generation: 'another' },
    { version: protocolVersion, generation: 'g', revision: 0, type: 'resync_required' as const, payload: { reason: 'client_lagged' } },
  ]) {
    assert.deepEqual(reduceStream(state, 's', incoming, ''), { state, effects: ['resync'] })
  }
  const first = reduceStream(state, 's', frame(1, 'accepted'), '').state
  assert.equal(reduceStream(first, 's', frame(1, 'duplicate'), '').state, first)
})

test('selected and background late deltas retain stopping and reject older snapshots', () => {
  let state = baseline()
  state = reduceStream(state, 's', sessionFrame(1, runtime({ sessionRevision: 3, phase: 'stopping' })), '').state
  state = reduceStream(state, 's', { ...frame(2, 'late'), payload: { ...frame(2, '').payload, sessionRevision: 4 } }, '').state
  assert.equal(state.session?.runtime.phase, 'stopping')
  state = reduceStream(state, 's', sessionFrame(3, runtime({ sessionRevision: 3, phase: 'stopping' }), 'other'), '').state
  state = reduceStream(state, 's', { ...frame(4, 'late'), sessionId: 'other' }, '').state
  state = reduceStream(state, 's', sessionFrame(5, runtime({ sessionRevision: 2 }), 'other'), '').state
  assert.equal(state.liveSessions.other.phase, 'stopping')
  assert.equal(state.liveSessions.other.sessionRevision, 4)
})

test('a control lifecycle snapshot replaces the queue without replaying active events', () => {
  const pending = control()
  const activeTurn = runtime().activeTurn!
  let state = acceptSessionRead(baseline(), session({ runtime: runtime({ sessionRevision: 1, pendingControls: [pending], activeTurn }), activeEvents: [frame(1, 'streamed').payload] }))
  const previousItem = state.session?.facts.active[0].items[0]
  state = reduceStream(state, 's', sessionFrame(1, liveRuntime({
    sessionRevision: 2, pendingControls: [],
    activeTurn: { turnId: activeTurn.turnId, startedAt: activeTurn.startedAt },
  })), '').state
  assert.deepEqual(state.session?.runtime.pendingControls, [])
  assert.equal(state.session?.facts.active[0].items[0].kind, 'assistant')
  assert.strictEqual(state.session?.facts.active[0].items[0], previousItem)
})

test('fresh history retains a loaded prefix only while it overlaps', () => {
  const loaded = (state: SyncState) => state.session?.facts.history.map(turn => turn.id)
  const page = (value: ReturnType<typeof historyPage>) => value.history.turns.map(turn => turn.turnId)
  let state = acceptSessionRead(baseline(), historyPage(1, 80))
  state = acceptSessionRead(state, historyPage(42, 81))
  assert.deepEqual(loaded(state), page(historyPage(1, 81)))
  assert.equal(state.session?.nextCursor, null)
  const gap = acceptSessionRead(state, historyPage(101, 140))
  assert.deepEqual(loaded(gap), page(historyPage(101, 140)))
  assert.equal(gap.session?.nextCursor, historyPage(101, 140).history.nextCursor)
})

test('settlement schedules a selected read and bootstrap refresh only for fresh session facts', () => {
  const incoming = { version: protocolVersion, generation: 'g', revision: 1, type: 'session_settled' as const, sessionId: 's',
    payload: { runtime: runtime({ phase: 'idle', sessionRevision: 1, activeTurn: null }) } }
  const streaming = reduceStream(baseline(), 's', frame(1, 'visible result'), '').state
  const reduced = reduceStream(streaming, 's', { ...incoming, revision: 2, payload: { runtime: runtime({ phase: 'idle', sessionRevision: 2, activeTurn: null }) } }, '')
  assert.deepEqual(reduced.effects, ['read_selected', 'refresh_bootstrap'])
  assert.equal(reduced.state.session?.runtime.phase, 'idle')
  assert.strictEqual(reduced.state.session?.runtime, reduced.state.liveSessions.s)
  assert.deepEqual(reduced.state.session?.facts.active, streaming.session?.facts.active)
  assert.equal(acceptSessionRead(reduced.state, session()), reduced.state)
  assert.deepEqual(reduceStream(reduced.state, 's', { ...incoming, revision: 2 }, '').effects, [])
})

// 生产者保证一次 session.read 内 history 与 runtime 属于同一逻辑状态（crates/cli
// 的一致捕获）。这里固定消费者的接纳规则：读取不得把更旧的生命周期事实写回。
test('a session read is accepted as one snapshot and never regresses the lifecycle', () => {
  // 读取可能对应一个比当前已知 revision 更旧的投影（例如捕获后又有事件到达）；
  // history 与 runtime 必须一起被拒绝，不能只取其中一半。
  let state = baseline()
  state = reduceStream(state, 's', frame(1, 'newer delta'), '').state
  assert.equal(acceptSessionRead(state, session()), state)
  assert.equal(state.session?.runtime.sessionRevision, 1)

  // 同 revision 的读取按快照接纳，history 与 runtime 一起更新。
  const same = acceptSessionRead(state, session({ runtime: runtime({ sessionRevision: 1 }) }))
  assert.notEqual(same, state)
  assert.equal(same.session?.runtime.sessionRevision, state.session?.runtime.sessionRevision)
  assert.deepEqual(same.liveSessions.s, same.session?.runtime)

  // 更新 revision 的读取同时替换 history 与 runtime：不会出现「版本新、history 旧」。
  const newer = acceptSessionRead(state, historyPage(1, 3))
  assert.equal(newer.session?.runtime.sessionRevision, 3)
  assert.equal(newer.session?.facts.history?.length, 3)
  assert.equal(newer.session?.runtime.phase, 'idle')
})
