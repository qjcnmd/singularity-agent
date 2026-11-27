import type {
  RpcResponse,
  RpcMethod, RpcParams, RpcResult,
  StreamEnvelope,
} from './protocol'

export class RpcFailure extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'RpcFailure'
  }
}

declare global {
  interface Window {
    desktop: {
      rpc<M extends RpcMethod>(request: { method: M; params: RpcParams<M> }): Promise<RpcResponse<M>>
      setTheme(theme: 'light' | 'dark'): Promise<void>
      connect(): Promise<StreamEnvelope>
      onFrame(listener: (frame: StreamEnvelope) => void): void
      pathForFile(file: File): string
    }
  }
}

export class RpcClient {
  constructor(private readonly onFrame: (frame: StreamEnvelope) => void) {}

  start(): void {
    window.desktop.onFrame(frame => this.onFrame(frame))
    void window.desktop.connect().then(frame => this.onFrame(frame), () => {})
  }

  async rpc<M extends RpcMethod>(method: M, params: RpcParams<M>): Promise<RpcResult<M>> {
    let envelope: RpcResponse<M>
    try {
      envelope = await window.desktop.rpc({ method, params })
    } catch {
      throw new RpcFailure('工作台后端不可用。')
    }
    if (envelope.type === 'error') {
      throw new RpcFailure(envelope.error.message)
    }
    return envelope.result
  }
}
