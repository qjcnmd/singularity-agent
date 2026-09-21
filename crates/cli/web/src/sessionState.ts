import type { ThreadSummary } from './protocol'
import type { LiveSessionState } from './sync'
import { phaseText, turnStatusText } from './copy'

/** 尚未命名且没有回合记录的任务；活动相位和草稿由各操作另行判断。 */
export function isBlankSession(session: ThreadSummary): boolean {
  return session.turnCount === 0 && session.status === null && !session.title?.trim()
}

/**
 * 侧栏任务状态的唯一派生：活动相位优先；空闲时状态只由**回合**终态决定。
 *
 * 独立压缩的终态属于那次压缩：它在对话区自成一行，不改变任务状态，因此压缩
 * 失败或中断不会把已经完成的任务标成失败。
 */
export function sessionState(session: ThreadSummary, live: LiveSessionState | undefined) {
  if (live !== undefined && live.phase !== 'idle') {
    return { className: live.phase, label: phaseText[live.phase] }
  }
  const status = live?.terminal?.source === 'turn' ? live.terminal.status : session.status
  return status === null || status === undefined
    ? { className: 'idle', label: '就绪' }
    : {
        className: status === 'interrupted' && session.manuallyStopped ? 'stopped' : status,
        label: status === 'interrupted' && !session.manuallyStopped ? '任务异常中断' : turnStatusText[status],
      }
}
