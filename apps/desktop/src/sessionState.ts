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
 * 侧栏任务状态的派生规则：活动相位优先，空闲时只由**回合**终态决定。
 *
 * 独立压缩的终态属于那次压缩。它在对话区自成一行，不改任务状态，压缩失败或
 * 中断不会把已完成的任务标成失败。
 */
export function sessionState(session: ThreadSummary, live: LiveSessionState | undefined) {
  if (live !== undefined && live.phase !== 'idle') {
    return { className: live.phase, label: phaseText[live.phase] }
  }
  const terminal = live?.terminal?.source === 'turn' ? live.terminal : session
  const { status, manuallyStopped } = terminal
  return status === null
    ? { className: 'idle', label: '就绪' }
    : {
        className: status === 'interrupted' && manuallyStopped ? 'stopped' : status,
        label: status === 'interrupted' ? interruptionText(manuallyStopped) : turnStatusText[status],
      }
}
