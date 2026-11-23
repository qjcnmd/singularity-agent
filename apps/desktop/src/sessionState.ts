import type { ThreadSummary } from './protocol'
import type { LiveSessionState, SyncState } from './sync'
import { interruptionText, phaseText, turnStatusText } from './copy'

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
