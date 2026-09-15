import type * as Wire from './protocol.generated'
export type * from './protocol.generated'
/** 与 Rust 的 WORKBENCH_PROTOCOL_VERSION 相同：浏览器无法引用它，改动一侧必须同步另一侧。 */
export const protocolVersion = 3 as const

/** 连接生命周期只描述传输层确实会宣告的状态；RPC 失败码另见 RpcErrorCode。 */
export type ConnectionStatus = 'connecting' | 'ready' | 'recovering' | 'forbidden'
export type DeliveryIntent = 'steer' | 'follow_up'
export type TurnEventEnvelope = Wire.WorkbenchTurnEvent

export type RpcMethod = keyof Wire.RpcContract
export type RpcParams<M extends RpcMethod> = Wire.RpcContract[M]['params']
export type RpcResult<M extends RpcMethod> = Wire.RpcContract[M]['result']
export type RpcResponse<M extends RpcMethod> = Omit<Wire.RpcResponse, 'result'> & { result?: RpcResult<M> }

export function eventTurnId(event: TurnEventEnvelope): string {
  if ('turn' in event.params) return event.params.turn.turnId
  if ('control' in event.params) return event.params.control.turnId
  return event.params.turnId
}

export interface ViewportAnchor {
  mode: 'following' | 'anchored'
  anchorItemId: string | null
  offset: number
}
