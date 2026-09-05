import { turnStatusText } from './copy'
import type {
  ControlSnapshot,
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
  hiddenLines: number
  filePath: string | null
  addedLines: number
  removedLines: number
  startedAt: string | null
  durationMs: number | null
  sections: TimelineSection[]
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
  const controls = session.runtime.controls.map(controlItem)
  const projectedTerminal = active.findLast((item) => item.kind === 'terminal')
    ?? (activeTurn === null ? stable.findLast((item) => item.kind === 'terminal') : undefined)
  const terminal = session.runtime.terminal === null
    || projectedTerminal?.status === session.runtime.terminal.status
    ? []
    : [terminalItem(session.runtime.terminal.status, session.runtime.terminal.message)]
  return [...stable, ...active, ...controls, ...terminal]
}

function projectHistoryTurn(
  items: HistoryItem[],
  turnId: string,
  status: TurnStatus | null,
): TimelineItemModel[] {
  const projected: TimelineItemModel[] = []
  const tools = new Map<string, { position: number; name: string; args: unknown }>()
  for (const item of items) {
    if (item.type === 'tool_call') {
      tools.set(item.id, { position: projected.length, name: item.name, args: item.args })
      projected.push(toolItem(`history:${turnId}:${item.id}`, item.name, item.args, 'stable'))
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
          null,
        )
        continue
      }
    }
    projected.push(historyItem(item, turnId))
  }
  if (status !== null) projected.push(terminalItem(status, null, `history:${turnId}:terminal`))
  return projected
}

function historyItem(item: HistoryItem, turnId: string): TimelineItemModel {
  switch (item.type) {
    case 'message':
      return itemModel(
        `history:${turnId}:${item.id}`,
        item.role === 'user' ? 'user' : 'assistant',
        item.role === 'user' ? '你' : 'Singularity',
        item.text,
        'stable',
      )
    case 'thinking':
      return itemModel(`history:${turnId}:${item.id}`, 'thinking', '思考过程', item.text, 'stable')
    case 'tool_call':
      return toolItem(`history:${turnId}:${item.id}`, item.name, item.args, 'stable')
    case 'tool_result':
      return itemModel(
        `history:${turnId}:${item.id}`,
        'tool',
        item.isError ? '工具执行失败' : '工具输出',
        firstLine(item.output),
        item.isError ? 'failed' : 'stable',
        [{ label: item.isError ? '错误' : '输出', content: item.output, kind: item.isError ? 'error' : 'code' }],
      )
    case 'settings':
      return itemModel(
        `history:${turnId}:${item.id}`,
        'diagnostic',
        '模型设置已更新',
        `${item.provider}/${item.model}${item.reasoning === null ? '' : ` · ${item.reasoning}`}`,
        'stable',
      )
    case 'compaction':
      return itemModel(`history:${turnId}:${item.id}`, 'diagnostic', '上下文已压缩', item.summary, 'stable')
  }
}

function newActiveProjection(events: TurnEventEnvelope[]) {
  return { events, items: [] as TimelineItemModel[], positions: new Map<string, number>(),
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
        upsert(itemModel(`active:${turnId}:user`, 'user', '你', stringAt(params, 'input'), 'stable'))
        upsert(withTiming(
          itemModel(`active:${turnId}:turn`, 'diagnostic', '任务已开始', '正在项目中执行', 'running'),
          stringAt(params, 'startedAt') || activeStartedAt,
          null,
        ))
        break
      case 'item/started': {
        const itemId = objectStringAt(params, 'item', 'itemId') || `item-${eventIndex}`
        upsert(itemModel(
          `active:${turnId}:${itemId}`,
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
        upsert(itemModel(`active:${turnId}:${itemId}`, 'assistant', 'Singularity', stringAt(params, 'delta'), 'running'), true)
        break
      }
      case 'item/agentThinking':
        upsert(itemModel(
          `active:${turnId}:thinking:${eventIndex}`,
          'thinking',
          '思考过程',
          stringAt(params, 'text'),
          'running',
        ))
        break
      case 'tool/execution/start':
      case 'tool/execution/update':
      case 'tool/execution/end': {
        const callId = stringAt(params, 'toolCallId') || `tool-${eventIndex}`
        const name = stringAt(params, 'toolName') || 'tool'
        const key = `active:${turnId}:${callId}`
        const args = params.args ?? toolFacts.get(key)?.args ?? {}
        const startedAt = toolFacts.get(key)?.startedAt ?? (stringAt(params, 'startedAt') || activeStartedAt)
        if (event.method === 'tool/execution/start' || positions.get(key) === undefined) {
          toolFacts.set(key, { name, args, startedAt })
          upsert(withTiming(toolItem(key, name, args, 'running'), startedAt, elapsedDuration(startedAt, now)))
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
          elapsedDuration(startedAt, now),
          event.method === 'tool/execution/end' ? 'completed' : 'running',
        )
        break
      }
      case 'item/completed':
      case 'item/failed': {
        const itemId = objectStringAt(params, 'item', 'itemId') || `item-${eventIndex}`
        const key = `active:${turnId}:${itemId}`
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
        upsert(itemModel(
          `active:${turnId}:diagnostic:${eventIndex}`,
          'diagnostic',
          severityLabel(stringAt(params, 'severity')),
          stringAt(params, 'message'),
          stringAt(params, 'severity') === 'error' ? 'failed' : 'running',
          [payloadSection(params)],
        ))
        break
      case 'provider/attempt': {
        const duration = numberAt(params, 'attemptDurationMs')
        upsert(withTiming(itemModel(
          `active:${turnId}:attempt:${String(params.modelTurnOrdinal)}:${String(params.attempt)}`,
          'diagnostic',
          `模型请求 ${String(params.attempt ?? '')}`,
          `${String(params.provider ?? '')} · ${String(params.model ?? '')} · ${attemptStatus(String(params.status ?? ''))}`,
          params.status === 'error' ? 'failed' : params.status === 'ok' ? 'completed' : 'running',
          [payloadSection(params)],
        ), activeStartedAt, duration))
        break
      }
      case 'turn/completed': {
        const status = objectStringAt(params, 'turn', 'status') as TurnStatus
        const position = positions.get(`active:${turnId}:turn`)
        if (position !== undefined) items[position] = { ...items[position], status: status === 'running' ? 'running' : status }
        upsert(terminalItem(status, null, `active:${turnId}:terminal`))
        break
      }
      case 'turn/error':
        upsert(terminalItem('failed', objectStringAt(params, 'error', 'message'), `active:${turnId}:terminal`))
        break
      default: {
        const itemId = objectStringAt(params, 'item', 'itemId') || stringAt(params, 'itemId')
        upsert(itemModel(
          `active:${turnId}:unknown:${itemId || `${event.method}:${eventIndex}`}`,
          'unknown',
          event.method,
          itemId === '' ? '收到当前版本尚未识别的运行事件' : itemId,
          'running',
          [payloadSection(params)],
        ))
      }
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
  const sections: TimelineSection[] = [{ label: '参数', content: JSON.stringify(args, null, 2), kind: 'json' }]
  return {
    ...itemModel(key, isDiffTool(name) ? 'diff' : 'tool', toolTitle(name), body, status, sections),
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
  const summary = path !== null && diff !== ''
    ? `${path} · +${stats.added} −${stats.removed}`
    : item.body || firstLine(output)
  return {
    ...item,
    kind: diff === '' ? item.kind : 'diff',
    body: summary,
    detail,
    status: isError ? 'failed' : completedStatus,
    hiddenLines: hiddenLines(detail),
    filePath: path ?? item.filePath,
    addedLines: stats.added,
    removedLines: stats.removed,
    durationMs,
    sections,
  }
}


function controlItem(control: ControlSnapshot): TimelineItemModel {
  const channel = control.channel === 'follow_up'
    ? '后续消息'
    : control.channel === 'steer'
      ? '即时转向'
      : '停止请求'
  const disposition = {
    pending: '等待处理',
    injected: '已送入当前回合',
    started_as_new_turn: '已作为新回合开始',
    cancelled: '已取消',
  }[control.disposition]
  return itemModel(
    `control:${control.controlId}`,
    'control',
    `${channel} #${control.sequence}`,
    control.text ?? disposition,
    control.disposition === 'pending' ? 'pending' : control.disposition === 'cancelled' ? 'interrupted' : 'completed',
    [{
      label: '投递记录',
      content: JSON.stringify({
        controlId: control.controlId,
        turnId: control.turnId,
        channel: control.channel,
        sequence: control.sequence,
        disposition: control.disposition,
        text: control.text,
      }, null, 2),
      kind: 'json',
    }],
  )
}

function terminalItem(status: TurnStatus, message: string | null, key = `terminal:${status}`): TimelineItemModel {
  return itemModel(
    key,
    'terminal',
    turnStatusText[status],
    message ?? '',
    status === 'running' ? 'running' : status,
    message === null ? [] : [{ label: '信息', content: message, kind: status === 'failed' ? 'error' : 'text' }],
  )
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
    hiddenLines: hiddenLines(detail),
    filePath: null,
    addedLines: 0,
    removedLines: 0,
    startedAt: null,
    durationMs: null,
    sections,
  }
}

function withDetail(item: TimelineItemModel, detail: string): TimelineItemModel {
  return { ...item, detail, hiddenLines: hiddenLines(detail), sections: [{ label: '内容', content: detail, kind: 'text' }] }
}

function withTiming(item: TimelineItemModel, startedAt: string | null, durationMs: number | null): TimelineItemModel {
  return { ...item, startedAt, durationMs }
}

function payloadSection(params: Record<string, unknown>): TimelineSection {
  return { label: '原始事件', content: JSON.stringify(params, null, 2), kind: 'json' }
}

function hiddenLines(text: string): number {
  return Math.max(0, text.split('\n').length - 8)
}

function isDiffTool(name: string): boolean {
  return name === 'edit' || name === 'write'
}

function toolTitle(name: string): string {
  const names: Record<string, string> = {
    read: '读取文件',
    grep: '搜索内容',
    glob: '查找文件',
    bash: '运行命令',
    edit: '修改文件',
    write: '写入文件',
  }
  return names[name] ?? name
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


function severityLabel(severity: string): string {
  return severity === 'error' ? '错误' : severity === 'warning' ? '提醒' : '运行信息'
}

function attemptStatus(status: string): string {
  return ({ started: '正在请求', ok: '完成', error: '失败', cancelled: '已取消' } as Record<string, string>)[status] ?? status
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
