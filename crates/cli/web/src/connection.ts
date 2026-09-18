import type {
  ConnectionStatus,
  RpcResponse,
  RpcMethod, RpcParams, RpcResult,
  StreamEnvelope,
} from './protocol'
import { protocolVersion } from './protocol'

export type StreamListener = (frame: StreamEnvelope) => void
export type StatusListener = (status: ConnectionStatus) => void

export interface WorkbenchTransport {
  start(): void
  stop(): void
  reconnect(): void
  rpc<M extends RpcMethod>(method: M, params: RpcParams<M>): Promise<RpcResult<M>>
}

export class RpcFailure extends Error {
  readonly code: string
  readonly recovery: string

  constructor(code: string, message: string, recovery: string) {
    super(message)
    this.name = 'RpcFailure'
    this.code = code
    this.recovery = recovery
  }
}

/** 连接级失败码：它们描述宿主通道本身的状态（不可达/被拒绝），不是某个业务
 *  动作的结果。只有本模块的 rpc 会合成这两个码；调用方必须据此保留或更新
 *  连接状态，不能把它降级成读侧或动作级的业务错误。 */
export function isConnectionFailure(error: unknown): boolean {
  return error instanceof RpcFailure && (error.code === 'unavailable' || error.code === 'forbidden')
}

export class WorkbenchConnection {
  private socket: WebSocket | null = null
  private reconnectTimer: number | null = null
  private reconnectAttempt = 0
  private stopped = false

  constructor(
    private readonly onFrame: StreamListener,
    private readonly onStatus: StatusListener,
  ) {}

  start(): void {
    if (this.socket !== null || this.reconnectTimer !== null) return
    this.stopped = false
    this.connect()
  }

  stop(): void {
    this.stopped = true
    if (this.reconnectTimer !== null) window.clearTimeout(this.reconnectTimer)
    this.reconnectTimer = null
    this.socket?.close()
    this.socket = null
  }

  async rpc<M extends RpcMethod>(method: M, params: RpcParams<M>): Promise<RpcResult<M>> {
    const requestId = crypto.randomUUID()
    let response: Response
    try {
      response = await fetch('/api/rpc', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ version: protocolVersion, requestId, method, params }),
      })
    } catch {
      this.reconnect()
      throw new RpcFailure(
        'unavailable',
        '暂时无法连接工作台。',
        '连接恢复后可重试。',
      )
    }
    if (response.status === 403) {
      this.onStatus('forbidden')
      throw new RpcFailure('forbidden', '请求来源不符合工作台要求。', '请使用启动终端显示的本机地址直接打开工作台。')
    }
    let envelope: RpcResponse<M>
    try {
      envelope = (await response.json()) as RpcResponse<M>
    } catch {
      // 已经拿到 HTTP 响应但读不出信封：服务端变更可能已经生效，结果不确定。
      // 与 fetch 失败走同一条重连 + 基线重读的校准路径，绝不重发 mutation。
      this.reconnect()
      throw new RpcFailure('invalid_response', 'Host 返回了无法读取的响应。', '刷新页面后重试。')
    }
    if (envelope.version !== protocolVersion || envelope.requestId !== requestId) {
      // 版本或请求标识不符同样只说明「这次响应不可信」，不代表变更没有生效。
      this.reconnect()
      throw new RpcFailure('invalid_response', 'Host 响应版本或请求标识不匹配。', '刷新页面后重试。')
    }
    if (!envelope.ok || envelope.result === undefined) {
      const error = envelope.error
      throw new RpcFailure(
        error?.code ?? 'unknown',
        error?.message ?? '动作未被接受。',
        error?.recovery ?? '刷新当前 Session 后重试。',
      )
    }
    return envelope.result
  }

  /** RPC 响应不确定时，复用事件重连与 baseline sync。 */
  reconnect(): void {
    const socket = this.socket
    this.socket = null
    socket?.close()
    this.scheduleReconnect()
  }

  private connect(): void {
    if (this.stopped) return
    this.onStatus(this.reconnectAttempt === 0 ? 'connecting' : 'recovering')
    const scheme = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    const socket = new WebSocket(`${scheme}//${window.location.host}/api/events`)
    this.socket = socket
    socket.addEventListener('message', (event: MessageEvent<string>) => {
      if (this.socket !== socket || this.stopped) return
      let frame: StreamEnvelope
      try {
        frame = JSON.parse(event.data) as StreamEnvelope
      } catch {
        socket.close()
        return
      }
      if (frame.version !== protocolVersion) {
        socket.close()
        return
      }
      // ready 帧只重置传输层退避；应用就绪状态由 store 在其
      // baseline sync 收敛后自行持有。
      if (frame.type === 'ready') this.reconnectAttempt = 0
      this.onFrame(frame)
    })
    socket.addEventListener('close', () => {
      if (this.socket !== socket) return
      this.socket = null
      if (this.stopped) return
      this.scheduleReconnect()
    })
    socket.addEventListener('error', () => socket.close())
  }

  private scheduleReconnect(): void {
    if (this.reconnectTimer !== null || this.stopped) return
    this.onStatus('recovering')
    const delay = Math.min(8_000, 400 * 2 ** this.reconnectAttempt)
    this.reconnectAttempt = Math.min(this.reconnectAttempt + 1, 5)
    this.reconnectTimer = window.setTimeout(() => {
      this.reconnectTimer = null
      this.connect()
    }, delay)
  }
}
