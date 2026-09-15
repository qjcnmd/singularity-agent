import type * as Wire from './protocol.generated'
export type * from './protocol.generated'
/** 与 Rust 的 WORKBENCH_PROTOCOL_VERSION 相同：浏览器无法引用它，改动一侧必须同步另一侧。 */
export const protocolVersion = 2 as const

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

/**
 * 用户消息的公开内容块身份：事件携带持久条目 id，公开投影统一加首个文本块后缀。
 * 后缀规则来自 Rust 侧的 `singularity_agent::session::format::text_item_id(id, 0)`；
 * 浏览器不能调用它，只能按同一规则构造，改动一侧必须同步另一侧。
 */
export function userMessageItemId(entryId: string): string {
  return `${entryId}:text:0`
}

export interface ViewportAnchor {
  mode: 'following' | 'anchored'
  anchorItemId: string | null
  offset: number
}
