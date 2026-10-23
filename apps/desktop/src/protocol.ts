import type * as Wire from './protocol.generated'
export type * from './protocol.generated'

/** 渲染进程与后端管道的连接状态。 */
export type ConnectionStatus = 'connecting' | 'ready' | 'recovering'

export type RpcMethod = keyof Wire.RpcContract
export type RpcParams<M extends RpcMethod> = Wire.RpcContract[M]['params']
export type RpcResult<M extends RpcMethod> = Wire.RpcContract[M]['result']
export type RpcResponse<M extends RpcMethod> =
  | { type: 'success'; result: RpcResult<M> }
  | Extract<Wire.RpcResponse, { type: 'error' }>

export type ViewportAnchor =
  | { mode: 'following'; anchorItemId: null; offset: 0 }
  | { mode: 'anchored'; anchorItemId: string; offset: number }
