import { parsePatch, type StructuredPatch } from 'diff'
import type { ExecutionItem, SessionView } from './execution'

export type TimelineKind =
  | 'user'
  | 'assistant'
  | 'thinking'
  | 'tool'
  | 'diff'
  | 'diagnostic'
  | 'terminal'
  | 'unknown'

export interface TimelineSection {
  label: string
  content: string
  kind: 'text' | 'code' | 'error' | 'json'
}

export interface TimelineItemModel {
  key: string
  kind: TimelineKind
  title: string
  body: string
  status: 'stable' | 'running' | 'completed' | 'failed' | 'interrupted'
  filePath: string | null
  addedLines: number
  removedLines: number
  sections: TimelineSection[]
  tool?: { args: unknown; output: string; diff: string; patches: StructuredPatch[] }
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
        const status = fact.status === 'ok' ? 'completed' : fact.status === 'error' ? 'failed' : fact.status === 'cancelled' ? 'interrupted' : fact.status
        if (fact.kind === 'tool') {
          item = toolItem(key, fact.name, fact.args, status)
          if (fact.output || fact.diff || fact.status === 'ok' || fact.status === 'error') {
            item = finishTool(item, fact.args, fact.output, fact.status === 'error', fact.diff)
          }
        } else {
          const kind = fact.kind === 'compaction' ? 'diagnostic' : fact.kind
          const title = kind === 'user' ? '你' : kind === 'assistant' ? 'Singularity' : kind === 'unknown' ? '项目' : kind
          item = itemModel(key, kind, title, fact.text, status)
        }
        if (fact.error) item.sections.push({ label: '错误', content: fact.error, kind: 'error' })
        projectedItems.set(fact, item)
      }
      result.push(item)
    }
    if (turn.status === 'interrupted') result.push(stoppedItem(`content:${turn.id}:terminal`))
  }
  if (session.runtime.terminal?.status === 'interrupted' && !result.some(item => item.kind === 'terminal')) result.push(stoppedItem())
  return result
}

function toolItem(
  key: string,
  name: string,
  args: unknown,
  status: TimelineItemModel['status'],
): TimelineItemModel {
  const path = pathFromArgs(args)
  const body = toolSummary(name, args)
  return {
    ...itemModel(key, isDiffTool(name) ? 'diff' : 'tool', name, body, status, []),
    tool: { args, output: '', diff: '', patches: [] },
    filePath: path,
  }
}

function finishTool(
  item: TimelineItemModel,
  args: unknown,
  output: string,
  isError: boolean,
  savedDiff: string | undefined,
): TimelineItemModel {
  const diff = isError ? '' : savedDiff ?? ''
  let patches: StructuredPatch[] = []
  try { patches = parsePatch(diff) } catch { /* Malformed patches remain visible as their original text. */ }
  const path = pathFromArgs(args)
  const stats = diffStats(patches)
  const summary = isError ? firstLine(output) : path !== null && diff !== ''
    ? path
    : item.body || firstLine(output)
  return {
    ...item,
    kind: diff === '' ? item.kind : 'diff',
    body: summary,
    filePath: path ?? item.filePath,
    addedLines: stats.added,
    removedLines: stats.removed,
    tool: { args, output, diff, patches },
  }
}


function stoppedItem(key = 'terminal:interrupted'): TimelineItemModel {
  return itemModel(key, 'terminal', '已停止', '', 'interrupted')
}

function itemModel(
  key: string,
  kind: TimelineKind,
  title: string,
  body: string,
  status: TimelineItemModel['status'],
  sections: TimelineSection[] = body === '' ? [] : [{ label: '内容', content: body, kind: 'text' }],
): TimelineItemModel {
  return {
    key,
    kind,
    title,
    body,
    status,
    filePath: null,
    addedLines: 0,
    removedLines: 0,
    sections,
  }
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
