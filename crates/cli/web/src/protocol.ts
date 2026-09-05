export const protocolVersion = 1 as const

export type ConnectionStatus =
  | 'connecting'
  | 'ready'
  | 'recovering'
  | 'unauthorized'
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

export type HistoryItem =
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
  events: TurnEventEnvelope[]
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
  applyTiming: 'nothing_to_apply' | 'next_turn'
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
  apiProtocol: string
  maxContextTokens: number | null
  maxOutputTokens: number | null
  reasoningVariants: RedactedReasoningVariant[]
  defaultVariant: string | null
  toolReasoningHistory: string | null
}

export interface RedactedProvider {
  providerId: string
  baseUrl: string
  credentialConfigured: boolean
  models: RedactedModel[]
}

export interface RedactedModelCatalog {
  configuration: 'ready' | 'missing' | 'invalid'
  message: string | null
  defaultSelector: string | null
  providers: RedactedProvider[]
}

export interface DirectoryEntry {
  name: string
  path: string
  kind: 'root' | 'parent' | 'directory' | 'file'
}

export interface CommandDescriptor {
  name: string
  description: string
  availability: string
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
  commands: CommandDescriptor[]
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

export interface TurnEventEnvelope {
  sessionRevision: number
  method: string
  params: Record<string, unknown>
}

export interface ProviderConfigurationInput {
  providerId: string
  baseUrl: string
  models: Array<{
    modelId: string
    apiProtocol: 'chat' | 'responses'
    maxContextTokens: number | null
    maxOutputTokens: number | null
    reasoningVariants: Array<{ id: string; enabled: boolean; wireEffort: string | null }>
    defaultVariant: string | null
    toolReasoningHistory: string | null
  }>
  makeDefault: boolean
}

export interface ViewportAnchor {
  mode: 'following' | 'anchored'
  anchorItemId: string | null
  offset: number
  unseenCount: number
}
