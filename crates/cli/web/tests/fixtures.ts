import type * as Wire from '../src/protocol.generated'
import type { RpcMethod, RpcParams, RpcResult, TurnEventEnvelope } from '../src/protocol'
import { protocolVersion } from '../src/protocol'

export const startedAt = '2026-09-05T00:00:00Z'
export const model = (overrides: Partial<Wire.RedactedModel> = {}): Wire.RedactedModel => ({
  modelId: 'm', displayName: null, apiProtocol: 'chat', maxContextTokens: null, maxOutputTokens: null,
  reasoningVariants: [], defaultVariant: null, thinkingWireFormat: null, ...overrides,
})
export const summary = (overrides: Partial<Wire.ThreadSummary> = {}): Wire.ThreadSummary => ({
  threadId: 's', cwd: '/workspace', createdAt: startedAt, updatedAt: startedAt,
  title: null, model: null, status: 'running', manuallyStopped: false, turnCount: 1, totalTokens: 0,
  ...overrides,
})
export const runtime = (overrides: Partial<Wire.SessionSnapshot> = {}): Wire.SessionSnapshot => ({
  sessionRevision: 0, phase: 'running', selector: null, controls: [], pendingControls: [],
  activeCompaction: null, terminal: null,
  activeTurn: { turnId: 't', events: [], startedAt },
  ...overrides,
})
export const session = (overrides: Partial<Wire.SessionReadResult> = {}): Wire.SessionReadResult => ({
  summary: summary(), history: { summary: summary(), compactionSummary: null, turns: [], nextCursor: null },
  runtime: runtime(), ...overrides,
})
export function historyPage(first: number, last: number, total = last): Wire.SessionReadResult {
  const head = summary({ turnCount: total })
  return session({ summary: head,
    history: { summary: head, compactionSummary: null,
      turns: Array.from({ length: last - first + 1 }, (_, offset) => ({ turnId: `t${first + offset}`, status: 'completed', items: [] })),
      nextCursor: first === 1 ? null : `turn:t${first}`,
    },
    runtime: runtime({ sessionRevision: total, phase: 'idle', activeTurn: null }),
  })
}
export const bootstrap = (overrides: Partial<Wire.WorkbenchBootstrap> = {}): Wire.WorkbenchBootstrap => ({
  generation: 'g', revision: 0, endpoint: { authority: '127.0.0.1:3081' }, sessionPhases: { s: 'running' },
  workspaces: [{ workspaceId: 'w', name: 'Workspace', root: '/workspace' }],
  sessionsByWorkspace: { w: [summary()] },
  modelCatalog: { configuration: 'missing', message: null, defaultSelector: null, providers: [], presets: [] },
  ...overrides,
})
export const receipt = (overrides: Partial<Wire.ActionReceipt> = {}): Wire.ActionReceipt => ({
  requestId: 'request', accepted: true, generation: 'g', revision: 0, sessionId: 's', turnId: 't', control: null,
  ...overrides,
})
export const observation = (overrides: Partial<Wire.RequestObservation> = {}): Wire.RequestObservation => ({
  requestId: 'request', purpose: 'generation', ordinal: 1, attempt: 1, provider: 'p', model: 'm',
  status: 'started', durationMs: 0, inputTokens: null, outputTokens: null, cachedInputTokens: null, error: null,
  ...overrides,
})
export const requestSnapshot = (overrides: Omit<Partial<Wire.ModelRequestSnapshot>, 'model_preferences'> & {
  model_preferences?: Partial<Wire.RequestPreferences>
} = {}): Wire.ModelRequestSnapshot => ({
  request_id: 'request', messages: [], tools: [], ...overrides,
  model_preferences: { model_name: null, max_output_tokens: null, ...overrides.model_preferences },
})
export const control = (overrides: Partial<Wire.ControlSnapshot> = {}): Wire.ControlSnapshot => ({
  controlId: 'control', turnId: 't', channel: 'follow_up', sequence: 1, text: 'queued', disposition: 'pending',
  ...overrides,
})

type EventMethod = TurnEventEnvelope['method']
type Params<M extends EventMethod> = Extract<TurnEventEnvelope, { method: M }>['params']
const ids = { threadId: 's', turnId: 't' }
const item = { ...ids, item: { itemId: 'a' } }
const tool = { ...ids, toolCallId: 'tool', toolName: 'bash' }
const defaults = {
  'turn/started': { turn: { ...ids, status: 'running' }, input: 'hello', startedAt },
  'item/started': item,
  'item/completed': item,
  'item/failed': { ...item, error: 'failed' },
  'item/agentMessage/delta': { ...item, delta: '' },
  'item/agentThinking/delta': { ...item, delta: '' },
  'item/agentThinking': { ...item, text: '' },
  'tool/execution/start': { ...tool, args: {}, startedAt },
  'tool/execution/update': { ...tool, args: {}, partialResult: '' },
  'tool/execution/end': { ...tool, result: { content: [{ type: 'text', text: '' }], isError: false } },
  'agent/diagnostic': { ...ids, severity: 'warning', code: 'test', message: 'diagnostic' },
  'provider/attempt': {
    ...ids, requestId: 'request', purpose: 'generation', attempt: 1, modelTurnOrdinal: 1,
    provider: 'p', model: 'm', protocol: 'chat', status: 'started', attemptDurationMs: null,
    inputTokens: null, outputTokens: null, cachedInputTokens: null, errorCategory: null,
    diagnosticCode: null, retryAfterMs: null, retryAfterSource: null,
  },
  'turn/completed': { turn: { ...ids, status: 'completed' } },
  'turn/error': { ...ids, error: { stage: 'agent_loop', cause: 'internal', message: 'failed' } },
} satisfies { [M in EventMethod]: Params<M> }

type EventOverrides<M extends EventMethod> = Omit<Partial<Params<M>>, 'turn'> & (
  Params<M> extends { turn: Wire.Turn } ? { turn?: Partial<Wire.Turn> } : {}
)
/** Tests specify the facts they exercise; defaults supply the complete current wire contract. */
export function event<M extends EventMethod>(value: {
  method: M; params?: EventOverrides<M>; sessionRevision?: number
}): Extract<TurnEventEnvelope, { method: M }> {
  const base: Params<EventMethod> = defaults[value.method]
  const params = { ...base, ...value.params }
  if (value.method === 'provider/attempt' && 'modelTurnOrdinal' in params && !Object.hasOwn(value.params ?? {}, 'requestId')) {
    Object.assign(params, { requestId: `request-${params.modelTurnOrdinal}-${'attempt' in params ? params.attempt : 1}` })
  }
  if ('turn' in base) Object.assign(params, { turn: { ...base.turn, ...('turn' in params ? params.turn : {}) } })
  // Generic key lookup loses its correlation in TS; both the defaults and overrides above are checked by method.
  return { method: value.method, params, sessionRevision: value.sessionRevision ?? 1 } as Extract<TurnEventEnvelope, { method: M }>
}
export const readyFrame = (): Wire.StreamEnvelope => ({ version: protocolVersion, generation: 'g', revision: 0, type: 'ready', payload: {} })
export const frame = (revision: number, delta: string): Extract<Wire.StreamEnvelope, { type: 'turn_event' }> => ({
  version: protocolVersion, generation: 'g', revision, sessionId: 's', type: 'turn_event',
  payload: event({ method: 'item/agentMessage/delta', sessionRevision: revision, params: { delta } }),
})
export const sessionFrame = (revision: number, payload: Wire.SessionSnapshot, sessionId = 's'): Wire.StreamEnvelope => ({
  version: protocolVersion, generation: 'g', revision, sessionId, type: 'session_changed', payload,
})
export const bootstrapFrame = (revision: number, payload: Wire.WorkbenchBootstrap): Wire.StreamEnvelope => ({
  version: protocolVersion, generation: 'g', revision, type: 'workbench_changed', payload,
})

// Negative compile cases protect association at the same API used by production and the fake transport.
export function checkRpcTypes(rpc: <M extends RpcMethod>(method: M, params: RpcParams<M>) => Promise<RpcResult<M>>) {
  const read: Promise<Wire.SessionReadResult> = rpc('session.read', { workspaceId: 'w', sessionId: 's', limit: 40 })
  // @ts-expect-error Unknown methods are rejected.
  rpc('session.unknown', {})
  // @ts-expect-error Text is required for submission.
  rpc('session.submit', { workspaceId: 'w', sessionId: 's' })
  // @ts-expect-error Parameters belong to a different method.
  rpc('directory.list', { workspaceId: 'w' })
  // @ts-expect-error Empty method params reject unknown fields.
  rpc('directory.pick', { path: '/' })
  // @ts-expect-error Callers cannot invent a result type.
  const wrong: Promise<Wire.ActionReceipt> = rpc('session.read', { workspaceId: 'w', sessionId: 's', limit: 40 })
  return { read, wrong }
}
