import { interruptionText, compactionText } from './copy'
import { hasErrorItem, type SessionView } from './execution'
import type { HistoryItem, ThreadTurn } from './protocol'

export type TimelineContent = HistoryItem
  | { type: 'duration'; startedAt: string; finishedAt?: string }
  | { type: 'terminal'; status: 'failed' | 'interrupted'; text: string }

/** 只补充排列和阅读位置；历史条目原样交给组件。 */
export interface TimelineRow {
  key: string
  stable: boolean
  item: TimelineContent
}

export function buildTimeline(session: SessionView | null): TimelineRow[] {
  if (!session) return []
  const result: TimelineRow[] = []
  const turns = session.facts
  const failure = session.runtime.failure
  const activeTurn = session.runtime.phase === 'idle' ? undefined : turns.at(-1)
  for (const turn of turns) {
    const failureFromRuntime = turn === turns.at(-1) && failure?.source === 'turn'
    result.push(...turnRows(turn, turn === activeTurn, failureFromRuntime))
  }
  // 日志写入失败时错误可能只保存在运行状态中，已有相同错误条目时不重复显示。
  if (failure && !turns.some(turn => hasErrorItem(turn, failure.error))) {
    result.push({ key: 'terminal:failed', stable: false, item: { type: 'terminal', status: 'failed',
      text: failure.source === 'compaction' ? `${compactionText.failed}：${failure.error.message}` : failure.error.message } })
  }
  if (session.runtime.pendingMessages.length === 0) return result
  // 用户消息写入事件可能先于运行快照到达，按 ID 去重。
  const savedMessages = new Set(turns.flatMap(turn => turn.items.filter(item => item.type === 'message' && item.role === 'user').map(item => item.id)))
  for (const message of session.runtime.pendingMessages) {
    if (savedMessages.has(message.itemId)) continue
    result.push({ key: `pending:${message.itemId}`, stable: false,
      item: { id: message.itemId, type: 'message', role: 'user', status: 'completed', text: message.text, images: message.images } })
  }
  return result
}

function turnRows(turn: ThreadTurn, active: boolean, failureFromRuntime: boolean): TimelineRow[] {
  const result: TimelineRow[] = turn.items.map(item => ({
    key: item.type === 'compaction' || item.type === 'compaction_result'
      ? `compaction:${item.id}` : `content:${turn.turnId}:${item.id}`,
    stable: !('status' in item) || item.status !== 'running' || item.type === 'tool',
    item,
  }))
  if (turn.finishedAt || active) {
    const workStartIndex = turn.items.findIndex(item => item.type !== 'message' || item.role !== 'user')
    result.splice(workStartIndex < 0 ? result.length : workStartIndex, 0, {
      key: `content:${turn.turnId}:duration`, stable: true,
      item: { type: 'duration', startedAt: turn.startedAt, finishedAt: turn.finishedAt },
    })
  }
  if (turn.status === 'interrupted') result.push({
    key: `content:${turn.turnId}:terminal`, stable: true,
    item: { type: 'terminal', status: turn.manuallyStopped ? 'interrupted' : 'failed', text: interruptionText(turn.manuallyStopped) },
  })
  // 保存失败时没有回合错误详情，具体原因由运行状态显示。
  if (turn.status === 'failed' && !hasErrorItem(turn, turn.error) && (turn.error || !failureFromRuntime)) {
    result.push({ key: `content:${turn.turnId}:terminal`, stable: true,
      item: { type: 'terminal', status: 'failed', text: turn.error?.message ?? '执行失败' } })
  }
  return result
}

export function toolArgument(name: string, args: unknown): string | null {
  const input = record(args)
  const value = name === 'bash' ? input.command : name === 'read' ? input.path : undefined
  return typeof value === 'string' ? value : null
}

export function backgroundToolSummary(name: string, args: unknown): string | null {
  if (name === 'task_list') return '列出后台命令'
  if (name !== 'task_output' && name !== 'task_stop') return null
  const taskId = record(args).task_id
  const action = name === 'task_output' ? '查看后台命令输出' : '停止后台命令'
  return typeof taskId === 'string' ? `${action} · ${taskId}` : action
}

export function toolSummary(name: string, args: unknown): string {
  const background = backgroundToolSummary(name, args)
  if (background !== null) return background
  if (name === 'ask_user_question') {
    const questions = record(args).questions
    const question = Array.isArray(questions) ? record(questions[0]).question : undefined
    return typeof question === 'string' ? firstLine(question) : ''
  }
  if (name === 'bash') {
    const description = record(args).description
    if (typeof description === 'string' && description.trim()) return firstLine(description.trim())
  }
  return firstLine(toolArgument(name, args) ?? '')
}

export function displayPath(path: string, cwd: string, userHome?: string | null): string {
  const normalized = path.replace(/\\/g, '/')
  const root = cwd.replace(/\\/g, '/').replace(/\/$/, '')
  if (normalized.toLowerCase().startsWith(`${root.toLowerCase()}/`)) return normalized.slice(root.length + 1)
  const home = userHome?.replace(/\\/g, '/').replace(/\/$/, '')
  if (home && normalized.toLowerCase() === home.toLowerCase()) return '~'
  if (home && normalized.toLowerCase().startsWith(`${home.toLowerCase()}/`)) return `~${normalized.slice(home.length)}`
  return path
}

export function pathFromArgs(name: string, args: unknown): string | null {
  if (name !== 'read') return null
  const value = toolArgument(name, args)
  return value?.trim() ? value : null
}

export function firstLine(text: string): string {
  return text.split(/\r?\n/, 1)[0]?.trim() ?? ''
}

function record(value: unknown): Record<string, unknown> {
  return typeof value === 'object' && value !== null ? value as Record<string, unknown> : {}
}

export function formatTurnDuration(milliseconds: number): string {
  const seconds = Math.max(0, Math.floor(milliseconds / 1000))
  return seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m${seconds % 60}s`
}

export function compactionResultText(item: Extract<HistoryItem, { type: 'compaction_result' }>): string {
  return item.message ?? (item.status === 'running' ? compactionText.running : item.status === 'completed' ? compactionText.unchanged : item.status === 'interrupted' ? compactionText.unfinished : compactionText.failed)
}
