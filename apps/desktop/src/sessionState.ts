import type { ThreadSummary } from './protocol'
import type { LiveSessionState, SyncState } from './sync'
import { interruptionText, phaseText, turnStatusText } from './copy'

/** 正文暂未加载或读取失败时，仍使用当前任务已收到的运行状态。 */
export function selectedSessionPhase(state: Pick<SyncState, 'session' | 'liveSessions'> & { selectedSessionId: string | null }) {
  return state.session?.runtime.phase
    ?? (state.selectedSessionId === null ? undefined : state.liveSessions[state.selectedSessionId]?.phase)
    ?? 'idle'
}

/**
 * 侧栏显示的任务状态：运行中时显示当前阶段，空闲时只看最近一个**回合**的结果。
 *
 * 独立压缩的结果只显示在对话区的那一行，不改变任务状态；压缩失败或中断不会把
 * 已完成的任务标成失败。
 */
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
