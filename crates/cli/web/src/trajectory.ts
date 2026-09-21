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
  /** 该条目的类型化失败细节；只有失败终态条目携带。 */
  error?: TurnErrorDetail
  duration: number | null
  startedAt: string | null
  status: FactStatus
}
interface TrajectoryTurn { id: string; title: string; entries: TrajectoryEntry[] }

function entry(id: string, kind: TrajectoryKind, title: string, text = ''): TrajectoryEntry {
  return { id, kind, title, text, thinking: '', duration: null, startedAt: null, status: 'stable' }
}


const promptSignatures = new WeakMap<ModelRequestSnapshot, string>()
const projections = new WeakMap<ExecutionTurn, { previous: ModelRequestSnapshot | undefined; next: ModelRequestSnapshot | undefined; turn: TrajectoryTurn }>()
const requestTitle = (r: RequestObservation) => `${r.purpose === 'compaction' ? '摘要请求' : '请求'} #${r.attempt}`

/** 布局与 prompt 对比都消费已解析的执行事实。 */
export function buildTrajectory(session: SessionView | null): TrajectoryTurn[] {
  if (!session) return []
  let previousPrompt: ModelRequestSnapshot | undefined
  let ordinal = 0
  const turns = [...session.facts.history, ...session.facts.active].map(turn => {
    const title = turn.id === null ? '会话设置' : `第 ${++ordinal} 轮`
    const cached = projections.get(turn)
    if (cached && cached.previous === previousPrompt && cached.turn.title === title) {
      previousPrompt = cached.next
      return cached.turn
    }
    const previous = previousPrompt
    const entries: TrajectoryEntry[] = []
    for (const fact of turn.items) {
      if (fact.kind === 'unknown') continue
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
        if (prompt && promptSignature(prompt) !== (previousPrompt && promptSignature(previousPrompt))) {
          entries.push({ ...entry(`system-${fact.id}`, 'system', previousPrompt ? '系统提示词更新' : '初始系统提示词', systemText(prompt)), prompt, previousPrompt })
        }
        if (prompt) previousPrompt = prompt
      } else if (fact.kind === 'tool') {
        item = { ...entry(fact.id, 'tool', fact.name, fact.diff ? `${fact.output}\n\n${fact.diff}` : fact.output),
          input: fact.args, schema: previousPrompt?.tools.find(tool => tool.name === fact.name), duration: fact.duration ?? null }
      } else if (fact.kind === 'thinking') {
        item = { ...entry(fact.id, 'assistant', '助手'), thinking: fact.text }
      } else if (fact.kind === 'settings') {
        item = entry(fact.id, 'settings', '模型设置', settingsText(fact))
      } else {
        const titles = { user: '用户', assistant: '助手', compaction: '上下文压缩', event: '运行信息' }
        item = entry(fact.id, fact.kind, titles[fact.kind], fact.text)
      }
      entries.push({ ...item, status: fact.status, startedAt: fact.startedAt })
    }
    // 失败终态的细节是这一轮的事实：显示层只决定格式，说明正文取 message。
    if (turn.error) entries.push({ ...entry(`turn-error:${turn.id ?? 'leading'}`, 'event', '运行错误', turn.error.message),
      status: 'error', error: turn.error })
    const projected = { id: turn.id ?? 'leading', title, entries }
    projections.set(turn, { previous, next: previousPrompt, turn: projected })
    return projected
  })
  // runtime 终态只承载无法落盘（启动前或存储失效）的失败：已持久化该轮错误时
  // 不再重复一条同样的说明。
  const terminal = session.runtime.terminal
  if (!session.facts.active.length && terminal?.status === 'failed' && terminal.message
    && !turns.at(-1)?.entries.some(entry => entry.error)) {
    const failure = { ...entry('runtime-error', 'event', '运行错误', terminal.message), status: 'error' as const }
    const last = turns.at(-1)
    if (last) turns[turns.length - 1] = { ...last, entries: [...last.entries, failure] }
    else turns.push({ id: 'runtime', title: `第 ${++ordinal} 轮`, entries: [failure] })
  }
  return turns.filter(turn => turn.entries.length)
}

/** settings 保持为结构化事实；显示字符串在此生成。 */
function settingsText(settings: { provider: string; model: string; reasoning: string | null }): string {
  return `${settings.provider}/${settings.model}${settings.reasoning ? ` · ${settings.reasoning}` : ''}`
}

export function systemText(request: ModelRequestSnapshot): string {
  return request.messages.filter(message => message.role === 'system' || message.role === 'developer').map(message => message.content).join('\n\n')
}
function promptSignature(request: ModelRequestSnapshot): string {
  let signature = promptSignatures.get(request)
  if (signature === undefined) {
    signature = JSON.stringify([systemText(request), request.tools])
    promptSignatures.set(request, signature)
  }
  return signature
}
