import { spawn } from 'node:child_process'
import { createInterface } from 'node:readline'
import { EventEmitter } from 'node:events'
import type { RpcResponse, StreamEnvelope } from '../src/protocol.generated.js'

/** 持有一个 Rust 进程，载荷始终是 singularity_protocol 的值。 */
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
  private exited: Promise<void>
  private compacting = new Set<string>()
  private stopTimer: ReturnType<typeof setTimeout> | undefined

  constructor(binary: string) {
    super()
    this.ready = new Promise((resolve, reject) => { this.readyResolve = resolve; this.readyReject = reject })
    this.child = spawn(binary, ['--app-server'], { windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] })
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
          if (frame.type === 'session_changed' || frame.type === 'session_settled') {
            if (frame.payload.phase === 'compacting') this.compacting.add(frame.sessionId)
            else this.compacting.delete(frame.sessionId)
          } else if (frame.type === 'app_changed') {
            this.compacting = new Set(Object.entries(frame.payload.sessionPhases)
              .filter(([, phase]) => phase === 'compacting').map(([id]) => id))
          }
          if (this.stopping) this.updateStopTimer()
          if (frame.type === 'ready') {
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
    this.readyReject(error)
    for (const pending of this.pending.values()) pending.reject(error)
    this.pending.clear()
    if (!this.stopping) this.emit('failure', error)
  }

  async stop(): Promise<void> {
    this.stopping = true
    this.child.stdin.end()
    this.updateStopTimer()
    await this.exited
    clearTimeout(this.stopTimer)
  }

  private updateStopTimer(): void {
    // 仅压缩期间暂停退出超时；普通任务仍有原来的 10 秒退出上限。
    if (this.compacting.size > 0) {
      clearTimeout(this.stopTimer)
      this.stopTimer = undefined
    } else if (!this.stopTimer) {
      this.stopTimer = setTimeout(() => this.child.kill(), 10_000)
    }
  }
}
