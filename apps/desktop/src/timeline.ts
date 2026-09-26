import { factStatusText, compactionTitle } from './copy'
import { parsePatch, type StructuredPatch } from 'diff'
import Anser from 'anser'
import type { ExecutionItem, ExecutionTurn, FactStatus, SessionView } from './execution'

type TimelineKind = 'user' | 'assistant' | 'thinking' | 'tool' | 'diff' | 'compaction' | 'terminal' | 'duration'

export interface TimelineItemModel {
  key: string
  kind: TimelineKind
  title: string
  fact: ExecutionItem | null
  summary: string
  filePath: string | null
  addedLines: number
  removedLines: number
  /** 条目自身状态；没有执行事实的运行时反馈（如正在压缩）用它表达状态。 */
  status?: FactStatus
  timing?: { startedAt: string; finishedAt?: string }
  /** 工具展示的派生数据；工具运行事实本身只由顶层 fact 持有。 */
  tool?: { diff: string; patches: StructuredPatch[] }
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
  for (const turn of [...session.facts.history, ...session.facts.active]) {
    const active = session.runtime.activeTurn?.turnId === turn.id ? session.runtime.activeTurn : null
    for (const item of projectTurn(turn, session.summary.cwd, userHome, active)) result.push(item)
  }
  // 独立压缩的反馈只描述那次压缩：进行中显示“正在压缩…”，结束后显示这次压缩
  // 的结果（没有可压缩的内容 / 已停止 / 失败原因）。它不表示任务被停止或失败，
  // 因此不参与下面的回合停止提示。
  if (session.runtime.activeCompaction) {
    result.push(compactionItem('compaction:running', 'running', '正在压缩…'))
  } else if (session.runtime.terminal?.source === 'compaction') {
    const terminal = session.runtime.terminal
    if (terminal.status === 'completed') result.push(compactionItem('compaction:completed', 'stable', '没有可压缩的内容'))
    else if (terminal.status === 'interrupted') result.push(compactionItem('compaction:interrupted', 'cancelled', factStatusText.cancelled))
    else result.push(compactionItem('compaction:failed', 'error', terminal.message ?? '压缩失败'))
  }
  // 当前停止提示只看可见尾部：历史上更早的 terminal 不遮蔽本次停止；尾部已经
  // 是停止提示时（相邻、没有新可见内容）合并为同一条。
  if (session.runtime.terminal?.source !== 'compaction'
    && session.runtime.terminal?.status === 'interrupted'
    && result[result.length - 1]?.kind !== 'terminal') result.push(stoppedItem())
  return result
}

// 只重投影发生变化的轮次；弱引用随历史页和任务释放，不维护额外失效版本。
const projectedTurns = new WeakMap<ExecutionTurn, {
  cwd: string; userHome: string | null | undefined; startedAt: string | undefined; showDuration: boolean; items: TimelineItemModel[]
}>()

function projectTurn(turn: ExecutionTurn, cwd: string, userHome: string | null | undefined, active: SessionView['runtime']['activeTurn']): TimelineItemModel[] {
  const startedAt = turn.startedAt ?? active?.startedAt
  const showDuration = Boolean(startedAt && (turn.finishedAt || active))
  const cachedTurn = projectedTurns.get(turn)
  if (cachedTurn?.cwd === cwd && cachedTurn.userHome === userHome && cachedTurn.startedAt === startedAt && cachedTurn.showDuration === showDuration) return cachedTurn.items
  const result: TimelineItemModel[] = []
  // 开头的 settings 组没有 turn id；显示键为它统一使用一个稳定占位符。
  const group = turn.id ?? 'leading'
  let workStartIndex = -1
  for (const fact of turn.items) {
    if (fact.kind === 'request' || fact.kind === 'settings' || fact.kind === 'event' || fact.kind === 'unknown') continue
    const cached = projectedItems.get(fact)
    let item = cached?.cwd === cwd && cached.userHome === userHome ? cached?.item : undefined
    if (!item) {
      const key = `content:${group}:${fact.id}`
      if (fact.kind === 'tool') {
        const diff = fact.status === 'error' ? '' : fact.diff ?? ''
        let patches: StructuredPatch[] = []
        try { patches = parsePatch(diff) } catch { /* Malformed patches remain visible as their original text. */ }
        const filePath = pathFromArgs(fact.name, fact.args)
        const stats = diffStats(patches)
        const summary = fact.status === 'error' ? failureSummary(fact.output)
          : filePath !== null ? displayPath(filePath, cwd, userHome) : toolSummary(fact.name, fact.args) || firstLine(fact.output)
        item = { key, fact, kind: diff !== '' || toolDisplay(fact.name)?.output === 'diff' ? 'diff' : 'tool', title: fact.name,
          summary, filePath, addedLines: stats.added, removedLines: stats.removed, tool: { diff, patches } }
      } else {
        const kind = fact.kind === 'compaction' ? 'compaction' : fact.kind
        const title = kind === 'user' ? '你' : kind === 'assistant' ? 'Singularity' : kind === 'compaction' ? compactionTitle : kind
        item = { key, fact, kind, title, summary: '', filePath: null, addedLines: 0, removedLines: 0 }
      }
      projectedItems.set(fact, { item, cwd, userHome })
    }
    if (item.kind !== 'user' && workStartIndex < 0) workStartIndex = result.length
    result.push(item)
  }
  if (startedAt && showDuration) {
    result.splice(workStartIndex < 0 ? result.length : workStartIndex, 0, {
      key: `content:${group}:duration`, kind: 'duration', title: '',
      timing: { startedAt, finishedAt: turn.finishedAt },
      fact: null, summary: '', filePath: null, addedLines: 0, removedLines: 0,
    })
  }
  if (turn.status === 'interrupted') result.push(stoppedItem(`content:${group}:terminal`))
  projectedTurns.set(turn, { cwd, userHome, startedAt, showDuration, items: result })
  return result
}

/** 压缩反馈行：标题固定，正文是这次压缩的状态或结果。 */
function compactionItem(key: string, status: FactStatus, text: string): TimelineItemModel {
  return { key, kind: 'compaction', title: compactionTitle, fact: null, summary: text, filePath: null, addedLines: 0, removedLines: 0, status }
}

function stoppedItem(key = 'terminal:interrupted'): TimelineItemModel {
  return { key, kind: 'terminal', title: factStatusText.cancelled, fact: null, summary: '', filePath: null, addedLines: 0, removedLines: 0 }
}

interface ToolDisplay {
  argument: string
  output: 'terminal' | 'search' | 'read' | 'diff' | 'text'
}

const toolDisplays: Record<string, ToolDisplay | undefined> = {
  bash: { argument: 'command', output: 'terminal' },
  grep: { argument: 'pattern', output: 'search' },
  glob: { argument: 'pattern', output: 'search' },
  skill: { argument: 'name', output: 'text' },
  read: { argument: 'path', output: 'read' },
  edit: { argument: 'path', output: 'diff' },
  write: { argument: 'path', output: 'diff' },
}

export function toolDisplay(name: string): ToolDisplay | undefined {
  return Object.hasOwn(toolDisplays, name) ? toolDisplays[name] : undefined
}

export function toolArgument(name: string, args: unknown): string | null {
  const display = toolDisplay(name)
  const value = display ? record(args)[display.argument] : undefined
  return typeof value === 'string' ? value : null
}

function toolSummary(name: string, args: unknown): string {
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
  if (toolDisplay(name)?.argument !== 'path') return null
  const value = toolArgument(name, args)
  return value?.trim() ? value : null
}

function diffStats(patches: StructuredPatch[]): { added: number; removed: number } {
  let added = 0
  let removed = 0
  for (const patch of patches) {
    for (const hunk of patch.hunks) {
      for (const line of hunk.lines) {
        if (line.startsWith('+')) added += 1
        if (line.startsWith('-')) removed += 1
      }
    }
  }
  return { added, removed }
}


function firstLine(text: string): string {
  return text.split(/\r?\n/, 1)[0]?.trim() ?? ''
}

export function failureSummary(output: string): string {
  const lines = Anser.ansiToText(output).split(/\r?\n/).map(line => line.trim()).filter(Boolean)
  const diagnostic = lines.findLast(line => /\b[\w.]*(?:error|exception)\b\s*:|:\s*(?:command not found|No such file or directory|Permission denied)\b/i.test(line))
  const message = diagnostic ?? lines.findLast(line => /^Command (?:exited|timed out|terminated|aborted)\b/.test(line)) ?? lines[0] ?? ''
  // 折叠行只展示简短原因，命令、路径、错误编号和堆栈保留在详情。
  const reason = message.match(/\b(?:No such file or directory|Permission denied|command not found)\b/i)?.[0]
  if (reason) return reason
  if (/^Command timed out\b/.test(message)) return '执行超时'
  if (/^Command (?:exited|terminated|aborted)\b/.test(message)) return '命令未能完成'
  const brief = message.replace(/^.*?\b[\w.]*(?:error|exception)\b\s*:\s*/i, '').replace(/^\[Errno \d+\]\s*/, '')
  return brief && brief.length <= 80 && !/[\\/]|\(os error \d+\)/.test(brief) ? brief : '执行失败，请展开查看'
}

function record(value: unknown): Record<string, unknown> {
  return typeof value === 'object' && value !== null ? value as Record<string, unknown> : {}
}

export function formatTurnDuration(milliseconds: number): string {
  const seconds = Math.max(0, Math.floor(milliseconds / 1000))
  return seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m${seconds % 60}s`
}
