import { compactionText, formatTokenCount } from './copy'
import { hasErrorItem } from './execution'
import type { HistoryItem, PendingQuestion, SessionModelUsage, SessionRuntime, ThreadTurn, ToolStatus, VisibleBackgroundTask } from './protocol'

type ToolItem = Extract<HistoryItem, { type: 'tool' }>
export type ToolGroup = { type: 'tool_group'; name: string; rows: Array<{ key: string; item: ToolItem }> }
type TurnDurationInfo = { startedAt: string; finishedAt?: string }
type TerminalItem = { type: 'terminal'; status: 'failed' | 'interrupted'; text: string; at?: string }
type TypingItem = { type: 'typing' }
type TurnStatsItem = { type: 'stats'; at: string }
export type TurnFileEdit = { path: string; added: number; deleted: number; created: boolean }
export type TurnChangesItem = { type: 'turn_changes'; edits: TurnFileEdit[] }
export type ActivityRowItem =
  | Extract<HistoryItem, { type: 'thinking' | 'tool' | 'compaction' | 'compaction_result' }>
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
  | TurnChangesItem
  | TurnStatsItem
  | { type: 'question'; request: PendingQuestion; duration?: TurnDurationInfo }

type RawTimelineRow = {
  key: string
  stable: boolean
  item: TimelineContent
}

export interface TimelineRow {
  key: string
  stable: boolean
  item: Exclude<TimelineContent, ActivityRowItem>
}

const turnRowCache = new WeakMap<ThreadTurn, RawTimelineRow[]>()
const groupedRowCache = new WeakMap<RawTimelineRow, { sources: RawTimelineRow[]; row: TimelineRow }>()

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
  if (failure && !turns?.some(turn => hasErrorItem(turn, failure.error))) {
    result = [...result, { key: 'terminal:failed', stable: false, item: { type: 'terminal', status: 'failed',
      text: failure.source === 'compaction' ? `${compactionText.failed}：${failure.error.message}` : failure.error.message } }]
  }
  const asking = question ? result.findIndex(row => row.item.type === 'tool' && row.item.id === question.itemId) : -1
  if (question && asking >= 0) {
    if (result === rows) result = [...result]
    result[asking] = { key: `question:${question.itemId}`, stable: false, item: { type: 'question', request: question } }
  }
  if (runtime.pendingMessages.length === 0) return groupActivityRuns(result)
  const savedMessages = new Set(rows.flatMap(row => row.item.type === 'message' && row.item.role === 'user' ? [row.item.id] : []))
  if (result === rows) result = [...result]
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
    || item.type === 'compaction'
    || item.type === 'compaction_result'
    || item.type === 'typing'
    || item.type === 'terminal'
}

function groupActivityRuns(rows: RawTimelineRow[]): TimelineRow[] {
  const grouped: TimelineRow[] = []
  for (let start = 0; start < rows.length;) {
    const first = rows[start]
    const durationItem = first.item.type === 'duration' ? first.item : undefined
    const headIndex = durationItem ? start + 1 : start
    const head = rows[headIndex]
    const activity = head && isActivityItem(head.item)
    const question = durationItem && head?.item.type === 'question' ? head.item : undefined
    if (!activity && !question) {
      grouped.push(first as TimelineRow)
      start++
      continue
    }

    let end = headIndex + 1
    if (activity) {
      while (end < rows.length && isActivityItem(rows[end].item)) end++
    }
    const cached = groupedRowCache.get(first)
    if (cached?.sources.length === end - start && cached.sources.every((row, index) => row === rows[start + index])) {
      grouped.push(cached.row)
      start = end
      continue
    }

    const duration = durationItem && {
      startedAt: durationItem.startedAt, finishedAt: durationItem.finishedAt,
    }
    const sources = rows.slice(start, end)
    const row: TimelineRow = question
      ? { ...head, item: { ...question, duration } }
      : {
        key: `activity:${head.key}`,
        stable: sources.every((row, index) => (duration && index === 0) || row.stable),
        item: {
          type: 'activity_group',
          rows: (duration ? sources.slice(1) : sources).map(({ key, item }) => ({ key, item: item as ActivityRowItem })),
          ...(duration && { duration }),
        },
      }
    groupedRowCache.set(first, { sources, row })
    grouped.push(row)
    start = end
  }
  return grouped
}

function hasModelStats(stats: SessionModelUsage | null | undefined): stats is SessionModelUsage {
  return stats != null && (stats.ttftRequests > 0 || stats.decodeMs > 0 || stats.usagePresent)
}

export function formatSessionMetrics(stats: SessionModelUsage | null | undefined): string {
  if (!hasModelStats(stats)) return ''
  const parts: string[] = []
  if (stats.ttftRequests > 0) parts.push(`TTFT ${(stats.ttftMs / stats.ttftRequests / 1000).toFixed(1)}s`)
  if (stats.decodeMs > 0) parts.push(`${Math.round(stats.decodeTokens * 1000 / stats.decodeMs)} tok/s`)
  if (stats.usagePresent) parts.push(`${formatTokenCount(stats.totalTokens)} tokens`)
  if (stats.usagePresent && stats.cacheUsageComplete && stats.inputTokens > 0) parts.push(`${Math.floor(stats.cachedInputTokens / stats.inputTokens * 100)}%`)
  return parts.join(' · ')
}

function extractTurnEdits(items: HistoryItem[]): TurnFileEdit[] {
  const edits: TurnFileEdit[] = []
  for (const item of items) {
    if (item.type !== 'tool' || (item.name !== 'edit' && item.name !== 'write')) continue
    if (item.status !== 'completed' || item.result?.isError) continue
    const args = item.args
    const rawPath = typeof args === 'object' && args !== null && !Array.isArray(args) && typeof args.path === 'string'
      ? args.path.trim()
      : ''
    if (!rawPath) continue
    const output = item.result?.output ?? ''
    const firstNewline = output.indexOf('\n')
    const summaryLine = firstNewline >= 0 ? output.slice(0, firstNewline) : output
    const counts = /^\+(\d+) -(\d+)$/.exec(summaryLine)
    const added = counts ? Number(counts[1]) : 0
    const deleted = counts ? Number(counts[2]) : 0
    const created = item.result?.fileCreated === true
    if (!created && added === 0 && deleted === 0) continue
    edits.push({ path: rawPath, added, deleted, created })
  }
  return edits
}

function turnRows(turn: ThreadTurn, failureFromRuntime: boolean): RawTimelineRow[] {
  const items = mergeAdjacentThinking(turn.items).filter(item =>
    !(item.type === 'message'
      && item.role === 'assistant'
      && item.text.trim() === ''
      && !item.images?.length
      && !item.files?.length
      && item.status !== 'failed'),
  )
  const turnEdits = turn.status !== 'running' ? extractTurnEdits(turn.items) : []
  const hasTurnChanges = turnEdits.length > 0
  const lastStepIndex = items.findLastIndex(item => item.type !== 'compaction' && item.type !== 'compaction_result')
  const lastStep = lastStepIndex >= 0 ? items[lastStepIndex] : undefined
  const lastAssistant = lastStep?.type === 'message' && lastStep.role === 'assistant' ? lastStep : undefined
  const turnStats = hasModelStats(lastAssistant?.stats)
    ? lastAssistant.stats
    : hasModelStats(turn.stats)
      ? turn.stats
      : undefined
  const keepAssistantEndTimeInline = !hasTurnChanges && Boolean(lastAssistant?.at)
  const trailingAt = lastAssistant?.at ?? turn.finishedAt
  const hasTrailingEndTime = turn.status !== 'running'
    && !keepAssistantEndTimeInline
    && trailingAt !== undefined
    && (turnStats !== undefined || hasTurnChanges || lastAssistant !== undefined)
  const hasBottomEndTime = keepAssistantEndTimeInline || hasTrailingEndTime
  const terminalAt = hasBottomEndTime ? undefined : turn.finishedAt
  const result: RawTimelineRow[] = items.map((item, index) => {
    const stripEndTime = index < lastStepIndex || hasTurnChanges
    return {
      key: item.type === 'compaction' || item.type === 'compaction_result'
        ? `compaction:${item.id}` : `content:${turn.turnId}:${item.id}`,
      stable: !('status' in item) || item.status !== 'running' || item.type === 'tool',
      item: item.type === 'message'
        && item.role === 'assistant'
        && (item.stats !== undefined || (stripEndTime && item.at !== undefined))
        ? { ...item, stats: undefined, at: stripEndTime ? undefined : item.at } : item,
    }
  })
  if (turn.finishedAt || turn.status === 'running') {
    const workStartIndex = items.findIndex(item => item.type !== 'message' || item.role !== 'user')
    result.splice(workStartIndex < 0 ? result.length : workStartIndex, 0, {
      key: `content:${turn.turnId}:duration`, stable: true,
      item: { type: 'duration', startedAt: turn.startedAt, finishedAt: turn.finishedAt },
    })
  }
  const working = turn.items.some(item => 'status' in item && (item.status === 'running' || item.status === 'pending'))
  if (turn.status === 'running' && !working) result.push({ key: `content:${turn.turnId}:typing`, stable: false, item: { type: 'typing' } })
  if (turn.status === 'interrupted') result.push({
    key: `content:${turn.turnId}:terminal`, stable: true,
    item: { type: 'terminal', status: 'interrupted', text: turn.manuallyStopped ? '已停止' : '已中断', at: terminalAt },
  })
  if (turn.status === 'failed' && !hasErrorItem(turn, turn.error) && (turn.error || !failureFromRuntime)) {
    result.push({ key: `content:${turn.turnId}:terminal`, stable: true,
      item: { type: 'terminal', status: 'failed', text: turn.error?.message ?? '执行失败', at: terminalAt } })
  }
  if (hasTurnChanges) {
    result.push({
      key: `content:${turn.turnId}:changes`, stable: true,
      item: { type: 'turn_changes', edits: turnEdits },
    })
  }
  if (hasTrailingEndTime && trailingAt !== undefined) {
    result.push({
      key: `content:${turn.turnId}:stats`, stable: true,
      item: { type: 'stats', at: trailingAt },
    })
  }
  return groupAdjacentTools(result)
}

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

export function compactionResultText(item: Extract<HistoryItem, { type: 'compaction_result' }>): string {
  return item.message ?? (item.status === 'running' ? compactionText.running : item.status === 'completed' ? compactionText.unchanged : item.status === 'interrupted' ? compactionText.unfinished : compactionText.failed)
}

export function formatClock(value: string, includeDate = false): string {
  const date = new Date(value)
  const clock = date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit', hour12: false })
  return includeDate && date.toDateString() !== new Date().toDateString() ? `${date.getMonth() + 1}/${date.getDate()} ${clock}` : clock
}

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
