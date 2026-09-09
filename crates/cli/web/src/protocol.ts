import type { EventSequence } from './eventLog'
export const protocolVersion = 1 as const

export type ConnectionStatus =
  | 'connecting'
  | 'ready'
  | 'recovering'
  | 'forbidden'
  | 'unavailable'

export type SessionPhase = 'idle' | 'reserved' | 'running' | 'compacting' | 'stopping'
export type TurnStatus = 'running' | 'completed' | 'failed' | 'interrupted'
export type DeliveryIntent = 'steer' | 'follow_up'

export interface Workspace {
  workspaceId: string
  name: string
  root: string
}

export interface ThreadSummary {
  threadId: string
  cwd: string
  createdAt: string
  updatedAt: string
  title: string | null
  model: string | null
  status: TurnStatus | null
  manuallyStopped: boolean
  turnCount: number
  totalTokens: number
}

export interface HistoryMessage {
  type: 'message'
  id: string
  role: string
  text: string
}

export interface HistoryThinking {
  type: 'thinking'
  id: string
  text: string
}

export interface HistoryToolCall {
  type: 'tool_call'
  id: string
  name: string
  args: unknown
}

export interface HistoryToolResult {
  type: 'tool_result'
  id: string
  output: string
  isError: boolean
  durationMs?: number
}

export interface HistorySettings {
  type: 'settings'
  id: string
  provider: string
  model: string
  reasoning: string | null
}

export interface HistoryCompaction {
  type: 'compaction'
  id: string
  summary: string
}

export interface RequestObservation {
  ordinal: number; attempt: number; provider: string; model: string
  status: 'started' | 'ok' | 'error' | 'cancelled'; durationMs: number
  inputTokens: number | null; outputTokens: number | null; cachedInputTokens: number | null; error: string | null
  request?: ModelRequestSnapshot
  requestError?: string
}

export interface ModelRequestSnapshot {
  request_id: string
  messages: Array<{ role: string; content: string; tool_call_id: string | null; tool_calls?: unknown[] }>
  tools: Array<{ name: string; description: string; parameters_schema: unknown }>
  model_preferences: { model_name: string | null; max_output_tokens: number | null }
}

export type HistoryItem =
  | { type: 'request'; id: string; timestamp: string; observation: RequestObservation }
  | HistoryMessage
  | HistoryThinking
  | HistoryToolCall
  | HistoryToolResult
  | HistorySettings
  | HistoryCompaction

export interface ThreadTurn {
  turnId: string | null
  status: TurnStatus | null
  items: HistoryItem[]
}

export interface ThreadReadPage {
  summary: ThreadSummary
  compactionSummary: string | null
  turns: ThreadTurn[]
  nextCursor: string | null
}

export interface ControlSnapshot {
  controlId: string
  turnId: string
  channel: 'steer' | 'follow_up' | 'cancel'
  sequence: number
  text: string | null
  disposition: 'pending' | 'injected' | 'started_as_new_turn' | 'cancelled'
}

export interface ActiveTurnSnapshot {
  turnId: string
  events: EventSequence
  startedAt: string
}

export interface SessionSnapshot {
  sessionRevision: number
  phase: SessionPhase
  selector: string | null
  controls: ControlSnapshot[]
  pendingControls: ControlSnapshot[]
  activeTurn: ActiveTurnSnapshot | null
  activeCompaction: { startedAt: string } | null
  terminal: { status: TurnStatus; message: string | null } | null
}

export interface SettingsUpdateResult {
  selector: string | null
  applyTiming: 'next_turn'
  revision: number
}

export interface SessionReadResult {
  summary: ThreadSummary
  history: ThreadReadPage
  runtime: SessionSnapshot
}

export interface RedactedReasoningVariant {
  id: string
  enabled: boolean
  wireEffort: string | null
}

export interface RedactedModel {
  modelId: string
  displayName: string | null
  apiProtocol: string
  maxContextTokens: number | null
  maxOutputTokens: number | null
  reasoningVariants: RedactedReasoningVariant[]
  defaultVariant: string | null
  toolReasoningHistory: string | null
  thinkingWireFormat: string | null
}

export interface RedactedProvider {
  providerId: string
  displayName: string | null
  baseUrl: string
  credentialConfigured: boolean
  models: RedactedModel[]
}

export interface RedactedModelCatalog {
  configuration: 'ready' | 'missing' | 'invalid'
  message: string | null
  defaultSelector: string | null
  providers: RedactedProvider[]
  presets: ProviderConfigurationInput[]
}

export interface DirectoryEntry {
  name: string
  path: string
  kind: 'root' | 'parent' | 'directory' | 'file'
}

export interface WorkbenchBootstrap {
  sessionPhases: Record<string, SessionPhase>
  generation: string
  revision: number
  endpoint: { authority: string }
  workspaces: Workspace[]
  sessionsByWorkspace: Record<string, ThreadSummary[]>
  modelCatalog: RedactedModelCatalog
  execution: { fileAccess: 'full_local_access' }
}

export interface ActionReceipt {
  requestId: string
  accepted: boolean
  generation: string
  revision: number
  sessionId: string | null
  turnId: string | null
  control: ControlSnapshot | null
}

export interface RpcError {
  code: string
  message: string
  recovery: string
  preservedInput?: string
}

export interface RpcResponse<T> {
  version: number
  requestId: string
  ok: boolean
  generation: string
  revision: number
  result?: T
  error?: RpcError
}

export interface StreamEnvelope {
  version: number
  generation: string
  revision: number
  type:
    | 'ready'
    | 'workbench_changed'
    | 'session_changed'
    | 'turn_event'
    | 'session_settled'
    | 'resync_required'
  sessionId?: string
  payload: unknown
}

interface TurnIdentity { threadId: string; turnId: string }
interface ItemIdentity extends TurnIdentity { item: { itemId: string } }
interface EventTurn {
  threadId: string; turnId: string; status: TurnStatus
  usage?: {
    inputTokens: number; outputTokens: number; totalTokens: number
    cachedInputTokens: number; reasoningTokens: number
    usagePresent: boolean; usageComplete: boolean
  }
}
interface ToolIdentity extends TurnIdentity { toolCallId: string; toolName: string }
interface ProviderAttemptParams extends TurnIdentity {
  attempt: number; modelTurnOrdinal: number; provider: string; model: string; protocol: string
  status: RequestObservation['status']; attemptDurationMs: number | null
  inputTokens: number | null; outputTokens: number | null; cachedInputTokens: number | null
  request?: ModelRequestSnapshot
  errorCategory: string | null; diagnosticCode: string | null
  retryAfterMs: number | null; retryAfterSource: 'provider_header' | null
}

// 方法名决定载荷形状，消费者在对应分支直接读取字段。
export type TurnEventEnvelope = { sessionRevision: number } & (
  | { method: 'turn/started'; params: { turn: EventTurn; input: string; startedAt: string } }
  | { method: 'item/started'; params: ItemIdentity }
  | { method: 'item/completed'; params: ItemIdentity }
  | { method: 'item/agentMessage/delta'; params: ItemIdentity & { delta: string } }
  | { method: 'item/agentThinking/delta'; params: ItemIdentity & { delta: string } }
  | { method: 'item/agentThinking'; params: ItemIdentity & { text: string } }
  | { method: 'item/failed'; params: ItemIdentity & { error: string } }
  | { method: 'tool/execution/start'; params: ToolIdentity & { args: unknown; startedAt?: string } }
  | { method: 'tool/execution/update'; params: ToolIdentity & { args: unknown; partialResult: string } }
  | { method: 'tool/execution/end'; params: ToolIdentity & { result: { content: Array<{ type: 'text'; text: string }>; isError: boolean }; durationMs?: number } }
  | { method: 'agent/diagnostic'; params: TurnIdentity & { severity: 'info' | 'warning' | 'error'; code: string; message: string } }
  | { method: 'provider/attempt'; params: ProviderAttemptParams }
  | { method: 'turn/completed'; params: { turn: EventTurn } }
  | { method: 'turn/error'; params: TurnIdentity & { error: { stage: string; cause: string; message: string } } }
)

export function eventTurnId(event: TurnEventEnvelope): string {
  return 'turn' in event.params ? event.params.turn.turnId : event.params.turnId
}

export interface ProviderConfigurationInput {
  providerId: string
  displayName: string | null
  baseUrl: string
  models: Array<{
    modelId: string
    displayName: string | null
    apiProtocol: 'chat' | 'responses'
    maxContextTokens: number | null
    maxOutputTokens: number | null
    reasoningVariants: Array<{ id: string; enabled: boolean; wireEffort: string | null }>
    defaultVariant: string | null
    toolReasoningHistory: string | null
    thinkingWireFormat: string | null
  }>
  makeDefault: boolean
}

export interface DiscoveredModel {
  modelId: string
  displayName: string | null
  maxContextTokens: number | null
  maxOutputTokens: number | null
  reasoningVariants: RedactedReasoningVariant[]
  defaultVariant: string | null
  thinkingWireFormat: string | null
}

export interface ViewportAnchor {
  mode: 'following' | 'anchored'
  anchorItemId: string | null
  offset: number
}
