import type {
  HistoryItem,
  SessionReadResult,
  TurnEventEnvelope,
  TurnStatus,
} from './protocol'

export type TimelineKind =
  | 'user'
  | 'assistant'
  | 'thinking'
  | 'tool'
  | 'diff'
  | 'diagnostic'
  | 'control'
  | 'terminal'
  | 'unknown'

export interface TimelineSection {
  label: string
  content: string
  kind: 'text' | 'code' | 'diff' | 'error' | 'json'
}

export interface TimelineItemModel {
  key: string
  kind: TimelineKind
  title: string
  body: string
  detail: string
  status: 'stable' | 'running' | 'completed' | 'failed' | 'interrupted' | 'pending'
  filePath: string | null
  addedLines: number
  removedLines: number
  startedAt: string | null
  durationMs: number | null
  sections: TimelineSection[]
  toolRequest?: string
  requestRunning?: boolean
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
  const tools = new Map<string, { position: number; name: string; args: unknown }>()
  let request: string | undefined
  for (const item of items) {
    if (item.type === 'request') {
      request = `request:${turnId}:${item.observation.ordinal}:${item.observation.attempt}`
      continue
    }
    if (item.type === 'settings') continue
    if (item.type === 'tool_call') {
      tools.set(item.id, { position: projected.length, name: item.name, args: item.args })
      projected.push({ ...toolItem(`content:${turnId}:${item.id}`, item.name, item.args, 'stable'), toolRequest: request })
      continue
    }
    if (item.type === 'tool_result') {
      const tool = tools.get(item.id)
      if (tool !== undefined) {
        projected[tool.position] = finishTool(
          projected[tool.position],
          tool.name,
          tool.args,
          item.output,
          item.isError,
          item.durationMs ?? null,
        )
        continue
      }
    }
    projected.push(historyItem(item, turnId))
  }
  if (status === 'interrupted') projected.push(stoppedItem(`content:${turnId}:terminal`))
  return projected
}

function historyItem(item: HistoryItem, turnId: string): TimelineItemModel {
  switch (item.type) {
    case 'request':
      return itemModel(`content:${turnId}:${item.id}`, 'diagnostic', 'request', item.observation.model, 'stable')
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
    case 'tool_call':
      return toolItem(`content:${turnId}:${item.id}`, item.name, item.args, 'stable')
    case 'tool_result':
      return itemModel(
        `content:${turnId}:${item.id}`,
        'tool',
        item.isError ? 'tool error' : 'tool output',
        firstLine(item.output),
        item.isError ? 'failed' : 'stable',
        [{ label: item.isError ? '错误' : '输出', content: item.output, kind: item.isError ? 'error' : 'code' }],
      )
    case 'settings':
      return itemModel(
        `content:${turnId}:${item.id}`,
        'diagnostic',
        'model settings',
        `${item.provider}/${item.model}${item.reasoning === null ? '' : ` · ${item.reasoning}`}`,
        'stable',
      )
    case 'compaction':
      return itemModel(`content:${turnId}:${item.id}`, 'diagnostic', 'compaction', item.summary, 'stable')
  }
}

function newActiveProjection(events: TurnEventEnvelope[]) {
  return { events, request: undefined as string | undefined, ended: false, items: [] as TimelineItemModel[], positions: new Map<string, number>(),
    toolFacts: new Map<string, { name: string; args: unknown; startedAt: string | null }>() }
}
let activeProjection = newActiveProjection([])

function reduceActive(
  events: TurnEventEnvelope[],
  activeStartedAt: string | null,
  now: number,
): TimelineItemModel[] {
  const previous = activeProjection.events
  const appended = previous.length <= events.length && previous[0] === events[0]
    && previous.at(-1) === events[previous.length - 1]
  const start = appended ? previous.length : 0
  if (!appended) activeProjection = newActiveProjection(events)
  activeProjection.events = events
  const { items, positions, toolFacts } = activeProjection
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
        : withDetail({ ...item, body: previous.body + item.body }, previous.detail + item.detail)
      : item
  }
  for (let eventIndex = start; eventIndex < events.length; eventIndex += 1) {
    const event = events[eventIndex]
    const params = event.params
    const turnId = stringAt(params, 'turnId') || objectStringAt(params, 'turn', 'turnId') || 'active'
    switch (event.method) {
      case 'turn/started':
        upsert(itemModel(`content:${turnId}:user`, 'user', '你', stringAt(params, 'input'), 'stable'))
        break
      case 'item/started': {
        const itemId = objectStringAt(params, 'item', 'itemId') || `item-${eventIndex}`
        upsert(itemModel(
          `content:${turnId}:${itemId}`,
          'unknown',
          '项目已开始',
          itemId,
          'running',
          [payloadSection(params)],
        ))
        break
      }
      case 'item/agentMessage/delta': {
        const itemId = objectStringAt(params, 'item', 'itemId') || 'assistant'
        upsert(itemModel(`content:${turnId}:${itemId}`, 'assistant', 'Singularity', stringAt(params, 'delta'), 'running'), true)
        break
      }
      case 'item/agentThinking/delta':
      case 'item/agentThinking': {
        const itemId = objectStringAt(params, 'item', 'itemId') || `thinking-${eventIndex}`
        const streaming = event.method.endsWith('/delta')
        upsert(itemModel(
          `content:${turnId}:${itemId}`,
          'thinking',
          'thinking',
          stringAt(params, streaming ? 'delta' : 'text'),
          streaming ? 'running' : 'completed',
        ), streaming)
        break
      }
      case 'tool/execution/start':
      case 'tool/execution/update':
      case 'tool/execution/end': {
        const callId = stringAt(params, 'toolCallId') || `tool-${eventIndex}`
        const name = stringAt(params, 'toolName') || 'tool'
        const key = `content:${turnId}:${callId}`
        const args = params.args ?? toolFacts.get(key)?.args ?? {}
        const startedAt = toolFacts.get(key)?.startedAt ?? (stringAt(params, 'startedAt') || activeStartedAt)
        if (event.method === 'tool/execution/start' || positions.get(key) === undefined) {
          toolFacts.set(key, { name, args, startedAt })
          upsert({ ...withTiming(toolItem(key, name, args, 'running'), startedAt, elapsedDuration(startedAt, now)), toolRequest: activeProjection.request })
          if (event.method !== 'tool/execution/end') break
        }
        const position = positions.get(key)
        if (position === undefined) break
        const result = params.result as { content?: Array<{ text?: string }>; isError?: boolean } | undefined
        const output = event.method === 'tool/execution/update'
          ? stringAt(params, 'partialResult')
          : result?.content?.map((part) => part.text ?? '').join('\n') ?? JSON.stringify(result ?? {}, null, 2)
        items[position] = finishTool(
          items[position],
          name,
          args,
          output,
          result?.isError ?? false,
          numberAt(params, 'durationMs') ?? elapsedDuration(startedAt, now),
          event.method === 'tool/execution/end' ? 'completed' : 'running',
        )
        break
      }
      case 'item/completed':
      case 'item/failed': {
        const itemId = objectStringAt(params, 'item', 'itemId') || `item-${eventIndex}`
        const key = `content:${turnId}:${itemId}`
        const position = positions.get(key)
        const failed = event.method === 'item/failed'
        const error = stringAt(params, 'error')
        if (position === undefined) {
          upsert(itemModel(
            key,
            'unknown',
            failed ? '项目失败' : '项目已完成',
            failed ? error : itemId,
            failed ? 'failed' : 'completed',
            [payloadSection(params)],
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
      case 'provider/attempt': {
        activeProjection.request = `request:${turnId}:${String(params.modelTurnOrdinal)}:${String(params.attempt)}`
        break
      }
      case 'turn/completed': {
        activeProjection.ended = true
        const status = objectStringAt(params, 'turn', 'status') as TurnStatus
        const position = positions.get(`content:${turnId}:turn`)
        if (position !== undefined) items[position] = { ...items[position], status: status === 'running' ? 'running' : status }
        if (status === 'interrupted') upsert(stoppedItem(`content:${turnId}:terminal`))
        break
      }
      case 'turn/error':
        activeProjection.ended = true
        break
      default: {
        const itemId = objectStringAt(params, 'item', 'itemId') || stringAt(params, 'itemId')
        upsert(itemModel(
          `content:${turnId}:unknown:${itemId || `${event.method}:${eventIndex}`}`,
          'unknown',
          event.method,
          itemId === '' ? '收到当前版本尚未识别的运行事件' : itemId,
          'running',
          [payloadSection(params)],
        ))
      }
    }
  }
  return items.map(item => item.toolRequest === undefined ? item : {
    ...item, requestRunning: !activeProjection.ended && item.toolRequest === activeProjection.request,
  })
}

export interface ToolGroupModel {
  key: string
  tools: TimelineItemModel[]
}

/** Request observations bound batches even when the model emits no thinking. */
export function groupTimelineTools(items: TimelineItemModel[]): Array<TimelineItemModel | ToolGroupModel> {
  const rows: Array<TimelineItemModel | ToolGroupModel> = []
  for (const item of items) {
    if (item.toolRequest && (item.kind === 'tool' || item.kind === 'diff')) {
      const previous = rows.at(-1)
      if (previous && 'tools' in previous && previous.tools[0].toolRequest === item.toolRequest) previous.tools.push(item)
      else rows.push({ key: `tools:${item.key}`, tools: [item] })
    } else rows.push(item)
  }
  return rows
}

function toolItem(
  key: string,
  name: string,
  args: unknown,
  status: TimelineItemModel['status'],
): TimelineItemModel {
  const path = pathFromArgs(args)
  const body = toolSummary(name, args)
  const sections: TimelineSection[] = [{ label: '参数', content: JSON.stringify(args, null, 2), kind: 'json' }]
  return {
    ...itemModel(key, isDiffTool(name) ? 'diff' : 'tool', name, body, status, sections),
    filePath: path,
  }
}

function finishTool(
  item: TimelineItemModel,
  name: string,
  args: unknown,
  output: string,
  isError: boolean,
  durationMs: number | null,
  completedStatus: 'running' | 'completed' = 'completed',
): TimelineItemModel {
  const diff = !isError && isDiffTool(name) ? extractUnifiedDiff(output) : ''
  const path = pathFromArgs(args)
  const stats = diffStats(diff)
  const outputSection: TimelineSection = {
    label: isError ? '错误' : '输出',
    content: output,
    kind: isError ? 'error' : 'code',
  }
  const sections = item.sections.filter((section) => section.label !== '输出' && section.label !== '错误' && section.label !== '变更')
  if (diff !== '') sections.push({ label: '变更', content: diff, kind: 'diff' })
  if (output !== '') sections.push(outputSection)
  const detail = sections.map((section) => `${section.label}\n${section.content}`).join('\n\n')
  const summary = isError ? firstLine(output) : path !== null && diff !== ''
    ? path
    : item.body || firstLine(output)
  return {
    ...item,
    kind: diff === '' ? item.kind : 'diff',
    body: summary,
    detail,
    status: isError ? 'failed' : completedStatus,
    filePath: path ?? item.filePath,
    addedLines: stats.added,
    removedLines: stats.removed,
    durationMs,
    sections,
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
  const detail = sections.map((section) => `${section.label}\n${section.content}`).join('\n\n') || body
  return {
    key,
    kind,
    title,
    body,
    detail,
    status,
    filePath: null,
    addedLines: 0,
    removedLines: 0,
    startedAt: null,
    durationMs: null,
    sections,
  }
}

function withDetail(item: TimelineItemModel, detail: string): TimelineItemModel {
  return { ...item, detail, sections: [{ label: '内容', content: detail, kind: 'text' }] }
}

function withTiming(item: TimelineItemModel, startedAt: string | null, durationMs: number | null): TimelineItemModel {
  return { ...item, startedAt, durationMs }
}

function payloadSection(params: Record<string, unknown>): TimelineSection {
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

function extractUnifiedDiff(output: string): string {
  const header = output.search(/^--- /m)
  if (header < 0) return ''
  const candidate = output.slice(header).trimEnd()
  return /^--- .*\n\+\+\+ .*\n@@ /m.test(candidate) ? candidate : ''
}


function diffStats(diff: string): { added: number; removed: number } {
  let added = 0
  let removed = 0
  for (const line of diff.split(/\r?\n/)) {
    if (line.startsWith('+++') || line.startsWith('---')) continue
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

function stringValue(value: unknown): string {
  return typeof value === 'string' ? value : ''
}

function stringAt(value: Record<string, unknown>, key: string): string {
  return typeof value[key] === 'string' ? value[key] : ''
}

function numberAt(value: Record<string, unknown>, key: string): number | null {
  return typeof value[key] === 'number' && Number.isFinite(value[key]) ? value[key] : null
}

function objectStringAt(value: Record<string, unknown>, objectKey: string, key: string): string {
  const nested = value[objectKey]
  return typeof nested === 'object' && nested !== null && typeof (nested as Record<string, unknown>)[key] === 'string'
    ? (nested as Record<string, string>)[key]
    : ''
}
