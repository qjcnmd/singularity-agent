import type { ContextUsage, ThreadSummary, VisibleBackgroundTask } from './protocol'
import type { LiveSessionState, SyncState } from './sync'
import { interruptionText, phaseText, turnStatusText } from './copy'

const emptyTasks: VisibleBackgroundTask[] = []

export function sessionBackgroundTasks(
  liveSessions: SyncState['liveSessions'],
  sessionId: string | null | undefined,
): VisibleBackgroundTask[] {
  return sessionId ? liveSessions[sessionId]?.backgroundTasks ?? emptyTasks : emptyTasks
}

export function hasRunningBackgroundTask(
  liveSessions: SyncState['liveSessions'],
  sessionId: string | null | undefined,
): boolean {
  return sessionBackgroundTasks(liveSessions, sessionId).some(task => task.state.status === 'running')
}

export function contextUsageStats(usage: ContextUsage | null | undefined): {
  percent: number
  tone: '' | 'warning' | 'danger'
} {
  const percent = usage ? Math.min(100, Math.round(usage.used / usage.capacity * 100)) : 0
  const tone = percent >= 90 ? 'danger' : percent >= 75 ? 'warning' : ''
  return { percent, tone }
}

export function selectedSessionPhase(state: Pick<SyncState, 'session' | 'liveSessions'> & { selectedSessionId: string | null }) {
  return state.session?.runtime.phase
    ?? (state.selectedSessionId === null ? undefined : state.liveSessions[state.selectedSessionId]?.phase)
    ?? 'idle'
}

export function sessionState(session: ThreadSummary, live: LiveSessionState | undefined) {
  if (live !== undefined && live.phase !== 'idle') {
    return { className: live.phase, label: phaseText[live.phase] }
  }
  if (live?.failure?.source === 'turn') return { className: 'failed', label: '执行失败' }
  const { status, manuallyStopped } = session
  return status === null
    ? { className: 'idle', label: '就绪' }
    : {
        className: status === 'interrupted' && manuallyStopped ? 'stopped' : status,
        label: status === 'interrupted' ? interruptionText(manuallyStopped) : turnStatusText[status],
      }
}
