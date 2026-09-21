import type * as Wire from './protocol.generated'
export type * from './protocol.generated'
/** 握手版本与 Rust 同源：值在生成文件里，浏览器侧不手工维护。 */
export { protocolVersion } from './protocol.generated'

/** 连接生命周期只描述传输层确实会宣告的状态；RPC 失败码另见 RpcErrorCode。 */
export type ConnectionStatus = 'connecting' | 'ready' | 'recovering' | 'forbidden'
export type DeliveryIntent = Extract<Wire.ControlChannel, 'steer' | 'follow_up'>
type TurnEventEnvelope = Wire.TurnEventEnvelope

export type RpcMethod = keyof Wire.RpcContract
export type RpcParams<M extends RpcMethod> = Wire.RpcContract[M]['params']
export type RpcResult<M extends RpcMethod> = Wire.RpcContract[M]['result']
export type RpcResponse<M extends RpcMethod> = Omit<Wire.RpcResponse, 'result'> & { result?: RpcResult<M> }

/** 事件归属的 turn；排队中的控制尚无关联 turn 时为 null。 */
export function eventTurnId(event: TurnEventEnvelope): string | null {
  if ('turn' in event.params) return event.params.turn.turnId
  if ('control' in event.params) return event.params.control.turnId
  return event.params.turnId
}

export interface ViewportAnchor {
  mode: 'following' | 'anchored'
  anchorItemId: string | null
  offset: number
}
