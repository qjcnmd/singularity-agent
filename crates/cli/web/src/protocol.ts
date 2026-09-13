import type * as Wire from './protocol.generated'
export type * from './protocol.generated'
export const protocolVersion = 1 as const

export type ConnectionStatus = 'connecting' | 'ready' | 'recovering' | 'forbidden' | 'unavailable'
export type DeliveryIntent = 'steer' | 'follow_up'
export type TurnEventEnvelope = Wire.WorkbenchTurnEvent
export type HistoryToolResult = Extract<Wire.HistoryItem, { type: 'tool_result' }>

export type RpcMethod = keyof Wire.RpcContract
export type RpcParams<M extends RpcMethod> = Wire.RpcContract[M]['params']
export type RpcResult<M extends RpcMethod> = Wire.RpcContract[M]['result']
export type RpcResponse<M extends RpcMethod> = Omit<Wire.RpcResponse, 'result'> & { result?: RpcResult<M> }

export function eventTurnId(event: TurnEventEnvelope): string {
  if ('turn' in event.params) return event.params.turn.turnId
  if ('control' in event.params) return event.params.control.turnId
  return event.params.turnId
}

/** 用户消息的公开内容块身份：事件携带持久条目 id，公开投影统一加首个文本块后缀。 */
export function userMessageItemId(entryId: string): string {
  return `${entryId}:text:0`
}

export interface ViewportAnchor {
  mode: 'following' | 'anchored'
  anchorItemId: string | null
  offset: number
}
