import { compactionTitle } from './copy'
import type { ExecutionTurn, FactStatus, SessionView } from './execution'
import type { ModelRequestSnapshot, RequestObservation, TurnErrorDetail } from './protocol'

type TrajectoryKind = 'system' | 'user' | 'assistant' | 'tool' | 'compaction' | 'settings' | 'event'
export interface TrajectoryEntry {
  id: string
  kind: TrajectoryKind
  title: string
  text: string
  thinking: string
  input?: unknown
  schema?: unknown
  request?: RequestObservation
  prompt?: ModelRequestSnapshot
  previousPrompt?: ModelRequestSnapshot
  /** 失败终态的结构化错误详情 */
  error?: TurnErrorDetail
  duration: number | null
  startedAt: string | null
  status: FactStatus
}
interface TrajectoryTurn { id: string; title: string; entries: TrajectoryEntry[] }

function entry(id: string, kind: TrajectoryKind, title: string, text = ''): TrajectoryEntry {
  return { id, kind, title, text, thinking: '', duration: null, startedAt: null, status: 'stable' }
}


const projections = new WeakMap<ExecutionTurn, { previous: ModelRequestSnapshot | undefined; next: ModelRequestSnapshot | undefined; turn: TrajectoryTurn }>()
const requestTitle = (r: RequestObservation) => `${r.purpose === 'compaction' ? '摘要请求' : '请求'} #${r.attempt}`

/** 构建右侧轨迹面板所需的分轮次详细执行记录 */
export function buildTrajectory(session: SessionView | null): TrajectoryTurn[] {
  if (!session) return []
  let previousPrompt: ModelRequestSnapshot | undefined
  let ordinal = 0
  const turns = [...session.facts.history, ...session.facts.active].map(turn => {
    const standalone = turn.id === null && session.facts.active.includes(turn)
    const title = standalone ? '上下文压缩' : turn.id === null ? '会话设置' : `第 ${++ordinal} 轮`
    const cached = projections.get(turn)
    if (cached && cached.previous === previousPrompt && cached.turn.title === title) {
      previousPrompt = cached.next
      return cached.turn
    }
    const previous = previousPrompt
    const entries: TrajectoryEntry[] = []
    for (const fact of turn.items) {
      if (fact.kind === 'assistant' || fact.kind === 'thinking') {
        const parent = fact.requestId ? entries.find(item => item.id === fact.requestId) : undefined
        if (parent) {
          if (fact.kind === 'assistant') parent.text += (parent.text ? '\n\n' : '') + fact.text
          else parent.thinking += (parent.thinking ? '\n\n' : '') + fact.text
          continue
        }
      }
      let item: TrajectoryEntry
      if (fact.kind === 'request') {
        const r = fact.observation
        item = { ...entry(fact.id, 'assistant', requestTitle(r)), request: r, duration: r.status === 'started' ? null : r.durationMs }
        const prompt = r.requestHead
        if (prompt && prompt.definitionsId !== previousPrompt?.definitionsId) {
          entries.push({ ...entry(`system-${fact.id}`, 'system', previousPrompt ? 'Harness 指令更新' : '初始 Harness 指令', instructionText(prompt)), prompt, previousPrompt })
        }
        if (prompt) previousPrompt = prompt
      } else if (fact.kind === 'tool') {
        item = { ...entry(fact.id, 'tool', fact.name, fact.output),
          input: fact.args, schema: previousPrompt?.tools.find(tool => tool.name === fact.name), duration: fact.duration ?? null }
      } else if (fact.kind === 'thinking') {
        item = { ...entry(fact.id, 'assistant', '助手'), thinking: fact.text }
      } else if (fact.kind === 'settings') {
        item = entry(fact.id, 'settings', '模型设置', settingsText(fact))
      } else {
        const titles = { user: '用户', assistant: '助手', compaction: compactionTitle, event: '运行信息' }
        const kind = fact.kind === 'compaction_result' ? 'compaction' : fact.kind
        item = entry(fact.id, kind, titles[kind], fact.text)
      }
      entries.push({ ...item, status: fact.status, startedAt: fact.startedAt })
    }
    // 追加回合失败错误条目
    if (turn.error) entries.push({ ...entry(`turn-error:${turn.id ?? 'leading'}`, 'event', '运行错误', turn.error.message),
      status: 'error', error: turn.error })
    const projected = { id: turn.id ?? (standalone ? 'compaction' : 'leading'), title, entries }
    projections.set(turn, { previous, next: previousPrompt, turn: projected })
    return projected
  })
  // 补齐未持久化到历史中的运行时失败（如独立的上下文压缩失败）
  const terminal = session.runtime.terminal
  if (!session.facts.active.length && terminal?.status === 'failed' && terminal.message
    && (terminal.source === 'compaction' || !turns.at(-1)?.entries.some(entry => entry.error))) {
    const failure = { ...entry('runtime-error', 'event', terminal.source === 'compaction' ? '压缩失败' : '运行错误', terminal.message), status: 'error' as const }
    const last = turns.at(-1)
    if (last) turns[turns.length - 1] = { ...last, entries: [...last.entries, failure] }
    else turns.push({ id: 'runtime', title: `第 ${++ordinal} 轮`, entries: [failure] } )
  }
  return turns.filter(turn => turn.entries.length)
}

/** 格式化显示模型配置信息 */
function settingsText(settings: { provider: string; model: string; reasoning: string | null }): string {
  return `${settings.provider}/${settings.model}${settings.reasoning ? ` · ${settings.reasoning}` : ''}`
}

export function instructionText(request: ModelRequestSnapshot): string {
  return request.messages.map(message => `[${message.role}]\n${message.content}`).join('\n\n')
}
