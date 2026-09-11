import { appendEvent } from './eventLog'
import { eventTurnId } from './protocol'
import type { SessionPhase, SessionReadResult, SessionSnapshot, StreamEnvelope, ThreadReadPage, WorkbenchBootstrap } from './protocol'

export interface LiveSessionState {
  sessionRevision: number
  phase: SessionPhase
  terminal: SessionSnapshot['terminal']
}

export interface SyncState {
  generation: string | null
  revision: number
  bootstrap: WorkbenchBootstrap | null
  session: SessionReadResult | null
  liveSessions: Record<string, LiveSessionState>
}

export const initialSyncState = (): SyncState => ({
  generation: null, revision: 0, bootstrap: null, session: null, liveSessions: {},
})

/** Keep a loaded prefix only while the fresh tail overlaps it; otherwise expose the gap via its cursor. */
function mergeTailHistory(previous: ThreadReadPage | undefined, latest: ThreadReadPage): ThreadReadPage {
  const first = latest.turns[0]
  if (previous === undefined || first === undefined) return latest
  const overlap = previous.turns.findIndex(turn => turn.turnId === first.turnId)
  if (overlap < 0) return latest
  return { ...latest, turns: [...previous.turns.slice(0, overlap), ...latest.turns], nextCursor: previous.nextCursor }
}

export function acceptLiveSession(state: SyncState, sessionId: string, runtime: LiveSessionState): SyncState {
  const previous = state.liveSessions[sessionId]
  if (previous !== undefined && runtime.sessionRevision <= previous.sessionRevision) return state
  return { ...state, liveSessions: { ...state.liveSessions,
    [sessionId]: { sessionRevision: runtime.sessionRevision, phase: runtime.phase, terminal: runtime.terminal },
  } }
}

/** RPC snapshots do not consume stream revisions; that stream may still carry unseen events. */
export function acceptBootstrap(state: SyncState, bootstrap: WorkbenchBootstrap): SyncState {
  if (state.bootstrap !== null && bootstrap.revision < state.bootstrap.revision) return state
  return { ...state, bootstrap }
}

export function resetBaseline(state: SyncState, bootstrap: WorkbenchBootstrap): SyncState {
  return {
    generation: bootstrap.generation, revision: bootstrap.revision, bootstrap,
    session: state.generation === bootstrap.generation ? state.session : null,
    liveSessions: Object.fromEntries(Object.entries(bootstrap.sessionPhases)
      .map(([id, phase]) => [id, { sessionRevision: 0, phase, terminal: null }])),
  }
}

/** Selection and read identity are checked by the caller before this revision/history reduction. */
export function acceptSessionRead(state: SyncState, session: SessionReadResult): SyncState {
  const current = state.session?.summary.threadId === session.summary.threadId ? state.session : null
  if (current !== null && session.runtime.sessionRevision < current.runtime.sessionRevision) return state
  return { ...acceptLiveSession(state, session.summary.threadId, session.runtime),
    session: { ...session, history: mergeTailHistory(current?.history, session.history) },
  }
}

export type SyncEffect = 'resync' | 'read_selected' | 'refresh_bootstrap'
export interface SyncReduction { state: SyncState; effects: SyncEffect[] }

/** Pure stream reduction: IO, buffering and user actions are coordinated by the store. */
export function reduceStream(state: SyncState, selectedSessionId: string | null, frame: StreamEnvelope, now: string): SyncReduction {
  if (frame.type === 'ready' || frame.type === 'resync_required' || frame.generation !== state.generation) {
    return { state, effects: ['resync'] }
  }
  if (frame.revision <= state.revision) return { state, effects: [] }
  if (frame.revision !== state.revision + 1) return { state, effects: ['resync'] }
  let next = { ...state, revision: frame.revision }
  if (frame.type === 'workbench_changed') {
    return { state: acceptBootstrap(next, { ...frame.payload, revision: frame.revision }), effects: [] }
  }
  const sessionId = frame.sessionId
  if (frame.type === 'session_changed') {
    const runtime = frame.payload
    const live = acceptLiveSession(next, sessionId, runtime)
    if (live !== next && sessionId === selectedSessionId && next.session !== null
      && runtime.sessionRevision > next.session.runtime.sessionRevision) {
      next = { ...live, session: { ...next.session, runtime } }
    } else next = live
  } else if (frame.type === 'turn_event') {
    const event = frame.payload
    const previous = next.liveSessions[sessionId]
    const phase = previous?.phase === 'stopping' ? 'stopping' : 'running'
    const live = acceptLiveSession(next, sessionId, {
      sessionRevision: event.sessionRevision, phase, terminal: previous?.terminal ?? null,
    })
    if (live !== next && sessionId === selectedSessionId && next.session !== null
      && event.sessionRevision > next.session.runtime.sessionRevision) {
      const runtime = next.session.runtime
      const turnId = eventTurnId(event)
      const active = runtime.activeTurn ?? { turnId, events: [], startedAt: now }
      next = { ...live, session: { ...next.session, runtime: {
        ...runtime, sessionRevision: event.sessionRevision, phase,
        activeTurn: { ...active,
          turnId: event.method === 'turn/started' ? turnId : active.turnId,
          startedAt: event.method === 'turn/started' ? event.params.startedAt : active.startedAt,
          events: appendEvent(active.events, event),
        },
      } } }
    } else next = live
  } else if (frame.type === 'session_settled') {
    const live = acceptLiveSession(next, sessionId, frame.payload.runtime)
    if (live !== next) {
      return { state: live, effects: sessionId === selectedSessionId
        ? ['read_selected', 'refresh_bootstrap'] : ['refresh_bootstrap'] }
    }
  }
  return { state: next, effects: [] }
}
