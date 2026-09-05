import type {
  ConnectionStatus,
  RpcResponse,
  StreamEnvelope,
} from './protocol'
import { protocolVersion } from './protocol'

type StreamListener = (frame: StreamEnvelope) => void
type StatusListener = (status: ConnectionStatus) => void
const maxReconnectAttempts = 6

export class RpcFailure extends Error {
  readonly code: string
  readonly recovery: string
  readonly preservedInput?: string

  constructor(code: string, message: string, recovery: string, preservedInput?: string) {
    super(message)
    this.name = 'RpcFailure'
    this.code = code
    this.recovery = recovery
    this.preservedInput = preservedInput
  }
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

  retry(): void {
    this.stopped = false
    this.reconnectAttempt = 0
    if (this.reconnectTimer !== null) window.clearTimeout(this.reconnectTimer)
    this.reconnectTimer = null
    this.socket?.close()
    this.socket = null
    this.connect()
  }

  async rpc<T>(method: string, params: Record<string, unknown>): Promise<T> {
    const requestId = crypto.randomUUID()
    let response: Response
    try {
      response = await fetch('/api/rpc', {
        method: 'POST',
        credentials: 'same-origin',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ version: protocolVersion, requestId, method, params }),
      })
    } catch {
      this.onStatus('unavailable')
      throw new RpcFailure(
        'unavailable',
        '工作台连接中断，动作结果未知。',
        '恢复连接并刷新当前 Session；系统不会自动重放这次动作。',
      )
    }
    if (response.status === 401) {
      this.onStatus('unauthorized')
      throw new RpcFailure('unauthorized', '浏览器会话无效。', '从启动终端重新打开入口。')
    }
    let envelope: RpcResponse<T>
    try {
      envelope = (await response.json()) as RpcResponse<T>
    } catch {
      throw new RpcFailure('invalid_response', 'Host 返回了无法读取的响应。', '刷新页面后重试。')
    }
    if (envelope.version !== protocolVersion || envelope.requestId !== requestId) {
      throw new RpcFailure('invalid_response', 'Host 响应版本或请求标识不匹配。', '刷新页面后重试。')
    }
    if (!envelope.ok || envelope.result === undefined) {
      const error = envelope.error
      throw new RpcFailure(
        error?.code ?? 'unknown',
        error?.message ?? '动作未被接受。',
        error?.recovery ?? '刷新当前 Session 后重试。',
        error?.preservedInput,
      )
    }
    return envelope.result
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
      if (frame.type === 'ready') {
        this.reconnectAttempt = 0
        this.onStatus('ready')
      }
      this.onFrame(frame)
    })
    socket.addEventListener('close', (event) => {
      if (this.socket !== socket) return
      this.socket = null
      if (this.stopped) return
      if (event.code === 1008) {
        this.onStatus('unauthorized')
        return
      }
      this.scheduleReconnect()
    })
    socket.addEventListener('error', () => socket.close())
  }

  private scheduleReconnect(): void {
    if (this.reconnectTimer !== null || this.stopped) return
    if (this.reconnectAttempt >= maxReconnectAttempts) {
      this.onStatus('unavailable')
      return
    }
    this.onStatus('recovering')
    const delay = Math.min(8_000, 400 * 2 ** this.reconnectAttempt)
    this.reconnectAttempt += 1
    this.reconnectTimer = window.setTimeout(() => {
      this.reconnectTimer = null
      this.connect()
    }, delay)
  }
}
