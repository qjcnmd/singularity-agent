import { factStatusText, compactionTitle } from './copy'
import { parsePatch, type StructuredPatch } from 'diff'
import type { ExecutionItem, FactStatus, SessionView } from './execution'

type TimelineKind = 'user' | 'assistant' | 'thinking' | 'tool' | 'diff' | 'compaction' | 'terminal' | 'unknown'

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
  /** 工具展示的派生数据；工具运行事实本身只由顶层 fact 持有。 */
  tool?: { diff: string; patches: StructuredPatch[] }
}

export function timelineStatus(item: TimelineItemModel): FactStatus {
  return item.status ?? item.fact?.status ?? 'cancelled'
}

export function timelineBody(item: TimelineItemModel): string {
  return item.fact && 'text' in item.fact ? item.fact.text : item.summary
}

const projectedItems = new WeakMap<ExecutionItem, TimelineItemModel>()

export function buildTimeline(session: SessionView | null): TimelineItemModel[] {
  if (!session) return []
  const result: TimelineItemModel[] = []
  for (const turn of [...session.facts.history, ...session.facts.active]) {
    // 开头的 settings 组没有 turn id；显示键为它统一使用一个稳定占位符。
    const group = turn.id ?? 'leading'
    for (const fact of turn.items) {
      if (fact.kind === 'request' || fact.kind === 'settings' || fact.kind === 'event') continue
      let item = projectedItems.get(fact)
      if (!item) {
        const key = `content:${group}:${fact.id}`
        if (fact.kind === 'tool') {
          const diff = fact.status === 'error' ? '' : fact.diff ?? ''
          let patches: StructuredPatch[] = []
          try { patches = parsePatch(diff) } catch { /* Malformed patches remain visible as their original text. */ }
          const filePath = pathFromArgs(fact.name, fact.args)
          const stats = diffStats(patches)
          const summary = fact.status === 'error' ? firstLine(fact.output)
            : filePath !== null && diff !== '' ? filePath : toolSummary(fact.name, fact.args) || firstLine(fact.output)
          item = { key, fact, kind: diff !== '' || toolDisplay(fact.name)?.output === 'diff' ? 'diff' : 'tool', title: fact.name,
            summary, filePath, addedLines: stats.added, removedLines: stats.removed, tool: { diff, patches } }
        } else {
          const kind = fact.kind === 'compaction' ? 'compaction' : fact.kind
          const title = kind === 'user' ? '你' : kind === 'assistant' ? 'Singularity' : kind === 'unknown' ? '项目' : kind === 'compaction' ? compactionTitle : kind
          item = { key, fact, kind, title, summary: '', filePath: null, addedLines: 0, removedLines: 0 }
        }
        projectedItems.set(fact, item)
      }
      result.push(item)
    }
    if (turn.status === 'interrupted') result.push(stoppedItem(`content:${group}:terminal`))
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
  return firstLine(toolArgument(name, args) ?? '')
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

function record(value: unknown): Record<string, unknown> {
  return typeof value === 'object' && value !== null ? value as Record<string, unknown> : {}
}
