import { workspaceKey } from './workspace'
import { acceptExecutionEvent, finishTurn, readExecution, type SessionView } from './execution'
import type { SessionReadResult, SessionStartResult, SessionRuntime, StreamEnvelope, AppBootstrap, TurnEventEnvelope, ThreadSummary, ModelCatalogResult } from './protocol'

export type LiveSessionState = Pick<SessionRuntime, 'phase' | 'failure'> & {
  backgroundTasks: SessionReadResult['backgroundTasks']
  revision: number
}
type LiveSessionUpdate = Pick<LiveSessionState, 'phase' | 'failure'> & Partial<SessionRuntime> & Partial<Pick<LiveSessionState, 'backgroundTasks'>>

export interface SyncState {
  revision: number
  bootstrap: AppBootstrap | null
  session: SessionView | null
  liveSessions: Record<string, LiveSessionState>
}

export const initialSyncState = (): SyncState => ({
  revision: 0, bootstrap: null, session: null, liveSessions: {},
})

function acceptLiveSession(state: SyncState, sessionId: string, incoming: LiveSessionUpdate, revision: number, event?: TurnEventEnvelope): SyncState {
  const previous = state.liveSessions[sessionId]
  if (previous && revision <= previous.revision) return state
  const selected = state.session?.summary.threadId === sessionId ? state.session : null
  const { backgroundTasks = previous?.backgroundTasks ?? [], ...update } = incoming
  const lifecycle: LiveSessionState = { revision, phase: update.phase, failure: update.failure, backgroundTasks }
  const liveSessions = { ...state.liveSessions, [sessionId]: lifecycle }
  if (selected === null) {
    return { ...state, liveSessions }
  }
  const runtime: SessionRuntime = { ...selected.runtime, ...update }
  const session = {
    ...selected,
    runtime,
    context: event?.method === 'context/loaded' ? event.params.context : selected.context,
  }
  return {
    ...state,
    liveSessions,
    session: event ? { ...session, facts: acceptExecutionEvent(session.facts, event) } : session,
  }
}

function acceptBootstrap(state: SyncState, bootstrap: AppBootstrap): SyncState {
  if (state.bootstrap !== null && bootstrap.revision < state.bootstrap.revision) return state
  const summary = Object.values(bootstrap.sessionsByWorkspace).flat().find(item => item.threadId === state.session?.summary.threadId)
  return { ...state, bootstrap, session: state.session && summary ? { ...state.session, summary } : state.session }
}

export function acceptModelCatalog(state: SyncState, result: ModelCatalogResult): SyncState {
  if (!state.bootstrap || result.revision < state.bootstrap.revision) return state
  return { ...state, bootstrap: { ...state.bootstrap, modelCatalog: result.catalog } }
}

function sortSessions(sessions: ThreadSummary[]): ThreadSummary[] {
  return sessions.sort((a, b) => b.updatedAt.localeCompare(a.updatedAt) || a.threadId.localeCompare(b.threadId))
}

function acceptSummary(state: SyncState, summary: ThreadSummary, revision: number): SyncState {
  const bootstrap = state.bootstrap
  if (!bootstrap || revision <= bootstrap.revision) return state
  const workspaceRoot = workspaceKey(summary.cwd)
  if (!(workspaceRoot in bootstrap.sessionsByWorkspace)) return state
  const sessions = bootstrap.sessionsByWorkspace[workspaceRoot] ?? []
  return {
    ...state,
    session: state.session?.summary.threadId === summary.threadId ? { ...state.session, summary } : state.session,
    bootstrap: {
      ...bootstrap,
      sessionsByWorkspace: {
        ...bootstrap.sessionsByWorkspace,
        [workspaceRoot]: sortSessions([summary, ...sessions.filter(item => item.threadId !== summary.threadId)]),
      },
    },
  }
}

export function acceptSessionStart(state: SyncState, started: SessionStartResult): SyncState {
  if (state.bootstrap && state.bootstrap.revision >= started.revision) return state
  const next = acceptLiveSession(state, started.summary.threadId, started.status, started.revision)
  return acceptSummary(next, started.summary, started.revision)
}

export function pruneStaleSelection(
  bootstrap: AppBootstrap,
  state: Pick<SyncState, 'liveSessions' | 'session'> & { selectedWorkspaceRoot: string | null; selectedSessionId: string | null },
) {
  const workspaces = new Set(bootstrap.workspaces.map(workspace => workspaceKey(workspace.root)))
  const sessions = new Set(Object.values(bootstrap.sessionsByWorkspace).flat().map(session => session.threadId))
  const liveEntries = Object.entries(state.liveSessions)
  const liveSessions = liveEntries.every(([id]) => sessions.has(id))
    ? state.liveSessions
    : Object.fromEntries(liveEntries.filter(([id]) => sessions.has(id)))
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

export function resetBaseline(bootstrap: AppBootstrap): SyncState {
  const liveSessions: SyncState['liveSessions'] = Object.fromEntries(
    Object.entries(bootstrap.sessionStates).map(([id, status]) => [
      id,
      { revision: 0, ...status, backgroundTasks: [] },
    ]),
  )
  return { revision: bootstrap.revision, bootstrap, session: null, liveSessions }
}

export function acceptSessionRead(state: SyncState, source: SessionReadResult): SyncState {
  const id = source.history.summary.threadId
  const previous = state.liveSessions[id]
  if (state.session !== null && source.revision < (previous?.revision ?? 0)) return state
  state = acceptSummary(state, source.history.summary, source.revision)
  const session = readExecution(source)
  return {
    ...state,
    session,
    liveSessions: {
      ...state.liveSessions,
      [id]: {
        phase: session.runtime.phase,
        failure: session.runtime.failure,
        backgroundTasks: source.backgroundTasks,
        revision: source.revision,
      },
    },
  }
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
      if (frame.payload.failure?.source === 'turn' && session?.summary.threadId === frame.sessionId && turn?.status === 'running') {
        accepted = {
          ...accepted,
          session: {
            ...session,
            facts: [...session.facts.slice(0, -1), finishTurn(turn, 'failed')],
          },
        }
      }
      return acceptSummary(accepted, frame.summary, frame.revision)
    }
  }
}

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
