import type * as Wire from '../src/protocol.generated'
import type { RpcMethod, RpcParams, RpcResult, TurnEventEnvelope } from '../src/protocol'
import { protocolVersion } from '../src/protocol'

export const startedAt = '2026-09-05T00:00:00Z'
export const model = (overrides: Partial<Wire.ModelConfigurationInput> = {}): Wire.ModelConfigurationInput => ({
  modelId: 'm', displayName: null, apiProtocol: 'chat', maxContextTokens: null, maxOutputTokens: null,
  reasoningVariants: [], defaultVariant: null, thinkingWireFormat: null, chatOutputTokensField: null, ...overrides,
})
export const usage = (overrides: Partial<Wire.SessionModelUsage> = {}): Wire.SessionModelUsage => ({
  inputTokens: 0, cachedInputTokens: 0, outputTokens: 0, generationMs: 0, usagePresent: false, usageComplete: true,
  ...overrides,
})
export const summary = (overrides: Partial<Wire.ThreadSummary> = {}): Wire.ThreadSummary => ({
  threadId: 's', cwd: '/workspace', createdAt: startedAt, updatedAt: startedAt,
  title: null, model: null, status: 'running', manuallyStopped: false, turnCount: 1, usage: usage(),
  ...overrides,
})
export const runtime = (overrides: Partial<Wire.SessionRuntime> = {}): Wire.SessionRuntime => ({
  sessionRevision: 0, phase: 'running', selector: null, modelContextWindow: null, pendingControls: [],
  activeCompaction: null, terminal: null,
  activeTurn: { turnId: 't', startedAt },
  ...overrides,
})
export const liveRuntime = (overrides: Partial<Wire.SessionRuntime> = {}): Wire.SessionRuntime => ({
  sessionRevision: 0, phase: 'running', selector: null, modelContextWindow: null, pendingControls: [],
  activeCompaction: null, terminal: null,
  activeTurn: { turnId: 't', startedAt },
  ...overrides,
})
export const session = (overrides: Partial<Wire.SessionReadResult> = {}): Wire.SessionReadResult => ({
  history: { summary: summary(), turns: [], nextCursor: null },
  runtime: runtime(), activeEvents: [], ...overrides,
})
export function historyPage(first: number, last: number, total = last): Wire.SessionReadResult {
  const head = summary({ turnCount: total })
  return session({
    history: { summary: head,
      turns: Array.from({ length: last - first + 1 }, (_, offset) => ({ turnId: `t${first + offset}`, status: 'completed', items: [] })),
      nextCursor: first === 1 ? null : `turn:t${first}`,
    },
    runtime: runtime({ sessionRevision: total, phase: 'idle', activeTurn: null }),
  })
}
export const bootstrap = (overrides: Partial<Wire.WorkbenchBootstrap> = {}): Wire.WorkbenchBootstrap => ({
  generation: 'g', revision: 0, sessionPhases: { s: 'running' },
  workspaces: [{ workspaceId: 'w', name: 'Workspace', root: '/workspace' }],
  sessionsByWorkspace: { w: [summary()] },
  modelCatalog: { configuration: 'missing', message: null, defaultSelector: null, providers: [] },
  ...overrides,
})
export const observation = (overrides: Partial<Wire.RequestObservation> = {}): Wire.RequestObservation => ({
  requestId: 'request', purpose: 'generation', ordinal: 1, attempt: 1, provider: 'p', model: 'm',
  status: 'started', durationMs: 0, inputTokens: null, outputTokens: null, cachedInputTokens: null, error: null,
  ...overrides,
})
export const requestSnapshot = (overrides: Omit<Partial<Wire.ModelRequestSnapshot>, 'modelPreferences'> & {
  modelPreferences?: Partial<Wire.RequestPreferences>
} = {}): Wire.ModelRequestSnapshot => ({
  messages: [], tools: [], ...overrides,
  modelPreferences: { maxOutputTokens: null, ...overrides.modelPreferences },
})
export const control = (overrides: Partial<Wire.ControlSnapshot> = {}): Wire.ControlSnapshot => ({
  controlId: 'control', turnId: 't', channel: 'follow_up', sequence: 1, text: 'queued', disposition: 'pending',
  ...overrides,
})

type EventMethod = TurnEventEnvelope['method']
type Params<M extends EventMethod> = Extract<TurnEventEnvelope, { method: M }>['params']
const ids = { threadId: 's', turnId: 't' }
const item = { ...ids, item: { itemId: 'a' } }
const tool = { ...ids, item: { itemId: 'tool' } }
const defaults = {
  'turn/started': { turn: { ...ids, status: 'running' }, startedAt },
  'turn/userMessage': { ...ids, item: { itemId: 'user-entry:text:0' }, text: 'hello' },
  'turn/controlChanged': { control: control() },
  'item/started': item,
  'item/completed': item,
  'item/failed': { ...item, error: 'failed' },
  'item/agentMessage/delta': { ...item, delta: '' },
  'item/agentThinking/delta': { ...item, delta: '' },
  'tool/execution/start': { ...tool, toolName: 'bash', args: {}, startedAt },
  'tool/execution/update': { ...tool, partialResult: '' },
  'tool/execution/end': { ...tool, output: '', isError: false },
  'agent/diagnostic': { ...ids, severity: 'warning', code: 'test', message: 'diagnostic' },
  'provider/attempt': {
    ...ids,
    observation: observation(),
    protocol: 'chat', retryAfterMs: null, retryAfterSource: null,
  },
  'turn/completed': { turn: { ...ids, status: 'completed' } },
  'turn/error': { ...ids, error: { stage: 'agent_loop', cause: 'internal', message: 'failed' } },
} satisfies { [M in EventMethod]: Params<M> }

type EventOverrides<M extends EventMethod> = Omit<Partial<Params<M>>, 'turn'> & (
  Params<M> extends { turn: Wire.Turn } ? { turn?: Partial<Wire.Turn> } : {}
)
/** 测试只声明自己使用的事实；默认值提供当前完整的 wire 契约。 */
export function event<M extends EventMethod>(value: {
  method: M; params?: EventOverrides<M>; sessionRevision?: number
}): Extract<TurnEventEnvelope, { method: M }> {
  const base: Params<EventMethod> = defaults[value.method]
  const params = { ...base, ...value.params }
  if ('turn' in base) Object.assign(params, { turn: { ...base.turn, ...('turn' in params ? params.turn : {}) } })
  // 泛型键查找在 TS 中丢失关联；上面的默认值与覆盖值都按 method 校验。
  return { method: value.method, params, sessionRevision: value.sessionRevision ?? 1 } as Extract<TurnEventEnvelope, { method: M }>
}
export const readyFrame = (): Wire.StreamEnvelope => ({ version: protocolVersion, generation: 'g', revision: 0, type: 'ready', payload: {} })
export const frame = (revision: number, delta: string): Extract<Wire.StreamEnvelope, { type: 'turn_event' }> => ({
  version: protocolVersion, generation: 'g', revision, sessionId: 's', type: 'turn_event',
  payload: event({ method: 'item/agentMessage/delta', sessionRevision: revision, params: { delta } }),
})
export const sessionFrame = (revision: number, payload: Wire.SessionRuntime, sessionId = 's'): Wire.StreamEnvelope => {
  const { activeTurn, ...runtime } = payload
  return { version: protocolVersion, generation: 'g', revision, sessionId, type: 'session_changed',
    payload: { ...runtime, activeTurn: activeTurn && { turnId: activeTurn.turnId, startedAt: activeTurn.startedAt } } }
}
export const bootstrapFrame = (revision: number, payload: Wire.WorkbenchBootstrap): Wire.StreamEnvelope => ({
  version: protocolVersion, generation: 'g', revision, type: 'workbench_changed', payload,
})

// 负向编译用例在生产与 fake transport 共用的同一 API 上保护关联关系。
export function checkRpcTypes(rpc: <M extends RpcMethod>(method: M, params: RpcParams<M>) => Promise<RpcResult<M>>) {
  const read: Promise<Wire.SessionReadResult> = rpc('session.read', { workspaceId: 'w', sessionId: 's', limit: 40 })
  // @ts-expect-error 未知 method 会被拒绝。
  rpc('session.unknown', {})
  // @ts-expect-error 提交时必须提供 text。
  rpc('session.submit', { workspaceId: 'w', sessionId: 's' })
  // @ts-expect-error 参数属于另一个 method。
  rpc('directory.list', { workspaceId: 'w' })
  // @ts-expect-error 空 method 参数拒绝未知字段。
  rpc('directory.pick', { path: '/' })
  // @ts-expect-error 调用方不能自行指定 result 类型。
  const wrong: Promise<Wire.ActionReceipt> = rpc('session.read', { workspaceId: 'w', sessionId: 's', limit: 40 })
  return { read, wrong }
}
