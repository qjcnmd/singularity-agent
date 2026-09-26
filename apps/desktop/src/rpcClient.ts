import type {
  ConnectionStatus,
  RpcResponse,
  RpcMethod, RpcParams, RpcResult,
  StreamEnvelope,
} from './protocol'
import { protocolVersion } from './protocol'

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

/** 通道不可用或响应版本不符时，调用方保留连接失败状态。 */
export function isConnectionFailure(error: unknown): boolean {
  return error instanceof RpcFailure
    && (error.code === 'unavailable' || error.code === 'invalid_response')
}

declare global {
  interface Window {
    singularity: {
      rpc<M extends RpcMethod>(request: { version: number; method: M; params: RpcParams<M> }): Promise<RpcResponse<M>>
      connect(): Promise<StreamEnvelope>
      onFrame(listener: (frame: StreamEnvelope) => void): () => void
      onFailure(listener: () => void): () => void
    }
  }
}

export class RpcClient {
  private unsubscribe: (() => void) | null = null
  private unsubscribeFailure: (() => void) | null = null
  private stopped = true

  constructor(
    private readonly onFrame: (frame: StreamEnvelope) => void,
    private readonly onStatus: (status: ConnectionStatus) => void,
  ) {}

  start(): void {
    if (!this.stopped) return
    this.stopped = false
    this.unsubscribe = window.singularity.onFrame(frame => this.accept(frame))
    this.unsubscribeFailure = window.singularity.onFailure(() => this.onStatus('recovering'))
    this.onStatus('connecting')
    void window.singularity.connect().then(frame => {
      if (!this.stopped) this.accept(frame)
    }).catch(() => { if (!this.stopped) this.onStatus('recovering') })
  }

  stop(): void {
    this.stopped = true
    this.unsubscribe?.()
    this.unsubscribeFailure?.()
    this.unsubscribe = null
    this.unsubscribeFailure = null
  }

  async rpc<M extends RpcMethod>(method: M, params: RpcParams<M>): Promise<RpcResult<M>> {
    let envelope: RpcResponse<M>
    try {
      envelope = await window.singularity.rpc({ version: protocolVersion, method, params })
    } catch {
      this.onStatus('recovering')
      throw new RpcFailure('unavailable', '工作台后端不可用。', '请重新启动桌面应用。')
    }
    if (envelope.version !== protocolVersion) {
      this.onStatus('recovering')
      throw new RpcFailure('invalid_response', '工作台响应版本不匹配。', '请重新启动桌面应用。')
    }
    if (!envelope.ok || envelope.result === undefined) {
      const error = envelope.error
      throw new RpcFailure(error?.code ?? 'unknown', error?.message ?? '动作未被接受。', error?.recovery ?? '刷新当前任务后重试。')
    }
    return envelope.result
  }

  private accept(frame: StreamEnvelope): void {
    if (this.stopped) return
    if (frame.version !== protocolVersion) { this.onStatus('recovering'); return }
    this.onFrame(frame)
  }
}
