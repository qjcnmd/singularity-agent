import { contextRotationText } from './copy'
import { hasErrorItem } from './execution'
import type { HistoryItem, PendingQuestion, SessionModelUsage, SessionRuntime, ThreadTurn, ToolStatus, VisibleBackgroundTask } from './protocol'

type ToolItem = Extract<HistoryItem, { type: 'tool' }>
export type ToolGroup = { type: 'tool_group'; name: string; rows: Array<{ key: string; item: ToolItem }> }
type TurnDurationInfo = { startedAt: string; firstTokenAt?: string; finishedAt?: string }
type TerminalItem = { type: 'terminal'; status: 'failed' | 'interrupted'; text: string; at?: string }
type TypingItem = { type: 'typing' }
type TurnStatsItem = { type: 'stats'; stats: SessionModelUsage; at?: string }
export type ActivityRowItem =
  | Extract<HistoryItem, { type: 'thinking' | 'tool' | 'context_rotation' | 'context_rotation_result' }>
  | ToolGroup
  | TerminalItem
  | TypingItem
export type ActivityGroup = {
  type: 'activity_group'
  rows: Array<{ key: string; item: ActivityRowItem }>
  duration?: TurnDurationInfo
}

type TimelineContent = HistoryItem | ToolGroup | ActivityGroup
  | ({ type: 'duration' } & TurnDurationInfo)
  | TerminalItem
  | TypingItem
  | TurnStatsItem
  | { type: 'question'; request: PendingQuestion; duration?: TurnDurationInfo }

type RawTimelineRow = {
  key: string
  stable: boolean
  item: TimelineContent
}

/** 只补充排列和阅读位置；活动条目均由 `groupActivityRuns` 收进 `activity_group`。 */
export interface TimelineRow {
  key: string
  stable: boolean
  item: Exclude<TimelineContent, ActivityRowItem>
}

const turnRowCache = new WeakMap<ThreadTurn, RawTimelineRow[]>()

/** 只有事实或上一回合的保存失败状态变化时，才重新排列历史。 */
export function buildHistoryRows(turns: ThreadTurn[] | undefined, failureFromRuntime: boolean): RawTimelineRow[] {
  return turns?.flatMap(turn => {
    if (failureFromRuntime && turn === turns.at(-1)) return turnRows(turn, true)
    let cached = turnRowCache.get(turn)
    if (!cached) {
      cached = turnRows(turn, false)
      turnRowCache.set(turn, cached)
    }
    return cached
  }) ?? []
}

export function buildTimeline(rows: RawTimelineRow[], turns: ThreadTurn[] | undefined, runtime: Pick<SessionRuntime, 'failure' | 'pendingQuestion' | 'pendingMessages'> | undefined): TimelineRow[] {
  if (!runtime) return groupActivityRuns(rows)
  const failure = runtime.failure
  const question = runtime.pendingQuestion
  let result = rows
  // 日志写入失败时错误可能只保存在运行状态中，已有相同错误条目时不重复显示。
  if (failure && !turns?.some(turn => hasErrorItem(turn, failure.error))) {
    result = [...result, { key: 'terminal:failed', stable: false, item: { type: 'terminal', status: 'failed',
      text: failure.source === 'context_rotation' ? `${contextRotationText.failed}：${failure.error.message}` : failure.error.message } }]
  }
  // 等待回答的提问替换对应的提问工具卡片，回答后恢复成普通工具卡片。
  const asking = question ? result.findIndex(row => row.item.type === 'tool' && row.item.id === question.itemId) : -1
  if (question && asking >= 0) {
    result = [...result]
    result[asking] = { key: `question:${question.itemId}`, stable: false, item: { type: 'question', request: question } }
  }
  if (runtime.pendingMessages.length === 0) return groupActivityRuns(result)
  // 用户消息写入事件可能先于运行快照到达，按 ID 去重。
  const savedMessages = new Set(rows.flatMap(row => row.item.type === 'message' && row.item.role === 'user' ? [row.item.id] : []))
  result = [...result]
  for (const message of runtime.pendingMessages) {
    if (savedMessages.has(message.itemId)) continue
    result.push({ key: `pending:${message.itemId}`, stable: false,
      item: {
        id: message.itemId,
        type: 'message',
        role: 'user',
        status: 'completed',
        text: message.text,
        images: message.images,
        files: message.files,
        annotations: message.annotations,
      } })
  }
  return groupActivityRuns(result)
}

function isActivityItem(item: TimelineContent): item is ActivityRowItem {
  return item.type === 'thinking'
    || item.type === 'tool'
    || item.type === 'tool_group'
    || item.type === 'context_rotation'
    || item.type === 'context_rotation_result'
    || item.type === 'typing'
    || item.type === 'terminal'
}

/** 将连续的非文本步骤与状态（思考、工具、轮换、等待、中断、报错）合入白卡；前置耗时并入卡顶栏，提问独立成同款白卡。 */
function groupActivityRuns(rows: RawTimelineRow[]): TimelineRow[] {
  const grouped: TimelineRow[] = []
  for (const row of rows) {
    if (row.item.type === 'question') {
      const previous = grouped.at(-1)
      if (previous?.item.type === 'duration') {
        const { startedAt, firstTokenAt, finishedAt } = previous.item
        grouped[grouped.length - 1] = {
          ...row,
          item: { ...row.item, duration: { startedAt, firstTokenAt, finishedAt } },
        }
      } else {
        grouped.push({ key: row.key, stable: row.stable, item: row.item })
      }
      continue
    }
    if (!isActivityItem(row.item)) {
      grouped.push({ key: row.key, stable: row.stable, item: row.item })
      continue
    }
    const previous = grouped.at(-1)
    if (previous?.item.type === 'activity_group') {
      previous.item.rows.push({ key: row.key, item: row.item })
      previous.stable = previous.stable && row.stable
    } else if (previous?.item.type === 'duration') {
      const { startedAt, firstTokenAt, finishedAt } = previous.item
      grouped[grouped.length - 1] = {
        key: `activity:${row.key}`,
        stable: row.stable,
        item: {
          type: 'activity_group',
          rows: [{ key: row.key, item: row.item }],
          duration: { startedAt, firstTokenAt, finishedAt },
        },
      }
    } else {
      grouped.push({
        key: `activity:${row.key}`,
        stable: row.stable,
        item: { type: 'activity_group', rows: [{ key: row.key, item: row.item }] },
      })
    }
  }
  return grouped
}

function hasModelStats(stats: SessionModelUsage | undefined): stats is SessionModelUsage {
  return stats !== undefined && (stats.ttftRequests > 0 || stats.decodeMs > 0 || stats.usagePresent)
}

function turnRows(turn: ThreadTurn, failureFromRuntime: boolean): RawTimelineRow[] {
  const items = mergeAdjacentThinking(turn.items)
  const lastStepIndex = items.findLastIndex(item => item.type !== 'context_rotation' && item.type !== 'context_rotation_result')
  const lastStep = lastStepIndex >= 0 ? items[lastStepIndex] : undefined
  const hasFinalAssistantStats = lastStep?.type === 'message' && lastStep.role === 'assistant' && hasModelStats(lastStep.stats)
  const result: RawTimelineRow[] = items.map((item, index) => ({
    key: item.type === 'context_rotation' || item.type === 'context_rotation_result'
      ? `contextRotation:${item.id}` : `content:${turn.turnId}:${item.id}`,
    stable: !('status' in item) || item.status !== 'running' || item.type === 'tool',
    // 同一回合中间的过渡回复（后面还跟着工具或后续步骤）不重复显示累计统计行。
    item: item.type === 'message' && item.role === 'assistant' && index < lastStepIndex && (item.stats !== undefined || item.at !== undefined)
      ? { ...item, stats: undefined, at: undefined } : item,
  }))
  // 准备新回合或独立轮换不会重新启动上一轮的计时；进行中的回合收到首个 token 后才显示时间行。
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
  // 没有最终助手正文承接统计时，在活动卡片下方单独显示一行用量统计。
  if (turn.status !== 'running' && !hasFinalAssistantStats && hasModelStats(turn.stats)) {
    result.push({
      key: `content:${turn.turnId}:stats`, stable: true,
      item: { type: 'stats', stats: turn.stats, at: turn.finishedAt },
    })
  }
  return groupAdjacentTools(result)
}

/** 提问保持独立，避免把等待用户回答的入口藏在折叠组里。 */
function groupAdjacentTools(rows: RawTimelineRow[]): RawTimelineRow[] {
  const grouped: RawTimelineRow[] = []
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

export function formatTurnDuration(milliseconds: number): string {
  const seconds = Math.max(0, Math.floor(milliseconds / 1000))
  return seconds < 60 ? `${seconds}s` : seconds < 3600 ? `${Math.floor(seconds / 60)}m${seconds % 60}s` : `${Math.floor(seconds / 3600)}h${Math.floor(seconds % 3600 / 60)}m`
}

export function contextRotationResultText(item: Extract<HistoryItem, { type: 'context_rotation_result' }>): string {
  return item.message ?? (item.status === 'running' ? contextRotationText.running : item.status === 'completed' ? contextRotationText.unchanged : item.status === 'interrupted' ? contextRotationText.unfinished : contextRotationText.failed)
}

export function formatClock(value: string, includeDate = false): string {
  const date = new Date(value)
  const clock = date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit', hour12: false })
  return includeDate && date.toDateString() !== new Date().toDateString() ? `${date.getMonth() + 1}/${date.getDate()} ${clock}` : clock
}

/** 单条详情与分组计数共用后台任务终态，避免命令转后台后仍显示成功。 */
export function toolDisplay(item: ToolItem, tasks: VisibleBackgroundTask[] = []) {
  const id = item.name === 'bash' ? item.result?.backgroundTaskId ?? null : null
  const task = id ? tasks.find(task => task.taskId === id) : undefined
  const status: ToolStatus = task ? task.state.status === 'running' ? 'running'
    : task.state.status === 'stopped' || task.state.status === 'interrupted' ? 'interrupted'
    : task.state.status === 'failed' || (task.state.status === 'exited' && task.state.exitCode !== 0) ? 'failed' : 'completed' : item.status
  const shown = task ? { ...item, result: {
    output: task.state.status === 'failed' ? [task.output, task.state.message].filter(Boolean).join('\n') : task.output,
    isError: status === 'failed',
    durationMs: 'durationMs' in task.state ? task.state.durationMs : undefined,
  } } : item
  return { status, shown }
}
