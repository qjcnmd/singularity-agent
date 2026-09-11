import assert from 'node:assert/strict'
import { test } from 'node:test'
import { acceptBootstrap, acceptSessionRead, initialSyncState, reduceStream, resetBaseline } from '../src/sync'
import { bootstrap, control, frame, historyPage, runtime, session, sessionFrame } from './fixtures'

function baseline() { return acceptSessionRead(resetBaseline(initialSyncState(), bootstrap()), session()) }

test('bootstrap refresh leaves unseen stream events available and rejects older titles', () => {
  let state = acceptBootstrap(baseline(), bootstrap({ revision: 2 }))
  state = reduceStream(state, 's', frame(1, 'unseen'), '').state
  assert.equal(state.session?.runtime.activeTurn?.events.length, 1)
  assert.equal(state.revision, 1)
  const same = acceptBootstrap(state, bootstrap({ revision: 1 }))
  assert.equal(same, state)
})

test('late reads cannot overwrite applied deltas; snapshot watermark suppresses covered events', () => {
  let state = reduceStream(baseline(), 's', frame(1, 'newer'), '').state
  assert.equal(acceptSessionRead(state, session()), state)
  state = acceptSessionRead(state, session({ runtime: runtime({ sessionRevision: 3, phase: 'stopping' }) }))
  state = reduceStream(state, 's', frame(2, 'covered by snapshot'), '').state
  assert.equal(state.session?.runtime.activeTurn?.events.length, 0)
  assert.equal(state.liveSessions.s.phase, 'stopping')
  assert.equal(state.revision, 2)
})

test('gaps, host changes and lag signals request resync without consuming a partial stream', () => {
  const state = baseline()
  for (const incoming of [
    frame(2, 'gap'), { ...frame(1, 'new host'), generation: 'another' },
    { version: 1, generation: 'g', revision: 0, type: 'resync_required' as const, payload: { reason: 'client_lagged' } },
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

test('a control lifecycle snapshot replaces the queue without dropping active events', () => {
  const pending = control()
  const activeTurn = { ...runtime().activeTurn!, events: [frame(1, 'streamed').payload] }
  let state = acceptSessionRead(baseline(), session({ runtime: runtime({ sessionRevision: 1, controls: [pending], pendingControls: [pending], activeTurn }) }))
  const started = { ...pending, disposition: 'started_as_new_turn' as const }
  state = reduceStream(state, 's', sessionFrame(1, runtime({ sessionRevision: 2, controls: [started], pendingControls: [], activeTurn })), '').state
  assert.deepEqual(state.session?.runtime.controls, [started])
  assert.deepEqual(state.session?.runtime.pendingControls, [])
  assert.deepEqual(state.session?.runtime.activeTurn?.events, activeTurn.events)
})

test('fresh history retains a loaded prefix only while it overlaps', () => {
  let state = acceptSessionRead(baseline(), historyPage(1, 80))
  state = acceptSessionRead(state, historyPage(42, 81))
  assert.deepEqual(state.session?.history.turns, historyPage(1, 81).history.turns)
  assert.equal(state.session?.history.nextCursor, null)
  state = acceptSessionRead(state, historyPage(101, 140))
  assert.deepEqual(state.session?.history, historyPage(101, 140).history)
})

test('settlement schedules a selected read and bootstrap refresh only for fresh session facts', () => {
  const incoming = { version: 1, generation: 'g', revision: 1, type: 'session_settled' as const, sessionId: 's',
    payload: { runtime: runtime({ phase: 'idle', sessionRevision: 1, activeTurn: null }) } }
  const reduced = reduceStream(baseline(), 's', incoming, '')
  assert.deepEqual(reduced.effects, ['read_selected', 'refresh_bootstrap'])
  assert.deepEqual(reduceStream(reduced.state, 's', { ...incoming, revision: 2 }, '').effects, [])
})
