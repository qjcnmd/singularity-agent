import { acceptExecutionEvent, readExecution, updateExecutionRuntime, type SessionRuntime, type SessionView } from './execution'
import { eventTurnId } from './protocol'
import type { SessionReadResult, SessionRuntime as WireSessionRuntime, StreamEnvelope, WorkbenchBootstrap } from './protocol'

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

/** 所选 detail 持有对此 map 所拥有的 lifecycle 对象的引用。 */
export function acceptLiveSession(state: SyncState, sessionId: string, incoming: LiveSessionState | WireSessionRuntime): SyncState {
  const previous = state.liveSessions[sessionId]
  if (previous && incoming.sessionRevision <= previous.sessionRevision) return state
  const selected = state.session?.summary.threadId === sessionId ? state.session : null
  // 后台会话只有生命周期字段，不伪造完整 runtime；所选会话的 runtime 是同一
  // 份 lifecycle 对象的完整形状，detail 因此与列表共享这一个引用。
  if (selected === null) {
    const lifecycle: LiveSessionState = { sessionRevision: incoming.sessionRevision, phase: incoming.phase, terminal: incoming.terminal }
    return { ...state, liveSessions: { ...state.liveSessions, [sessionId]: lifecycle } }
  }
  const owner: SessionRuntime = { ...selected.runtime, ...incoming }
  return { ...state, liveSessions: { ...state.liveSessions, [sessionId]: owner },
    session: updateExecutionRuntime(selected, owner) }
}

/** RPC 快照不消耗 stream revision；未见过的 stream 事件仍然可用。 */
export function acceptBootstrap(state: SyncState, bootstrap: WorkbenchBootstrap): SyncState {
  if (state.bootstrap !== null && bootstrap.revision < state.bootstrap.revision) return state
  return { ...state, bootstrap }
}

export function resetBaseline(state: SyncState, bootstrap: WorkbenchBootstrap): SyncState {
  const session = state.generation === bootstrap.generation ? state.session : null
  const liveSessions: SyncState['liveSessions'] = Object.fromEntries(Object.entries(bootstrap.sessionPhases)
    .map(([id, phase]) => [id, { sessionRevision: 0, phase, terminal: null }]))
  if (session) liveSessions[session.summary.threadId] = session.runtime
  return {
    generation: bootstrap.generation, revision: bootstrap.revision, bootstrap,
    session, liveSessions,
  }
}

export function acceptSessionRead(state: SyncState, source: SessionReadResult): SyncState {
  const id = source.history.summary.threadId
  if (source.runtime.sessionRevision < (state.liveSessions[id]?.sessionRevision ?? 0)) return state
  const previous = state.session?.summary.threadId === id ? state.session : null
  const session = readExecution(source, previous)
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
    next = acceptLiveSession(next, id, frame.payload)
  } else if (frame.type === 'turn_event') {
    const event = frame.payload
    const previous = next.liveSessions[id]
    const accepted = acceptLiveSession(next, id, {
      sessionRevision: event.sessionRevision, phase: previous?.phase === 'stopping' ? 'stopping' : 'running', terminal: previous?.terminal ?? null,
    })
    if (accepted !== next && accepted.session?.summary.threadId === id) {
      const session = accepted.session
      const turnId = eventTurnId(event)
      const activeTurn = event.method === 'turn/started' && turnId !== null
        ? { turnId, startedAt: event.params.startedAt }
        : session.runtime.activeTurn ?? (turnId === null ? null : { turnId, startedAt: now })
      const runtime = { ...session.runtime, activeTurn }
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

/** 所选 task 之外完成的内容变为未读；activity 与 selection 会清除它。 */
export function reduceUnread(
  unread: ReadonlySet<string>,
  previous: SyncState['liveSessions'],
  next: SyncState['liveSessions'],
  selected: string | null,
): ReadonlySet<string> {
  const result = new Set(unread)
  for (const [id, runtime] of Object.entries(next)) {
    if (runtime.phase !== 'idle') result.delete(id)
    else if (previous[id] !== undefined && previous[id].phase !== 'idle' && id !== selected) result.add(id)
  }
  for (const id of result) if (next[id] === undefined) result.delete(id)
  if (selected !== null) result.delete(selected)
  return result.size === unread.size && [...result].every(id => unread.has(id)) ? unread : result
}
