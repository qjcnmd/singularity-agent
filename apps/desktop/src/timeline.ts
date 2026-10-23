import { interruptionText, compactionText } from './copy'
import { hasErrorItem, type SessionView } from './execution'
import type { HistoryItem, ThreadTurn, ToolStatus, TurnStatus } from './protocol'

interface TimelineItemBase {
  key: string
  /** 是否对应已保存的历史；只有已保存的条目能作为阅读位置，临时输出不能。 */
  stable?: boolean
  title: string
  fact: HistoryItem | null
  summary: string
  filePath: string | null
  /** 没有内容条目的回合错误或停止提示使用此状态。 */
  status?: TurnStatus
}

export type TimelineItemModel = TimelineItemBase & (
  | { kind: 'tool'; fact: Extract<HistoryItem, { type: 'tool' }> }
  | { kind: 'duration'; fact: null; timing: { startedAt: string; finishedAt?: string } }
  | { kind: 'user' }
  | { kind: 'assistant' }
  | { kind: 'thinking' }
  | { kind: 'compaction' }
  | { kind: 'terminal' }
)

export function timelineStatus(item: TimelineItemModel): ToolStatus | TurnStatus | 'recorded' {
  return item.status ?? (item.fact && 'status' in item.fact ? item.fact.status : 'recorded')
}

export function timelineBody(item: TimelineItemModel): string {
  if (item.fact?.type === 'compaction') return item.fact.summary
  if (item.fact?.type === 'compaction_result') return compactionResultText(item.fact)
  return item.fact && 'text' in item.fact ? item.fact.text : item.summary
}

const projectedItems = new WeakMap<HistoryItem, { item: TimelineItemModel; cwd: string; userHome: string | null | undefined }>()

export function buildTimeline(session: SessionView | null, userHome?: string | null): TimelineItemModel[] {
  if (!session) return []
  const result: TimelineItemModel[] = []
  const turns = session.facts
  const failure = session.runtime.failure
  const activeTurn = session.runtime.phase === 'idle' ? undefined : session.facts.at(-1)
  for (const turn of turns) {
    const active = turn === activeTurn
    const failureFromRuntime = turn === session.facts.at(-1)
      && failure?.source === 'turn'
    for (const item of projectTurn(turn, session.summary.cwd, userHome, active, failureFromRuntime)) result.push(item)
  }
  // 写入失败可能只留下实时反馈；按条目身份关联，不比较错误文字。
  if (failure && !turns.some(turn => hasErrorItem(turn, failure.error))) {
    result.push(failedItem('terminal:failed', failure.source === 'compaction'
      ? `${compactionText.failed}：${failure.error.message}` : failure.error.message))
  }
  if (session.runtime.pendingMessages.length === 0) return result
  // 用户消息落盘的事件可能先于下一份运行快照到达，按身份判断，不重复显示。
  const savedMessages = new Set(turns.flatMap(turn => turn.items.filter(item => item.type === 'message' && item.role === 'user').map(item => item.id)))
  for (const message of session.runtime.pendingMessages) {
    if (savedMessages.has(message.itemId)) continue
    result.push({
      key: `pending:${message.itemId}`, kind: 'user', title: '你', summary: '', filePath: null,
      fact: { id: message.itemId, type: 'message', role: 'user', status: 'completed', text: message.text, images: message.images },
    })
  }
  return result
}

function projectTurn(turn: ThreadTurn, cwd: string, userHome: string | null | undefined, active: boolean, failureFromRuntime: boolean): TimelineItemModel[] {
  const startedAt = turn.startedAt
  const result: TimelineItemModel[] = []
  const group = turn.turnId
  let workStartIndex = -1
  for (const fact of turn.items) {
    const cached = projectedItems.get(fact)
    let item: TimelineItemModel
    if (cached?.cwd === cwd && cached.userHome === userHome) {
      item = cached.item
    } else {
      const key = fact.type === 'compaction' || fact.type === 'compaction_result'
        ? `compaction:${fact.id}` : `content:${group}:${fact.id}`
      if (fact.type === 'tool') {
        const filePath = pathFromArgs(fact.name, fact.args)
        const summary = filePath !== null ? displayPath(filePath, cwd, userHome)
          : toolSummary(fact.name, fact.args) || firstLine(fact.result?.output ?? '')
        item = { key, fact, kind: 'tool', title: fact.name === 'ask_user_question' ? '询问用户' : filePath?.replace(/\\/g, '/').endsWith('/SKILL.md') ? '读取技能' : fact.name,
          summary, filePath }
      } else {
        const kind = fact.type === 'compaction' || fact.type === 'compaction_result' ? 'compaction' : fact.type === 'message' ? fact.role : fact.type
        let title = kind === 'user' ? '你' : kind === 'assistant' ? 'Singularity' : kind === 'compaction' ? compactionText.completed : '思考'
        if (fact.type === 'compaction_result') title = fact.status === 'failed' ? compactionText.failed : compactionResultText(fact)
        item = { key, fact, kind, title, summary: '', filePath: null }
      }
      if (!('status' in fact) || fact.status !== 'running' || fact.type === 'tool') item.stable = true
      projectedItems.set(fact, { item, cwd, userHome })
    }
    if (item.kind !== 'user' && workStartIndex < 0) workStartIndex = result.length
    result.push(item)
  }
  if (turn.finishedAt || active) {
    result.splice(workStartIndex < 0 ? result.length : workStartIndex, 0, {
      key: `content:${group}:duration`, stable: true, kind: 'duration', title: '',
      timing: { startedAt, finishedAt: turn.finishedAt },
      fact: null, summary: '', filePath: null,
    })
  }
  if (turn.status === 'interrupted') result.push({ ...interruptedItem(`content:${group}:terminal`, turn.manuallyStopped), stable: true })
  // 保存失败时没有回合错误详情，具体原因由运行状态显示。
  const failureShown = hasErrorItem(turn, turn.error)
  if (turn.status === 'failed' && !failureShown && (turn.error || !failureFromRuntime)) {
    result.push({ ...failedItem(`content:${group}:terminal`, turn.error?.message ?? '执行失败'), stable: true })
  }
  return result
}

function interruptedItem(key: string, manuallyStopped: boolean): TimelineItemModel {
  const title = interruptionText(manuallyStopped)
  return { key, kind: 'terminal', title, fact: null, summary: manuallyStopped ? '' : title,
    filePath: null, status: manuallyStopped ? 'interrupted' : 'failed' }
}

function failedItem(key: string, message: string): TimelineItemModel {
  return { key, kind: 'terminal', title: '执行失败', fact: null, summary: message, filePath: null, status: 'failed' }
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

function toolSummary(name: string, args: unknown): string {
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

function displayPath(path: string, cwd: string, userHome?: string | null): string {
  const normalized = path.replace(/\\/g, '/')
  const root = cwd.replace(/\\/g, '/').replace(/\/$/, '')
  if (normalized.toLowerCase().startsWith(`${root.toLowerCase()}/`)) return normalized.slice(root.length + 1)
  const home = userHome?.replace(/\\/g, '/').replace(/\/$/, '')
  if (home && normalized.toLowerCase() === home.toLowerCase()) return '~'
  if (home && normalized.toLowerCase().startsWith(`${home.toLowerCase()}/`)) return `~${normalized.slice(home.length)}`
  return path
}

function pathFromArgs(name: string, args: unknown): string | null {
  if (name !== 'read') return null
  const value = toolArgument(name, args)
  return value?.trim() ? value : null
}

function firstLine(text: string): string {
  return text.split(/\r?\n/, 1)[0]?.trim() ?? ''
}

function record(value: unknown): Record<string, unknown> {
  return typeof value === 'object' && value !== null ? value as Record<string, unknown> : {}
}

export function formatTurnDuration(milliseconds: number): string {
  const seconds = Math.max(0, Math.floor(milliseconds / 1000))
  return seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m${seconds % 60}s`
}

function compactionResultText(item: Extract<HistoryItem, { type: 'compaction_result' }>): string {
  return item.message ?? (item.status === 'running' ? compactionText.running : item.status === 'completed' ? compactionText.unchanged : item.status === 'interrupted' ? compactionText.unfinished : compactionText.failed)
}
