import { compactionText } from './copy'
import { hasErrorItem } from './execution'
import type { HistoryItem, PendingQuestion, SessionRuntime, ThreadTurn } from './protocol'

export type ToolItem = Extract<HistoryItem, { type: 'tool' }>
export type ToolGroup = { type: 'tool_group'; name: string; rows: Array<{ key: string; item: ToolItem }> }

export type TimelineContent = HistoryItem | ToolGroup
  | { type: 'duration'; startedAt: string; firstTokenAt?: string; finishedAt?: string }
  | { type: 'terminal'; status: 'failed' | 'interrupted'; text: string; at?: string }
  | { type: 'typing' }
  | { type: 'question'; request: PendingQuestion }

/** 只补充排列和阅读位置；历史条目原样交给组件。 */
export interface TimelineRow {
  key: string
  stable: boolean
  item: TimelineContent
}

/** 只有事实或上一回合的保存失败状态变化时，才重新排列历史。 */
export function buildHistoryRows(turns: ThreadTurn[] | undefined, failureFromRuntime: boolean): TimelineRow[] {
  return turns?.flatMap(turn => turnRows(turn, failureFromRuntime && turn === turns.at(-1))) ?? []
}

export function buildTimeline(rows: TimelineRow[], turns: ThreadTurn[] | undefined, runtime: Pick<SessionRuntime, 'failure' | 'pendingQuestion' | 'pendingMessages'> | undefined): TimelineRow[] {
  if (!runtime) return rows
  const failure = runtime.failure
  const question = runtime.pendingQuestion
  let result = rows
  // 日志写入失败时错误可能只保存在运行状态中，已有相同错误条目时不重复显示。
  if (failure && !turns?.some(turn => hasErrorItem(turn, failure.error))) {
    result = [...result, { key: 'terminal:failed', stable: false, item: { type: 'terminal', status: 'failed',
      text: failure.source === 'compaction' ? `${compactionText.failed}：${failure.error.message}` : failure.error.message } }]
  }
  // 等待回答的提问替换对应的提问工具卡片，回答后恢复成普通工具卡片。
  const asking = question ? result.findIndex(row => row.item.type === 'tool' && row.item.id === question.itemId) : -1
  if (question && asking >= 0) {
    result = [...result]
    result[asking] = { key: `question:${question.itemId}`, stable: false, item: { type: 'question', request: question } }
  }
  if (runtime.pendingMessages.length === 0) return result
  // 用户消息写入事件可能先于运行快照到达，按 ID 去重。
  const savedMessages = new Set(rows.flatMap(row => row.item.type === 'message' && row.item.role === 'user' ? [row.item.id] : []))
  result = [...result]
  for (const message of runtime.pendingMessages) {
    if (savedMessages.has(message.itemId)) continue
    result.push({ key: `pending:${message.itemId}`, stable: false,
      item: { id: message.itemId, type: 'message', role: 'user', status: 'completed', text: message.text, images: message.images, files: message.files } })
  }
  return result
}

function turnRows(turn: ThreadTurn, failureFromRuntime: boolean): TimelineRow[] {
  const items = mergeAdjacentThinking(turn.items)
  const result: TimelineRow[] = items.map(item => ({
    key: item.type === 'compaction' || item.type === 'compaction_result'
      ? `compaction:${item.id}` : `content:${turn.turnId}:${item.id}`,
    stable: !('status' in item) || item.status !== 'running' || item.type === 'tool',
    item,
  }))
  // 准备新回合或独立压缩不会重新启动上一轮的计时；进行中的回合收到首个 token 后才显示时间行。
  if (turn.finishedAt || (turn.status === 'running' && turn.firstTokenAt)) {
    const workStartIndex = items.findIndex(item => item.type !== 'message' || item.role !== 'user')
    result.splice(workStartIndex < 0 ? result.length : workStartIndex, 0, {
      key: `content:${turn.turnId}:duration`, stable: true,
      item: { type: 'duration', startedAt: turn.startedAt, firstTokenAt: turn.firstTokenAt, finishedAt: turn.finishedAt },
    })
  }
  // 回合进行中但没有正在输出或执行的条目，说明在等模型回复：显示“正在输入”气泡。
  const working = turn.items.some(item => 'status' in item && (item.status === 'running' || item.status === 'pending'))
  if (turn.status === 'running' && !working) result.push({ key: `content:${turn.turnId}:typing`, stable: false, item: { type: 'typing' } })
  if (turn.status === 'interrupted') result.push({
    key: `content:${turn.turnId}:terminal`, stable: true,
    item: { type: 'terminal', status: 'interrupted', text: turn.manuallyStopped ? '已停止' : '已中断', at: turn.finishedAt },
  })
  // 保存失败时没有回合错误详情，具体原因由运行状态显示。
  if (turn.status === 'failed' && !hasErrorItem(turn, turn.error) && (turn.error || !failureFromRuntime)) {
    result.push({ key: `content:${turn.turnId}:terminal`, stable: true,
      item: { type: 'terminal', status: 'failed', text: turn.error?.message ?? '执行失败', at: turn.finishedAt } })
  }
  return groupAdjacentTools(result)
}

/** 提问保持独立，避免把等待用户回答的入口藏在折叠组里。 */
function groupAdjacentTools(rows: TimelineRow[]): TimelineRow[] {
  const grouped: TimelineRow[] = []
  for (const row of rows) {
    const tool = row.item
    const previous = grouped.at(-1)
    if (tool.type !== 'tool' || tool.name === 'ask_user_question') {
      grouped.push(row)
    } else if (previous?.item.type === 'tool' && previous.item.name === tool.name) {
      grouped[grouped.length - 1] = { ...previous, item: { type: 'tool_group', name: tool.name,
        rows: [{ key: previous.key, item: previous.item }, { key: row.key, item: tool }] } }
    } else if (previous?.item.type === 'tool_group' && previous.item.name === tool.name) {
      previous.item.rows.push({ key: row.key, item: tool })
    } else grouped.push(row)
  }
  return grouped
}

/** 仅合并相邻思考的展示，保留首项 ID；工具、正文和回合边界不会被跨越。 */
function mergeAdjacentThinking(items: HistoryItem[]): HistoryItem[] {
  const merged: HistoryItem[] = []
  for (const item of items) {
    const previous = merged.at(-1)
    if (item.type === 'thinking' && previous?.type === 'thinking') {
      merged[merged.length - 1] = { ...previous, status: item.status,
        text: [previous.text, item.text].filter(Boolean).join('\n\n') }
    } else merged.push(item)
  }
  return merged
}

export function toolArgument(name: string, args: unknown): string | null {
  const input = record(args)
  const value = name === 'bash' ? input.command : name === 'read' || name === 'edit' || name === 'view_image' ? input.path : name === 'skill' ? input.name : undefined
  return typeof value === 'string' ? value : null
}

function backgroundToolSummary(name: string): string | null {
  if (name === 'task_list') return '列出后台命令'
  if (name !== 'task_output' && name !== 'task_stop') return null
  return name === 'task_output' ? '查看后台命令输出' : '停止后台命令'
}

export function toolSummary(name: string, args: unknown): string {
  const background = backgroundToolSummary(name)
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
  if (name !== 'read' && name !== 'edit' && name !== 'view_image') return null
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
  return seconds < 60 ? `${seconds}s` : seconds < 3600 ? `${Math.floor(seconds / 60)}m${seconds % 60}s` : `${Math.floor(seconds / 3600)}h${Math.floor(seconds % 3600 / 60)}m`
}

export function compactionResultText(item: Extract<HistoryItem, { type: 'compaction_result' }>): string {
  return item.message ?? (item.status === 'running' ? compactionText.running : item.status === 'completed' ? compactionText.unchanged : item.status === 'interrupted' ? compactionText.unfinished : compactionText.failed)
}

export function formatClock(value: string, includeDate = false): string {
  const date = new Date(value)
  const clock = date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit', hour12: false })
  return includeDate && date.toDateString() !== new Date().toDateString() ? `${date.getMonth() + 1}/${date.getDate()} ${clock}` : clock
}

export function backgroundTaskId(item: Extract<HistoryItem, { type: 'tool' }>): string | null {
  return item.name === 'bash' ? item.result?.backgroundTaskId ?? null : null
}
