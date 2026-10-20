import { acceptExecutionEvent, readExecution, updateExecutionRuntime, type SessionView } from './execution'
import type { SessionReadResult, SessionRuntime, StreamEnvelope, AppBootstrap, TurnEventEnvelope } from './protocol'

export type LiveSessionState = Pick<SessionRuntime, 'sessionRevision' | 'phase' | 'terminal' | 'backgroundTasks'>
type LiveSessionUpdate = Pick<LiveSessionState, 'sessionRevision' | 'phase' | 'terminal'> & Partial<SessionRuntime>
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
function acceptLiveSession(state: SyncState, sessionId: string, incoming: LiveSessionUpdate, event?: TurnEventEnvelope): SyncState {
  const previous = state.liveSessions[sessionId]
  if (previous && incoming.sessionRevision <= previous.sessionRevision) return state
  const selected = state.session?.summary.threadId === sessionId ? state.session : null
  // 未选中的任务也保留后台命令；普通执行增量不替换这份列表。
  if (selected === null) {
    const lifecycle: LiveSessionState = { sessionRevision: incoming.sessionRevision, phase: incoming.phase,
      terminal: incoming.terminal, backgroundTasks: incoming.backgroundTasks ?? previous?.backgroundTasks ?? [] }
    return { ...state, liveSessions: { ...state.liveSessions, [sessionId]: lifecycle } }
  }
  const runtime: SessionRuntime = { ...selected.runtime, ...incoming }
  if (event?.usage) runtime.usage = event.usage
  const session = updateExecutionRuntime(selected, runtime)
  return {
    ...state,
    liveSessions: { ...state.liveSessions, [sessionId]: runtime },
    session: event ? { ...session, facts: acceptExecutionEvent(session.facts, event),
      summary: event.method === 'turn/started' && !session.facts.some(turn => turn.id === event.params.turn.turnId)
        ? { ...session.summary, turnCount: session.summary.turnCount + 1 } : session.summary } : session,
  }
}

/** 应用全局 bootstrap 元数据快照 */
export function acceptBootstrap(state: SyncState, bootstrap: AppBootstrap): SyncState {
  if (state.bootstrap !== null && bootstrap.revision < state.bootstrap.revision) return state
  const summary = Object.values(bootstrap.sessionsByWorkspace).flat().find(item => item.threadId === state.session?.summary.threadId)
  return { ...state, bootstrap, session: state.session && summary ? { ...state.session, summary } : state.session }
}

/** 目录快照是任务与工作区存在的权威来源：刷新后剔除已不存在会话的活动状态与
 *  无效选择。刚创建但尚未进入目录快照的任务由豁免保留，进入目录后豁免结束。 */
export function pruneStaleSelection(
  bootstrap: AppBootstrap,
  state: Pick<SyncState, 'liveSessions' | 'session'> & { selectedWorkspaceId: string | null; selectedSessionId: string | null },
) {
  const workspaces = new Set(bootstrap.workspaces.map(workspace => workspace.workspaceId))
  const sessions = new Set(Object.values(bootstrap.sessionsByWorkspace).flat().map(session => session.threadId))
  const liveSessions = Object.fromEntries(Object.entries(state.liveSessions).filter(([id]) => sessions.has(id)))
  const session = state.session !== null && !sessions.has(state.session.summary.threadId) ? null : state.session
  const workspaceRemoved = state.selectedWorkspaceId !== null && !workspaces.has(state.selectedWorkspaceId)
  const sessionRemoved = state.selectedSessionId !== null && !sessions.has(state.selectedSessionId)
  const selectionDropped = workspaceRemoved || sessionRemoved
  return {
    liveSessions,
    session: selectionDropped ? null : session,
    selectionDropped,
    selectedWorkspaceId: workspaceRemoved ? null : state.selectedWorkspaceId,
    selectedSessionId: selectionDropped ? null : state.selectedSessionId,
  }
}

export function resetBaseline(state: SyncState, bootstrap: AppBootstrap): SyncState {
  const session = state.session
  const liveSessions: SyncState['liveSessions'] = Object.fromEntries(Object.entries(bootstrap.sessionPhases)
    .map(([id, phase]) => [id, { sessionRevision: 0, phase, terminal: null,
      backgroundTasks: state.liveSessions[id]?.backgroundTasks ?? [] }]))
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

type SyncEffect = 'resync' | 'refresh_bootstrap'
interface SyncReduction { state: SyncState; effect: SyncEffect | null }

export function reduceStream(state: SyncState, frame: StreamEnvelope): SyncReduction {
  if (frame.type === 'ready' || frame.type === 'resync_required') return { state, effect: 'resync' }
  if (frame.revision <= state.revision) return { state, effect: null }
  const next = { ...state, revision: frame.revision }
  switch (frame.type) {
    case 'app_changed':
      return { state: acceptBootstrap(next, { ...frame.payload, revision: frame.revision }), effect: null }
    case 'session_changed':
      return { state: acceptLiveSession(next, frame.sessionId, frame.payload), effect: null }
    case 'turn_event': {
      const previous = state.liveSessions[frame.sessionId]
      return { state: acceptLiveSession(next, frame.sessionId, {
        sessionRevision: frame.payload.sessionRevision,
        phase: frame.payload.phase,
        terminal: previous?.terminal ?? null,
      }, frame.payload), effect: null }
    }
    case 'background_task_updated': {
      const previous = state.liveSessions[frame.sessionId]
      const tasks = previous?.backgroundTasks ?? []
      const task = frame.payload.task
      const backgroundTasks = tasks.some(value => value.taskId === task.taskId)
        ? tasks.map(value => value.taskId === task.taskId ? task : value) : [...tasks, task]
      return { state: acceptLiveSession(next, frame.sessionId, {
        sessionRevision: frame.payload.sessionRevision, phase: frame.payload.phase,
        terminal: previous?.terminal ?? null, backgroundTasks,
      }), effect: null }
    }
    case 'session_settled': {
      const accepted = acceptLiveSession(next, frame.sessionId, frame.payload)
      if (accepted === next) return { state: next, effect: null }
      return { state: accepted, effect: 'refresh_bootstrap' }
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
