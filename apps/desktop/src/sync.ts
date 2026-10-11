import { acceptExecutionEvent, readExecution, updateExecutionRuntime, type SessionView } from './execution'
import type { SessionReadResult, SessionRuntime, StreamEnvelope, AppBootstrap, TurnEventEnvelope } from './protocol'

export type LiveSessionState = Pick<SessionRuntime, 'sessionRevision' | 'phase' | 'terminal'>
export interface SyncState {
  revision: number
  bootstrap: AppBootstrap | null
  session: SessionView | null
  liveSessions: Record<string, LiveSessionState>
}
export const initialSyncState = (): SyncState => ({
  revision: 0, bootstrap: null, session: null, liveSessions: {},
})

/** 更新指定会话的实时运行状态，按 sessionRevision 丢弃过时更新 */
function acceptLiveSession(state: SyncState, sessionId: string, incoming: LiveSessionState, event?: TurnEventEnvelope): SyncState {
  const previous = state.liveSessions[sessionId]
  if (previous && incoming.sessionRevision <= previous.sessionRevision) return state
  const selected = state.session?.summary.threadId === sessionId ? state.session : null
  // 当前选中会话维护完整 runtime，非选中的后台会话仅维护基础生命周期状态
  if (selected === null) {
    const lifecycle: LiveSessionState = { sessionRevision: incoming.sessionRevision, phase: incoming.phase, terminal: incoming.terminal }
    return { ...state, liveSessions: { ...state.liveSessions, [sessionId]: lifecycle } }
  }
  const runtime: SessionRuntime = { ...selected.runtime, ...incoming }
  if (event?.method === 'turn/started') runtime.activeTurn = { turnId: event.params.turn.turnId }
  const session = updateExecutionRuntime(selected, runtime)
  return {
    ...state,
    liveSessions: { ...state.liveSessions, [sessionId]: runtime },
    session: event ? { ...session, facts: acceptExecutionEvent(session.facts, event) } : session,
  }
}

/** 应用全局 bootstrap 元数据快照 */
export function acceptBootstrap(state: SyncState, bootstrap: AppBootstrap): SyncState {
  if (state.bootstrap !== null && bootstrap.revision < state.bootstrap.revision) return state
  return { ...state, bootstrap }
}

export function resetBaseline(state: SyncState, bootstrap: AppBootstrap): SyncState {
  const session = state.session
  const liveSessions: SyncState['liveSessions'] = Object.fromEntries(Object.entries(bootstrap.sessionPhases)
    .map(([id, phase]) => [id, { sessionRevision: 0, phase, terminal: null }]))
  if (session) liveSessions[session.summary.threadId] = session.runtime
  return {
    revision: bootstrap.revision, bootstrap,
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

type SyncEffect = 'resync' | 'read_selected' | 'refresh_bootstrap'
interface SyncReduction { state: SyncState; effects: SyncEffect[] }

export function reduceStream(state: SyncState, selectedSessionId: string | null, frame: StreamEnvelope): SyncReduction {
  if (frame.type === 'ready' || frame.type === 'resync_required') return { state, effects: ['resync'] }
  if (frame.revision <= state.revision) return { state, effects: [] }
  const next = { ...state, revision: frame.revision }
  switch (frame.type) {
    case 'app_changed':
      return { state: acceptBootstrap(next, { ...frame.payload, revision: frame.revision }), effects: [] }
    case 'session_changed':
      return { state: acceptLiveSession(next, frame.sessionId, frame.payload), effects: [] }
    case 'turn_event': {
      const previous = state.liveSessions[frame.sessionId]
      return { state: acceptLiveSession(next, frame.sessionId, {
        sessionRevision: frame.payload.sessionRevision,
        phase: previous?.phase === 'stopping' ? 'stopping' : previous?.phase === 'compacting' ? 'compacting' : 'running',
        terminal: previous?.terminal ?? null,
      }, frame.payload), effects: [] }
    }
    case 'session_settled': {
      const accepted = acceptLiveSession(next, frame.sessionId, frame.payload)
      if (accepted === next) return { state: next, effects: [] }
      const effects: SyncEffect[] = frame.sessionId === selectedSessionId
        ? ['read_selected', 'refresh_bootstrap']
        : ['refresh_bootstrap']
      return { state: accepted, effects }
    }
  }
}

/** 计算会话未读状态：非当前选中任务执行完毕时标记未读，用户切换进入时清除 */
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
