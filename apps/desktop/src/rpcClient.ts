import type {
  ConnectionStatus,
  RpcResponse,
  RpcMethod, RpcParams, RpcResult,
  StreamEnvelope,
} from './protocol'

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

declare global {
  interface Window {
    singularity: {
      rpc<M extends RpcMethod>(request: { method: M; params: RpcParams<M> }): Promise<RpcResponse<M>>
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
  private status: ConnectionStatus = 'connecting'

  constructor(
    private readonly onFrame: (frame: StreamEnvelope) => void,
    private readonly onStatus: (status: ConnectionStatus) => void,
  ) {}

  start(): void {
    if (!this.stopped) return
    this.stopped = false
    this.unsubscribe = window.singularity.onFrame(frame => this.accept(frame))
    this.unsubscribeFailure = window.singularity.onFailure(() => this.setStatus('recovering'))
    this.setStatus('connecting')
    void window.singularity.connect().then(frame => {
      if (this.stopped || this.status === 'recovering') return
      this.setStatus('ready')
      this.accept(frame)
    })
      .catch(() => { if (!this.stopped) this.setStatus('recovering') })
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
      envelope = await window.singularity.rpc({ method, params })
    } catch {
      this.setStatus('recovering')
      throw new RpcFailure('unavailable', '工作台后端不可用。', '请重新启动桌面应用。')
    }
    if (envelope.type === 'error') {
      const error = envelope.error
      throw new RpcFailure(error.code, error.message, error.recovery)
    }
    return envelope.result
  }

  private setStatus(status: ConnectionStatus): void {
    this.status = status
    this.onStatus(status)
  }

  private accept(frame: StreamEnvelope): void {
    if (this.stopped) return
    this.onFrame(frame)
  }
}
