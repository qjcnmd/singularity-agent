import { parsePatch, type StructuredPatch } from 'diff'
import type { ExecutionItem, FactStatus, SessionView } from './execution'

export type TimelineKind = 'user' | 'assistant' | 'thinking' | 'tool' | 'diff' | 'diagnostic' | 'terminal' | 'unknown'

export interface TimelineSection {
  label: string
  content: string
  kind: 'text' | 'code' | 'error' | 'json'
}

export interface TimelineItemModel {
  key: string
  kind: TimelineKind
  title: string
  fact: ExecutionItem | null
  summary: string
  filePath: string | null
  addedLines: number
  removedLines: number
  tool?: { fact: Extract<ExecutionItem, { kind: 'tool' }>; diff: string; patches: StructuredPatch[] }
}

export function timelineStatus(item: TimelineItemModel): FactStatus {
  return item.fact?.status ?? 'cancelled'
}

export function timelineBody(item: TimelineItemModel): string {
  return item.fact && 'text' in item.fact ? item.fact.text : item.summary
}

const projectedItems = new WeakMap<ExecutionItem, TimelineItemModel>()

export function buildTimeline(session: SessionView | null): TimelineItemModel[] {
  if (!session) return []
  const result: TimelineItemModel[] = []
  for (const turn of [...session.facts.history, ...session.facts.active]) {
    for (const fact of turn.items) {
      if (fact.kind === 'request' || fact.kind === 'settings' || fact.kind === 'event') continue
      let item = projectedItems.get(fact)
      if (!item) {
        const key = `content:${turn.id}:${fact.id}`
        if (fact.kind === 'tool') {
          const diff = fact.status === 'error' ? '' : fact.diff ?? ''
          let patches: StructuredPatch[] = []
          try { patches = parsePatch(diff) } catch { /* Malformed patches remain visible as their original text. */ }
          const filePath = pathFromArgs(fact.args)
          const stats = diffStats(patches)
          const summary = fact.status === 'error' ? firstLine(fact.output)
            : filePath !== null && diff !== '' ? filePath : toolSummary(fact.name, fact.args) || firstLine(fact.output)
          item = { key, fact, kind: diff !== '' || isDiffTool(fact.name) ? 'diff' : 'tool', title: fact.name,
            summary, filePath, addedLines: stats.added, removedLines: stats.removed, tool: { fact, diff, patches } }
        } else {
          const kind = fact.kind === 'compaction' ? 'diagnostic' : fact.kind
          const title = kind === 'user' ? '你' : kind === 'assistant' ? 'Singularity' : kind === 'unknown' ? '项目' : kind
          item = { key, fact, kind, title, summary: '', filePath: null, addedLines: 0, removedLines: 0 }
        }
        projectedItems.set(fact, item)
      }
      result.push(item)
    }
    if (turn.status === 'interrupted') result.push(stoppedItem(`content:${turn.id}:terminal`))
  }
  if (session.runtime.terminal?.status === 'interrupted' && !result.some(item => item.kind === 'terminal')) result.push(stoppedItem())
  return result
}

function stoppedItem(key = 'terminal:interrupted'): TimelineItemModel {
  return { key, kind: 'terminal', title: '已停止', fact: null, summary: '', filePath: null, addedLines: 0, removedLines: 0 }
}

function isDiffTool(name: string): boolean {
  return name === 'edit' || name === 'write'
}


function toolSummary(name: string, args: unknown): string {
  const values = record(args)
  const keys = name === 'bash'
    ? ['description', 'command', 'cmd']
    : name === 'grep' || name === 'glob'
      ? ['query', 'pattern', 'path']
      : ['path', 'filePath', 'file_path', 'name']
  for (const key of keys) {
    const value = values[key]
    if (typeof value === 'string' && value.trim() !== '') return firstLine(value)
  }
  return ''
}

function pathFromArgs(args: unknown): string | null {
  const values = record(args)
  for (const key of ['path', 'filePath', 'file_path']) {
    if (typeof values[key] === 'string' && values[key].trim() !== '') return values[key]
  }
  return null
}

function diffStats(patches: StructuredPatch[]): { added: number; removed: number } {
  let added = 0
  let removed = 0
  for (const line of patches.flatMap(patch => patch.hunks.flatMap(hunk => hunk.lines))) {
    if (line.startsWith('+')) added += 1
    if (line.startsWith('-')) removed += 1
  }
  return { added, removed }
}


function firstLine(text: string): string {
  return text.split(/\r?\n/, 1)[0]?.trim() ?? ''
}

function record(value: unknown): Record<string, unknown> {
  return typeof value === 'object' && value !== null ? value as Record<string, unknown> : {}
}
