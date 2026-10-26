import { workspaceKey } from './workspace'
import { acceptExecutionEvent, finishTurn, readExecution, type SessionView } from './execution'
import type { SessionReadResult, SessionStartResult, SessionRuntime, StreamEnvelope, AppBootstrap, TurnEventEnvelope, ThreadSummary, ModelCatalogResult } from './protocol'

export type LiveSessionState = Pick<SessionRuntime, 'phase' | 'failure'> & {
  backgroundTasks: SessionReadResult['backgroundTasks']; revision: number; summaryRevision?: number
}
type LiveSessionUpdate = Pick<LiveSessionState, 'phase' | 'failure'> & Partial<SessionRuntime> & Partial<Pick<LiveSessionState, 'backgroundTasks'>>

export interface SyncState {
  revision: number
  modelCatalogRevision: number
  bootstrap: AppBootstrap | null
  session: SessionView | null
  liveSessions: Record<string, LiveSessionState>
}
export const initialSyncState = (): SyncState => ({
  revision: 0, modelCatalogRevision: 0, bootstrap: null, session: null, liveSessions: {},
})

/** 更新指定会话的实时运行状态，按全局事件位置丢弃过时更新 */
function acceptLiveSession(state: SyncState, sessionId: string, incoming: LiveSessionUpdate, revision: number, event?: TurnEventEnvelope): SyncState {
  const previous = state.liveSessions[sessionId]
  if (previous && revision <= previous.revision) return state
  const selected = state.session?.summary.threadId === sessionId ? state.session : null
  const { backgroundTasks = previous?.backgroundTasks ?? [], ...update } = incoming
  const lifecycle: LiveSessionState = { revision, summaryRevision: previous?.summaryRevision, phase: update.phase, failure: update.failure, backgroundTasks }
  const liveSessions = { ...state.liveSessions, [sessionId]: lifecycle }
  if (selected === null) {
    return { ...state, liveSessions }
  }
  const runtime: SessionRuntime = { ...selected.runtime, ...update }
  const session = { ...selected, runtime,
    context: event?.method === 'context/loaded' ? event.params.context : selected.context }
  return {
    ...state,
    liveSessions,
    session: event ? { ...session, facts: acceptExecutionEvent(session.facts, event) } : session,
  }
}

/** 合并 `app.bootstrap` 快照，同时保留读取期间已收到更新的事件修订版本。 */
function acceptBootstrap(state: SyncState, bootstrap: AppBootstrap): SyncState {
  if (state.bootstrap !== null && bootstrap.revision < state.bootstrap.revision) return state
  const sessionsByWorkspace = { ...bootstrap.sessionsByWorkspace }
  // 目录读取期间任务仍可更新；保留在这次读取开始之后收到的摘要。
  for (const [workspaceRoot, sessions] of Object.entries(state.bootstrap?.sessionsByWorkspace ?? {})) {
    const incoming = sessionsByWorkspace[workspaceRoot] ?? []
    const newer = sessions.filter(session => (state.liveSessions[session.threadId]?.summaryRevision ?? 0) > bootstrap.revision)
    if (newer.length > 0) sessionsByWorkspace[workspaceRoot] = sortSessions([
      ...newer, ...incoming.filter(item => !newer.some(session => session.threadId === item.threadId)),
    ])
  }
  bootstrap = { ...bootstrap, sessionsByWorkspace,
    modelCatalog: state.bootstrap && state.modelCatalogRevision > bootstrap.revision ? state.bootstrap.modelCatalog : bootstrap.modelCatalog }
  const summary = Object.values(bootstrap.sessionsByWorkspace).flat().find(item => item.threadId === state.session?.summary.threadId)
  return { ...state, bootstrap, session: state.session && summary ? { ...state.session, summary } : state.session }
}

export function acceptModelCatalog(state: SyncState, result: ModelCatalogResult): SyncState {
  if (!state.bootstrap || result.revision < Math.max(state.modelCatalogRevision, state.bootstrap.revision)) return state
  return { ...state, modelCatalogRevision: result.revision, bootstrap: { ...state.bootstrap, modelCatalog: result.catalog } }
}

function sortSessions(sessions: ThreadSummary[]): ThreadSummary[] {
  return sessions.sort((a, b) => b.updatedAt.localeCompare(a.updatedAt) || a.threadId.localeCompare(b.threadId))
}

/** 标题和结束状态只替换所属项目中的一行，其他项目保持原来的引用。 */
function acceptSummary(state: SyncState, summary: ThreadSummary, revision: number): SyncState {
  const bootstrap = state.bootstrap
  const previous = state.liveSessions[summary.threadId]
  if (!bootstrap || revision <= Math.max(bootstrap.revision, previous?.summaryRevision ?? 0)) return state
  const workspaceRoot = workspaceKey(summary.cwd)
  if (!(workspaceRoot in bootstrap.sessionsByWorkspace)) return state
  const sessions = bootstrap.sessionsByWorkspace[workspaceRoot] ?? []
  return { ...state,
    liveSessions: { ...state.liveSessions, [summary.threadId]: {
      ...(previous ?? { phase: 'idle', failure: null, backgroundTasks: [], revision: 0 }), summaryRevision: revision,
    } },
    session: state.session?.summary.threadId === summary.threadId ? { ...state.session, summary } : state.session,
    bootstrap: { ...bootstrap, sessionsByWorkspace: { ...bootstrap.sessionsByWorkspace,
      [workspaceRoot]: sortSessions([summary, ...sessions.filter(item => item.threadId !== summary.threadId)]),
    } },
  }
}

/** 首次发送只更新这个任务，不改变整个目录的事件序号。 */
export function acceptSessionStart(state: SyncState, started: SessionStartResult): SyncState {
  if (state.bootstrap!.revision >= started.revision) return state
  const next = acceptLiveSession(state, started.summary.threadId, started.status, started.revision)
  return acceptSummary(next, started.summary, started.revision)
}

/** 根据最新目录剔除已删除项目的选中态与已删除任务的内存状态，并在未选中项目时默认选中第一个项目。 */
export function pruneStaleSelection(
  bootstrap: AppBootstrap,
  state: Pick<SyncState, 'liveSessions' | 'session'> & { selectedWorkspaceRoot: string | null; selectedSessionId: string | null },
) {
  const workspaces = new Set(bootstrap.workspaces.map(workspace => workspaceKey(workspace.root)))
  const sessions = new Set(Object.values(bootstrap.sessionsByWorkspace).flat().map(session => session.threadId))
  const liveSessions = Object.fromEntries(Object.entries(state.liveSessions).filter(([id]) => sessions.has(id)))
  const session = state.session !== null && !sessions.has(state.session.summary.threadId) ? null : state.session
  const workspaceRemoved = state.selectedWorkspaceRoot !== null && !workspaces.has(state.selectedWorkspaceRoot)
  const sessionRemoved = state.selectedSessionId !== null && !sessions.has(state.selectedSessionId)
  const selectionDropped = workspaceRemoved || sessionRemoved
  return {
    liveSessions,
    session: selectionDropped ? null : session,
    selectionDropped,
    selectedWorkspaceRoot: workspaceRemoved ? null : state.selectedWorkspaceRoot,
    selectedSessionId: selectionDropped ? null : state.selectedSessionId,
  }
}

export function resetBaseline(state: SyncState, bootstrap: AppBootstrap): SyncState {
  const accepted = acceptBootstrap(state, bootstrap)
  bootstrap = accepted.bootstrap!
  const session = accepted.session
  const liveSessions: SyncState['liveSessions'] = Object.fromEntries(Object.entries(bootstrap.sessionStates)
    .map(([id, status]) => [id, { revision: 0, ...status,
      summaryRevision: state.liveSessions[id]?.summaryRevision,
      backgroundTasks: state.liveSessions[id]?.backgroundTasks ?? [] }]))
  for (const [id, live] of Object.entries(state.liveSessions)) {
    if (live.revision > bootstrap.revision || (live.summaryRevision ?? 0) > bootstrap.revision) liveSessions[id] = live
  }
  if (session) liveSessions[session.summary.threadId] = {
    ...state.liveSessions[session.summary.threadId], phase: session.runtime.phase, failure: session.runtime.failure,
    revision: state.liveSessions[session.summary.threadId]?.revision ?? 0,
  }
  return {
    revision: bootstrap.revision, modelCatalogRevision: Math.max(state.modelCatalogRevision, bootstrap.revision), bootstrap,
    session, liveSessions,
  }
}

export function acceptSessionRead(state: SyncState, source: SessionReadResult): SyncState {
  const id = source.history.summary.threadId
  const previous = state.liveSessions[id]
  if (source.revision < Math.max(previous?.revision ?? 0, previous?.summaryRevision ?? 0)) return state
  state = acceptSummary(state, source.history.summary, source.revision)
  const session = readExecution(source)
  return { ...state, session, liveSessions: { ...state.liveSessions, [id]: {
    phase: session.runtime.phase, failure: session.runtime.failure,
    backgroundTasks: source.backgroundTasks, revision: source.revision,
    summaryRevision: state.liveSessions[id]?.summaryRevision,
  } } }
}

export function reduceStream(state: SyncState, frame: Exclude<StreamEnvelope, { type: 'ready' }>): SyncState {
  if (frame.revision <= state.revision) return state
  const next = { ...state, revision: frame.revision }
  switch (frame.type) {
    case 'app_read_failed':
      return next
    case 'app_changed':
      return acceptBootstrap(next, frame.payload)
    case 'session_summary_changed':
      return acceptSummary(next, frame.summary, frame.revision)
    case 'session_changed':
      return acceptLiveSession(next, frame.sessionId, frame.payload, frame.revision)
    case 'turn_event': {
      const previous = state.liveSessions[frame.sessionId]
      return acceptLiveSession(next, frame.sessionId, {
        phase: frame.payload.phase,
        failure: previous?.failure ?? null,
      }, frame.revision, frame.payload)
    }
    case 'background_task_updated': {
      const previous = state.liveSessions[frame.sessionId]
      const tasks = previous?.backgroundTasks ?? []
      const task = frame.payload.task
      const backgroundTasks = tasks.some(value => value.taskId === task.taskId)
        ? tasks.map(value => value.taskId === task.taskId ? task : value) : [...tasks, task]
      return acceptLiveSession(next, frame.sessionId, {
        phase: frame.payload.phase,
        failure: previous?.failure ?? null, backgroundTasks,
      }, frame.revision)
    }
    case 'session_settled': {
      let accepted = acceptLiveSession(next, frame.sessionId, frame.payload, frame.revision)
      if (accepted === next) return next
      const session = accepted.session
      const turn = session?.facts.at(-1)
      // 保存失败时没有 turn/error 事件；结束页面上的临时输出，不补造已保存的结果或时间。
      if (frame.payload.failure?.source === 'turn' && session?.summary.threadId === frame.sessionId && turn?.status === 'running') {
        accepted = { ...accepted, session: { ...session,
          facts: [...session.facts.slice(0, -1), finishTurn(turn, 'failed')],
        } }
      }
      return acceptSummary(accepted, frame.summary, frame.revision)
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
