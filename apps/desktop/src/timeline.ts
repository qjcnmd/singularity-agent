import { factStatusText, compactionText } from './copy'
import type { ExecutionItem, ExecutionTurn, FactStatus, SessionView } from './execution'

type TimelineKind = 'user' | 'assistant' | 'thinking' | 'tool' | 'compaction' | 'terminal' | 'duration'

export interface TimelineItemModel {
  key: string
  /** 可恢复的历史归属；临时输出不提供阅读锚点。 */
  anchorTurnId?: string | null
  kind: TimelineKind
  title: string
  fact: ExecutionItem | null
  summary: string
  filePath: string | null
  /** 条目状态；用于无执行事实的运行时项（如压缩中） */
  status?: FactStatus
  timing?: { startedAt: string; finishedAt?: string }
}

export function timelineStatus(item: TimelineItemModel): FactStatus {
  return item.status ?? item.fact?.status ?? 'cancelled'
}

export function timelineBody(item: TimelineItemModel): string {
  return item.fact && 'text' in item.fact ? item.fact.text : item.summary
}

const projectedItems = new WeakMap<ExecutionItem, { item: TimelineItemModel; cwd: string; userHome: string | null | undefined }>()

export function buildTimeline(session: SessionView | null, userHome?: string | null): TimelineItemModel[] {
  if (!session) return []
  const result: TimelineItemModel[] = []
  const turns = [...session.facts.history, ...session.facts.active]
  const terminal = session.runtime.terminal
  let historyTurnId: string | null = null
  for (const turn of turns) {
    historyTurnId = turn.id ?? historyTurnId
    const active = session.runtime.activeTurn?.turnId === turn.id
    const failureFromRuntime = turn === session.facts.active.at(-1)
      && terminal?.source === 'turn' && terminal.status === 'failed'
    for (const item of projectTurn(turn, historyTurnId, session.summary.cwd, userHome, active, failureFromRuntime)) result.push(item)
  }
  // 摘要请求成功不代表摘要已经保存；失败或停止后不能继续显示“正在压缩”。
  if (session.runtime.phase === 'idle') {
    for (const [index, item] of result.entries()) {
      if (item.kind !== 'compaction' || item.status !== 'running') continue
      const failed = terminal?.status === 'failed'
      result[index] = { ...item, status: failed ? 'error' : 'cancelled',
        title: failed ? compactionText.failed : compactionText.unfinished }
    }
  }
  // 请求发出前也显示压缩进度；完成结果由历史或执行事件替换。
  if (session.runtime.activeCompaction && !session.facts.active.some(turn => turn.items.some(item =>
    item.kind === 'compaction' || (item.kind === 'request' && item.observation.purpose === 'compaction')))) {
    result.push(compactionItem('compaction:running', 'running', compactionText.running))
  }
  // 后端只在失败原因未保存时提供 message；它属于当前操作，不与历史错误去重。
  if (terminal?.message) {
    result.push(failedItem('terminal:failed', terminal.source === 'compaction'
      ? `${compactionText.failed}：${terminal.message}` : terminal.message))
  }
  // 若执行被手动中断，末尾追加“已停止”提示
  if (session.runtime.terminal?.source !== 'compaction'
    && session.runtime.terminal?.status === 'interrupted'
    && result[result.length - 1]?.kind !== 'terminal') result.push(stoppedItem())
  return result
}

function projectTurn(turn: ExecutionTurn, anchorTurnId: string | null, cwd: string, userHome: string | null | undefined, active: boolean, failureFromRuntime: boolean): TimelineItemModel[] {
  const startedAt = turn.startedAt
  const result: TimelineItemModel[] = []
  // 首部 settings 记录没有 turnId，使用 'leading' 占位 key
  const group = turn.id ?? 'leading'
  let workStartIndex = -1
  let compactionIndex = -1
  for (const fact of turn.items) {
    if (fact.kind === 'request') {
      if (fact.observation.purpose === 'compaction') {
        compactionIndex = result.length
        if (workStartIndex < 0) workStartIndex = result.length
        const status = fact.status === 'error' || fact.status === 'cancelled' ? fact.status : 'running'
        result.push({ ...compactionItem(`compaction:${fact.id}`, status,
          status === 'error' ? compactionText.failed : status === 'cancelled' ? compactionText.stopped : compactionText.running), anchorTurnId })
      }
      continue
    }
    if (fact.kind === 'settings' || fact.kind === 'event') continue
    const cached = projectedItems.get(fact)
    let item = cached?.cwd === cwd && cached.userHome === userHome ? cached?.item : undefined
    if (!item) {
      const key = `content:${group}:${fact.id}`
      if (fact.kind === 'tool') {
        const filePath = pathFromArgs(fact.name, fact.args)
        const summary = filePath !== null ? displayPath(filePath, cwd, userHome)
          : toolSummary(fact.name, fact.args) || firstLine(fact.output)
        item = { key, fact, kind: 'tool', title: fact.name === 'ask_user_question' ? '询问用户' : fact.name,
          summary, filePath }
      } else {
        const kind = fact.kind === 'compaction' || fact.kind === 'compaction_result' ? 'compaction' : fact.kind
        let title = kind === 'user' ? '你' : kind === 'assistant' ? 'Singularity' : kind === 'compaction' ? compactionText.completed : kind
        if (fact.kind === 'compaction_result') title = fact.status === 'error' ? compactionText.failed : fact.text
        item = { key, fact, kind, title, summary: '', filePath: null }
      }
      if (fact.status !== 'running' || fact.kind === 'tool') item.anchorTurnId = anchorTurnId
      projectedItems.set(fact, { item, cwd, userHome })
    }
    if (item.kind !== 'user' && workStartIndex < 0) workStartIndex = result.length
    if (item.kind === 'compaction' && compactionIndex >= 0) {
      // 摘要和失败、停止结果都替换请求行，保留同一个显示位置和身份。
      result[compactionIndex] = { ...item, key: result[compactionIndex].key, anchorTurnId }
      compactionIndex = -1
    } else result.push(item)
  }
  if (startedAt && (turn.finishedAt || active)) {
    result.splice(workStartIndex < 0 ? result.length : workStartIndex, 0, {
      key: `content:${group}:duration`, anchorTurnId, kind: 'duration', title: '',
      timing: { startedAt, finishedAt: turn.finishedAt },
      fact: null, summary: '', filePath: null,
    })
  }
  if (turn.status === 'interrupted') result.push({ ...stoppedItem(`content:${group}:terminal`), anchorTurnId })
  // 保存失败时没有回合错误详情，具体原因由运行状态显示。
  if (turn.status === 'failed' && (turn.error || !failureFromRuntime)) {
    result.push({ ...failedItem(`content:${group}:terminal`, turn.error?.message ?? '执行失败'), anchorTurnId })
  }
  return result
}

function compactionItem(key: string, status: FactStatus, title: string, summary = ''): TimelineItemModel {
  return { key, kind: 'compaction', title, fact: null, summary, filePath: null, status }
}

function stoppedItem(key = 'terminal:interrupted'): TimelineItemModel {
  return { key, kind: 'terminal', title: factStatusText.cancelled, fact: null, summary: '', filePath: null }
}

function failedItem(key: string, message: string): TimelineItemModel {
  return { key, kind: 'terminal', title: '执行失败', fact: null, summary: message, filePath: null, status: 'error' }
}

export function toolArgument(name: string, args: unknown): string | null {
  const input = record(args)
  const value = name === 'bash' ? input.command : name === 'read' ? input.path : undefined
  return typeof value === 'string' ? value : null
}

function toolSummary(name: string, args: unknown): string {
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
