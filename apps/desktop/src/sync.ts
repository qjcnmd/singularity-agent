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

/** 列表和所选任务共用同一份运行状态；旧版本的更新不覆盖当前状态。 */
function acceptLiveSession(state: SyncState, sessionId: string, incoming: LiveSessionState, event?: TurnEventEnvelope): SyncState {
  const previous = state.liveSessions[sessionId]
  if (previous && incoming.sessionRevision <= previous.sessionRevision) return state
  const selected = state.session?.summary.threadId === sessionId ? state.session : null
  // 后台会话只有生命周期字段，不伪造完整 runtime；所选会话的 runtime 是同一份
  // lifecycle 对象的完整形状，detail 与列表共享这个引用。
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

/** RPC 快照不消耗 stream revision；未见过的 stream 事件仍然可用。 */
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

/** 后台任务完成时标记未读；重新运行或选中任务时清除。 */
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
