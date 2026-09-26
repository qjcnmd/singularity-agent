import { spawn } from 'node:child_process'
import { createInterface } from 'node:readline'
import { EventEmitter } from 'node:events'
import type { RpcResponse, StreamEnvelope } from '../src/protocol.generated.js'

/** Owns one Rust process. Payloads remain singularity_protocol values. */
export class Backend extends EventEmitter {
  readonly child
  readonly ready: Promise<StreamEnvelope>
  private nextId = 0
  private pending = new Map<number, { resolve: (value: RpcResponse) => void; reject: (error: Error) => void }>()
  private failure: Error | null = null
  private stopping = false
  private diagnostic = ''
  private readyResolve!: (frame: StreamEnvelope) => void
  private readyReject!: (error: Error) => void
  private startupTimer: ReturnType<typeof setTimeout>
  private exited: Promise<void>

  constructor(binary: string) {
    super()
    this.ready = new Promise((resolve, reject) => { this.readyResolve = resolve; this.readyReject = reject })
    this.child = spawn(binary, ['--app-server'], { windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] })
    this.startupTimer = setTimeout(() => this.fail(new Error('Rust AppServer 启动超时。')), 30_000)
    this.exited = new Promise(resolve => this.child.once('close', () => resolve()))
    this.child.on('error', error => this.fail(error))
    this.child.stdin.on('error', error => this.fail(error))
    this.child.stderr.setEncoding('utf8')
    this.child.stderr.on('data', (text: string) => {
      this.diagnostic = (this.diagnostic + text).slice(-4000)
      process.stderr.write(text)
    })
    createInterface({ input: this.child.stdout, crlfDelay: Infinity }).on('line', line => {
      try {
        const message = JSON.parse(line)
        if ('id' in message) {
          const pending = this.pending.get(message.id)
          this.pending.delete(message.id)
          pending?.resolve(message.response)
        } else {
          const frame = message as StreamEnvelope
          if (frame.type === 'ready') {
            clearTimeout(this.startupTimer)
            this.readyResolve(frame)
          }
          this.emit('frame', frame)
        }
      } catch (error) {
        this.fail(new Error(`Rust pipe response invalid: ${String(error)}`))
      }
    })
    this.child.once('exit', (code, signal) => {
      this.fail(new Error(`Rust AppServer 已退出 (${code ?? signal})。${this.diagnostic}`))
    })
  }

  async connect(): Promise<StreamEnvelope> {
    const frame = await this.ready
    if (this.failure || this.stopping) throw this.failure ?? new Error('AppServer 正在退出。')
    return frame
  }

  async rpc(request: unknown): Promise<RpcResponse> {
    await this.connect()
    const id = ++this.nextId
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject })
      this.child.stdin.write(`${JSON.stringify({ id, request })}\n`, error => {
        if (error) this.fail(error)
      })
    })
  }

  private fail(error: Error): void {
    if (this.failure) return
    this.failure = error
    clearTimeout(this.startupTimer)
    this.readyReject(error)
    for (const pending of this.pending.values()) pending.reject(error)
    this.pending.clear()
    if (!this.stopping) this.emit('failure', error)
  }

  async stop(): Promise<void> {
    this.stopping = true
    clearTimeout(this.startupTimer)
    this.child.stdin.end()
    // EOF cancels Rust work and lets it persist terminal facts. A stuck provider must not prevent exit.
    const timer = setTimeout(() => this.child.kill(), 10_000)
    await this.exited
    clearTimeout(timer)
  }
}
