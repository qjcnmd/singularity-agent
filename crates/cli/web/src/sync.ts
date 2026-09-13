import { acceptExecutionEvent, readExecution, runtimeDetails, updateExecutionRuntime, type SessionRuntime, type SessionView } from './execution'
import { eventTurnId } from './protocol'
import type { SessionReadResult, SessionSnapshot, StreamEnvelope, ThreadReadPage, WorkbenchBootstrap } from './protocol'

export type LiveSessionState = Pick<SessionRuntime, 'sessionRevision' | 'phase' | 'terminal'>
export interface SyncState {
  generation: string | null
  revision: number
  bootstrap: WorkbenchBootstrap | null
  session: SessionView | null
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


/** The selected detail keeps a reference to the lifecycle object owned by this map. */
export function acceptLiveSession(state: SyncState, sessionId: string, incoming: LiveSessionState | SessionSnapshot): SyncState {
  const previous = state.liveSessions[sessionId]
  if (previous && incoming.sessionRevision <= previous.sessionRevision) return state
  const runtime = 'activeTurn' in incoming ? runtimeDetails(incoming) : incoming
  const selected = state.session?.history.summary.threadId === sessionId ? state.session : null
  const owner = selected ? { ...selected.runtime, ...runtime }
    : { sessionRevision: runtime.sessionRevision, phase: runtime.phase, terminal: runtime.terminal }
  return { ...state, liveSessions: { ...state.liveSessions, [sessionId]: owner },
    session: selected ? updateExecutionRuntime(selected, owner as SessionRuntime) : state.session }
}

/** RPC snapshots do not consume stream revisions; unseen stream events remain available. */
export function acceptBootstrap(state: SyncState, bootstrap: WorkbenchBootstrap): SyncState {
  if (state.bootstrap !== null && bootstrap.revision < state.bootstrap.revision) return state
  return { ...state, bootstrap }
}

export function resetBaseline(state: SyncState, bootstrap: WorkbenchBootstrap): SyncState {
  const session = state.generation === bootstrap.generation ? state.session : null
  const liveSessions: SyncState['liveSessions'] = Object.fromEntries(Object.entries(bootstrap.sessionPhases)
    .map(([id, phase]) => [id, { sessionRevision: 0, phase, terminal: null }]))
  if (session) liveSessions[session.history.summary.threadId] = session.runtime
  return {
    generation: bootstrap.generation, revision: bootstrap.revision, bootstrap,
    session, liveSessions,
  }
}

export function acceptSessionRead(state: SyncState, source: SessionReadResult): SyncState {
  const id = source.history.summary.threadId
  if (source.runtime.sessionRevision < (state.liveSessions[id]?.sessionRevision ?? 0)) return state
  const previous = state.session?.history.summary.threadId === id ? state.session : null
  const session = readExecution({ ...source, history: mergeTailHistory(previous?.history, source.history) })
  return { ...state, session, liveSessions: { ...state.liveSessions, [id]: session.runtime } }
}

export type SyncEffect = 'resync' | 'read_selected' | 'refresh_bootstrap'
export interface SyncReduction { state: SyncState; effects: SyncEffect[] }

export function reduceStream(state: SyncState, selectedSessionId: string | null, frame: StreamEnvelope, now: string): SyncReduction {
  if (frame.type === 'ready' || frame.type === 'resync_required' || frame.generation !== state.generation) return { state, effects: ['resync'] }
  if (frame.revision <= state.revision) return { state, effects: [] }
  if (frame.revision !== state.revision + 1) return { state, effects: ['resync'] }
  let next = { ...state, revision: frame.revision }
  if (frame.type === 'workbench_changed') return { state: acceptBootstrap(next, { ...frame.payload, revision: frame.revision }), effects: [] }
  const id = frame.sessionId
  if (frame.type === 'session_changed') {
    const accepted = acceptLiveSession(next, id, frame.payload)
    if (accepted !== next && accepted.session?.history.summary.threadId === id) {
      const session = readExecution({ history: accepted.session.history, runtime: frame.payload })
      next = { ...accepted, session, liveSessions: { ...accepted.liveSessions, [id]: session.runtime } }
    } else next = accepted
  } else if (frame.type === 'turn_event') {
    const event = frame.payload
    const previous = next.liveSessions[id]
    const accepted = acceptLiveSession(next, id, {
      sessionRevision: event.sessionRevision, phase: previous?.phase === 'stopping' ? 'stopping' : 'running', terminal: previous?.terminal ?? null,
    })
    if (accepted !== next && accepted.session?.history.summary.threadId === id) {
      const session = accepted.session
      const turnId = eventTurnId(event)
      const runtime = { ...session.runtime, activeTurn: event.method === 'turn/started'
        ? { turnId, startedAt: event.params.startedAt } : session.runtime.activeTurn ?? { turnId, startedAt: now } }
      next = { ...accepted, session: { ...session, runtime, facts: acceptExecutionEvent(session.facts, event) },
        liveSessions: { ...accepted.liveSessions, [id]: runtime } }
    } else next = accepted
  } else if (frame.type === 'session_settled') {
    const accepted = acceptLiveSession(next, id, frame.payload.runtime)
    if (accepted !== next) return { state: accepted, effects: id === selectedSessionId
      ? ['read_selected', 'refresh_bootstrap'] : ['refresh_bootstrap'] }
  }
  return { state: next, effects: [] }
}
