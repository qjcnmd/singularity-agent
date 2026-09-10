import { eventsSince, isEventPrefix, type EventSequence } from './eventLog'
import { eventTurnId } from './protocol'
import { parsePatch, type StructuredPatch } from 'diff'
import type {
  HistoryItem,
  SessionReadResult,
  TurnStatus,
} from './protocol'

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
  startedAt: string | null
  durationMs: number | null
  sections: TimelineSection[]
  tool?: { args: unknown; output: string; diff: string; patches: StructuredPatch[] }
}


const historyProjections = new WeakMap<SessionReadResult['history']['turns'], TimelineItemModel[]>()

export function buildTimeline(session: SessionReadResult | null, now = Date.now()): TimelineItemModel[] {
  if (session === null) return []
  const stable = historyProjections.get(session.history.turns) ?? session.history.turns.flatMap((turn, turnIndex) =>
    projectHistoryTurn(turn.items, turn.turnId ?? `leading-${turnIndex}`, turn.status),
  )
  historyProjections.set(session.history.turns, stable)
  const activeTurn = session.runtime.activeTurn
  const active = reduceActive(activeTurn?.events ?? [], activeTurn?.startedAt ?? null, now)
  const projectedTerminal = active.findLast((item) => item.kind === 'terminal')
    ?? (activeTurn === null ? stable.findLast((item) => item.kind === 'terminal') : undefined)
  const combined = [...stable, ...active]
  const terminal = session.runtime.terminal
  if (terminal?.status === 'interrupted' && projectedTerminal === undefined) combined.push(stoppedItem())
  return combined
}

function projectHistoryTurn(
  items: HistoryItem[],
  turnId: string,
  status: TurnStatus | null,
): TimelineItemModel[] {
  const projected: TimelineItemModel[] = []
  const tools = new Map<string, number>()
  for (const item of items) {
    if (item.type === 'request') continue
    if (item.type === 'settings') continue
    if (item.type === 'tool_call') {
      tools.set(item.id, projected.length)
      projected.push(toolItem(`content:${turnId}:${item.id}`, item.name, item.args, 'stable'))
      continue
    }
    if (item.type === 'tool_result') {
      const position = tools.get(item.id)
      if (position !== undefined) {
        projected[position] = finishTool(
          projected[position],
          projected[position].tool?.args,
          item.output,
          item.isError,
          item.durationMs ?? null,
          item.diff,
        )
        continue
      }
    }
    projected.push(historyItem(item, turnId))
  }
  if (status === 'interrupted') projected.push(stoppedItem(`content:${turnId}:terminal`))
  return projected
}

function historyItem(item: Exclude<HistoryItem, { type: 'request' | 'settings' | 'tool_call' }>, turnId: string): TimelineItemModel {
  switch (item.type) {
    case 'message':
      return itemModel(
        `content:${turnId}:${item.id}`,
        item.role === 'user' ? 'user' : 'assistant',
        item.role === 'user' ? '你' : 'Singularity',
        item.text,
        'stable',
      )
    case 'thinking':
      return itemModel(`content:${turnId}:${item.id}`, 'thinking', 'thinking', item.text, 'stable')
    case 'tool_result':
      return itemModel(
        `content:${turnId}:${item.id}`,
        'tool',
        item.isError ? 'tool error' : 'tool output',
        firstLine(item.output),
        item.isError ? 'failed' : 'stable',
        [{ label: item.isError ? '错误' : '输出', content: item.output, kind: item.isError ? 'error' : 'code' }],
      )
    case 'compaction':
      return itemModel(`content:${turnId}:${item.id}`, 'diagnostic', 'compaction', item.summary, 'stable')
  }
}

function newActiveProjection(events: EventSequence) {
  return { events, items: [] as TimelineItemModel[], positions: new Map<string, number>() }
}
let activeProjection = newActiveProjection([])

function reduceActive(
  events: EventSequence,
  activeStartedAt: string | null,
  now: number,
): TimelineItemModel[] {
  const previous = activeProjection.events
  const appended = isEventPrefix(previous, events)
  const start = appended ? previous.length : 0
  if (!appended) activeProjection = newActiveProjection(events)
  activeProjection.events = events
  const { items, positions } = activeProjection
  const upsert = (item: TimelineItemModel, append = false) => {
    const position = positions.get(item.key)
    if (position === undefined) {
      positions.set(item.key, items.length)
      items.push(item)
      return
    }
    const previous = items[position]
    items[position] = append
      ? previous.kind === 'unknown'
        ? item
        : { ...item, body: previous.body + item.body }
      : item
  }
  for (const event of eventsSince(events, start, appended ? previous : undefined)) {
    const turnId = eventTurnId(event)
    switch (event.method) {
      case 'turn/started':
        upsert(itemModel(`content:${turnId}:user`, 'user', '你', event.params.input, 'stable'))
        break
      case 'item/started': {
        const itemId = event.params.item.itemId
        upsert(itemModel(
          `content:${turnId}:${itemId}`,
          'unknown',
          '项目已开始',
          itemId,
          'running',
          [payloadSection(event.params)],
        ))
        break
      }
      case 'item/agentMessage/delta': {
        const itemId = event.params.item.itemId
        upsert(itemModel(`content:${turnId}:${itemId}`, 'assistant', 'Singularity', event.params.delta, 'running'), true)
        break
      }
      case 'item/agentThinking/delta':
      case 'item/agentThinking': {
        const itemId = event.params.item.itemId
        const streaming = event.method === 'item/agentThinking/delta'
        upsert(itemModel(
          `content:${turnId}:${itemId}`,
          'thinking',
          'thinking',
          event.method === 'item/agentThinking/delta' ? event.params.delta : event.params.text,
          streaming ? 'running' : 'completed',
        ), streaming)
        break
      }
      case 'tool/execution/start':
      case 'tool/execution/update':
      case 'tool/execution/end': {
        const callId = event.params.toolCallId
        const name = event.params.toolName
        const key = `content:${turnId}:${callId}`
        const existingPosition = positions.get(key)
        const existing = existingPosition === undefined ? undefined : items[existingPosition]
        const args = 'args' in event.params ? event.params.args : existing?.tool?.args ?? {}
        const startedAt = existing?.startedAt ?? ('startedAt' in event.params ? event.params.startedAt ?? activeStartedAt : activeStartedAt)
        if (event.method === 'tool/execution/start' || !existing?.tool) {
          upsert(withTiming(toolItem(key, name, args, 'running'), startedAt, elapsedDuration(startedAt, now)))
        }
        if (event.method === 'tool/execution/start') break
        const position = positions.get(key)
        if (position === undefined) break
        const result = event.method === 'tool/execution/end' ? event.params.result : undefined
        const output = event.method === 'tool/execution/update'
          ? event.params.partialResult
          : event.params.result.content.map(part => part.text).join('\n')
        items[position] = finishTool(
          items[position],
          args,
          output,
          result?.isError ?? false,
          (event.method === 'tool/execution/end' ? event.params.durationMs : undefined) ?? elapsedDuration(startedAt, now),
          result?.diff,
          event.method === 'tool/execution/end' ? 'completed' : 'running',
        )
        break
      }
      case 'item/completed':
      case 'item/failed': {
        const itemId = event.params.item.itemId
        const key = `content:${turnId}:${itemId}`
        const position = positions.get(key)
        const failed = event.method === 'item/failed'
        const error = event.method === 'item/failed' ? event.params.error : ''
        if (position === undefined) {
          upsert(itemModel(
            key,
            'unknown',
            failed ? '项目失败' : '项目已完成',
            failed ? error : itemId,
            failed ? 'failed' : 'completed',
            [payloadSection(event.params)],
          ))
        } else {
          const previous = items[position]
          items[position] = {
            ...previous,
            status: failed ? 'failed' : 'completed',
            sections: failed && error !== ''
              ? [...previous.sections, { label: '错误', content: error, kind: 'error' }]
              : previous.sections,
          }
        }
        break
      }
      case 'agent/diagnostic':
        break
      case 'turn/completed': {
        const status = event.params.turn.status
        if (status === 'interrupted') upsert(stoppedItem(`content:${turnId}:terminal`))
        break
      }
      case 'turn/error':
        break

    }
  }
  return items
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
  durationMs: number | null,
  savedDiff: string | undefined,
  completedStatus: 'running' | 'completed' = 'completed',
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
    status: isError ? 'failed' : completedStatus,
    filePath: path ?? item.filePath,
    addedLines: stats.added,
    removedLines: stats.removed,
    durationMs,
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
    startedAt: null,
    durationMs: null,
    sections,
  }
}

function withTiming(item: TimelineItemModel, startedAt: string | null, durationMs: number | null): TimelineItemModel {
  return { ...item, startedAt, durationMs }
}

function payloadSection(params: unknown): TimelineSection {
  return { label: '原始事件', content: JSON.stringify(params, null, 2), kind: 'json' }
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


function elapsedDuration(startedAt: string | null, now: number): number | null {
  if (startedAt === null) return null
  const started = Date.parse(startedAt)
  return Number.isFinite(started) ? Math.max(0, now - started) : null
}

function firstLine(text: string): string {
  return text.split(/\r?\n/, 1)[0]?.trim() ?? ''
}

function record(value: unknown): Record<string, unknown> {
  return typeof value === 'object' && value !== null ? value as Record<string, unknown> : {}
}
