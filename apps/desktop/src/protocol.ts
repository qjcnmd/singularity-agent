import type * as Wire from './protocol.generated'
export type * from './protocol.generated'

/** 连接生命周期只描述传输层确实会宣告的状态；RPC 失败码另见 RpcErrorCode。 */
export type ConnectionStatus = 'connecting' | 'ready' | 'recovering'
type TurnEventEnvelope = Wire.TurnEventEnvelope

export type RpcMethod = keyof Wire.RpcContract
export type RpcParams<M extends RpcMethod> = Wire.RpcContract[M]['params']
export type RpcResult<M extends RpcMethod> = Wire.RpcContract[M]['result']
export type RpcResponse<M extends RpcMethod> =
  | { type: 'success'; result: RpcResult<M> }
  | Extract<Wire.RpcResponse, { type: 'error' }>

/** 事件归属的 turn；会话控制状态变更不绑定 turn。 */
export function eventTurnId(event: TurnEventEnvelope): string | null {
  if ('turn' in event.params) return event.params.turn.turnId
  if (event.method === 'turn/sessionStateChanged') return null
  return event.params.turnId
}

export interface ViewportAnchor {
  mode: 'following' | 'anchored'
  anchorItemId: string | null
  /** 条目所属的历史回合；null 为前导记录，缺省表示旧的阅读位置。 */
  turnId?: string | null
  offset: number
}
