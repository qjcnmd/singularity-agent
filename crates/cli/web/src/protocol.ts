import type { EventSequence } from './eventLog'
import type * as Wire from './protocol.generated'
export type * from './protocol.generated'
export const protocolVersion = 1 as const

export type ConnectionStatus = 'connecting' | 'ready' | 'recovering' | 'forbidden' | 'unavailable'
export type DeliveryIntent = 'steer' | 'follow_up'
export type TurnEventEnvelope = Wire.WorkbenchTurnEvent
export type HistoryToolResult = Extract<Wire.HistoryItem, { type: 'tool_result' }>

// EventSequence is a client view, never a network payload.
export type ActiveTurnSnapshot = Omit<Wire.ActiveTurnSnapshot, 'events'> & { events: EventSequence }
export type SessionSnapshot = Omit<Wire.SessionSnapshot, 'activeTurn'> & { activeTurn: ActiveTurnSnapshot | null }
export type SessionReadResult = Omit<Wire.SessionReadResult, 'runtime'> & { runtime: SessionSnapshot }

export type RpcMethod = keyof Wire.RpcContract
export type RpcParams<M extends RpcMethod> = Wire.RpcContract[M]['params']
export type RpcResult<M extends RpcMethod> = Wire.RpcContract[M]['result']
export type RpcResponse<M extends RpcMethod> = Omit<Wire.RpcResponse, 'result'> & { result?: RpcResult<M> }

export function eventTurnId(event: TurnEventEnvelope): string {
  return 'turn' in event.params ? event.params.turn.turnId : event.params.turnId
}

export interface ViewportAnchor {
  mode: 'following' | 'anchored'
  anchorItemId: string | null
  offset: number
}
